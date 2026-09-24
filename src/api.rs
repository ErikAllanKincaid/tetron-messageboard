//! HTTP handlers and shared state.
//!
//! Access control is the mesh itself: the server binds only to the
//! network's mesh IP (main.rs), so every request already comes from an
//! admitted peer, and the source IP -- authenticated by tetron -- is the
//! poster's identity. No login layer, no ownership checks. Identity
//! (hostname) is resolved server-side from the live roster, never
//! self-reported by the client.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Multipart, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use tokio::sync::{Mutex, RwLock};

use crate::ratelimit::RateLimiter;
use crate::roster::RosterView;
use crate::store::{Attachment, Message, Store};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<Mutex<Store>>,
    pub limiter: Arc<Mutex<RateLimiter>>,
    pub roster: Arc<RwLock<RosterView>>,
    pub network: String,
    pub installed_by_coordinator: bool,
    pub max_attachment_bytes: u64,
    pub max_storage_bytes: u64,
    pub eviction_grace_secs: u64,
    pub version: &'static str,
}

fn err(code: StatusCode, msg: impl Into<String>) -> Response {
    (code, Json(json!({ "error": msg.into() }))).into_response()
}

fn message_to_json(m: &Message, roster: &RosterView, viewer_ip: &str) -> Value {
    json!({
        "id": m.id,
        "ts": m.ts,
        "ip": m.ip,
        "host": roster.label(&m.ip),
        "mine": m.ip == viewer_ip,
        "text": m.text,
        "attachment": m.attachment.as_ref().map(|a| json!({
            "hash": a.hash,
            "name": a.name,
            "size": a.size,
            "content_type": a.content_type,
        })),
        "deleted": m.deleted,
        "deleted_by": m.deleted_by_ip.as_ref().map(|ip| roster.label(ip)),
        "attachment_evicted": m.attachment_evicted,
    })
}

/// `GET /health` -- the network-wide discovery probe reads this. Cheap,
/// no roster round-trip: the network name and coordinator flag were fixed
/// at install time.
pub async fn health(State(st): State<AppState>) -> Response {
    Json(json!({
        "service": "tetron-messageboard",
        "network": st.network,
        "installed_by_coordinator": st.installed_by_coordinator,
        "version": st.version,
    }))
    .into_response()
}

/// `GET /api/messages` -- the 10s poll target. The viewer's own mesh IP is
/// the request source, so each message is tagged `mine` from the viewer's
/// perspective and the header carries the viewer's resolved hostname.
pub async fn list_messages(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    let viewer_ip = addr.ip().to_string();
    let roster = st.roster.read().await;
    let store = st.store.lock().await;
    let messages: Vec<Value> = store
        .messages()
        .iter()
        .map(|m| message_to_json(m, &roster, &viewer_ip))
        .collect();
    Json(json!({
        "network": roster.network,
        "subnet": roster.subnet,
        "host": roster.my_hostname,
        "installed_by_coordinator": st.installed_by_coordinator,
        "viewer": { "ip": viewer_ip, "host": roster.label(&viewer_ip) },
        "storage": {
            "max_bytes": st.max_storage_bytes,
            "used_bytes": attachment_bytes_used(&store),
        },
        "limits": { "max_attachment_bytes": st.max_attachment_bytes },
        "messages": messages,
    }))
    .into_response()
}

fn attachment_bytes_used(store: &Store) -> u64 {
    // Distinct hashes only (dedup): the same file referenced twice is one
    // file on disk.
    let mut seen = std::collections::HashSet::new();
    let mut total = 0u64;
    for m in store.messages() {
        if let Some(a) = &m.attachment
            && seen.insert(a.hash.clone())
        {
            total += a.size;
        }
    }
    total
}

/// `POST /api/messages` -- multipart: an optional `text` field and an
/// optional `file` (image) field. At least one is required.
pub async fn post_message(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    mut multipart: Multipart,
) -> Response {
    let ip = addr.ip();
    if !st.limiter.lock().await.check(ip) {
        return err(StatusCode::TOO_MANY_REQUESTS, "slow down -- too many posts in a short time");
    }

    let mut text: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut file_name: Option<String> = None;

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return err(StatusCode::BAD_REQUEST, format!("malformed upload: {e}")),
        };
        match field.name() {
            Some("text") => {
                text = field.text().await.ok().map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
            }
            Some("file") => {
                file_name = field.file_name().map(|s| s.to_string());
                match field.bytes().await {
                    Ok(b) if !b.is_empty() => file_bytes = Some(b.to_vec()),
                    Ok(_) => {}
                    Err(e) => return err(StatusCode::BAD_REQUEST, format!("failed to read upload: {e}")),
                }
            }
            _ => {}
        }
    }

    let mut attachment: Option<Attachment> = None;
    if let Some(bytes) = file_bytes {
        if bytes.len() as u64 > st.max_attachment_bytes {
            return err(
                StatusCode::PAYLOAD_TOO_LARGE,
                format!(
                    "attachment is {:.1} MB; the limit is {} MB",
                    bytes.len() as f64 / 1_048_576.0,
                    st.max_attachment_bytes / 1_048_576
                ),
            );
        }
        let Some(content_type) = sniff_image(&bytes) else {
            return err(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "only image uploads are accepted (PNG, JPEG, WebP, GIF)",
            );
        };
        let store = st.store.lock().await;
        let hash = match store.store_attachment(&bytes) {
            Ok(h) => h,
            Err(e) => return err(StatusCode::INTERNAL_SERVER_ERROR, format!("failed to store attachment: {e}")),
        };
        drop(store);
        attachment = Some(Attachment {
            hash,
            name: sanitize_name(file_name.as_deref().unwrap_or("image")),
            size: bytes.len() as u64,
            content_type: content_type.to_string(),
        });
    }

    if text.is_none() && attachment.is_none() {
        return err(StatusCode::BAD_REQUEST, "a message needs text or an image");
    }

    let created = {
        let mut store = st.store.lock().await;
        let m = store.post(ip.to_string(), text, attachment);
        // Sweep only when this post added an attachment.
        if m.attachment.is_some()
            && let Err(e) = store.enforce_quota(st.max_storage_bytes, st.eviction_grace_secs)
        {
            eprintln!("warning: quota sweep failed: {e}");
        }
        m
    };

    let roster = st.roster.read().await;
    (StatusCode::CREATED, Json(message_to_json(&created, &roster, &ip.to_string()))).into_response()
}

/// `DELETE /api/messages/{id}` -- soft-delete. No ownership check: mesh
/// reachability is the authorization. The requester's mesh IP is recorded
/// as the deleter for the ghost's `[deleted by <host>]` label.
pub async fn delete_message(
    State(st): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Path(id): Path<u64>,
) -> Response {
    let mut store = st.store.lock().await;
    if store.delete(id, addr.ip().to_string()) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        err(StatusCode::NOT_FOUND, "no such message (or already deleted)")
    }
}

/// `GET /attachments/{hash}` -- serve a stored image with locked-down
/// headers. Only validated raster images are ever stored, and they are
/// served inline with `nosniff` + a `sandbox` CSP so a disguised upload
/// could never execute as script even if one slipped past validation.
pub async fn serve_attachment(State(st): State<AppState>, Path(hash): Path<String>) -> Response {
    // blake3 hex is exactly 64 lowercase hex chars -- reject anything else
    // outright, which also closes any path-traversal attempt.
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return err(StatusCode::BAD_REQUEST, "invalid attachment id");
    }

    // Resolve display metadata from a live message that references this
    // hash. A hash no live message references (orphaned by deletion) is not
    // served, even if the file lingers before the next sweep.
    let (content_type, name) = {
        let store = st.store.lock().await;
        let found = store.messages().iter().find_map(|m| {
            m.attachment.as_ref().filter(|a| a.hash == hash).map(|a| (a.content_type.clone(), a.name.clone()))
        });
        match found {
            Some(v) => v,
            None => return err(StatusCode::NOT_FOUND, "attachment not found"),
        }
    };

    let path = st.store.lock().await.attachment_path(&hash);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(_) => return err(StatusCode::NOT_FOUND, "attachment not found"),
    };

    (
        [
            (header::CONTENT_TYPE, content_type),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
            (header::CONTENT_SECURITY_POLICY, "default-src 'none'; sandbox".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("inline; filename=\"{}\"", name.replace('"', "")),
            ),
            (header::CACHE_CONTROL, "public, max-age=31536000, immutable".to_string()),
        ],
        bytes,
    )
        .into_response()
}

/// Identify a raster image by magic bytes. The filename extension and any
/// client-supplied content type are ignored -- an SVG or HTML file renamed
/// `.png` must not pass. Returns the real content type, or `None` to reject.
fn sniff_image(b: &[u8]) -> Option<&'static str> {
    if b.len() >= 8 && b[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A] {
        return Some("image/png");
    }
    if b.len() >= 3 && b[..3] == [0xFF, 0xD8, 0xFF] {
        return Some("image/jpeg");
    }
    if b.len() >= 6 && (&b[..6] == b"GIF87a" || &b[..6] == b"GIF89a") {
        return Some("image/gif");
    }
    if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

/// Keep only the basename and a conservative character set for display, so
/// an uploaded filename can never carry a path or control characters.
fn sanitize_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let cleaned: String = base
        .chars()
        .filter(|c| !c.is_control())
        .take(120)
        .collect();
    if cleaned.trim().is_empty() { "image".to_string() } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_real_formats_and_rejects_others() {
        assert_eq!(sniff_image(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0]), Some("image/png"));
        assert_eq!(sniff_image(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("image/jpeg"));
        assert_eq!(sniff_image(b"GIF89a....."), Some("image/gif"));
        let mut webp = b"RIFF____WEBPVP8 ".to_vec();
        webp.truncate(16);
        assert_eq!(sniff_image(&webp), Some("image/webp"));
        // An SVG (the stored-XSS vector) must be rejected.
        assert_eq!(sniff_image(b"<svg xmlns=\"http://www.w3.org/2000/svg\">"), None);
        assert_eq!(sniff_image(b"<!DOCTYPE html>"), None);
    }

    #[test]
    fn sanitize_strips_paths_and_control_chars() {
        assert_eq!(sanitize_name("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_name("a\nb.png"), "ab.png");
        assert_eq!(sanitize_name(""), "image");
    }
}

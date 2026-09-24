//! Message log and attachment storage.
//!
//! **Messages**: an NDJSON (JSON-Lines) file, `messages.ndjson`. Not sqlite
//! -- this is a single long-running service process, so every write already
//! funnels through one in-process lock and sqlite's concurrency guarantees
//! buy nothing here; it also keeps the zero-embedded-DB posture every other
//! tetron component shares (`InviteStore` TOML, `peercache.msgpack`). The
//! whole log is held in memory behind a [`tokio::sync::Mutex`] and mirrored
//! to disk on each mutation; reads serve from memory.
//!
//! - Append on post.
//! - **Soft-delete**: the row stays, its body/attachment is cleared and it
//!   becomes a `[deleted by <host>]` ghost. Rewrite-on-delete (read all,
//!   mutate, write a temp file, atomic rename) -- the same atomic-write
//!   technique tetron core's `InviteStore` uses. O(n), negligible at this
//!   addon's personal-fleet scale.
//!
//! **Attachments**: content-addressed files at `attachments/<blake3-hash>`,
//! with the message row holding only the hash + display metadata. Free
//! dedup (the same image posted twice costs one file) and a trivial
//! eviction sweep: over quota, delete the largest files older than the
//! grace period until back under, marking their messages as evicted (the
//! post survives, the image becomes a placeholder).

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config;

#[derive(Serialize, Deserialize, Clone)]
pub struct Attachment {
    /// blake3 hash of the bytes; also the on-disk filename.
    pub hash: String,
    /// Original filename, for display and the download `filename=`.
    pub name: String,
    pub size: u64,
    /// Validated raster content type (image/png|jpeg|webp|gif). Never
    /// trusted from the client -- set from magic bytes in api.rs.
    pub content_type: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Message {
    pub id: u64,
    /// Unix seconds.
    pub ts: u64,
    /// Source mesh IP of the poster, authenticated by tetron itself.
    /// Hostname is resolved from the live roster at read time, never stored.
    pub ip: String,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub attachment: Option<Attachment>,
    /// Soft-delete ghost: body/attachment cleared, `deleted_by_ip` set.
    #[serde(default)]
    pub deleted: bool,
    #[serde(default)]
    pub deleted_by_ip: Option<String>,
    /// The attachment was reclaimed by the quota sweep; the post is kept and
    /// shows a "[attachment removed -- storage limit]" placeholder.
    #[serde(default)]
    pub attachment_evicted: bool,
}

pub struct Store {
    path: PathBuf,
    attach_dir: PathBuf,
    messages: Vec<Message>,
    next_id: u64,
}

pub fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Store {
    /// Load the log from disk (empty on first run). Malformed lines are
    /// skipped with a warning rather than failing the whole service -- one
    /// corrupt line should not take the board down.
    pub fn load() -> Result<Store> {
        let path = config::messages_path()?;
        let attach_dir = config::attachments_dir()?;
        let mut messages = Vec::new();
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("failed to read {}", path.display()))?;
            for (n, line) in raw.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Message>(line) {
                    Ok(m) => messages.push(m),
                    Err(e) => eprintln!("skipping malformed message on line {}: {e}", n + 1),
                }
            }
        }
        let next_id = messages.iter().map(|m| m.id).max().unwrap_or(0) + 1;
        Ok(Store { path, attach_dir, messages, next_id })
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    fn rewrite(&self) -> Result<()> {
        // Atomic replace: write a temp file in the same directory, then
        // rename over the original (same technique as InviteStore).
        let tmp = self.path.with_extension("ndjson.tmp");
        let mut f = std::fs::File::create(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        for m in &self.messages {
            let line = serde_json::to_string(m)?;
            writeln!(f, "{line}")?;
        }
        f.sync_all().ok();
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("failed to rename {} -> {}", tmp.display(), self.path.display()))?;
        Ok(())
    }

    fn append(&self, m: &Message) -> Result<()> {
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        writeln!(f, "{}", serde_json::to_string(m)?)?;
        Ok(())
    }

    /// Persist an attachment's bytes content-addressed. Returns the hash.
    /// A byte-identical upload reuses the existing file (dedup).
    pub fn store_attachment(&self, bytes: &[u8]) -> Result<String> {
        let hash = blake3::hash(bytes).to_hex().to_string();
        let file = self.attach_dir.join(&hash);
        if !file.exists() {
            std::fs::write(&file, bytes)
                .with_context(|| format!("failed to write attachment {}", file.display()))?;
        }
        Ok(hash)
    }

    pub fn attachment_path(&self, hash: &str) -> PathBuf {
        self.attach_dir.join(hash)
    }

    pub fn post(&mut self, ip: String, text: Option<String>, attachment: Option<Attachment>) -> Message {
        let m = Message {
            id: self.next_id,
            ts: now_secs(),
            ip,
            text,
            attachment,
            deleted: false,
            deleted_by_ip: None,
            attachment_evicted: false,
        };
        self.next_id += 1;
        self.messages.push(m.clone());
        if let Err(e) = self.append(&m) {
            eprintln!("warning: failed to append message: {e}");
        }
        m
    }

    /// Soft-delete: keep the row, clear its content, record the deleter.
    /// No ownership check -- mesh reachability is the authorization (see the
    /// design doc's moderation section). Returns false if the id is unknown
    /// or already deleted.
    pub fn delete(&mut self, id: u64, by_ip: String) -> bool {
        let Some(m) = self.messages.iter_mut().find(|m| m.id == id && !m.deleted) else {
            return false;
        };
        m.deleted = true;
        m.deleted_by_ip = Some(by_ip);
        m.text = None;
        m.attachment = None;
        m.attachment_evicted = false;
        if let Err(e) = self.rewrite() {
            eprintln!("warning: failed to persist delete: {e}");
        }
        true
    }

    /// Enforce the attachment storage quota. Delete the largest files older
    /// than the grace period until total on-disk attachment bytes are back
    /// under `max_bytes`, marking each affected message as evicted. Called
    /// after every successful upload. Orphan files (no live message
    /// referencing them, e.g. a hash only a since-deleted post used) are
    /// eligible too and reclaimed first-class.
    pub fn enforce_quota(&mut self, max_bytes: u64, grace_secs: u64) -> Result<()> {
        let mut total: u64 = 0;
        // (hash, size, mtime_secs)
        let mut files: Vec<(String, u64, u64)> = Vec::new();
        for entry in std::fs::read_dir(&self.attach_dir)? {
            let entry = entry?;
            let meta = entry.metadata()?;
            if !meta.is_file() {
                continue;
            }
            let size = meta.len();
            total += size;
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let hash = entry.file_name().to_string_lossy().into_owned();
            files.push((hash, size, mtime));
        }

        if total <= max_bytes {
            return Ok(());
        }

        let cutoff = now_secs().saturating_sub(grace_secs);
        // Eligible = older than the grace period; largest first.
        files.retain(|(_, _, mtime)| *mtime <= cutoff);
        files.sort_by_key(|f| std::cmp::Reverse(f.1));

        let mut evicted_hashes = Vec::new();
        for (hash, size, _) in files {
            if total <= max_bytes {
                break;
            }
            let path = self.attach_dir.join(&hash);
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(size);
                evicted_hashes.push(hash);
            }
        }

        if evicted_hashes.is_empty() {
            return Ok(());
        }
        let mut changed = false;
        for m in &mut self.messages {
            if let Some(a) = &m.attachment
                && evicted_hashes.contains(&a.hash)
            {
                m.attachment = None;
                m.attachment_evicted = true;
                changed = true;
            }
        }
        if changed {
            self.rewrite()?;
        }
        Ok(())
    }
}

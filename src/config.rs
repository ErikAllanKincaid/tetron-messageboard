//! Configuration and small persisted install-state for `tetron-messageboard`.
//!
//! Two distinct things live here:
//!
//! 1. **Runtime knobs** read from the environment on every start (port,
//!    storage quotas, eviction grace). Env-driven, not a config file, so the
//!    systemd `--user` unit can carry them and `install` can set them once --
//!    same shape as tetron-webui's `TETRON_WEBUI_PORT`.
//! 2. **Install state** persisted to `board.json` in the data dir: which
//!    tetron network this board is bound to, and whether it was installed by
//!    a coordinator. Decided once at `install` time (a self-assertion, see
//!    main.rs) and read back by the running service and the `/health`
//!    discovery probe -- it must survive a restart without re-probing.
//!
//! The message log (`messages.ndjson`) and content-addressed attachment
//! files (`attachments/<hash>`) also live under the data dir; see store.rs.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Default listen port. Sits in the "manually assigned" band *below* the
/// Linux ephemeral range (`net.ipv4.ip_local_port_range`, typically
/// 32768-60999) so the kernel never hands it out to an outbound socket --
/// the same reasoning tetron-sync-receiver used to land on 28873, kept
/// clear of that neighbour and of the heavily-squatted 8000-9000 dev-tool
/// range. Override with `TETRON_MESSAGEBOARD_PORT` or `install --port`.
pub const DEFAULT_PORT: u16 = 28088;

/// Total attachment bytes allowed on disk before eviction kicks in
/// (largest-first, past the grace period -- see store.rs). Text messages
/// are unbounded; only attachments are quota'd.
pub const DEFAULT_MAX_STORAGE_MB: u64 = 1024;

/// Largest single upload accepted, rejected at request time rather than
/// silently truncated. A distinct control from the total quota: without it
/// one upload could consume the whole quota and evict everything else.
pub const DEFAULT_MAX_ATTACHMENT_MB: u64 = 25;

/// How long a freshly-posted attachment is exempt from eviction, so a large
/// image is not evicted seconds after posting just for being the biggest
/// file on disk. Only files older than this are eligible.
pub const DEFAULT_EVICTION_GRACE_SECS: u64 = 3600;

/// Resolved runtime knobs.
pub struct Config {
    pub port: u16,
    pub max_storage_bytes: u64,
    pub max_attachment_bytes: u64,
    pub eviction_grace_secs: u64,
}

impl Config {
    /// Read every knob from the environment, falling back to the defaults
    /// above when unset or unparsable.
    pub fn from_env() -> Config {
        Config {
            port: env_num("TETRON_MESSAGEBOARD_PORT", DEFAULT_PORT as u64) as u16,
            max_storage_bytes: env_num("TETRON_MESSAGEBOARD_MAX_STORAGE_MB", DEFAULT_MAX_STORAGE_MB)
                * 1024
                * 1024,
            max_attachment_bytes: env_num(
                "TETRON_MESSAGEBOARD_MAX_ATTACHMENT_MB",
                DEFAULT_MAX_ATTACHMENT_MB,
            ) * 1024
                * 1024,
            eviction_grace_secs: env_num(
                "TETRON_MESSAGEBOARD_EVICTION_GRACE_SECS",
                DEFAULT_EVICTION_GRACE_SECS,
            ),
        }
    }
}

fn env_num(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(default)
}

/// The network name this board is bound to, if the operator pinned one via
/// `TETRON_MESSAGEBOARD_NETWORK` / `install --network`. When unset, the service
/// auto-selects the sole network the local daemon belongs to (and errors if
/// there is more than one -- see roster.rs).
pub fn network_from_env() -> Option<String> {
    std::env::var("TETRON_MESSAGEBOARD_NETWORK").ok().filter(|s| !s.is_empty())
}

/// Filesystem- and systemd-instance-safe token for a network name. Readable
/// verbatim when the name is already safe (`[A-Za-z0-9._-]`); otherwise it is
/// sanitised and disambiguated with a short stable hash so two different names
/// can never collide onto the same unit/data dir.
pub fn instance_token(network: &str) -> String {
    let safe: String = network
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' })
        .collect();
    if !network.is_empty() && safe == network {
        safe
    } else {
        format!("{safe}-{:08x}", fnv1a(network))
    }
}

/// FNV-1a (32-bit). Deterministic across runs (unlike `DefaultHasher`'s
/// `RandomState`), which the token mapping between `install` and `run`
/// requires.
fn fnv1a(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// Base data dir shared by all boards (legacy single-board data also lives
/// here, at the root, before migration into a per-network subdir).
pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("could not determine data directory")?
        .join("tetron-messageboard");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Per-network data dir. `key` is the network name; an empty key is the
/// legacy single-board layout at the base dir (kept so an existing install's
/// messages survive an upgrade until it is re-added per network).
pub fn net_data_dir(key: &str) -> Result<PathBuf> {
    let base = data_dir()?;
    let dir = if key.is_empty() { base } else { base.join("net").join(instance_token(key)) };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn attachments_dir(key: &str) -> Result<PathBuf> {
    let dir = net_data_dir(key)?.join("attachments");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn messages_path(key: &str) -> Result<PathBuf> {
    Ok(net_data_dir(key)?.join("messages.ndjson"))
}

fn install_state_path(key: &str) -> Result<PathBuf> {
    Ok(net_data_dir(key)?.join("board.json"))
}

/// Best-effort one-time migration of a legacy single-board layout (data at
/// the base dir) into `network`'s per-network subdir, run when that board is
/// (re)installed. Never overwrites existing per-network data, and leaves
/// legacy data belonging to a *different* pinned network untouched.
pub fn migrate_legacy_into(network: &str) -> Result<()> {
    if network.is_empty() {
        return Ok(());
    }
    let base = data_dir()?;
    let legacy_msgs = base.join("messages.ndjson");
    let legacy_state = base.join("board.json");
    let legacy_attach = base.join("attachments");
    if !legacy_msgs.exists() && !legacy_state.exists() {
        return Ok(());
    }
    let legacy_net = if legacy_state.exists() {
        std::fs::read_to_string(&legacy_state)
            .ok()
            .and_then(|r| serde_json::from_str::<InstallState>(&r).ok())
            .map(|s| s.network)
            .unwrap_or_default()
    } else {
        String::new()
    };
    if !legacy_net.is_empty() && !legacy_net.eq_ignore_ascii_case(network) {
        return Ok(());
    }
    let dest = net_data_dir(network)?;
    if dest.join("messages.ndjson").exists() {
        return Ok(());
    }
    if legacy_msgs.exists() {
        let _ = std::fs::rename(&legacy_msgs, dest.join("messages.ndjson"));
    }
    if legacy_attach.is_dir() && !dest.join("attachments").exists() {
        let _ = std::fs::rename(&legacy_attach, dest.join("attachments"));
    }
    if legacy_state.exists() {
        let _ = std::fs::rename(&legacy_state, dest.join("board.json"));
    }
    eprintln!("migrated legacy message-board data into {}", dest.display());
    Ok(())
}

/// Persisted decisions from `install` time.
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct InstallState {
    /// The tetron network this board serves. Empty means "auto-select the
    /// sole network at runtime" (an unpinned install on a single-network
    /// host).
    #[serde(default)]
    pub network: String,
    /// Self-asserted at install: was the installing node a coordinator of
    /// `network`? Only a coordinator-installed board is surfaced by the
    /// network-wide discovery probe (advisory nudge, not a security gate --
    /// see the design doc's discovery section).
    #[serde(default)]
    pub installed_by_coordinator: bool,
}

pub fn load_install_state(key: &str) -> Result<InstallState> {
    let path = install_state_path(key)?;
    if !path.exists() {
        return Ok(InstallState::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn save_install_state(key: &str, state: &InstallState) -> Result<()> {
    let path = install_state_path(key)?;
    let raw = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::instance_token;

    #[test]
    fn token_passes_through_safe_names() {
        // A network name that is already filesystem/systemd-instance safe is
        // used verbatim, so units and data dirs stay human-readable.
        assert_eq!(instance_token("home"), "home");
        assert_eq!(instance_token("home-lan_2.0"), "home-lan_2.0");
    }

    #[test]
    fn token_sanitises_and_disambiguates_unsafe_names() {
        // Unsafe chars are replaced and a stable hash appended, so two
        // different names cannot collide onto one token.
        let a = instance_token("my net");
        let b = instance_token("my/net");
        assert!(a.starts_with("my_net-"), "{a}");
        assert!(b.starts_with("my_net-"), "{b}");
        assert_ne!(a, b);
        // Deterministic across calls (install and run must agree).
        assert_eq!(a, instance_token("my net"));
    }
}

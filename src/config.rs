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

pub fn data_dir() -> Result<PathBuf> {
    let dir = dirs::data_dir()
        .context("could not determine data directory")?
        .join("tetron-messageboard");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn attachments_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("attachments");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn messages_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("messages.ndjson"))
}

fn install_state_path() -> Result<PathBuf> {
    Ok(data_dir()?.join("board.json"))
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

pub fn load_install_state() -> Result<InstallState> {
    let path = install_state_path()?;
    if !path.exists() {
        return Ok(InstallState::default());
    }
    let raw = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

pub fn save_install_state(state: &InstallState) -> Result<()> {
    let path = install_state_path()?;
    let raw = serde_json::to_string_pretty(state)?;
    std::fs::write(&path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

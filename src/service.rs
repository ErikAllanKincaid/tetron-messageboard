//! Per-user service install/uninstall: a templated `systemd --user` unit on
//! Linux (`tetron-messageboard@<token>`), a per-instance launchd LaunchAgent
//! on macOS (`com.tetron.messageboard.<token>`) -- the same per-user shape as
//! tetron-webui and tetron-sync-receiver (no root to *run*, only to place the
//! binary in `/usr/local/bin`).
//!
//! One board per tetron network, several at once on a multi-network node.
//! Each board binds its network's own mesh IP, so they all share the default
//! port without conflict. The pinned network + port for each board live in a
//! small env file `~/.config/tetron-messageboard/<token>.env`, which doubles
//! as the registry of installed boards for `list`/`restart-all`/`uninstall`.
//! On Linux systemd reads that file directly (`EnvironmentFile`); on macOS it
//! is bookkeeping only (the plist carries the env inline).

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::config;

fn run_cmd(program: &str, args: &[&str]) {
    match Command::new(program).args(args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("warning: `{program}` exited with {status}"),
        Err(e) => eprintln!("warning: failed to run `{program}`: {e}"),
    }
}

#[allow(dead_code)] // used only on macOS
fn run_cmd_quiet(program: &str, args: &[&str]) {
    let _ = Command::new(program).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

/// One installed board, as reported by `list`.
pub struct Instance {
    pub network: String,
    pub port: u16,
    pub token: String,
    pub active: bool,
    /// The pre-multi-board single unit (`tetron-messageboard`), still present
    /// on a host installed before this version.
    pub legacy: bool,
}

/// `~/.config/tetron-messageboard`, matching the unit's `%h/.config/...`
/// `EnvironmentFile` path regardless of `$XDG_CONFIG_HOME`.
fn env_file_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join(".config/tetron-messageboard");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn env_file_path(token: &str) -> Result<PathBuf> {
    Ok(env_file_dir()?.join(format!("{token}.env")))
}

fn instance_unit(token: &str) -> String {
    format!("tetron-messageboard@{token}")
}

#[allow(dead_code)] // used only on Linux
const LEGACY_UNIT: &str = "tetron-messageboard";
#[allow(dead_code)] // used only on macOS
const LEGACY_LABEL: &str = "com.tetron.messageboard";

#[cfg(target_os = "linux")]
fn systemd_user_dir() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("could not determine config directory")?
        .join("systemd/user");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(target_os = "macos")]
fn launch_agents_dir() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(target_os = "macos")]
fn plist_path(token: &str) -> Result<PathBuf> {
    Ok(launch_agents_dir()?.join(format!("com.tetron.messageboard.{token}.plist")))
}

#[cfg(target_os = "macos")]
fn log_path(token: &str) -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join("Library/Logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("tetron-messageboard-{token}.log")))
}

fn current_exe() -> Result<String> {
    Ok(std::env::current_exe()
        .context("failed to determine current executable path")?
        .to_string_lossy()
        .into_owned())
}

/// Write the env-file registry entry, install/refresh the platform unit for
/// `network`, start it, and wait for the board to come up on its mesh IP.
/// `bind_ip` is the network's own mesh address; the wait probes there, since
/// the board never binds `0.0.0.0`/`127.0.0.1`.
pub fn install(port: u16, network: &str, bind_ip: Ipv4Addr) -> Result<()> {
    println!("installing tetron-messageboard {}", crate::FULL_VERSION);
    let token = config::instance_token(network);
    let exe = current_exe()?;

    // Registry / env file (systemd reads it directly; on macOS it is our own
    // bookkeeping for list/restart/uninstall).
    let env_path = env_file_path(&token)?;
    let env_body = format!("TETRON_MESSAGEBOARD_PORT={port}\nTETRON_MESSAGEBOARD_NETWORK={network}\n");
    std::fs::write(&env_path, env_body)
        .with_context(|| format!("failed to write {}", env_path.display()))?;

    #[cfg(target_os = "linux")]
    {
        let unit_path = systemd_user_dir()?.join("tetron-messageboard@.service");
        let unit = include_str!("../contrib/tetron-messageboard@.service")
            .replace("/usr/local/bin/tetron-messageboard", &exe);
        std::fs::write(&unit_path, unit)
            .with_context(|| format!("failed to write {}", unit_path.display()))?;
        run_cmd("systemctl", &["--user", "daemon-reload"]);
        run_cmd("systemctl", &["--user", "enable", &instance_unit(&token)]);
        // Explicit restart, not `enable --now` -- the latter no-ops on an
        // already-running unit, so a reinstall over a live instance would
        // never pick up the new binary/config.
        run_cmd("systemctl", &["--user", "restart", &instance_unit(&token)]);
    }

    #[cfg(target_os = "macos")]
    {
        let path = plist_path(&token)?;
        let log = log_path(&token)?.to_string_lossy().into_owned();
        let plist = include_str!("../contrib/com.tetron.messageboard.plist")
            .replace(LEGACY_LABEL, &format!("com.tetron.messageboard.{token}"))
            .replace("/usr/local/bin/tetron-messageboard", &exe)
            .replace("/tmp/tetron-messageboard.log", &log)
            .replace("__TETRON_MESSAGEBOARD_PORT__", &port.to_string())
            .replace("__TETRON_MESSAGEBOARD_NETWORK__", network);
        std::fs::write(&path, plist).with_context(|| format!("failed to write {}", path.display()))?;
        run_cmd_quiet("launchctl", &["unload", &path.to_string_lossy()]);
        run_cmd("launchctl", &["load", "-w", &path.to_string_lossy()]);
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    anyhow::bail!("per-user service install not supported on this platform");

    let addr = format!("{bind_ip}:{port}");
    eprintln!("waiting for tetron-messageboard to come up on http://{addr}…");
    if wait_for_port(&addr, Duration::from_secs(10)) {
        println!("tetron-messageboard installed and serving network '{network}' on http://{addr}");
        Ok(())
    } else {
        anyhow::bail!(
            "service was installed but never became reachable on http://{addr}.\n\
             Check the logs (journalctl --user -u {} on Linux, \
             or ~/Library/Logs/tetron-messageboard-{token}.log on macOS).",
            instance_unit(&token)
        );
    }
}

/// Remove one board (by network) or, with `all`, every board. With neither
/// flag: uninstall the sole board if exactly one is installed, otherwise error
/// and list what is present (the require-a-flag rule for multi-board hosts).
pub fn uninstall_cli(network: Option<&str>, all: bool) -> Result<()> {
    let instances = list_instances();
    if all {
        if instances.is_empty() {
            println!("No boards installed.");
            return Ok(());
        }
        for inst in &instances {
            uninstall_instance(inst)?;
        }
        return Ok(());
    }
    if let Some(net) = network {
        match instances.iter().find(|i| i.network.eq_ignore_ascii_case(net)) {
            Some(inst) => return uninstall_instance(inst),
            None => anyhow::bail!("no board for network '{net}' is installed on this node"),
        }
    }
    match instances.len() {
        0 => {
            println!("No boards installed.");
            Ok(())
        }
        1 => uninstall_instance(&instances[0]),
        _ => {
            let names: Vec<_> = instances.iter().map(|i| i.network.as_str()).collect();
            anyhow::bail!(
                "several boards are installed ({}); specify which with \
                 `tetron-messageboard uninstall --network <name>`, or remove them all with --all",
                names.join(", ")
            )
        }
    }
}

/// Stop and remove one board's unit and its registry entry. Data (messages,
/// attachments) is deliberately kept.
fn uninstall_instance(inst: &Instance) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if inst.legacy {
            let path = systemd_user_dir()?.join("tetron-messageboard.service");
            run_cmd("systemctl", &["--user", "disable", "--now", LEGACY_UNIT]);
            let _ = std::fs::remove_file(&path);
        } else {
            run_cmd("systemctl", &["--user", "disable", "--now", &instance_unit(&inst.token)]);
        }
        run_cmd("systemctl", &["--user", "daemon-reload"]);
    }

    #[cfg(target_os = "macos")]
    {
        let path = if inst.legacy {
            launch_agents_dir()?.join("com.tetron.messageboard.plist")
        } else {
            plist_path(&inst.token)?
        };
        run_cmd_quiet("launchctl", &["unload", "-w", &path.to_string_lossy()]);
        let _ = std::fs::remove_file(&path);
    }

    if !inst.legacy {
        let _ = std::fs::remove_file(env_file_path(&inst.token)?);
    }
    println!(
        "Removed board for network '{}' (its messages and attachments are kept on disk).",
        inst.network
    );
    Ok(())
}

/// Restart every installed board so a freshly upgraded binary is picked up.
pub fn restart_all() -> Result<()> {
    let instances = list_instances();
    if instances.is_empty() {
        println!("No boards installed.");
        return Ok(());
    }
    for inst in &instances {
        #[cfg(target_os = "linux")]
        {
            let unit = if inst.legacy { LEGACY_UNIT.to_string() } else { instance_unit(&inst.token) };
            run_cmd("systemctl", &["--user", "restart", &unit]);
        }
        #[cfg(target_os = "macos")]
        {
            let path = if inst.legacy {
                launch_agents_dir()?.join("com.tetron.messageboard.plist")
            } else {
                plist_path(&inst.token)?
            };
            run_cmd_quiet("launchctl", &["unload", &path.to_string_lossy()]);
            run_cmd("launchctl", &["load", "-w", &path.to_string_lossy()]);
        }
        println!("restarted board for network '{}'", inst.network);
    }
    Ok(())
}

/// `list` subcommand: print a table, or JSON for tetron-webui / scripts.
pub fn list_cmd(json: bool) -> Result<()> {
    let instances = list_instances();
    if json {
        let arr: Vec<serde_json::Value> = instances
            .iter()
            .map(|i| {
                serde_json::json!({
                    "network": i.network,
                    "port": i.port,
                    "token": i.token,
                    "active": i.active,
                    "legacy": i.legacy,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&arr)?);
        return Ok(());
    }
    if instances.is_empty() {
        println!("No boards installed.");
        return Ok(());
    }
    for i in &instances {
        let state = if i.active { "running" } else { "stopped" };
        let legacy = if i.legacy { " (legacy unit)" } else { "" };
        println!("{:<24} port {:<6} {}{}", i.network, i.port, state, legacy);
    }
    Ok(())
}

/// Enumerate installed boards from the env-file registry, plus the legacy
/// single unit if it is still present.
pub fn list_instances() -> Vec<Instance> {
    let mut out = Vec::new();
    if let Ok(dir) = env_file_dir()
        && let Ok(entries) = std::fs::read_dir(&dir)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("env") {
                continue;
            }
            let token = match path.file_stem().and_then(|s| s.to_str()) {
                Some(t) => t.to_string(),
                None => continue,
            };
            let (network, port) = parse_env_file(&path);
            let active = is_active(&token, false);
            out.push(Instance { network, port, token, active, legacy: false });
        }
    }
    if legacy_present() {
        out.push(Instance {
            network: legacy_network(),
            port: config::DEFAULT_PORT,
            token: String::new(),
            active: is_active("", true),
            legacy: true,
        });
    }
    out.sort_by_key(|i| i.network.to_lowercase());
    out
}

fn parse_env_file(path: &std::path::Path) -> (String, u16) {
    let mut network = String::new();
    let mut port = config::DEFAULT_PORT;
    if let Ok(raw) = std::fs::read_to_string(path) {
        for line in raw.lines() {
            if let Some(v) = line.strip_prefix("TETRON_MESSAGEBOARD_NETWORK=") {
                network = v.trim().to_string();
            } else if let Some(v) = line.strip_prefix("TETRON_MESSAGEBOARD_PORT=")
                && let Ok(p) = v.trim().parse()
            {
                port = p;
            }
        }
    }
    (network, port)
}

/// Best-effort network label for the legacy single board (its persisted
/// install state), for display only.
fn legacy_network() -> String {
    config::load_install_state("").map(|s| s.network).unwrap_or_default()
}

#[cfg(target_os = "linux")]
fn legacy_present() -> bool {
    systemd_user_dir().map(|d| d.join("tetron-messageboard.service").exists()).unwrap_or(false)
}

#[cfg(target_os = "macos")]
fn legacy_present() -> bool {
    launch_agents_dir().map(|d| d.join("com.tetron.messageboard.plist").exists()).unwrap_or(false)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn legacy_present() -> bool {
    false
}

#[cfg(target_os = "linux")]
fn is_active(token: &str, legacy: bool) -> bool {
    let unit = if legacy { LEGACY_UNIT.to_string() } else { instance_unit(token) };
    matches!(
        Command::new("systemctl").args(["--user", "is-active", &unit]).output(),
        Ok(o) if o.status.success()
    )
}

#[cfg(target_os = "macos")]
fn is_active(token: &str, legacy: bool) -> bool {
    let label =
        if legacy { LEGACY_LABEL.to_string() } else { format!("com.tetron.messageboard.{token}") };
    matches!(
        Command::new("launchctl").args(["list", &label]).output(),
        Ok(o) if o.status.success()
    )
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn is_active(_token: &str, _legacy: bool) -> bool {
    false
}

fn wait_for_port(addr: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if std::net::TcpStream::connect(addr).is_ok() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

//! Per-user service install/uninstall: a `systemd --user` unit on Linux, a
//! launchd LaunchAgent on macOS -- the same per-user shape as tetron-webui
//! and tetron-sync-receiver (no root to *run*, only to place the binary in
//! `/usr/local/bin`). The unit execs `tetron-messageboard run`; the port and the
//! pinned network name are injected as environment variables so the running
//! service and the `/health` discovery probe agree on both.

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

fn run_cmd(program: &str, args: &[&str]) {
    match Command::new(program).args(args).status() {
        Ok(status) if status.success() => {}
        Ok(status) => eprintln!("warning: `{program}` exited with {status}"),
        Err(e) => eprintln!("warning: failed to run `{program}`: {e}"),
    }
}

#[allow(dead_code)]
fn run_cmd_quiet(program: &str, args: &[&str]) {
    let _ = Command::new(program).args(args).stdout(Stdio::null()).stderr(Stdio::null()).status();
}

#[cfg(target_os = "linux")]
fn unit_path() -> Result<PathBuf> {
    let dir = dirs::config_dir()
        .context("could not determine config directory")?
        .join("systemd/user");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("tetron-messageboard.service"))
}

#[cfg(target_os = "macos")]
fn plist_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join("Library/LaunchAgents");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("com.tetron.messageboard.plist"))
}

#[cfg(target_os = "macos")]
fn log_path() -> Result<PathBuf> {
    let dir = dirs::home_dir()
        .context("could not determine home directory")?
        .join("Library/Logs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join("tetron-messageboard.log"))
}

/// Write the unit/plist (substituting the running binary's path plus the
/// port and network), enable it, and wait for the board to come up on its
/// mesh IP before declaring success. `bind_ip` is the network's own mesh
/// address the server will bind to -- the wait probes there, not loopback,
/// since the board never binds `0.0.0.0`/`127.0.0.1`.
pub fn install(port: u16, network: &str, bind_ip: Ipv4Addr) -> Result<()> {
    println!("installing tetron-messageboard {}", crate::FULL_VERSION);
    let exe = std::env::current_exe()
        .context("failed to determine current executable path")?
        .to_string_lossy()
        .into_owned();
    let port_str = port.to_string();

    #[cfg(target_os = "linux")]
    {
        let path = unit_path()?;
        let unit = include_str!("../contrib/tetron-messageboard.service")
            .replace("/usr/local/bin/tetron-messageboard", &exe)
            .replace("__TETRON_MESSAGEBOARD_PORT__", &port_str)
            .replace("__TETRON_MESSAGEBOARD_NETWORK__", network);
        std::fs::write(&path, unit).with_context(|| format!("failed to write {}", path.display()))?;
        run_cmd("systemctl", &["--user", "daemon-reload"]);
        run_cmd("systemctl", &["--user", "enable", "tetron-messageboard"]);
        // Explicit restart, not `enable --now` -- the latter no-ops on an
        // already-running unit, so a reinstall over a live instance would
        // never pick up the new binary/config (same fix the sibling addons
        // carry).
        run_cmd("systemctl", &["--user", "restart", "tetron-messageboard"]);
    }

    #[cfg(target_os = "macos")]
    {
        let path = plist_path()?;
        let log = log_path()?.to_string_lossy().into_owned();
        let plist = include_str!("../contrib/com.tetron.messageboard.plist")
            .replace("/usr/local/bin/tetron-messageboard", &exe)
            .replace("/tmp/tetron-messageboard.log", &log)
            .replace("__TETRON_MESSAGEBOARD_PORT__", &port_str)
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
             Check the logs (journalctl --user -u tetron-messageboard on Linux, \
             or ~/Library/Logs/tetron-messageboard.log on macOS)."
        );
    }
}

pub fn uninstall() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let path = unit_path()?;
        if path.exists() {
            run_cmd("systemctl", &["--user", "disable", "--now", "tetron-messageboard"]);
            std::fs::remove_file(&path)?;
            run_cmd("systemctl", &["--user", "daemon-reload"]);
            println!("Removed systemd --user service.");
        } else {
            println!("Service not installed.");
        }
        return Ok(());
    }

    #[cfg(target_os = "macos")]
    {
        let path = plist_path()?;
        if path.exists() {
            run_cmd("launchctl", &["unload", "-w", &path.to_string_lossy()]);
            std::fs::remove_file(&path)?;
            println!("Removed launchd LaunchAgent.");
        } else {
            println!("Service not installed.");
        }
        return Ok(());
    }

    #[allow(unreachable_code)]
    {
        anyhow::bail!("per-user service uninstall not supported on this platform");
    }
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

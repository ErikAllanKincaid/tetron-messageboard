//! Reads the *local* tetron daemon's own peer roster over the IPC socket,
//! exactly as tetron-sync-receiver's roster.rs does. The board uses it for
//! three things:
//!
//! - the network's own mesh IP (`my_ip`) to bind the HTTP server on, so the
//!   board is reachable only over the mesh and never on `0.0.0.0`;
//! - a source-IP -> hostname map, so each post is labelled by the roster's
//!   view of who actually connected (identity is not self-reported);
//! - the installing node's own coordinator role for a network, for the
//!   install-time self-assertion baked into `/health`.
//!
//! Fresh connection per call, no persistent stream -- a daemon restart just
//! makes the next call reconnect, the same pattern the sibling addons use.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use tetron_proto::ipc::{self, IpcMessage, NetworkStatus};

/// A resolved, point-in-time view of one network, cached by the running
/// service and refreshed on a timer (see main.rs).
#[derive(Clone, Default)]
pub struct RosterView {
    pub network: String,
    pub subnet: String,
    pub my_ip: Option<Ipv4Addr>,
    pub my_hostname: Option<String>,
    /// mesh IPv4 (as a string) -> hostname, for every peer plus self.
    pub hostnames: HashMap<String, String>,
}

impl RosterView {
    /// Label a source IP: the roster's hostname if known, otherwise the raw
    /// mesh IP (the design doc's fallback for a peer this host has not
    /// connected to recently).
    pub fn label(&self, ip: &str) -> String {
        self.hostnames.get(ip).cloned().unwrap_or_else(|| ip.to_string())
    }
}

/// Every network the local daemon belongs to.
async fn fetch_networks() -> anyhow::Result<Vec<NetworkStatus>> {
    let mut stream = ipc::connect()
        .await
        .map_err(|e| anyhow::anyhow!("could not reach the tetron daemon: {e}"))?;
    ipc::send(&mut stream, IpcMessage::Status)
        .await
        .map_err(|e| anyhow::anyhow!("failed to send request to daemon: {e}"))?;
    let resp = ipc::recv(&mut stream)
        .await
        .map_err(|e| anyhow::anyhow!("failed to read daemon response: {e}"))?;
    let IpcMessage::StatusResponse { networks, .. } = resp else {
        anyhow::bail!("unexpected daemon response to Status");
    };
    Ok(networks)
}

/// Resolve which network this board serves. `pinned` is
/// `TETRON_MESSAGEBOARD_NETWORK` / `install --network`; when empty, require exactly
/// one network to exist and pick it. Errors list the available names so the
/// operator knows what to pass.
pub async fn select_network(pinned: &str) -> anyhow::Result<NetworkStatus> {
    let networks = fetch_networks().await?;
    anyhow::ensure!(
        !networks.is_empty(),
        "this node does not belong to any tetron network yet -- join or create one first"
    );

    if pinned.is_empty() {
        if networks.len() == 1 {
            return Ok(networks.into_iter().next().unwrap());
        }
        let names: Vec<_> = networks.iter().map(|n| n.network.clone()).collect();
        anyhow::bail!(
            "this node belongs to several networks ({}); pin one with \
             `tetron-messageboard install --network <name>` (or TETRON_MESSAGEBOARD_NETWORK)",
            names.join(", ")
        );
    }

    networks
        .into_iter()
        .find(|n| n.network.eq_ignore_ascii_case(pinned))
        .ok_or_else(|| anyhow::anyhow!("no network named '{pinned}' on this node"))
}

/// Build a fresh [`RosterView`] for the chosen network.
pub async fn snapshot(network: &str) -> anyhow::Result<RosterView> {
    let net = select_network(network).await?;
    let mut hostnames = HashMap::new();
    if let Some(h) = &net.my_hostname {
        hostnames.insert(net.my_ip.to_string(), h.clone());
    }
    for peer in &net.peers {
        if let Some(h) = &peer.hostname {
            hostnames.insert(peer.ip.to_string(), h.clone());
        }
    }
    Ok(RosterView {
        network: net.network,
        subnet: net.subnet,
        my_ip: Some(net.my_ip),
        my_hostname: net.my_hostname,
        hostnames,
    })
}

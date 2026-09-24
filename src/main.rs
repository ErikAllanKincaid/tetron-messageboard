//! `tetron-messageboard`: a mesh-hosted message board for one tetron network.
//! Anyone admitted to the mesh can read and post (text + images); the mesh
//! itself is the access control, so there is no login layer. A genuinely
//! separate, opt-in addon in the same family as tetron-relay/
//! tetron-sync-receiver/tetron-webui -- zero tetron core changes.
//!
//! The server binds only to the network's own mesh IP (never `0.0.0.0`),
//! and the source IP of each request -- authenticated by tetron -- is the
//! poster's identity, resolved to a hostname server-side from the live
//! roster. See the module docs in api.rs/store.rs/roster.rs.

mod api;
mod config;
mod ratelimit;
mod roster;
mod service;
mod store;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::DefaultBodyLimit;
use axum::response::IntoResponse;
use axum::routing::{delete, get};
use axum::Router;
use clap::{Parser, Subcommand};
use tokio::sync::{Mutex, RwLock};

use api::AppState;

pub(crate) const FULL_VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_SHA"), ")");

/// How often the cached roster view (source-IP -> hostname, mesh IP, subnet)
/// is refreshed from the daemon. ~60s matches the design doc's discovery
/// cache and tetron's own lazy/periodic style elsewhere.
const ROSTER_REFRESH: Duration = Duration::from_secs(60);

#[derive(Parser)]
#[command(name = "tetron-messageboard", version = FULL_VERSION)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Install and start the per-user service (systemd --user on Linux, a
    /// launchd LaunchAgent on macOS), bound to one tetron network
    Install {
        /// Port to bind on the mesh interface
        #[arg(short = 'p', long, env = "TETRON_MESSAGEBOARD_PORT", default_value_t = config::DEFAULT_PORT)]
        port: u16,
        /// Which tetron network to serve. Optional when this node belongs to
        /// exactly one network; required to disambiguate otherwise.
        #[arg(long, env = "TETRON_MESSAGEBOARD_NETWORK", default_value = "")]
        network: String,
    },
    /// Stop and remove the per-user service
    Uninstall,
    /// Print the tetron-messageboard version
    #[command(visible_alias = "ver")]
    Version,
    /// Foreground entry point the installed service execs into -- not meant
    /// to be run by hand.
    #[command(hide = true)]
    Run,
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const STYLE_CSS: &str = include_str!("../static/style.css");
const APP_JS: &str = include_str!("../static/app.js");
const FAVICON_SVG: &str = include_str!("../static/favicon.svg");

// App assets are embedded and change with every deploy, so revalidate on
// every load (same reasoning as tetron-webui's own no-cache on its assets).
async fn serve_index() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/html"), (axum::http::header::CACHE_CONTROL, "no-cache")], INDEX_HTML)
}
async fn serve_css() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "text/css"), (axum::http::header::CACHE_CONTROL, "no-cache")], STYLE_CSS)
}
async fn serve_js() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "application/javascript"), (axum::http::header::CACHE_CONTROL, "no-cache")], APP_JS)
}
async fn serve_favicon() -> impl IntoResponse {
    ([(axum::http::header::CONTENT_TYPE, "image/svg+xml")], FAVICON_SVG)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Install { port, network }) => install(port, network).await,
        Some(Command::Uninstall) => service::uninstall(),
        Some(Command::Version) => {
            println!("tetron-messageboard {FULL_VERSION}");
            Ok(())
        }
        // `run` is the default so `ExecStart=… run` and a bare invocation
        // both serve.
        Some(Command::Run) | None => run().await,
    }
}

/// Resolve the network (and confirm coordinator status), persist the install
/// state, then hand off to the platform service installer.
async fn install(port: u16, network: String) -> anyhow::Result<()> {
    let net = roster::select_network(&network).await?;
    let concrete = net.network.clone();
    let by_coord = net.role.is_coordinator();
    config::save_install_state(&config::InstallState {
        network: concrete.clone(),
        installed_by_coordinator: by_coord,
    })?;
    if !by_coord {
        eprintln!(
            "note: this node is not a coordinator of '{concrete}', so the board runs and is \
             reachable by IP but will not be auto-advertised by tetron-webui's discovery probe."
        );
    }
    service::install(port, &concrete, net.my_ip)
}

async fn run() -> anyhow::Result<()> {
    let cfg = config::Config::from_env();
    let state = config::load_install_state()?;
    // Network precedence: the concrete name pinned at install, else the env
    // override, else "" (auto-select the sole network).
    let network = if !state.network.is_empty() {
        state.network.clone()
    } else {
        config::network_from_env().unwrap_or_default()
    };

    // Wait for the daemon to be reachable and the network present -- on boot
    // the board's service may start before tetron has brought its TUN up.
    let initial = loop {
        match roster::snapshot(&network).await {
            Ok(v) => break v,
            Err(e) => {
                eprintln!("waiting for tetron daemon/network: {e}");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    };
    let network_name = initial.network.clone();
    let my_ip = initial.my_ip.expect("snapshot always sets my_ip");

    let store = store::Store::load()?;
    let app_state = AppState {
        store: Arc::new(Mutex::new(store)),
        limiter: Arc::new(Mutex::new(ratelimit::RateLimiter::default())),
        roster: Arc::new(RwLock::new(initial)),
        network: network_name.clone(),
        installed_by_coordinator: state.installed_by_coordinator,
        max_attachment_bytes: cfg.max_attachment_bytes,
        max_storage_bytes: cfg.max_storage_bytes,
        eviction_grace_secs: cfg.eviction_grace_secs,
        version: FULL_VERSION,
    };

    // Periodic roster refresh: keep the cached IP->hostname view fresh so
    // identity labels track roster changes. Keep the last good view on error.
    {
        let roster_arc = app_state.roster.clone();
        let net = network_name.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(ROSTER_REFRESH).await;
                match roster::snapshot(&net).await {
                    Ok(v) => *roster_arc.write().await = v,
                    Err(e) => eprintln!("roster refresh failed (keeping last view): {e}"),
                }
            }
        });
    }

    // Body limit sized to the max attachment plus slack for the multipart
    // envelope + text field, so an oversize upload is refused at the layer
    // (413) rather than buffered whole.
    let body_limit = (cfg.max_attachment_bytes + 1024 * 1024) as usize;

    let app = Router::new()
        .route("/", get(serve_index))
        .route("/style.css", get(serve_css))
        .route("/app.js", get(serve_js))
        .route("/favicon.svg", get(serve_favicon))
        .route("/health", get(api::health))
        .route("/api/messages", get(api::list_messages).post(api::post_message))
        .route("/api/messages/{id}", delete(api::delete_message))
        .route("/attachments/{hash}", get(api::serve_attachment))
        .layer(DefaultBodyLimit::max(body_limit))
        .with_state(app_state);

    // Bind to the mesh IP only. Retry: the TUN address may not be assigned
    // the instant the daemon reports the network.
    let addr = SocketAddr::from((my_ip, cfg.port));
    let listener = loop {
        match tokio::net::TcpListener::bind(addr).await {
            Ok(l) => break l,
            Err(e) => {
                eprintln!("waiting to bind {addr} (mesh interface not ready?): {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    };
    eprintln!("tetron-messageboard serving network '{network_name}' on http://{addr}");
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

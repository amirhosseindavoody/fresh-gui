//! fresh-gui-backend — remote ADE daemon (Phase 1: authenticated PTY over WebSocket).

mod pty;
mod server;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::info;

use crate::server::AppState;

#[derive(Debug, Parser)]
#[command(name = "fresh-gui-backend", version, about = "Remote daemon for fresh-gui")]
struct Args {
    /// Listen address. Non-loopback binds require `--token`.
    #[arg(long, default_value = "127.0.0.1:7420", env = "FRESH_GUI_LISTEN")]
    listen: SocketAddr,

    /// Shared auth token (also `FRESH_GUI_TOKEN`). Required for non-loopback binds.
    /// On loopback, if set, clients must present the same token.
    #[arg(long, env = "FRESH_GUI_TOKEN")]
    token: Option<String>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let loopback = args.listen.ip().is_loopback();
    let token = args.token.filter(|t| !t.is_empty());

    if !loopback && token.is_none() {
        bail!(
            "non-loopback listen ({}) requires --token / FRESH_GUI_TOKEN",
            args.listen
        );
    }

    let require_auth = !loopback || token.is_some();
    let state = Arc::new(AppState {
        token,
        require_auth,
    });

    info!(
        listen = %args.listen,
        auth_required = state.require_auth,
        "starting fresh-gui-backend"
    );

    server::serve(args.listen, state)
        .await
        .context("server exited with error")
}

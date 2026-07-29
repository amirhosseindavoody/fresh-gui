//! fresh-gui — host ADE shell stub (Phase 0).

use anyhow::Result;
use clap::Parser;
use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::PROTOCOL_VERSION;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "fresh-gui", version, about = "Terminal-first ADE GUI (host)")]
struct Args {
    /// Backend endpoint (host:port). Transport TBD.
    #[arg(long, default_value = "127.0.0.1:7420")]
    backend: String,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let client = Client::prepare(ConnectOptions::new(&args.backend));

    info!(
        backend = %client.endpoint(),
        protocol = PROTOCOL_VERSION,
        implementation = %client.hello().implementation,
        "fresh-gui stub; Tauri 2 + xterm.js shell not wired yet (see docs/DESIGN.md §10 D2)"
    );

    Ok(())
}

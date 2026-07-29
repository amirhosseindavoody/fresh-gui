//! fresh-gui-backend — remote daemon stub (Phase 0).

use anyhow::Result;
use clap::Parser;
use fresh_gui_protocol::{Hello, PROTOCOL_VERSION};
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "fresh-gui-backend", version, about = "Remote daemon for fresh-gui")]
struct Args {
    /// Listen address (loopback-only recommended until auth lands).
    #[arg(long, default_value = "127.0.0.1:7420")]
    listen: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();
    let hello = Hello::backend(
        format!("fresh-gui-backend/{}", env!("CARGO_PKG_VERSION")),
        vec!["ping".into(), "pty".into()],
    );

    info!(
        listen = %args.listen,
        protocol = PROTOCOL_VERSION,
        implementation = %hello.implementation,
        "fresh-gui-backend stub; PTY transport not implemented yet (see docs/DESIGN.md)"
    );

    // Keep the process useful as a smoke-test binary without blocking forever in CI.
    Ok(())
}

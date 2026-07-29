//! fresh-gui-backend — remote ADE daemon (PTY + FS + optional Fresh editor).

mod editor_worker;
mod fs;
mod pty;
mod server;
mod session;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::info;

use crate::editor_worker::EditorHandle;
use crate::fs::FsRoot;
use crate::server::AppState;
use crate::session::SessionStore;

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

    /// Sandbox root for read-only `fs` listing and editor open (default: current directory).
    #[arg(long, env = "FRESH_GUI_FS_ROOT")]
    root: Option<PathBuf>,

    /// Disable the in-process Fresh editor (omit `editor` capability).
    #[arg(long, env = "FRESH_GUI_NO_EDITOR")]
    no_editor: bool,
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

    let root_path = args
        .root
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let fs_root = FsRoot::new(root_path).context("init FS root")?;

    let require_auth = !loopback || token.is_some();
    let editor = if args.no_editor {
        info!("Fresh editor disabled (--no-editor)");
        None
    } else {
        EditorHandle::spawn(fs_root.root_path().to_path_buf())
    };

    let state = Arc::new(AppState {
        token,
        require_auth,
        fs_root,
        sessions: SessionStore::new(),
        editor,
    });

    info!(
        listen = %args.listen,
        auth_required = state.require_auth,
        fs_root = %state.fs_root.root_display(),
        editor = state.editor.is_some(),
        "starting fresh-gui-backend"
    );

    server::serve(args.listen, state)
        .await
        .context("server exited with error")
}

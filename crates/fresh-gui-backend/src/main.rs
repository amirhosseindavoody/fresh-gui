//! fresh-gui-backend — remote ADE daemon (PTY + FS + optional Fresh editor).

mod editor_worker;
mod fs;
mod fs_watch;
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
use crate::fs_watch::FsWatchStore;
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

    /// Directory of built host UI (`index.html` from `ui/dist`). Served at `/`.
    /// Default: search common workspace/package paths. Empty / missing → API only.
    #[arg(long, env = "FRESH_GUI_UI_DIR")]
    ui_dir: Option<PathBuf>,

    /// Do not serve the web UI (WebSocket + health only).
    #[arg(long, env = "FRESH_GUI_NO_UI")]
    no_ui: bool,
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

    // Bind before spawning Fresh so a busy port fails cleanly (no editor teardown panic).
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("bind {}", args.listen))?;

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
        watches: FsWatchStore::new(),
    });

    info!(
        listen = %args.listen,
        auth_required = state.require_auth,
        fs_root = %state.fs_root.root_display(),
        editor = state.editor.is_some(),
        "starting fresh-gui-backend"
    );

    let ui_dir = if args.no_ui {
        None
    } else {
        resolve_ui_dir(args.ui_dir.as_deref())
    };

    server::serve_listener(listener, state, ui_dir)
        .await
        .context("server exited with error")
}

/// Pick a directory that contains `index.html` for the embedded web UI.
fn resolve_ui_dir(explicit: Option<&std::path::Path>) -> Option<PathBuf> {
    if let Some(dir) = explicit {
        let dir = dir.to_path_buf();
        if dir.join("index.html").is_file() {
            info!(dir = %dir.display(), "serving web UI");
            return Some(dir);
        }
        tracing::warn!(
            dir = %dir.display(),
            "ui-dir missing index.html — API only (run `pixi run ui-build`)"
        );
        return None;
    }

    let mut candidates: Vec<PathBuf> = Vec::new();
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fresh-gui-app/ui/dist"),
    );
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("crates/fresh-gui-app/ui/dist"));
        candidates.push(cwd.join("ui/dist"));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("ui"));
            candidates.push(parent.join("../ui/dist"));
        }
    }

    for dir in candidates {
        let Ok(dir) = dir.canonicalize() else {
            continue;
        };
        if dir.join("index.html").is_file() {
            info!(dir = %dir.display(), "serving web UI");
            return Some(dir);
        }
    }

    tracing::warn!(
        "no web UI found (expected crates/fresh-gui-app/ui/dist) — API only; run `pixi run ui-build`"
    );
    None
}

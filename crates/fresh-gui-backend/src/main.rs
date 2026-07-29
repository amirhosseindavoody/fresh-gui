//! fresh-gui-backend — remote ADE daemon (PTY + FS + optional Fresh editor).

mod editor_worker;
mod fs;
mod fs_watch;
mod pty;
mod server;
mod session;

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use clap::Parser;
use tracing::{info, warn};

use crate::editor_worker::EditorHandle;
use crate::fs::FsRoot;
use crate::fs_watch::FsWatchStore;
use crate::server::AppState;
use crate::session::SessionStore;

/// How many ports above the preferred one to try when the preferred bind is busy.
const LISTEN_PORT_FALLBACK_SPAN: u16 = 64;

#[derive(Debug, Parser)]
#[command(name = "fresh-gui-backend", version, about = "Remote daemon for fresh-gui")]
struct Args {
    /// Listen address. Non-loopback binds require `--token`.
    /// If the port is busy, the next free ports on the same host are tried
    /// (unless `--strict-listen`).
    #[arg(long, default_value = "127.0.0.1:7420", env = "FRESH_GUI_LISTEN")]
    listen: SocketAddr,

    /// Fail if `--listen` is already in use (do not scan for a free port).
    #[arg(long, env = "FRESH_GUI_STRICT_LISTEN")]
    strict_listen: bool,

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
    let (listener, bound) = bind_listen(args.listen, args.strict_listen).await?;

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

    let ui_dir = if args.no_ui {
        None
    } else {
        resolve_ui_dir(args.ui_dir.as_deref())
    };

    let http_url = format!("http://{bound}/");
    let ws_url = format!("ws://{bound}/ws");
    info!(
        listen = %bound,
        preferred = %args.listen,
        auth_required = state.require_auth,
        fs_root = %state.fs_root.root_display(),
        editor = state.editor.is_some(),
        "starting fresh-gui-backend"
    );
    // Plain lines so the launch URL is obvious in `pixi run serve` output.
    println!();
    println!("  fresh-gui ready");
    println!("  UI:  {http_url}");
    println!("  WS:  {ws_url}");
    println!();

    server::serve_listener(listener, state, ui_dir)
        .await
        .context("server exited with error")
}

/// Bind `preferred`, or the next free ports on the same IP when busy.
async fn bind_listen(
    preferred: SocketAddr,
    strict: bool,
) -> Result<(tokio::net::TcpListener, SocketAddr)> {
    match tokio::net::TcpListener::bind(preferred).await {
        Ok(listener) => {
            let bound = listener.local_addr().context("local_addr after bind")?;
            return Ok((listener, bound));
        }
        Err(err) if err.kind() == ErrorKind::AddrInUse && !strict => {
            warn!(
                %preferred,
                "listen address in use — scanning for a free port"
            );
        }
        Err(err) => {
            return Err(err).with_context(|| format!("bind {preferred}"));
        }
    }

    let start = preferred.port();
    // Wrap around the u16 space carefully; stop after LISTEN_PORT_FALLBACK_SPAN tries.
    for offset in 1..=LISTEN_PORT_FALLBACK_SPAN {
        let port = start.wrapping_add(offset);
        if port == 0 {
            continue;
        }
        let candidate = SocketAddr::new(preferred.ip(), port);
        match tokio::net::TcpListener::bind(candidate).await {
            Ok(listener) => {
                let bound = listener.local_addr().context("local_addr after fallback bind")?;
                warn!(%preferred, %bound, "bound fallback listen address");
                return Ok((listener, bound));
            }
            Err(err) if err.kind() == ErrorKind::AddrInUse => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("bind {candidate}"));
            }
        }
    }

    bail!(
        "no free port near {preferred} (tried {LISTEN_PORT_FALLBACK_SPAN} ports); \
         stop the other process or pass --listen HOST:PORT"
    );
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

    for dir in ui_dir_candidates() {
        let Ok(dir) = dir.canonicalize() else {
            continue;
        };
        if dir.join("index.html").is_file() {
            info!(dir = %dir.display(), "serving web UI");
            return Some(dir);
        }
    }

    tracing::warn!(
        "no web UI found (expected share/fresh-gui/ui or crates/fresh-gui-app/ui/dist) — API only; \
         install the pixi package or run `pixi run ui-build`"
    );
    None
}

/// Search order for packaged installs, then workspace / dev layouts.
fn ui_dir_candidates() -> Vec<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    // Pixi / conda package layout: $PREFIX/bin/fresh-gui-backend → ../share/fresh-gui/ui
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            candidates.push(parent.join("../share/fresh-gui/ui"));
            candidates.push(parent.join("ui"));
            candidates.push(parent.join("../ui/dist"));
        }
    }
    if let Ok(prefix) = std::env::var("CONDA_PREFIX") {
        candidates.push(PathBuf::from(prefix).join("share/fresh-gui/ui"));
    }

    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../fresh-gui-app/ui/dist"),
    );
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("crates/fresh-gui-app/ui/dist"));
        candidates.push(cwd.join("ui/dist"));
        candidates.push(cwd.join("share/fresh-gui/ui"));
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::ui_dir_candidates;

    #[test]
    fn ui_dir_candidates_include_packaged_share_layout() {
        let candidates = ui_dir_candidates();
        assert!(
            candidates
                .iter()
                .any(|p| p.to_string_lossy().contains("share/fresh-gui/ui")),
            "expected a share/fresh-gui/ui candidate, got {candidates:?}"
        );
    }
}

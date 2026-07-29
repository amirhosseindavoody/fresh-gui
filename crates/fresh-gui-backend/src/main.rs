//! fresh-gui-backend — remote ADE daemon (PTY + FS + optional Fresh editor).

mod config;
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

use crate::config::Config;
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

    /// Hostname (or host:port) shown in startup UI/WS URLs.
    /// When unset, uses an assigned FQDN/domain if one is available; otherwise the bind address.
    #[arg(long, env = "FRESH_GUI_PUBLIC_HOST")]
    public_host: Option<String>,

    /// Path to JSON config (default: `$XDG_CONFIG_HOME/fresh-gui/config.json`
    /// or `~/.config/fresh-gui/config.json`). Missing file → built-in defaults
    /// (default shell: `zsh`).
    #[arg(long, env = "FRESH_GUI_CONFIG")]
    config: Option<PathBuf>,
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

    let (config, config_path) = Config::load(args.config.as_deref()).context("load config")?;
    let (default_shell, _) = config.resolve_shell();
    let config = Arc::new(std::sync::RwLock::new(config));

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
        config,
        config_path,
    });

    let ui_dir = if args.no_ui {
        None
    } else {
        resolve_ui_dir(args.ui_dir.as_deref())
    };

    let (http_url, ws_url) = public_urls(bound, args.public_host.as_deref());
    info!(
        listen = %bound,
        preferred = %args.listen,
        public_ui = %http_url,
        auth_required = state.require_auth,
        fs_root = %state.fs_root.root_display(),
        editor = state.editor.is_some(),
        default_shell = %default_shell,
        config = %state.config_path.display(),
        "starting fresh-gui-backend"
    );
    // Plain lines so the launch URL is obvious in `pixi run serve` output.
    println!();
    println!("  fresh-gui ready");
    println!("  UI:  {http_url}");
    println!("  WS:  {ws_url}");
    if bound.ip().is_loopback()
        && !http_url.contains("127.0.0.1")
        && !http_url.contains("[::1]")
        && !http_url.contains("localhost")
    {
        println!("  bind: {bound} (loopback — reach the domain via proxy/tunnel if needed)");
    }
    println!();

    server::serve_listener(listener, state, ui_dir, &http_url, &ws_url)
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

/// Build UI/WS URLs for the startup banner, preferring an assigned host domain.
fn public_urls(bound: SocketAddr, explicit_host: Option<&str>) -> (String, String) {
    let host_port = display_host_port(bound, explicit_host);
    (
        format!("http://{host_port}/"),
        format!("ws://{host_port}/ws"),
    )
}

fn display_host_port(bound: SocketAddr, explicit_host: Option<&str>) -> String {
    if let Some(host) = explicit_host.map(str::trim).filter(|s| !s.is_empty()) {
        return if host_has_port(host) {
            host.to_string()
        } else {
            format_host_port(host, bound.port())
        };
    }

    if let Some(domain) = assigned_host_domain() {
        return format_host_port(&domain, bound.port());
    }

    // Unspecified bind (0.0.0.0 / ::) is not a useful URL host.
    if bound.ip().is_unspecified() {
        return format_host_port("127.0.0.1", bound.port());
    }

    bound.to_string()
}

fn host_has_port(host: &str) -> bool {
    if let Some(rest) = host.strip_prefix('[') {
        // [ipv6]:port
        return rest
            .rsplit_once("]:")
            .map(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
            .unwrap_or(false);
    }
    // hostname:port or ipv4:port — reject multi-colon (bare IPv6).
    if host.chars().filter(|c| *c == ':').count() != 1 {
        return false;
    }
    host.rsplit_once(':')
        .map(|(_, port)| !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

fn format_host_port(host: &str, port: u16) -> String {
    if host.contains(':') && !host.starts_with('[') {
        // Bare IPv6 literal.
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

/// Return a machine domain/FQDN when one looks assigned (not localhost).
fn assigned_host_domain() -> Option<String> {
    let candidates = [
        std::env::var("FRESH_GUI_DOMAIN").ok(),
        std::env::var("HOSTNAME").ok(),
        hostname_command(&["-f"]),
        hostname_command(&[]),
    ];
    for candidate in candidates.into_iter().flatten() {
        if is_assigned_domain(&candidate) {
            return Some(candidate.trim().to_string());
        }
    }
    None
}

fn hostname_command(args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("hostname")
        .args(args)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

fn is_assigned_domain(name: &str) -> bool {
    let name = name.trim();
    if name.is_empty()
        || name.eq_ignore_ascii_case("localhost")
        || name.eq_ignore_ascii_case("(none)")
        || name.eq_ignore_ascii_case("localhost.localdomain")
    {
        return false;
    }
    // Require a dotted name so short DHCP labels (e.g. "ubuntu") do not replace loopback.
    let Some((label, rest)) = name.split_once('.') else {
        return false;
    };
    if label.is_empty() || rest.is_empty() {
        return false;
    }
    if name.ends_with(".localhost") || name.ends_with(".localdomain") {
        return false;
    }
    // Reject pure IPs — those are already covered by the bind address.
    if name.parse::<std::net::IpAddr>().is_ok() {
        return false;
    }
    true
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
    use super::{
        display_host_port, format_host_port, host_has_port, is_assigned_domain, public_urls,
        ui_dir_candidates,
    };
    use std::net::SocketAddr;

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

    #[test]
    fn assigned_domain_requires_dotted_non_local_name() {
        assert!(is_assigned_domain("gui.example.com"));
        assert!(is_assigned_domain("ip-10-0-0-1.ec2.internal"));
        assert!(!is_assigned_domain("localhost"));
        assert!(!is_assigned_domain("ubuntu"));
        assert!(!is_assigned_domain("foo.localhost"));
        assert!(!is_assigned_domain("127.0.0.1"));
        assert!(!is_assigned_domain(""));
    }

    #[test]
    fn host_has_port_detects_host_port_forms() {
        assert!(host_has_port("example.com:8443"));
        assert!(host_has_port("127.0.0.1:7420"));
        assert!(host_has_port("[::1]:7420"));
        assert!(!host_has_port("example.com"));
        assert!(!host_has_port("::1"));
    }

    #[test]
    fn explicit_public_host_wins_in_urls() {
        let bound: SocketAddr = "127.0.0.1:7420".parse().unwrap();
        let (http, ws) = public_urls(bound, Some("gui.example.com"));
        assert_eq!(http, "http://gui.example.com:7420/");
        assert_eq!(ws, "ws://gui.example.com:7420/ws");

        let (http, ws) = public_urls(bound, Some("gui.example.com:9000"));
        assert_eq!(http, "http://gui.example.com:9000/");
        assert_eq!(ws, "ws://gui.example.com:9000/ws");
    }

    #[test]
    fn display_host_port_formats_ipv6() {
        assert_eq!(format_host_port("::1", 7420), "[::1]:7420");
        let bound: SocketAddr = "[::1]:7420".parse().unwrap();
        // Without an assigned domain / explicit host, keep the bound address.
        let shown = display_host_port(bound, None);
        assert!(shown.contains("7420"), "{shown}");
    }
}

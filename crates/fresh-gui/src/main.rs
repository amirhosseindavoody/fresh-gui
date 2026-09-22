//! fresh-gui — remote ADE daemon (PTY + FS + optional Fresh editor).
//!
//! Default UX: start a **background** per-user session, print the access URL,
//! and return the terminal. Re-running `fresh-gui` prints status; `fresh-gui close`
//! stops the session.

mod config;
mod daemon;
mod editor_worker;
mod fs;
mod fs_watch;
mod memory_monitor;
mod path_open;
mod pty;
mod server;
mod session;
mod shell_resolve;
mod workspace;

use std::io::ErrorKind;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use crate::config::Config;
use crate::daemon::{
    SessionLock, SessionMeta, SessionPaths, close_session, live_session, print_session_info,
    serve_args_for_child, spawn_daemon, wait_until_ready, write_meta,
};
use crate::editor_worker::EditorHandle;
use crate::fs::FsRoot;
use crate::fs_watch::FsWatchStore;
use crate::memory_monitor::{DEFAULT_SAMPLE_INTERVAL, MemoryMonitor};
use crate::server::AppState;
use crate::session::SessionStore;
use crate::workspace::WorkspaceStore;

/// How many ports above the preferred one to try when the preferred bind is busy.
const LISTEN_PORT_FALLBACK_SPAN: u16 = 64;

#[derive(Debug, Parser)]
#[command(
    name = "fresh-gui",
    version,
    about = "Remote daemon for fresh-gui — one background session per user"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    serve: ServeArgs,

    /// Internal: run as the background server process (stdio already redirected).
    #[arg(long = "daemon-serve", hide = true, env = "FRESH_GUI_DAEMON_SERVE")]
    daemon_serve: bool,

    /// Print session meta as one JSON object on stdout (no banner).
    /// The desktop `fresh-gui` command uses this to read the loopback URL and
    /// token without putting the secret on the GUI process argv.
    #[arg(long, hide = true, global = true)]
    json: bool,

    /// Run the server in the foreground (do not detach). For tests / debugging.
    #[arg(long, env = "FRESH_GUI_FOREGROUND")]
    foreground: bool,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Stop the background session for this user.
    Close,
    /// Print URL / token / log path for the running session (if any).
    Status,
}

#[derive(Debug, Clone, clap::Args)]
struct ServeArgs {
    /// Listen address (default loopback). Prefer SSH tunnels over non-loopback binds.
    /// If the port is busy, the next free ports on the same host are tried
    /// (unless `--strict-listen`).
    #[arg(long, default_value = "127.0.0.1:7420", env = "FRESH_GUI_LISTEN")]
    listen: SocketAddr,

    /// Fail if `--listen` is already in use (do not scan for a free port).
    #[arg(long, env = "FRESH_GUI_STRICT_LISTEN")]
    strict_listen: bool,

    /// Shared auth token (also `FRESH_GUI_TOKEN`). Prefer the env var over this flag so
    /// the secret does not appear in `ps` output. When unset, a random token is
    /// generated for this process (printed once / stored in the private session meta).
    #[arg(long, env = "FRESH_GUI_TOKEN")]
    token: Option<String>,

    /// Disable auth (loopback binds only). For local integration tests — never use
    /// as a normal run mode.
    #[arg(long, env = "FRESH_GUI_ALLOW_NO_AUTH")]
    allow_no_auth: bool,

    /// Sandbox root for read-only `fs` listing and editor open (default: current directory).
    #[arg(long, env = "FRESH_GUI_FS_ROOT")]
    root: Option<PathBuf>,

    /// Disable the in-process Fresh editor (omit `editor` capability).
    #[arg(long, env = "FRESH_GUI_NO_EDITOR")]
    no_editor: bool,

    /// Accepted for compatibility. The daemon is always headless (WebSocket + health only).
    #[arg(long, env = "FRESH_GUI_NO_UI")]
    no_ui: bool,

    /// Hostname (or host:port) shown in startup UI/WS URLs.
    /// When unset, uses an assigned FQDN/domain if one is available; otherwise the bind address.
    #[arg(long, env = "FRESH_GUI_PUBLIC_HOST")]
    public_host: Option<String>,

    /// Path to JSON config (default: `$XDG_CONFIG_HOME/fresh-gui/config.json`
    /// or `~/.config/fresh-gui/config.json`; Windows: `%APPDATA%\fresh-gui\config.json`).
    /// Missing file → built-in defaults (shell: `zsh` on Unix, with fallback to
    /// `$SHELL`, then `bash`, then `sh` when that binary is missing;
    /// `powershell` on Windows).
    #[arg(long, env = "FRESH_GUI_CONFIG")]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Command::Close) => {
            let paths = SessionPaths::resolve()?;
            close_session(&paths)
        }
        Some(Command::Status) if cli.json => {
            let paths = SessionPaths::resolve()?;
            match live_session(&paths)? {
                Some(meta) => {
                    println!("{}", crate::daemon::session_meta_json(&meta)?);
                    Ok(())
                }
                None => {
                    // Quiet non-zero so the desktop launcher can tell "not running"
                    // apart from a JSON session without scraping a banner.
                    std::process::exit(1);
                }
            }
        }
        Some(Command::Status) => {
            let paths = SessionPaths::resolve()?;
            match live_session(&paths)? {
                Some(meta) => {
                    print_session_info(&meta);
                    Ok(())
                }
                None => {
                    println!("No fresh-gui session is running.");
                    println!("Start one with: fresh-gui");
                    Ok(())
                }
            }
        }
        None if cli.daemon_serve => {
            // Background child: lock + serve until SIGTERM.
            // Stdio is already redirected to the session log by the parent spawn.
            init_tracing_stdout();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("tokio runtime")?;
            rt.block_on(run_daemon_child(cli.serve))
        }
        None if cli.foreground => {
            init_tracing_stdout();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .context("tokio runtime")?;
            rt.block_on(run_server_foreground(cli.serve, /*write_meta=*/ false))
        }
        None => start_or_status(cli.serve, cli.json),
    }
}

fn emit_session(meta: &crate::daemon::SessionMeta, json: bool) -> Result<()> {
    if json {
        println!("{}", crate::daemon::session_meta_json(meta)?);
    } else {
        print_session_info(meta);
    }
    Ok(())
}

fn start_or_status(serve: ServeArgs, json: bool) -> Result<()> {
    let paths = SessionPaths::resolve()?;
    if let Some(meta) = live_session(&paths)? {
        emit_session(&meta, json)?;
        return Ok(());
    }

    let child_args = serve_args_for_child(
        &serve.listen.to_string(),
        serve.strict_listen,
        serve.token.as_deref(),
        serve.allow_no_auth,
        serve.root.as_deref(),
        serve.no_editor,
        serve.no_ui,
        serve.public_host.as_deref(),
        serve.config.as_deref(),
    );

    let child_pid = spawn_daemon(&child_args, &paths)?;
    if std::env::var_os("FRESH_GUI_QUIET").is_none() {
        eprintln!("Starting fresh-gui in the background (spawn pid {child_pid})…");
    }

    let meta = wait_until_ready(&paths).with_context(|| {
        format!(
            "daemon failed to start — check the log at {}",
            paths.log_path.display()
        )
    })?;
    emit_session(&meta, json)?;
    Ok(())
}

async fn run_daemon_child(serve: ServeArgs) -> Result<()> {
    let paths = SessionPaths::resolve()?;
    // Hold the lock for the process lifetime (dropped on exit).
    let _lock = SessionLock::try_acquire(&paths)?;
    run_server_foreground(serve, /*write_meta=*/ true).await
}

fn init_tracing_stdout() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_ansi(false)
        .init();
}

async fn run_server_foreground(args: ServeArgs, write_session_meta: bool) -> Result<()> {
    let paths = if write_session_meta {
        Some(SessionPaths::resolve()?)
    } else {
        None
    };

    let loopback = args.listen.ip().is_loopback();
    let AuthSetup {
        token,
        require_auth,
    } = resolve_auth(loopback, args.token.as_deref(), args.allow_no_auth)?;

    if !loopback {
        warn!(
            listen = %args.listen,
            "non-loopback bind — prefer 127.0.0.1 + an SSH tunnel; the token still protects the ADE handshake"
        );
    }
    if args.allow_no_auth {
        warn!("--allow-no-auth: authentication disabled (loopback test mode only)");
    }

    let root_path = args
        .root
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let root_display = root_path.display().to_string();
    let fs_root = FsRoot::new(root_path).context("init FS root")?;

    let (config, config_path) = Config::load(args.config.as_deref()).context("load config")?;
    let (default_shell, _) = config.resolve_shell();
    let config = Arc::new(std::sync::RwLock::new(config));

    // Bind before spawning Fresh so a busy port fails cleanly (no editor teardown panic).
    let (listener, bound) = bind_listen(args.listen, args.strict_listen).await?;

    // Sample daemon RSS cheaply for the session lifetime; summary is logged on shutdown.
    let memory = Arc::new(MemoryMonitor::start(DEFAULT_SAMPLE_INTERVAL));

    let editor = if args.no_editor {
        info!("Fresh editor disabled (--no-editor)");
        None
    } else {
        EditorHandle::spawn(fs_root.root_path().to_path_buf())
    };

    let sessions = SessionStore::new();
    let workspaces = match workspaces_state_path(paths.as_ref()) {
        Some(path) => WorkspaceStore::persistent(path),
        None => WorkspaceStore::new(),
    };
    for root in workspaces.load(&sessions).await {
        if let Err(err) = fs_root.authorize(&root).await {
            warn!(%root, "saved workspace root is not available: {err:#}");
        }
    }
    workspaces.spawn_saver();

    let state = Arc::new(AppState {
        token: token.clone(),
        require_auth,
        fs_root,
        sessions,
        workspaces,
        editor,
        watches: FsWatchStore::new(),
        config,
        config_path,
    });

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
        workspaces_file = %state
            .workspaces
            .state_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "(memory only)".into()),
        "starting fresh-gui"
    );

    if let Some(paths) = paths.as_ref() {
        let local_url = token
            .as_ref()
            .map(|t| format!("http://127.0.0.1:{}/?token={t}", bound.port()));
        let meta = SessionMeta {
            pid: std::process::id(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            bound: bound.to_string(),
            http_url: http_url.clone(),
            ws_url: ws_url.clone(),
            local_url,
            token: token.clone(),
            require_auth,
            root: root_display,
            log_path: paths.log_path.display().to_string(),
            started_at_unix: crate::daemon::now_unix(),
        };
        write_meta(paths, &meta)?;
        info!(path = %paths.meta_path.display(), "wrote session meta");
    } else {
        // Foreground / test mode: still print the banner to the terminal.
        print_startup_banner(bound, &http_url, &ws_url, token.as_deref());
    }

    let workspaces = state.workspaces.clone();
    let result =
        server::serve_listener(listener, state, &http_url, &ws_url, Some(memory.clone())).await;

    if let Err(err) = workspaces.save_now().await {
        warn!("final workspace save: {err:#}");
    }

    // Fallback if shutdown drained without the signal path finalizing (e.g. serve error).
    memory.finish();

    if let Some(paths) = paths.as_ref() {
        crate::daemon::remove_session_files(paths);
        info!("removed session meta");
    }

    result.context("server exited with error")
}

/// Where the workspace list is saved. The background daemon always saves to
/// the per-user state dir. `--foreground` (tests, debugging) stays in memory
/// unless `FRESH_GUI_WORKSPACES_FILE` names a file; that variable also
/// overrides the daemon's location.
fn workspaces_state_path(paths: Option<&SessionPaths>) -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("FRESH_GUI_WORKSPACES_FILE")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
    {
        return Some(explicit);
    }
    paths.map(|paths| paths.state_dir.join(crate::daemon::WORKSPACES_NAME))
}

#[derive(Debug)]
struct AuthSetup {
    token: Option<String>,
    require_auth: bool,
}

/// Resolve the process auth token and whether clients must present it.
fn resolve_auth(
    loopback: bool,
    explicit_token: Option<&str>,
    allow_no_auth: bool,
) -> Result<AuthSetup> {
    if allow_no_auth {
        if !loopback {
            bail!(
                "--allow-no-auth is only permitted on loopback binds (got a non-loopback listen)"
            );
        }
        return Ok(AuthSetup {
            token: None,
            require_auth: false,
        });
    }

    let token = match explicit_token.map(str::trim).filter(|t| !t.is_empty()) {
        Some(t) => t.to_owned(),
        None => uuid::Uuid::new_v4().simple().to_string(),
    };
    Ok(AuthSetup {
        token: Some(token),
        require_auth: true,
    })
}

fn print_startup_banner(bound: SocketAddr, http_url: &str, ws_url: &str, token: Option<&str>) {
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

    if let Some(token) = token {
        let port = bound.port();
        let local = format!("http://127.0.0.1:{port}/?token={token}");
        let user = ssh_tunnel_user();
        let host = ssh_tunnel_host();
        println!();
        println!("  Local access (this machine):");
        println!("    {local}");
        println!();
        println!(
            "  From another machine (e.g. your laptop) — SSH tunnel, nothing exposed to the network:"
        );
        println!("    ssh -L {port}:127.0.0.1:{port} {user}@{host}");
        println!("    fresh-gui user@{host}");
        println!("    fresh-gui --backend '{local}'");
    }
    println!();
}

fn ssh_tunnel_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "user".to_owned())
}

fn ssh_tunnel_host() -> String {
    assigned_host_domain().unwrap_or_else(|| "your-server".to_owned())
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
                let bound = listener
                    .local_addr()
                    .context("local_addr after fallback bind")?;
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
    if name.is_empty() { None } else { Some(name) }
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

#[cfg(test)]
mod tests {
    use super::{
        display_host_port, format_host_port, host_has_port, is_assigned_domain, public_urls,
        resolve_auth,
    };
    use std::net::SocketAddr;

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

    #[test]
    fn resolve_auth_auto_generates_token_by_default() {
        let setup = resolve_auth(true, None, false).unwrap();
        assert!(setup.require_auth);
        let token = setup.token.expect("token");
        assert_eq!(token.len(), 32);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn resolve_auth_keeps_explicit_token() {
        let setup = resolve_auth(true, Some("  pinned-secret  "), false).unwrap();
        assert!(setup.require_auth);
        assert_eq!(setup.token.as_deref(), Some("pinned-secret"));
    }

    #[test]
    fn resolve_auth_allow_no_auth_loopback_only() {
        let setup = resolve_auth(true, None, true).unwrap();
        assert!(!setup.require_auth);
        assert!(setup.token.is_none());

        let err = resolve_auth(false, None, true).unwrap_err();
        assert!(
            err.to_string().contains("allow-no-auth"),
            "unexpected error: {err}"
        );
    }
}

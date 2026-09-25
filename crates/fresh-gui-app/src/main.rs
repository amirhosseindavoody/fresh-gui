//! fresh-gui — native GPUI ADE host.
//!
//! Default: ensure the per-user headless daemon and open the desktop shell.
//! `user@host` and `remote connect` SSH-bootstrap a Linux daemon and open the
//! same window. `ping` / `smoke` / `attach` talk to a running daemon.
//!
//! The Cargo binary name stays `fresh-gui-app` so it does not collide with the
//! daemon package in `target/`. Installers place this executable on `PATH` as
//! `fresh-gui`.

mod gui;
mod launch;
mod ssh;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use fresh_gui_client::{Client, ConnectOptions, smoke_echo};
use fresh_gui_protocol::{Message, PROTOCOL_VERSION};
use ssh::{
    DaemonSource, SshTarget, Toolchain, bootstrap, load_remotes, remotes_config_path, save_remotes,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use tracing::info;
use tracing_subscriber::layer::{Context as LayerContext, Filter};
use tracing_subscriber::prelude::*;

/// Set before the window is removed, while its native handle is still valid.
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

pub(crate) fn note_window_shutdown() {
    SHUTTING_DOWN.store(true, Ordering::Release);
}

struct ShutdownWindowNoise;

fn known_shutdown_window_error(message: &str) -> bool {
    matches!(message.trim_matches('"'),
        "window not found" | "Invalid window handle (0x80040102)"
            | "Invalid window handle. (0x80070578)")
}

impl<S: tracing::Subscriber> Filter<S> for ShutdownWindowNoise {
    fn enabled(&self, _: &tracing::Metadata<'_>, _: &LayerContext<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &tracing::Event<'_>, _: &LayerContext<'_, S>) -> bool {
        if !cfg!(windows) || !SHUTTING_DOWN.load(Ordering::Acquire)
            || *event.metadata().level() != tracing::Level::ERROR
        {
            return true;
        }
        #[derive(Default)]
        struct MessageVisitor(Option<String>);
        impl tracing::field::Visit for MessageVisitor {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = Some(format!("{value:?}"));
                }
            }
            fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                if field.name() == "message" {
                    self.0 = Some(value.to_owned());
                }
            }
        }
        let mut message = MessageVisitor::default();
        event.record(&mut message);
        // GPUI's Windows teardown can report these after DestroyWindow has
        // invalidated the HWND (including its drag-drop target). The host's
        // layout and ADE writes are flushed before reaching this point.
        !message.0.as_deref().is_some_and(known_shutdown_window_error)
    }
}

#[derive(Debug, Parser)]
#[command(
    name = "fresh-gui",
    bin_name = "fresh-gui",
    version,
    about = "Open the fresh-gui desktop shell",
    after_help = "\
With no arguments, fresh-gui starts the local daemon when this user has none \
and opens the window against that session.\n\
A project path focuses that directory. user@host installs or reuses a Linux \
daemon over SSH and opens the window.\n\
Saved remotes: fresh-gui remote add / remote connect."
)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Local project directory, `user@host`, or a saved remote name.
    #[arg(value_name = "TARGET")]
    target: Option<String>,

    /// Project directory. With `user@host`, this is the remote `--root`.
    #[arg(long)]
    root: Option<String>,

    /// Start the local daemon if needed and return. Does not open a window.
    /// Remote bootstrap runs this on the Linux host (`fresh-gui --no-ui`).
    #[arg(long, env = "FRESH_GUI_NO_UI")]
    no_ui: bool,

    /// Backend WebSocket URL or a Local access HTTP URL (`?token=`).
    /// When set, fresh-gui does not start a local daemon.
    #[arg(long, global = true)]
    backend: Option<String>,

    /// Auth token if the backend requires one (also `FRESH_GUI_TOKEN`).
    /// The usual local and SSH paths read the token from the session file.
    #[arg(long, global = true, env = "FRESH_GUI_TOKEN")]
    token: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Open the native GPUI host (default when no subcommand is given).
    Gui,
    /// Print URL / token / log path for the local daemon session.
    Status,
    /// Stop the local daemon session.
    Close,
    /// Ping the backend (hello + ping/pong).
    Ping,
    /// Open a PTY, run a printf, print captured output (CI / smoke).
    Smoke,
    /// Interactive: open a PTY and forward stdin/stdout (no emulator; debug aid).
    Attach {
        #[arg(long, default_value_t = 80)]
        cols: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
    },
    /// Saved SSH remotes: install the Linux daemon if needed, tunnel ADE, open the GPUI host.
    Remote {
        #[command(subcommand)]
        cmd: RemoteCmd,
    },
}

#[derive(Debug, Subcommand)]
enum RemoteCmd {
    /// Save `user@host` or an OpenSSH `Host` alias. Auth is the system `ssh` client.
    Add {
        name: String,
        destination: String,
        /// SSH port (`ssh -p`).
        #[arg(long)]
        port: Option<u16>,
        /// Identity file (`ssh -i`).
        #[arg(long)]
        identity: Option<PathBuf>,
        /// Remote workspace passed to `fresh-gui --root` when this host starts the daemon.
        #[arg(long)]
        root: Option<String>,
        /// Local tunnel port. Omit to use 7420 when it is free, otherwise an ephemeral port.
        #[arg(long)]
        local_port: Option<u16>,
    },
    /// List saved SSH targets.
    List,
    /// Remove a saved SSH target.
    Remove { name: String },
    /// Probe, install or start the remote daemon, open a tunnel, and launch the GPUI host.
    Connect { name: String },
    /// Choose the Linux daemon binary used when the remote does not have `fresh-gui`.
    Daemon {
        /// Release `.tar.gz` URL (asset that contains `bin/fresh-gui`).
        #[arg(long)]
        url: Option<String>,
        /// Local Linux `fresh-gui` binary, or a `.tar.gz` archive of one.
        #[arg(long)]
        path: Option<PathBuf>,
        /// Clear a saved path/URL and use the latest GitHub linux-gnu release (the default).
        #[arg(long)]
        github_latest: bool,
    },
}

/// Our own logs at info. GPUI and gpui-kit log routine internals at info
/// (focus and a11y notes, frame details) that flood the launching shell, so
/// they start at warn. `RUST_LOG` replaces this whole filter.
const DEFAULT_LOG_FILTER: &str = "info,gpui=warn,gpui_base=warn,gpui_component=warn";

fn main() -> Result<()> {
    if std::env::var_os("FRESH_GUI_DAEMON_CHILD").is_some() {
        eprintln!("fresh-gui: this binary is the desktop app, not the headless daemon");
        std::process::exit(1);
    }

    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER)))
        .with(tracing_subscriber::fmt::layer().with_filter(ShutdownWindowNoise))
        .init();

    let args = Args::parse();
    match args.cmd {
        Some(Cmd::Ping) => {
            let (backend, token) = resolve_backend(args.backend, args.token)?;
            tokio_block_on(cmd_ping(backend, token))
        }
        Some(Cmd::Smoke) => {
            let (backend, token) = resolve_backend(args.backend, args.token)?;
            tokio_block_on(cmd_smoke(backend, token))
        }
        Some(Cmd::Attach { cols, rows }) => {
            let (backend, token) = resolve_backend(args.backend, args.token)?;
            tokio_block_on(cmd_attach(backend, token, cols, rows))
        }
        Some(Cmd::Remote { cmd }) => cmd_remote(cmd),
        Some(Cmd::Status) => delegate_daemon(&["status"]),
        Some(Cmd::Close) => delegate_daemon(&["close"]),
        Some(Cmd::Gui) | None => open_default(&args),
    }
}

fn open_default(args: &Args) -> Result<()> {
    if args.backend.is_some() && args.target.is_some() {
        anyhow::bail!("pass either TARGET or --backend");
    }
    if args.no_ui && args.backend.is_some() {
        anyhow::bail!("--no-ui starts the local daemon and does not use --backend");
    }
    if let Some(backend) = args.backend.clone() {
        return gui::run(backend, args.token.clone());
    }

    let path = ssh::remotes_config_path()?;
    let file = ssh::load_remotes(&path)?;
    let saved: Vec<launch::KnownRemote<'_>> = file
        .targets
        .iter()
        .map(|target| launch::KnownRemote {
            name: &target.name,
            destination: &target.destination,
        })
        .collect();
    let launch = launch::classify_launch(args.target.as_deref(), args.root.as_deref(), &saved)?;
    match launch {
        launch::LaunchTarget::Local { root } => {
            open_local(root.as_deref(), args.token.clone(), args.no_ui)
        }
        launch::LaunchTarget::SavedRemote {
            name,
            root_override,
        } => {
            if args.no_ui {
                anyhow::bail!(
                    "--no-ui is the local daemon. Use `fresh-gui remote connect {name}` to open a remote."
                );
            }
            cmd_remote_connect(&path, &name, root_override)
        }
        launch::LaunchTarget::AdHocRemote { destination, root } => {
            if args.no_ui {
                anyhow::bail!(
                    "--no-ui is the local daemon. Use `fresh-gui {destination}` to open that host."
                );
            }
            open_adhoc_remote(&destination, root)
        }
    }
}

fn open_local(root: Option<&std::path::Path>, token: Option<String>, no_ui: bool) -> Result<()> {
    let session = launch::ensure_local_session(root)?;
    if session.started {
        eprintln!("Started the local fresh-gui session (pid {}).", session.pid);
    } else {
        eprintln!(
            "Attaching to the running fresh-gui session (pid {}).",
            session.pid
        );
    }
    if no_ui {
        return Ok(());
    }
    let mut target = gui::parse_connect_target(&session.ws_url, token.or(session.token));
    target.local_daemon = true;
    target.preferred_root = session.preferred_root;
    gui::run_target(target)
}

fn open_adhoc_remote(destination: &str, root: Option<String>) -> Result<()> {
    let target = ssh::SshTarget {
        name: launch::ad_hoc_remote_name(destination),
        destination: destination.to_string(),
        port: None,
        identity: None,
        remote_root: root,
        local_port: None,
    };
    let path = ssh::remotes_config_path()?;
    let file = ssh::load_remotes(&path)?;
    let env_path = std::env::var("FRESH_GUI_DAEMON_PATH").ok();
    let env_url = std::env::var("FRESH_GUI_DAEMON_URL").ok();
    let daemon = ssh::DaemonSource::resolve(&file, env_path.as_deref(), env_url.as_deref());
    open_bootstrapped(&target, &daemon)
}

fn open_bootstrapped(target: &SshTarget, daemon: &DaemonSource) -> Result<()> {
    let session = bootstrap(target, daemon, &Toolchain::default(), |line| {
        eprintln!("{line}");
    })?;
    eprintln!(
        "Tunnel ready. Opening fresh-gui at {} ({})",
        session.ws_url, session.destination
    );
    let ws_url = session.ws_url.clone();
    let token = session.token.clone();
    let destination = session.destination.clone();
    let remote_control = ssh::RemoteControlHandle::new(
        target.clone(),
        daemon.clone(),
        Toolchain::default(),
        session,
    );
    let gui_target = gui::ConnectTarget {
        ws_url,
        token,
        local_daemon: false,
        label: Some(destination),
        preferred_root: target.remote_root.clone(),
        remote_control: Some(remote_control),
    };
    let result = gui::run_target(gui_target);
    eprintln!("Closed SSH tunnel.");
    result
}

fn resolve_backend(
    backend: Option<String>,
    token: Option<String>,
) -> Result<(String, Option<String>)> {
    if let Some(backend) = backend {
        return Ok((backend, token));
    }
    let session = launch::ensure_local_session(None)?;
    Ok((session.ws_url, token.or(session.token)))
}

fn delegate_daemon(args: &[&str]) -> Result<()> {
    let bin = launch::find_daemon()?;
    let status = std::process::Command::new(&bin)
        .args(args)
        .env("FRESH_GUI_DAEMON_CHILD", "1")
        .status()
        .with_context(|| format!("run {} {}", bin.display(), args.join(" ")))?;
    std::process::exit(status.code().unwrap_or(1));
}

fn tokio_block_on<F>(fut: F) -> Result<()>
where
    F: std::future::Future<Output = Result<()>>,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("tokio runtime")?
        .block_on(fut)
}

struct SharedArgs {
    backend: String,
    token: Option<String>,
}

async fn connect(args: &SharedArgs) -> Result<Client> {
    let mut opts = ConnectOptions::new(&args.backend);
    if let Some(token) = &args.token {
        opts = opts.with_token(token.clone());
    }
    Client::connect(opts)
        .await
        .with_context(|| format!("connect to {}", args.backend))
}

async fn cmd_ping(backend: String, token: Option<String>) -> Result<()> {
    let args = SharedArgs { backend, token };
    let mut client = connect(&args).await?;
    info!(
        protocol = PROTOCOL_VERSION,
        backend = %client.backend_hello.implementation,
        caps = ?client.backend_hello.capabilities,
        "connected"
    );
    client.ping(1).await?;
    loop {
        match client.recv().await? {
            Message::Pong { nonce } => {
                info!(nonce, "pong");
                break;
            }
            Message::Ping { nonce } => {
                info!(nonce, "unexpected ping");
            }
            other => info!(?other, "skip"),
        }
    }
    Ok(())
}

async fn cmd_smoke(backend: String, token: Option<String>) -> Result<()> {
    let out = smoke_echo(&backend, token.as_deref()).await?;
    println!("{out}");
    Ok(())
}

async fn cmd_attach(backend: String, token: Option<String>, cols: u16, rows: u16) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let args = SharedArgs { backend, token };
    let mut client = connect(&args).await?;
    let id = client.open_pty(cols, rows, None, None).await?;
    info!(%id, "pty opened — type to send; Ctrl-C to quit");

    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let mut in_buf = [0u8; 1024];

    loop {
        tokio::select! {
            n = stdin.read(&mut in_buf) => {
                let n = n?;
                if n == 0 {
                    break;
                }
                client.write_pty(&id, &in_buf[..n]).await?;
            }
            msg = client.recv() => {
                match msg? {
                    Message::PtyData { id: pid, data } if pid == id => {
                        let bytes = Client::decode_pty_data(&data)?;
                        stdout.write_all(&bytes).await?;
                        stdout.flush().await?;
                    }
                    Message::PtyClosed { id: pid, reason } if pid == id => {
                        info!(?reason, "pty closed");
                        break;
                    }
                    Message::Error { code, message } => {
                        anyhow::bail!("{code}: {message}");
                    }
                    _ => {}
                }
            }
        }
    }
    Ok(())
}

fn cmd_remote(cmd: RemoteCmd) -> Result<()> {
    let path = remotes_config_path()?;
    match cmd {
        RemoteCmd::Add {
            name,
            destination,
            port,
            identity,
            root,
            local_port,
        } => {
            let mut file = load_remotes(&path)?;
            file.add(SshTarget {
                name: name.clone(),
                destination: destination.clone(),
                port,
                identity: identity.map(|p| p.display().to_string()),
                remote_root: root,
                local_port,
            })?;
            save_remotes(&path, &file)?;
            println!(
                "Saved SSH target '{name}' ({destination}) in {}",
                path.display()
            );
            println!("Connect with: fresh-gui remote connect {name}");
            Ok(())
        }
        RemoteCmd::List => {
            let file = load_remotes(&path)?;
            if file.targets.is_empty() {
                println!("No saved SSH targets ({})", path.display());
                println!("Add one with: fresh-gui remote add <name> <user@host>");
                return Ok(());
            }
            println!("SSH targets ({})", path.display());
            for target in &file.targets {
                let mut extra = String::new();
                if let Some(port) = target.port {
                    extra.push_str(&format!("  port {port}"));
                }
                if let Some(root) = &target.remote_root {
                    extra.push_str(&format!("  root {root}"));
                }
                println!("  {}  {}{extra}", target.name, target.destination);
            }
            let source = DaemonSource::resolve(&file, None, None);
            if let Some(daemon_path) = source.path {
                println!("Daemon binary: {}", daemon_path.display());
            } else if let Some(url) = source.url {
                println!("Daemon URL: {url}");
            } else {
                println!("Daemon binary: latest GitHub linux-gnu release");
            }
            Ok(())
        }
        RemoteCmd::Remove { name } => {
            let mut file = load_remotes(&path)?;
            let removed = file.remove(&name)?;
            save_remotes(&path, &file)?;
            println!(
                "Removed SSH target '{}' ({}) from {}",
                removed.name,
                removed.destination,
                path.display()
            );
            Ok(())
        }
        RemoteCmd::Connect { name } => cmd_remote_connect(&path, &name, None),
        RemoteCmd::Daemon {
            url,
            path: daemon_path,
            github_latest,
        } => cmd_remote_daemon(&path, url, daemon_path, github_latest),
    }
}

fn cmd_remote_daemon(
    path: &std::path::Path,
    url: Option<String>,
    daemon_path: Option<PathBuf>,
    github_latest: bool,
) -> Result<()> {
    let chosen = [url.is_some(), daemon_path.is_some(), github_latest]
        .iter()
        .filter(|set| **set)
        .count();
    if chosen != 1 {
        anyhow::bail!("pass exactly one of --url, --path, or --github-latest");
    }
    let mut file = load_remotes(path)?;
    if github_latest {
        file.daemon_url = None;
        file.daemon_path = None;
        save_remotes(path, &file)?;
        println!("Daemon source: latest GitHub linux-gnu release");
    } else if let Some(url) = url {
        let url = url.trim().to_string();
        anyhow::ensure!(!url.is_empty(), "daemon URL is empty");
        file.daemon_url = Some(url.clone());
        file.daemon_path = None;
        save_remotes(path, &file)?;
        println!("Daemon URL: {url}");
    } else if let Some(daemon_path) = daemon_path {
        anyhow::ensure!(
            daemon_path.is_file(),
            "daemon path {} is not a file",
            daemon_path.display()
        );
        file.daemon_path = Some(daemon_path.display().to_string());
        file.daemon_url = None;
        save_remotes(path, &file)?;
        println!("Daemon path: {}", daemon_path.display());
    }
    println!("Saved in {}", path.display());
    println!("FRESH_GUI_DAEMON_PATH and FRESH_GUI_DAEMON_URL override this file for one connect.");
    Ok(())
}

fn cmd_remote_connect(
    path: &std::path::Path,
    name: &str,
    root_override: Option<String>,
) -> Result<()> {
    let file = load_remotes(path)?;
    let mut target = file.get(name)?.clone();
    if let Some(root) = root_override {
        target.remote_root = Some(root);
    }
    let env_path = std::env::var("FRESH_GUI_DAEMON_PATH").ok();
    let env_url = std::env::var("FRESH_GUI_DAEMON_URL").ok();
    let daemon = DaemonSource::resolve(&file, env_path.as_deref(), env_url.as_deref());
    open_bootstrapped(&target, &daemon)
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn shutdown_filter_matches_only_reported_window_errors() {
        assert!(known_shutdown_window_error("window not found"));
        assert!(known_shutdown_window_error("Invalid window handle (0x80040102)"));
        assert!(known_shutdown_window_error("Invalid window handle. (0x80070578)"));
        assert!(!known_shutdown_window_error("Invalid window handle (0x80070578)"));
        assert!(!known_shutdown_window_error("other error"));
    }

    #[test]
    fn default_log_filter_quiets_gpui_but_keeps_ours() {
        let filter = tracing_subscriber::EnvFilter::try_new(DEFAULT_LOG_FILTER).unwrap();
        let text = filter.to_string();
        assert!(text.contains("gpui=warn"));
        assert!(text.starts_with("info") || text.contains(",info") || text.contains("info,"));
    }

    #[test]
    fn bare_invocation_opens_the_local_app() {
        let args = Args::try_parse_from(["fresh-gui"]).unwrap();
        assert!(args.cmd.is_none());
        assert!(args.target.is_none());
        assert!(args.backend.is_none());
        assert!(!args.no_ui);
    }

    #[test]
    fn project_path_and_ssh_destination_are_positionals() {
        let path = Args::try_parse_from(["fresh-gui", "/work/app"]).unwrap();
        assert_eq!(path.target.as_deref(), Some("/work/app"));
        assert!(path.cmd.is_none());

        let remote = Args::try_parse_from(["fresh-gui", "ada@lab", "--root", "/srv/app"]).unwrap();
        assert_eq!(remote.target.as_deref(), Some("ada@lab"));
        assert_eq!(remote.root.as_deref(), Some("/srv/app"));
    }

    #[test]
    fn subcommands_stay_subcommands() {
        let status = Args::try_parse_from(["fresh-gui", "status"]).unwrap();
        assert!(matches!(status.cmd, Some(Cmd::Status)));

        let connect = Args::try_parse_from(["fresh-gui", "remote", "connect", "lab"]).unwrap();
        assert!(matches!(connect.cmd, Some(Cmd::Remote { .. })));

        let headless =
            Args::try_parse_from(["fresh-gui", "--no-ui", "--root", "/work/app"]).unwrap();
        assert!(headless.no_ui);
        assert_eq!(headless.root.as_deref(), Some("/work/app"));
        assert!(headless.cmd.is_none());
    }

    #[test]
    fn explicit_backend_skips_the_positional() {
        let args = Args::try_parse_from(["fresh-gui", "--backend", "ws://127.0.0.1:9/ws"]).unwrap();
        assert_eq!(args.backend.as_deref(), Some("ws://127.0.0.1:9/ws"));
        assert!(args.target.is_none());
    }
}

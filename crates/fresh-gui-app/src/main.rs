//! fresh-gui-app — native GPUI ADE host, plus CLI helpers and optional static UI.
//!
//! Default (no subcommand): open the native desktop shell.
//! CLI: `ping` / `smoke` / `attach` talk to a running daemon.
//! `serve-ui` still serves the Vite `ui/dist` bundle for browser smoke tests.

mod gui;
mod ssh;

use anyhow::{Context, Result};
use axum::Router;
use clap::{Parser, Subcommand};
use fresh_gui_client::{Client, ConnectOptions, smoke_echo};
use fresh_gui_protocol::{Message, PROTOCOL_VERSION};
use ssh::{
    DaemonSource, SshTarget, Toolchain, bootstrap, load_remotes, remotes_config_path, save_remotes,
};
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::services::ServeDir;
use tracing::info;

#[derive(Debug, Parser)]
#[command(
    name = "fresh-gui-app",
    version,
    about = "Native GPUI ADE host for fresh-gui"
)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Backend WebSocket URL or the printed Local access HTTP URL (`?token=`).
    #[arg(long, global = true, default_value = "ws://127.0.0.1:7420/ws")]
    backend: String,

    /// Auth token if the backend requires one (also `FRESH_GUI_TOKEN`).
    #[arg(long, global = true, env = "FRESH_GUI_TOKEN")]
    token: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Open the native GPUI host (default when no subcommand is given).
    Gui,
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
    /// Serve the built Vite UI over HTTP (browser smoke tests; not the primary host).
    ServeUi {
        #[arg(long, default_value = "127.0.0.1:1420")]
        listen: SocketAddr,
        /// Directory with index.html (defaults to `ui/dist` from a Vite build).
        #[arg(long)]
        dir: Option<PathBuf>,
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

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let Args {
        cmd,
        backend,
        token,
    } = Args::parse();

    match cmd.unwrap_or(Cmd::Gui) {
        Cmd::Gui => gui::run(backend, token),
        Cmd::Ping => tokio_block_on(cmd_ping(backend, token)),
        Cmd::Smoke => tokio_block_on(cmd_smoke(backend, token)),
        Cmd::Attach { cols, rows } => tokio_block_on(cmd_attach(backend, token, cols, rows)),
        Cmd::Remote { cmd } => cmd_remote(cmd),
        Cmd::ServeUi { listen, dir } => tokio_block_on(cmd_serve_ui(listen, dir)),
    }
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
            println!("Connect with: fresh-gui-app remote connect {name}");
            Ok(())
        }
        RemoteCmd::List => {
            let file = load_remotes(&path)?;
            if file.targets.is_empty() {
                println!("No saved SSH targets ({})", path.display());
                println!("Add one with: fresh-gui-app remote add <name> <user@host>");
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
        RemoteCmd::Connect { name } => cmd_remote_connect(&path, &name),
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

fn cmd_remote_connect(path: &std::path::Path, name: &str) -> Result<()> {
    let file = load_remotes(path)?;
    let target = file.get(name)?.clone();
    let env_path = std::env::var("FRESH_GUI_DAEMON_PATH").ok();
    let env_url = std::env::var("FRESH_GUI_DAEMON_URL").ok();
    let daemon = DaemonSource::resolve(&file, env_path.as_deref(), env_url.as_deref());
    let session = bootstrap(&target, &daemon, &Toolchain::default(), |line| {
        eprintln!("{line}");
    })?;
    eprintln!(
        "Tunnel ready. Opening the GPUI host at {} ({})",
        session.ws_url, session.destination
    );
    let gui_target = gui::ConnectTarget {
        ws_url: session.ws_url.clone(),
        token: session.token.clone(),
        label: Some(session.destination.clone()),
    };
    let result = gui::run_target(gui_target);
    drop(session);
    eprintln!("Closed SSH tunnel.");
    result
}

async fn cmd_serve_ui(listen: SocketAddr, dir: Option<PathBuf>) -> Result<()> {
    let dir = dir.unwrap_or_else(|| {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("ui")
            .join("dist")
    });
    anyhow::ensure!(
        dir.join("index.html").is_file(),
        "missing {} — run `pixi run ui-install && pixi run ui-build` (or `pixi run ui` for Vite dev). The native host is `fresh-gui-app` / `pixi run gui`.",
        dir.join("index.html").display()
    );

    let app = Router::new().fallback_service(ServeDir::new(&dir));
    info!(%listen, dir = %dir.display(), "serving Vite UI (smoke) — open http://{listen}/");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

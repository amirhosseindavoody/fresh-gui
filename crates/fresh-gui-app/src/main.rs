//! fresh-gui-app — native GPUI ADE host, plus CLI helpers and optional static UI.
//!
//! Default (no subcommand): open the native desktop shell.
//! CLI: `ping` / `smoke` / `attach` talk to a running daemon.
//! `serve-ui` still serves the Vite `ui/dist` bundle for browser smoke tests.

mod gui;

use anyhow::{Context, Result};
use axum::Router;
use clap::{Parser, Subcommand};
use fresh_gui_client::{Client, ConnectOptions, smoke_echo};
use fresh_gui_protocol::{Message, PROTOCOL_VERSION};
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
    /// Serve the built Vite UI over HTTP (browser smoke tests; not the primary host).
    ServeUi {
        #[arg(long, default_value = "127.0.0.1:1420")]
        listen: SocketAddr,
        /// Directory with index.html (defaults to `ui/dist` from a Vite build).
        #[arg(long)]
        dir: Option<PathBuf>,
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

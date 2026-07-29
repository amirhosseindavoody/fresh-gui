//! fresh-gui — host ADE shell (Phase 1).
//!
//! CLI: connect to a backend and run a PTY smoke test, or open the UI when built
//! with the `tauri` feature / `fresh-gui` desktop entry.

use anyhow::{Context, Result};
use axum::Router;
use clap::{Parser, Subcommand};
use fresh_gui_client::{smoke_echo, Client, ConnectOptions};
use fresh_gui_protocol::{Message, PROTOCOL_VERSION};
use std::net::SocketAddr;
use std::path::PathBuf;
use tower_http::services::ServeDir;
use tracing::info;

#[derive(Debug, Parser)]
#[command(name = "fresh-gui", version, about = "Terminal-first ADE GUI (host)")]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,

    /// Backend WebSocket URL (used by `smoke` / `ping`).
    #[arg(long, global = true, default_value = "ws://127.0.0.1:7420/ws")]
    backend: String,

    /// Auth token if the backend requires one.
    #[arg(long, global = true, env = "FRESH_GUI_TOKEN")]
    token: Option<String>,
}

#[derive(Debug, Subcommand)]
enum Cmd {
    /// Ping the backend (hello + ping/pong).
    Ping,
    /// Open a PTY, run a printf, print captured output (CI / smoke).
    Smoke,
    /// Interactive: open a PTY and forward stdin/stdout (no xterm; debug aid).
    Attach {
        #[arg(long, default_value_t = 80)]
        cols: u16,
        #[arg(long, default_value_t = 24)]
        rows: u16,
    },
    /// Serve the built Vite UI over HTTP (dev stand-in before / alongside Tauri).
    ServeUi {
        #[arg(long, default_value = "127.0.0.1:1420")]
        listen: SocketAddr,
        /// Directory with index.html (defaults to `ui/dist` from a Vite build).
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
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
    let args = SharedArgs { backend, token };
    match cmd.unwrap_or(Cmd::Ping) {
        Cmd::Ping => cmd_ping(&args).await,
        Cmd::Smoke => cmd_smoke(&args).await,
        Cmd::Attach { cols, rows } => cmd_attach(&args, cols, rows).await,
        Cmd::ServeUi { listen, dir } => cmd_serve_ui(listen, dir).await,
    }
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

async fn cmd_ping(args: &SharedArgs) -> Result<()> {
    let mut client = connect(args).await?;
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
                // backend shouldn't ping first in MVP
                info!(nonce, "unexpected ping");
            }
            other => info!(?other, "skip"),
        }
    }
    Ok(())
}

async fn cmd_smoke(args: &SharedArgs) -> Result<()> {
    let out = smoke_echo(&args.backend, args.token.as_deref()).await?;
    println!("{out}");
    Ok(())
}

async fn cmd_attach(args: &SharedArgs, cols: u16, rows: u16) -> Result<()> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let mut client = connect(args).await?;
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
        "missing {} — run `pixi run ui-install && pixi run ui-build` (or `pixi run ui` for Vite dev)",
        dir.join("index.html").display()
    );

    let app = Router::new().fallback_service(ServeDir::new(&dir));
    info!(%listen, dir = %dir.display(), "serving UI — open http://{listen}/");
    let listener = tokio::net::TcpListener::bind(listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

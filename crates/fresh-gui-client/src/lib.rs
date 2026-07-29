//! Host-side WebSocket client for the fresh-gui ADE protocol.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use fresh_gui_protocol::{Hello, Message, PtyInfo, SessionInfo, PROTOCOL_VERSION};
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_tungstenite::{
    connect_async, tungstenite::Message as WsMessage, MaybeTlsStream, WebSocketStream,
};
use url::Url;

pub use fresh_gui_protocol;

/// Connection options.
#[derive(Debug, Clone)]
pub struct ConnectOptions {
    /// e.g. `ws://127.0.0.1:7420/ws`
    pub url: String,
    pub token: Option<String>,
}

impl ConnectOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            token: None,
        }
    }

    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }
}

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// Connected ADE client.
pub struct Client {
    sink: futures_util::stream::SplitSink<Ws, WsMessage>,
    stream: futures_util::stream::SplitStream<Ws>,
    pub backend_hello: Hello,
    pub session_id: Option<String>,
}

impl Client {
    pub async fn connect(opts: ConnectOptions) -> Result<Self> {
        let url = Url::parse(&opts.url).context("parse backend url")?;
        let (ws, _) = connect_async(url.as_str())
            .await
            .with_context(|| format!("connect {}", opts.url))?;
        let (mut sink, mut stream) = ws.split();

        let backend_hello = match recv_msg(&mut stream).await.context("backend hello")? {
            Message::Hello(h) => h,
            other => bail!("expected Hello from backend, got {other:?}"),
        };
        if backend_hello.protocol_version != PROTOCOL_VERSION {
            bail!(
                "protocol mismatch: client {PROTOCOL_VERSION} vs backend {}",
                backend_hello.protocol_version
            );
        }

        let client_hello = Message::Hello(Hello::client(
            format!("fresh-gui-client/{}", env!("CARGO_PKG_VERSION")),
            Hello::default_client_caps(),
        ));
        send_msg(&mut sink, &client_hello).await?;

        if let Some(token) = opts.token {
            send_msg(&mut sink, &Message::Auth { token }).await?;
            match recv_msg(&mut stream).await.context("auth response")? {
                Message::AuthOk => {}
                Message::AuthError { message } => bail!("auth failed: {message}"),
                other => bail!("unexpected auth response: {other:?}"),
            }
        }

        Ok(Self {
            sink,
            stream,
            backend_hello,
            session_id: None,
        })
    }

    pub async fn ping(&mut self, nonce: u64) -> Result<()> {
        send_msg(&mut self.sink, &Message::Ping { nonce }).await
    }

    pub async fn create_session(&mut self, layout: Option<String>) -> Result<String> {
        send_msg(&mut self.sink, &Message::SessionCreate { layout }).await?;
        loop {
            match self.recv().await? {
                Message::SessionCreated { session_id } => {
                    self.session_id = Some(session_id.clone());
                    return Ok(session_id);
                }
                Message::Error { code, message } => {
                    bail!("session create failed: {code}: {message}")
                }
                Message::Pong { .. } | Message::Ping { .. } | Message::AuthOk => continue,
                other => bail!("unexpected while creating session: {other:?}"),
            }
        }
    }

    pub async fn attach_session(
        &mut self,
        session_id: impl Into<String>,
    ) -> Result<(Vec<PtyInfo>, Option<String>)> {
        let session_id = session_id.into();
        send_msg(
            &mut self.sink,
            &Message::SessionAttach {
                session_id: session_id.clone(),
            },
        )
        .await?;
        loop {
            match self.recv().await? {
                Message::SessionAttached {
                    session_id: sid,
                    ptys,
                    layout,
                } => {
                    self.session_id = Some(sid);
                    return Ok((ptys, layout));
                }
                Message::Error { code, message } => {
                    bail!("session attach failed: {code}: {message}")
                }
                Message::Pong { .. } | Message::Ping { .. } | Message::AuthOk => continue,
                other => bail!("unexpected while attaching session: {other:?}"),
            }
        }
    }

    pub async fn list_sessions(&mut self) -> Result<Vec<SessionInfo>> {
        send_msg(&mut self.sink, &Message::SessionList).await?;
        loop {
            match self.recv().await? {
                Message::SessionListed { sessions } => return Ok(sessions),
                Message::Error { code, message } => {
                    bail!("session list failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::Pong { .. }
                | Message::Ping { .. } => continue,
                other => bail!("unexpected while listing sessions: {other:?}"),
            }
        }
    }

    pub async fn set_layout(&mut self, layout: impl Into<String>) -> Result<()> {
        send_msg(
            &mut self.sink,
            &Message::LayoutSet {
                layout: layout.into(),
            },
        )
        .await
    }

    pub async fn open_pty(
        &mut self,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
        shell: Option<String>,
    ) -> Result<String> {
        send_msg(
            &mut self.sink,
            &Message::PtyOpen {
                cols,
                rows,
                cwd,
                shell,
            },
        )
        .await?;
        loop {
            match self.recv().await? {
                Message::PtyOpened { id, .. } => return Ok(id),
                Message::Error { code, message } => {
                    bail!("pty open failed: {code}: {message}")
                }
                Message::SessionCreated { session_id } => {
                    self.session_id = Some(session_id);
                }
                Message::Pong { .. }
                | Message::Ping { .. }
                | Message::AuthOk
                | Message::FsListed { .. }
                | Message::FsStatResult { .. } => continue,
                other => bail!("unexpected while opening pty: {other:?}"),
            }
        }
    }

    pub async fn write_pty(&mut self, id: &str, data: &[u8]) -> Result<()> {
        let data = base64::engine::general_purpose::STANDARD.encode(data);
        send_msg(
            &mut self.sink,
            &Message::PtyData {
                id: id.to_owned(),
                data,
            },
        )
        .await
    }

    pub async fn resize_pty(&mut self, id: &str, cols: u16, rows: u16) -> Result<()> {
        send_msg(
            &mut self.sink,
            &Message::PtyResize {
                id: id.to_owned(),
                cols,
                rows,
            },
        )
        .await
    }

    pub async fn close_pty(&mut self, id: &str) -> Result<()> {
        send_msg(
            &mut self.sink,
            &Message::PtyClose {
                id: id.to_owned(),
            },
        )
        .await
    }

    pub async fn list_dir(
        &mut self,
        path: impl Into<String>,
    ) -> Result<(String, Vec<fresh_gui_protocol::FsEntry>)> {
        let request_id = format!("fs-{}", uuid_simple());
        send_msg(
            &mut self.sink,
            &Message::FsList {
                request_id: request_id.clone(),
                path: path.into(),
            },
        )
        .await?;
        loop {
            match self.recv().await? {
                Message::FsListed {
                    request_id: rid,
                    path,
                    entries,
                } if rid == request_id => return Ok((path, entries)),
                Message::Error { code, message } => {
                    bail!("fs list failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::PtyClosed { .. }
                | Message::Pong { .. }
                | Message::Ping { .. } => continue,
                other => bail!("unexpected while listing dir: {other:?}"),
            }
        }
    }

    pub async fn recv(&mut self) -> Result<Message> {
        recv_msg(&mut self.stream).await
    }

    pub fn decode_pty_data(data_b64: &str) -> Result<Vec<u8>> {
        Ok(base64::engine::general_purpose::STANDARD.decode(data_b64)?)
    }
}

fn uuid_simple() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:x}")
}

async fn send_msg(
    sink: &mut futures_util::stream::SplitSink<Ws, WsMessage>,
    msg: &Message,
) -> Result<()> {
    let json = msg.to_json()?;
    sink.send(WsMessage::Text(json.into())).await?;
    Ok(())
}

async fn recv_msg(stream: &mut futures_util::stream::SplitStream<Ws>) -> Result<Message> {
    while let Some(frame) = stream.next().await {
        let frame = frame.context("ws read")?;
        match frame {
            WsMessage::Text(text) => return Ok(Message::from_json(&text)?),
            WsMessage::Ping(_) | WsMessage::Pong(_) => continue,
            WsMessage::Close(_) => bail!("websocket closed"),
            WsMessage::Binary(_) | WsMessage::Frame(_) => continue,
        }
    }
    bail!("websocket ended")
}

/// Run a short PTY echo smoke test against a running backend.
pub async fn smoke_echo(url: &str, token: Option<&str>) -> Result<String> {
    let mut opts = ConnectOptions::new(url);
    if let Some(t) = token {
        opts = opts.with_token(t);
    }
    let mut client = Client::connect(opts).await?;
    let _ = client.create_session(None).await?;
    let id = client
        .open_pty(80, 24, None, Some("/bin/bash".into()))
        .await?;

    client
        .write_pty(&id, b"printf 'fresh-gui-ok\\n'; exit\\n")
        .await?;

    let mut collected = String::new();
    let deadline = Duration::from_secs(5);
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        let msg = match timeout(remaining, client.recv()).await {
            Ok(Ok(m)) => m,
            Ok(Err(err)) => return Err(err),
            Err(_) => break,
        };
        match msg {
            Message::PtyData { id: pid, data } if pid == id => {
                let bytes = Client::decode_pty_data(&data)?;
                collected.push_str(&String::from_utf8_lossy(&bytes));
                if collected.contains("fresh-gui-ok") {
                    return Ok(collected);
                }
            }
            Message::PtyClosed { id: pid, .. } if pid == id => break,
            Message::Pong { .. } | Message::Ping { .. } => {}
            Message::Error { code, message } => bail!("error {code}: {message}"),
            _ => {}
        }
    }
    bail!("did not observe fresh-gui-ok in pty output: {collected:?}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_options_token() {
        let opts = ConnectOptions::new("ws://127.0.0.1:7420/ws").with_token("secret");
        assert_eq!(opts.token.as_deref(), Some("secret"));
    }
}

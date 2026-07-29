//! WebSocket ADE server (JSON frames).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use base64::Engine;
use fresh_gui_protocol::{Hello, Message, PROTOCOL_VERSION};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::{mpsc, Mutex};
use tracing::{info, warn};
use uuid::Uuid;

use crate::pty::PtySession;

pub struct AppState {
    pub token: Option<String>,
    pub require_auth: bool,
}

pub async fn serve(addr: SocketAddr, state: Arc<AppState>) -> Result<()> {
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    info!(%addr, "listening (ws path /ws)");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn ws_upgrade(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();

    let hello = Message::Hello(Hello::backend(
        format!("fresh-gui-backend/{}", env!("CARGO_PKG_VERSION")),
        Hello::default_backend_caps(),
    ));
    if send_msg(&mut sink, &hello).await.is_err() {
        return;
    }

    let mut authed = !state.require_auth;
    let ptys: Arc<Mutex<HashMap<String, PtySession>>> = Arc::new(Mutex::new(HashMap::new()));
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();

    loop {
        tokio::select! {
            maybe_out = out_rx.recv() => {
                match maybe_out {
                    Some(msg) => {
                        if send_msg(&mut sink, &msg).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            frame = stream.next() => {
                let Some(frame) = frame else { break };
                let Ok(frame) = frame else { break };
                let WsMessage::Text(text) = frame else {
                    // ignore binary/ping; axum handles ping/pong at protocol layer
                    continue;
                };
                let msg = match Message::from_json(&text) {
                    Ok(m) => m,
                    Err(err) => {
                        let _ = send_msg(
                            &mut sink,
                            &Message::Error {
                                code: "bad_json".into(),
                                message: err.to_string(),
                            },
                        )
                        .await;
                        continue;
                    }
                };

                if let Err(resp) = handle_client_msg(
                    msg,
                    &state,
                    &mut authed,
                    &ptys,
                    out_tx.clone(),
                    &mut sink,
                )
                .await
                {
                    let _ = send_msg(&mut sink, &resp).await;
                }
            }
        }
    }

    info!("websocket client disconnected");
}

async fn handle_client_msg(
    msg: Message,
    state: &AppState,
    authed: &mut bool,
    ptys: &Arc<Mutex<HashMap<String, PtySession>>>,
    out_tx: mpsc::UnboundedSender<Message>,
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
) -> Result<(), Message> {
    match msg {
        Message::Hello(client_hello) => {
            if client_hello.protocol_version != PROTOCOL_VERSION {
                return Err(Message::Error {
                    code: "protocol_mismatch".into(),
                    message: format!(
                        "client {} != server {}",
                        client_hello.protocol_version, PROTOCOL_VERSION
                    ),
                });
            }
            Ok(())
        }
        Message::Auth { token } => {
            let ok = match &state.token {
                Some(expected) => expected == &token,
                None => true,
            };
            if ok {
                *authed = true;
                send_msg(sink, &Message::AuthOk)
                    .await
                    .map_err(|_| Message::Error {
                        code: "send_failed".into(),
                        message: "failed to send AuthOk".into(),
                    })?;
            } else {
                *authed = false;
                send_msg(
                    sink,
                    &Message::AuthError {
                        message: "invalid token".into(),
                    },
                )
                .await
                .ok();
            }
            Ok(())
        }
        Message::Ping { nonce } => {
            send_msg(sink, &Message::Pong { nonce })
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send Pong".into(),
                })?;
            Ok(())
        }
        Message::PtyOpen {
            cols,
            rows,
            cwd,
            shell,
        } => {
            require_auth(*authed)?;
            let id = Uuid::new_v4().to_string();
            let (pty_out_tx, mut pty_out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
            let session = PtySession::spawn(id.clone(), cols, rows, cwd, shell, pty_out_tx)
                .map_err(|err| Message::Error {
                    code: "pty_open_failed".into(),
                    message: err.to_string(),
                })?;

            let id_for_task = id.clone();
            let out_tx2 = out_tx.clone();
            tokio::spawn(async move {
                while let Some(bytes) = pty_out_rx.recv().await {
                    let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
                    if out_tx2
                        .send(Message::PtyData {
                            id: id_for_task.clone(),
                            data,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                let _ = out_tx2.send(Message::PtyClosed {
                    id: id_for_task,
                    reason: Some("eof".into()),
                });
            });

            ptys.lock().await.insert(id.clone(), session);
            send_msg(sink, &Message::PtyOpened { id })
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send PtyOpened".into(),
                })?;
            Ok(())
        }
        Message::PtyData { id, data } => {
            require_auth(*authed)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .map_err(|err| Message::Error {
                    code: "bad_base64".into(),
                    message: err.to_string(),
                })?;
            let ptys = ptys.lock().await;
            let Some(session) = ptys.get(&id) else {
                return Err(Message::Error {
                    code: "unknown_pty".into(),
                    message: id,
                });
            };
            session.write_all(&bytes).map_err(|err| Message::Error {
                code: "pty_write_failed".into(),
                message: err.to_string(),
            })?;
            Ok(())
        }
        Message::PtyResize { id, cols, rows } => {
            require_auth(*authed)?;
            let ptys = ptys.lock().await;
            let Some(session) = ptys.get(&id) else {
                return Err(Message::Error {
                    code: "unknown_pty".into(),
                    message: id,
                });
            };
            session.resize(cols, rows).map_err(|err| Message::Error {
                code: "pty_resize_failed".into(),
                message: err.to_string(),
            })?;
            Ok(())
        }
        Message::PtyClose { id } => {
            require_auth(*authed)?;
            let mut ptys = ptys.lock().await;
            ptys.remove(&id);
            send_msg(
                sink,
                &Message::PtyClosed {
                    id,
                    reason: Some("client_close".into()),
                },
            )
            .await
            .ok();
            Ok(())
        }
        other => {
            warn!(?other, "unexpected client message");
            Ok(())
        }
    }
}

fn require_auth(authed: bool) -> Result<(), Message> {
    if authed {
        Ok(())
    } else {
        Err(Message::Error {
            code: "unauthorized".into(),
            message: "send auth first".into(),
        })
    }
}

async fn send_msg(
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    msg: &Message,
) -> Result<(), ()> {
    let json = msg.to_json().map_err(|_| ())?;
    sink.send(WsMessage::Text(json.into())).await.map_err(|_| ())
}

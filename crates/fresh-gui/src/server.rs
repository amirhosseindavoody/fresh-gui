//! WebSocket ADE server (JSON frames) with detachable sessions.

#![allow(clippy::result_large_err)] // ADE `Message` is the shared error envelope.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use base64::Engine;
use fresh_gui_protocol::{Hello, HelloUi, Message, CAP_EDITOR, CAP_SCENE, PROTOCOL_VERSION};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::Config;
use crate::editor_worker::EditorHandle;
use crate::fs::FsRoot;
use crate::fs_watch::FsWatchStore;
use crate::session::SessionStore;

pub struct AppState {
    pub token: Option<String>,
    pub require_auth: bool,
    pub fs_root: FsRoot,
    pub sessions: SessionStore,
    pub editor: Option<EditorHandle>,
    pub watches: FsWatchStore,
    /// Live config (reloaded when the settings file is saved).
    pub config: Arc<std::sync::RwLock<Config>>,
    /// Absolute path to `config.json`.
    pub config_path: PathBuf,
}

pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    state: Arc<AppState>,
    ui_dir: Option<std::path::PathBuf>,
    http_url: &str,
    ws_url: &str,
) -> Result<()> {
    let addr = listener.local_addr()?;
    let api = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    let app = if let Some(dir) = ui_dir {
        api.fallback_service(
            tower_http::services::ServeDir::new(dir).append_index_html_on_directories(true),
        )
    } else {
        api
    };

    info!(%addr, %http_url, %ws_url, "listening (http UI + ws path /ws)");
    // Keep a second clear line in case the startup banner scrolled off.
    eprintln!("listening on {http_url}  ({ws_url})");
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

    let mut caps = Hello::default_backend_caps();
    if state.editor.is_none() {
        caps.retain(|c| c != CAP_EDITOR && c != CAP_SCENE);
    }
    let ui = {
        let cfg = state.config.read().expect("config lock");
        HelloUi {
            theme: cfg.ui.theme.clone(),
            palette: cfg.ui.palette.clone(),
            terminal_font_size: cfg.ui.terminal_font_size,
            editor_font_size: cfg.ui.editor_font_size,
            font_weight: cfg.ui.font_weight,
            mono_font_weight: cfg.ui.mono_font_weight,
            font_family: cfg.ui.font_family.clone(),
            mono_font_family: cfg.ui.mono_font_family.clone(),
            webgl: cfg.ui.webgl,
            show_dotfiles: cfg.ui.show_dotfiles,
            show_git_dirs: cfg.ui.show_git_dirs,
        }
    };
    let mut hello = Hello::backend(
        format!("fresh-gui/{}", env!("CARGO_PKG_VERSION")),
        caps,
    );
    hello.config_path = Some(state.config_path.display().to_string());
    hello.ui = Some(ui);
    let hello = Message::Hello(hello);
    if send_msg(&mut sink, &hello).await.is_err() {
        return;
    }

    let mut authed = !state.require_auth;
    let mut session_id: Option<String> = None;
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
                    &mut session_id,
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

    if let Some(sid) = session_id {
        state.sessions.detach_subscriber(&sid).await;
    }
    info!("websocket client disconnected");
}

async fn handle_client_msg(
    msg: Message,
    state: &AppState,
    authed: &mut bool,
    session_id: &mut Option<String>,
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
                Some(expected) => tokens_equal(expected, &token),
                None => !state.require_auth,
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
        Message::SessionCreate { layout } => {
            require_auth(*authed)?;
            if let Some(prev) = session_id.take() {
                state.sessions.detach_subscriber(&prev).await;
            }
            let id = state.sessions.create(layout).await;
            state
                .sessions
                .attach(&id, out_tx)
                .await
                .map_err(|err| Message::Error {
                    code: "session_attach_failed".into(),
                    message: err.to_string(),
                })?;
            *session_id = Some(id.clone());
            send_msg(sink, &Message::SessionCreated { session_id: id })
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send SessionCreated".into(),
                })?;
            Ok(())
        }
        Message::SessionAttach {
            session_id: want_id,
        } => {
            require_auth(*authed)?;
            if let Some(prev) = session_id.take() {
                state.sessions.detach_subscriber(&prev).await;
            }
            let (ptys, layout, replay) = state
                .sessions
                .attach(&want_id, out_tx)
                .await
                .map_err(|err| Message::Error {
                    code: "session_attach_failed".into(),
                    message: err.to_string(),
                })?;
            *session_id = Some(want_id.clone());
            send_msg(
                sink,
                &Message::SessionAttached {
                    session_id: want_id,
                    ptys,
                    layout,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send SessionAttached".into(),
            })?;
            // Replay scrollback after the attach ack so clients can wire terminals first.
            for msg in replay {
                send_msg(sink, &msg).await.map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to replay scrollback".into(),
                })?;
            }
            Ok(())
        }
        Message::SessionList => {
            require_auth(*authed)?;
            let sessions = state.sessions.list().await;
            send_msg(sink, &Message::SessionListed { sessions })
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send SessionListed".into(),
                })?;
            Ok(())
        }
        Message::LayoutSet { layout } => {
            require_auth(*authed)?;
            let sid = require_session(session_id)?;
            state
                .sessions
                .set_layout(&sid, layout)
                .await
                .map_err(|err| Message::Error {
                    code: "layout_set_failed".into(),
                    message: err.to_string(),
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
            let sid = match session_id.as_ref() {
                Some(s) => s.clone(),
                None => {
                    // Compat: auto-create a session on first PTY open.
                    let id = state.sessions.create(None).await;
                    state
                        .sessions
                        .attach(&id, out_tx.clone())
                        .await
                        .map_err(|err| Message::Error {
                            code: "session_attach_failed".into(),
                            message: err.to_string(),
                        })?;
                    send_msg(
                        sink,
                        &Message::SessionCreated {
                            session_id: id.clone(),
                        },
                    )
                    .await
                    .ok();
                    *session_id = Some(id.clone());
                    id
                }
            };

            let cfg = state.config.read().expect("config lock").clone();
            let id = state
                .sessions
                .open_pty(&sid, cols, rows, cwd, shell, &cfg)
                .await
                .map_err(|err| Message::Error {
                    code: "pty_open_failed".into(),
                    message: err.to_string(),
                })?;

            send_msg(
                sink,
                &Message::PtyOpened {
                    id,
                    cols,
                    rows,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send PtyOpened".into(),
            })?;
            Ok(())
        }
        Message::PtyData { id, data } => {
            require_auth(*authed)?;
            let sid = require_session(session_id)?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&data)
                .map_err(|err| Message::Error {
                    code: "bad_base64".into(),
                    message: err.to_string(),
                })?;
            state
                .sessions
                .write_pty(&sid, &id, &bytes)
                .await
                .map_err(|err| Message::Error {
                    code: "pty_write_failed".into(),
                    message: err.to_string(),
                })?;
            Ok(())
        }
        Message::PtyResize { id, cols, rows } => {
            require_auth(*authed)?;
            let sid = require_session(session_id)?;
            state
                .sessions
                .resize_pty(&sid, &id, cols, rows)
                .await
                .map_err(|err| Message::Error {
                    code: "pty_resize_failed".into(),
                    message: err.to_string(),
                })?;
            Ok(())
        }
        Message::PtyClose { id } => {
            require_auth(*authed)?;
            let sid = require_session(session_id)?;
            state
                .sessions
                .close_pty(&sid, &id)
                .await
                .map_err(|err| Message::Error {
                    code: "pty_close_failed".into(),
                    message: err.to_string(),
                })?;
            Ok(())
        }
        Message::FsList { request_id, path } => {
            require_auth(*authed)?;
            match state.fs_root.list(&path).await {
                Ok((resolved, entries)) => {
                    send_msg(
                        sink,
                        &Message::FsListed {
                            request_id,
                            path: resolved,
                            entries,
                        },
                    )
                    .await
                    .map_err(|_| Message::Error {
                        code: "send_failed".into(),
                        message: "failed to send FsListed".into(),
                    })?;
                    Ok(())
                }
                Err(err) => Err(Message::Error {
                    code: "fs_list_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                }),
            }
        }
        Message::FsAuthorize { request_id, path } => {
            require_auth(*authed)?;
            match state.fs_root.authorize(&path).await {
                Ok(resolved) => {
                    send_msg(
                        sink,
                        &Message::FsAuthorized {
                            request_id,
                            path: resolved.display().to_string(),
                        },
                    )
                    .await
                    .map_err(|_| Message::Error {
                        code: "send_failed".into(),
                        message: "failed to send FsAuthorized".into(),
                    })?;
                    Ok(())
                }
                Err(err) => Err(Message::Error {
                    code: "fs_authorize_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                }),
            }
        }
        Message::FsStat { request_id, path } => {
            require_auth(*authed)?;
            match state.fs_root.stat(&path).await {
                Ok(entry) => {
                    send_msg(
                        sink,
                        &Message::FsStatResult { request_id, entry },
                    )
                    .await
                    .map_err(|_| Message::Error {
                        code: "send_failed".into(),
                        message: "failed to send FsStatResult".into(),
                    })?;
                    Ok(())
                }
                Err(err) => Err(Message::Error {
                    code: "fs_stat_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                }),
            }
        }
        Message::EditorOpen {
            request_id,
            path,
            preview,
            cwd,
            line,
            column,
        } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let resolved = resolve_editor_open(state, &path, cwd.as_deref(), line, column)
                .await
                .map_err(|err| Message::Error {
                    code: "editor_open_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            reply_editor_opened(sink, editor, request_id, resolved.path, preview, resolved.line, resolved.column)
                .await
        }
        Message::EditorOpenLink {
            request_id,
            line_text,
            column,
            preview,
            cwd,
        } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let resolved = crate::path_open::resolve_link_open(
                &state.fs_root,
                &line_text,
                column,
                cwd.as_deref(),
            )
            .await
            .map_err(|err| Message::Error {
                code: "editor_open_failed".into(),
                message: format!("{request_id}: {err:#}"),
            })?;
            // Settings config.json is still openable by explicit path; link
            // opens stay inside the FS sandbox / authorized cwds.
            reply_editor_opened(
                sink,
                editor,
                request_id,
                resolved.path,
                preview,
                resolved.line,
                resolved.column,
            )
            .await
        }
        Message::EditorClose { buffer_id } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: "editor capability not available".into(),
                });
            };
            editor.close(buffer_id).await.map_err(|err| Message::Error {
                code: "editor_close_failed".into(),
                message: err.to_string(),
            })?;
            Ok(())
        }
        Message::BufferEdit {
            request_id,
            buffer_id,
            base_rev,
            text,
        } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let rev = editor
                .edit(buffer_id.clone(), base_rev, text)
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_edit_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::BufferChanged {
                    request_id,
                    buffer_id,
                    rev,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferChanged".into(),
            })?;
            Ok(())
        }
        Message::BufferSave {
            request_id,
            buffer_id,
            base_rev,
        } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let (path, rev) = editor
                .save(buffer_id.clone(), base_rev)
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_save_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            if Config::path_matches(&state.config_path, &path) {
                match Config::load_from_path(&state.config_path) {
                    Ok(cfg) => {
                        info!(
                            path = %state.config_path.display(),
                            shell = %cfg.resolve_shell().0,
                            theme = %cfg.ui.theme,
                            "reloaded config after save"
                        );
                        *state.config.write().expect("config lock") = cfg;
                    }
                    Err(err) => {
                        warn!(
                            path = %state.config_path.display(),
                            %err,
                            "config save on disk but reload failed"
                        );
                    }
                }
            }
            send_msg(
                sink,
                &Message::BufferSaved {
                    request_id,
                    buffer_id,
                    path,
                    rev,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferSaved".into(),
            })?;
            Ok(())
        }
        Message::FsWatch {
            request_id,
            path,
            recursive,
        } => {
            require_auth(*authed)?;
            let resolved = state.fs_root.resolve(&path).await.map_err(|err| Message::Error {
                code: "fs_watch_failed".into(),
                message: format!("{request_id}: {err:#}"),
            })?;
            // Install watches off the WebSocket task. A recursive root walk can
            // take seconds on large trees; awaiting it here would stall PTY
            // output on the same connection (felt as slow shell init / lag).
            let watches = state.watches.clone();
            let fs_root = state.fs_root.clone();
            let out = out_tx.clone();
            tokio::task::spawn_blocking(move || {
                match watches.watch(&fs_root, resolved, recursive, out.clone()) {
                    Ok((watch_id, display)) => {
                        let _ = out.send(Message::FsWatchStarted {
                            request_id,
                            watch_id,
                            path: display,
                        });
                    }
                    Err(err) => {
                        let _ = out.send(Message::Error {
                            code: "fs_watch_failed".into(),
                            message: format!("{request_id}: {err:#}"),
                        });
                    }
                }
            });
            Ok(())
        }
        Message::FsUnwatch { watch_id } => {
            require_auth(*authed)?;
            if !state.watches.unwatch(&watch_id) {
                return Err(Message::Error {
                    code: "fs_unwatch_failed".into(),
                    message: format!("unknown watch_id {watch_id}"),
                });
            }
            Ok(())
        }
        Message::SceneGet { request_id } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "scene_unavailable".into(),
                    message: format!("{request_id}: scene capability not available"),
                });
            };
            let scene = editor.scene().await.map_err(|err| Message::Error {
                code: "scene_get_failed".into(),
                message: format!("{request_id}: {err:#}"),
            })?;
            send_msg(
                sink,
                &Message::SceneSnapshot {
                    request_id,
                    buffers: scene.buffers,
                    active_buffer_id: scene.active_buffer_id,
                    extra: None,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send SceneSnapshot".into(),
            })?;
            Ok(())
        }
        other => {
            warn!(?other, "unexpected client message");
            Ok(())
        }
    }
}

/// Resolve an editor open: settings `config.json` is always allowed (and
/// created on first open); everything else uses Fresh path/`cwd` resolution
/// inside the FS sandbox (+ authorized terminal cwds).
async fn resolve_editor_open(
    state: &AppState,
    path: &str,
    cwd: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
) -> anyhow::Result<crate::path_open::ResolvedOpen> {
    let (path_part, parsed_line, parsed_col) =
        fresh::input::quick_open::parse_path_line_col(path);
    let line = line.or(parsed_line.map(|n| n as u32));
    let column = column.or(parsed_col.map(|n| n as u32));

    if Config::path_matches(&state.config_path, &path_part)
        || path_part == state.config_path.display().to_string()
        || std::path::Path::new(&path_part) == state.config_path.as_path()
    {
        Config::ensure_file(&state.config_path)?;
        return Ok(crate::path_open::ResolvedOpen {
            path: state
                .config_path
                .canonicalize()
                .unwrap_or_else(|_| state.config_path.clone()),
            line,
            column,
        });
    }

    crate::path_open::resolve_path_open(&state.fs_root, path, cwd, line, column).await
}

async fn reply_editor_opened(
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    editor: &EditorHandle,
    request_id: String,
    path: std::path::PathBuf,
    preview: bool,
    line: Option<u32>,
    column: Option<u32>,
) -> Result<(), Message> {
    let opened = editor.open(path, preview).await.map_err(|err| Message::Error {
        code: "editor_open_failed".into(),
        message: format!("{request_id}: {err:#}"),
    })?;
    send_msg(
        sink,
        &Message::EditorOpened {
            request_id,
            buffer_id: opened.buffer_id.clone(),
            path: opened.path.clone(),
            language: opened.language,
            line,
            column,
        },
    )
    .await
    .map_err(|_| Message::Error {
        code: "send_failed".into(),
        message: "failed to send EditorOpened".into(),
    })?;
    send_msg(
        sink,
        &Message::BufferSnapshot {
            buffer_id: opened.buffer_id,
            rev: opened.rev,
            text: opened.text,
            path: opened.path,
        },
    )
    .await
    .map_err(|_| Message::Error {
        code: "send_failed".into(),
        message: "failed to send BufferSnapshot".into(),
    })?;
    Ok(())
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

/// Best-effort constant-time compare (still short-circuits on length mismatch).
fn tokens_equal(expected: &str, presented: &str) -> bool {
    if expected.len() != presented.len() {
        return false;
    }
    let mut diff = 0u8;
    for (a, b) in expected.bytes().zip(presented.bytes()) {
        diff |= a ^ b;
    }
    diff == 0
}

fn require_session(session_id: &Option<String>) -> Result<String, Message> {
    session_id.clone().ok_or_else(|| Message::Error {
        code: "no_session".into(),
        message: "create or attach a session first".into(),
    })
}

async fn send_msg(
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    msg: &Message,
) -> Result<(), ()> {
    let json = msg.to_json().map_err(|_| ())?;
    sink.send(WsMessage::Text(json.into())).await.map_err(|_| ())
}

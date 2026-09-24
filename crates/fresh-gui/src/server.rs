//! WebSocket ADE server (JSON frames) with detachable sessions.

#![allow(clippy::result_large_err)] // ADE `Message` is the shared error envelope.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine;
use fresh_gui_protocol::{CAP_EDITOR, CAP_SCENE, Hello, HelloUi, Message, PROTOCOL_VERSION};
use futures_util::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::editor_worker::EditorHandle;
use crate::fs::FsRoot;
use crate::fs_watch::FsWatchStore;
use crate::memory_monitor::MemoryMonitor;
use crate::session::SessionStore;
use crate::workspace::WorkspaceStore;

pub struct AppState {
    pub token: Option<String>,
    pub require_auth: bool,
    pub fs_root: FsRoot,
    pub sessions: SessionStore,
    pub workspaces: WorkspaceStore,
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
    http_url: &str,
    ws_url: &str,
    memory: Option<Arc<MemoryMonitor>>,
) -> Result<()> {
    let addr = listener.local_addr()?;
    let app = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .route("/ws", get(ws_upgrade))
        .with_state(state);

    info!(%addr, %http_url, %ws_url, "listening (ws /ws)");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal(memory.clone()))
        .await?;
    // Once-only fallback when the server stops without a signal (tests / errors).
    if let Some(monitor) = memory {
        monitor.finish();
    }
    Ok(())
}

async fn shutdown_signal(memory: Option<Arc<MemoryMonitor>>) {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    info!("shutdown signal received");
    // Log memory summary before graceful drain so `fresh-gui close` still captures
    // it if connections stall past the SIGKILL deadline.
    if let Some(monitor) = memory {
        monitor.finish();
    }
}

async fn ws_upgrade(ws: WebSocketUpgrade, State(state): State<Arc<AppState>>) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>) {
    let (mut sink, mut stream) = socket.split();
    let defaults_path = std::env::temp_dir().join(format!(
        "fresh-gui-defaults-{}.jsonc",
        uuid::Uuid::new_v4()
    ));

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
            editor_minimap: cfg.ui.editor_minimap,
            editor_line_wrap: cfg.ui.editor_line_wrap,
        }
    };
    let mut hello = Hello::backend(format!("fresh-gui/{}", env!("CARGO_PKG_VERSION")), caps);
    hello.config_path = Some(state.config_path.display().to_string());
    hello.defaults_path = Some(defaults_path.display().to_string());
    hello.ui = Some(ui);
    hello.shortkeys = state.config.read().expect("config lock").shortkeys.iter().map(|key| fresh_gui_protocol::Shortkey {
        action: key.action.clone(), shortkey: key.shortkey.clone(), when: key.when.clone(),
    }).collect();
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
                    &defaults_path,
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
    let _ = std::fs::remove_file(&defaults_path);
    info!("websocket client disconnected");
}

async fn handle_client_msg(
    msg: Message,
    state: &AppState,
    defaults_path: &Path,
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
            let (ptys, layout, replay) =
                state
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
            let workspace_root = state.workspaces.root_for_session(&sid).await;
            let cwd = crate::workspace::shell_working_directory(
                cwd.as_deref(),
                workspace_root.as_deref(),
                &state.fs_root.root_display(),
            );
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
                    id: id.clone(),
                    cols,
                    rows,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send PtyOpened".into(),
            })?;
            mirror_workspace_layout(state, state.workspaces.note_terminal(&sid, &id).await).await;
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
            mirror_workspace_layout(
                state,
                state.workspaces.note_terminal_closed(&sid, &id).await,
            )
            .await;
            Ok(())
        }
        Message::FsList { request_id, path } => {
            require_auth(*authed)?;
            match state.fs_root.list(&path).await {
                Ok((resolved, entries)) => {
                    let (show_dotfiles, show_git_dirs) = {
                        let config = state.config.read().expect("config lock");
                        (config.ui.show_dotfiles, config.ui.show_git_dirs)
                    };
                    let entries =
                        crate::fs::visible_entries(entries, show_dotfiles, show_git_dirs);
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
                    send_msg(sink, &Message::FsStatResult { request_id, entry })
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
        Message::FsCreate {
            request_id,
            parent,
            name,
            kind,
        } => {
            require_auth(*authed)?;
            match state.fs_root.create(&parent, &name, kind).await {
                Ok(entry) => {
                    send_msg(sink, &Message::FsCreated { request_id, entry })
                        .await
                        .map_err(|_| Message::Error {
                            code: "send_failed".into(),
                            message: "failed to send FsCreated".into(),
                        })?;
                    Ok(())
                }
                Err(err) => Err(Message::Error {
                    code: "fs_create_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                }),
            }
        }
        Message::FsCopy {
            request_id,
            sources,
            destination,
        } => {
            require_auth(*authed)?;
            match state.fs_root.copy_into(&sources, &destination).await {
                Ok(entries) => {
                    send_msg(
                        sink,
                        &Message::FsCopied {
                            request_id,
                            entries,
                        },
                    )
                    .await
                    .map_err(|_| Message::Error {
                        code: "send_failed".into(),
                        message: "failed to send FsCopied".into(),
                    })?;
                    Ok(())
                }
                Err(err) => Err(Message::Error {
                    code: "fs_copy_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                }),
            }
        }
        Message::FsMove {
            request_id,
            sources,
            destination,
        } => {
            require_auth(*authed)?;
            match state.fs_root.move_into(&sources, &destination).await {
                Ok(entries) => {
                    send_msg(
                        sink,
                        &Message::FsMoved {
                            request_id,
                            entries,
                        },
                    )
                    .await
                    .map_err(|_| Message::Error {
                        code: "send_failed".into(),
                        message: "failed to send FsMoved".into(),
                    })?;
                    Ok(())
                }
                Err(err) => Err(Message::Error {
                    code: "fs_move_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                }),
            }
        }
        Message::FsRename {
            request_id,
            path,
            name,
        } => {
            require_auth(*authed)?;
            match state.fs_root.rename(&path, &name).await {
                Ok(entry) => {
                    send_msg(sink, &Message::FsRenamed { request_id, entry })
                        .await
                        .map_err(|_| Message::Error {
                            code: "send_failed".into(),
                            message: "failed to send FsRenamed".into(),
                        })?;
                    Ok(())
                }
                Err(err) => Err(Message::Error {
                    code: "fs_rename_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                }),
            }
        }
        Message::FsDelete { request_id, paths } => {
            require_auth(*authed)?;
            if paths.len() == 1 && Path::new(&paths[0]) == defaults_path {
                match std::fs::remove_file(defaults_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => return Err(Message::Error {
                        code: "fs_delete_failed".into(),
                        message: format!("{request_id}: {err}"),
                    }),
                }
                send_msg(sink, &Message::FsDeleted { request_id, paths })
                    .await
                    .map_err(|_| Message::Error {
                        code: "send_failed".into(),
                        message: "failed to send FsDeleted".into(),
                    })?;
                return Ok(());
            }
            match state.fs_root.delete_paths(&paths).await {
                Ok(paths) => {
                    send_msg(sink, &Message::FsDeleted { request_id, paths })
                        .await
                        .map_err(|_| Message::Error {
                            code: "send_failed".into(),
                            message: "failed to send FsDeleted".into(),
                        })?;
                    Ok(())
                }
                Err(err) => Err(Message::Error {
                    code: "fs_delete_failed".into(),
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
            let resolved =
                resolve_editor_open(state, defaults_path, &path, cwd.as_deref(), line, column)
                    .await
                    .map_err(|err| Message::Error {
                        code: "editor_open_failed".into(),
                        message: format!("{request_id}: {err:#}"),
                    })?;
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
        Message::EditorNew { request_id } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let opened = editor.new_buffer().await.map_err(|err| Message::Error {
                code: "editor_new_failed".into(),
                message: format!("{request_id}: {err:#}"),
            })?;
            send_msg(
                sink,
                &Message::EditorOpened {
                    request_id,
                    buffer_id: opened.buffer_id.clone(),
                    path: opened.path.clone(),
                    language: opened.language,
                    line: None,
                    column: None,
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
        Message::BufferFormat {
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
            let (text, rev) = editor
                .format(buffer_id.clone(), base_rev)
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_format_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::BufferFormatted {
                    request_id,
                    buffer_id,
                    rev,
                    text,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferFormatted".into(),
            })?;
            Ok(())
        }
        Message::EditorClose { buffer_id } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: "editor capability not available".into(),
                });
            };
            editor
                .close(buffer_id)
                .await
                .map_err(|err| Message::Error {
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
            path,
        } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let dest = if path.is_empty() {
                None
            } else {
                Some(state.fs_root.resolve_new_file(&path).await.map_err(|err| {
                    Message::Error {
                        code: "buffer_save_failed".into(),
                        message: format!("{request_id}: {err:#}"),
                    }
                })?)
            };
            let (path, rev) = editor
                .save(buffer_id.clone(), base_rev, dest)
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
                        let shortkeys = cfg.shortkeys.iter().map(|key| fresh_gui_protocol::Shortkey {
                            action: key.action.clone(), shortkey: key.shortkey.clone(), when: key.when.clone(),
                        }).collect();
                        *state.config.write().expect("config lock") = cfg;
                        send_msg(sink, &Message::ConfigUpdated { shortkeys }).await.map_err(|_| Message::Error {
                            code: "send_failed".into(), message: "failed to send ConfigUpdated".into(),
                        })?;
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
            let resolved = state
                .fs_root
                .resolve(&path)
                .await
                .map_err(|err| Message::Error {
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
        Message::GitStatus {
            request_id,
            workspace_id,
            directory,
        } => {
            require_auth(*authed)?;
            git_status(state, sink, request_id, workspace_id, directory).await
        }
        Message::GitDiff {
            request_id,
            workspace_id,
            directory,
            path,
        } => {
            require_auth(*authed)?;
            git_diff(state, sink, request_id, workspace_id, directory, path).await
        }
        Message::GitStage {
            request_id,
            workspace_id,
            directory,
            paths,
            stage,
        } => {
            require_auth(*authed)?;
            git_op(state, sink, request_id, workspace_id, directory, move |dir| {
                crate::git::stage(&dir, &paths, stage)
            })
            .await
        }
        Message::GitCommit {
            request_id,
            workspace_id,
            directory,
            message,
        } => {
            require_auth(*authed)?;
            git_op(state, sink, request_id, workspace_id, directory, move |dir| {
                crate::git::commit(&dir, &message)
            })
            .await
        }
        Message::GitPull {
            request_id,
            workspace_id,
            directory,
        } => {
            require_auth(*authed)?;
            git_op(state, sink, request_id, workspace_id, directory, |dir| {
                crate::git::pull(&dir)
            })
            .await
        }
        Message::GitPush {
            request_id,
            workspace_id,
            directory,
        } => {
            require_auth(*authed)?;
            git_op(state, sink, request_id, workspace_id, directory, |dir| {
                crate::git::push(&dir)
            })
            .await
        }
        Message::FsOpenExternal { request_id, path } => {
            require_auth(*authed)?;
            let resolved = state
                .fs_root
                .resolve(&path)
                .await
                .map_err(|err| Message::Error {
                    code: "fs_open_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            let join_id = request_id.clone();
            let open_id = request_id.clone();
            tokio::task::spawn_blocking(move || crate::open_external::open_with_os(&resolved))
                .await
                .map_err(|err| Message::Error {
                    code: "fs_open_failed".into(),
                    message: format!("{join_id}: {err}"),
                })?
                .map_err(|err| Message::Error {
                    code: "fs_open_failed".into(),
                    message: format!("{open_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::FsOpened {
                    request_id,
                    message: format!("Opened {path}"),
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send FsOpened".into(),
            })?;
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
        Message::WorkspaceList => {
            require_auth(*authed)?;
            let (workspaces, focused_id) = workspace_list(state).await;
            send_msg(
                sink,
                &Message::WorkspaceListed {
                    workspaces,
                    focused_id,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send WorkspaceListed".into(),
            })?;
            Ok(())
        }
        Message::WorkspaceCreate { name, root } => {
            require_auth(*authed)?;
            let root = resolve_workspace_root(state, root, "workspace_create_failed").await?;
            let mut workspace = state.workspaces.create(&state.sessions, name, root).await;
            workspace.pty_count = state.sessions.pty_count(&workspace.session_id).await;
            debug!(id = %workspace.id, name = %workspace.name, "workspace created");
            send_msg(sink, &Message::WorkspaceCreated { workspace })
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send WorkspaceCreated".into(),
                })?;
            Ok(())
        }
        Message::WorkspaceRename { workspace_id, name } => {
            require_auth(*authed)?;
            let mut workspace = state
                .workspaces
                .rename(&workspace_id, &name)
                .await
                .map_err(|err| Message::Error {
                    code: "workspace_rename_failed".into(),
                    message: err.to_string(),
                })?;
            workspace.pty_count = state.sessions.pty_count(&workspace.session_id).await;
            send_msg(sink, &Message::WorkspaceRenamed { workspace })
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send WorkspaceRenamed".into(),
                })?;
            Ok(())
        }
        Message::WorkspaceSetRoot { workspace_id, root } => {
            require_auth(*authed)?;
            if root.trim().is_empty() {
                return Err(Message::Error {
                    code: "workspace_set_root_failed".into(),
                    message: "workspace root is empty".into(),
                });
            }
            let root =
                resolve_workspace_root(state, Some(root), "workspace_set_root_failed").await?;
            let mut workspace = state
                .workspaces
                .set_root(&workspace_id, root)
                .await
                .map_err(|err| Message::Error {
                    code: "workspace_set_root_failed".into(),
                    message: err.to_string(),
                })?;
            workspace.pty_count = state.sessions.pty_count(&workspace.session_id).await;
            send_msg(sink, &Message::WorkspaceRootSet { workspace })
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send WorkspaceRootSet".into(),
                })?;
            Ok(())
        }
        Message::WorkspaceClose { workspace_id } => {
            require_auth(*authed)?;
            let closed =
                state
                    .workspaces
                    .close(&workspace_id)
                    .await
                    .map_err(|err| Message::Error {
                        code: "workspace_close_failed".into(),
                        message: err.to_string(),
                    })?;
            if session_id.as_deref() == Some(closed.session_id.as_str()) {
                *session_id = None;
            }
            state.sessions.detach_subscriber(&closed.session_id).await;
            state
                .sessions
                .destroy(&closed.session_id)
                .await
                .map_err(|err| Message::Error {
                    code: "workspace_close_failed".into(),
                    message: err.to_string(),
                })?;
            debug!(%workspace_id, "workspace closed");
            send_msg(
                sink,
                &Message::WorkspaceClosed {
                    workspace_id,
                    focused_id: closed.focused_id,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send WorkspaceClosed".into(),
            })?;
            Ok(())
        }
        Message::WorkspaceSwitch { workspace_id } => {
            require_auth(*authed)?;
            let focused =
                state
                    .workspaces
                    .focus(&workspace_id)
                    .await
                    .map_err(|err| Message::Error {
                        code: "workspace_switch_failed".into(),
                        message: err.to_string(),
                    })?;
            if let Some(prev) = session_id.take() {
                state.sessions.detach_subscriber(&prev).await;
            }
            let (ptys, _layout, replay) = state
                .sessions
                .attach(&focused.session_id, out_tx)
                .await
                .map_err(|err| Message::Error {
                    code: "workspace_switch_failed".into(),
                    message: err.to_string(),
                })?;
            *session_id = Some(focused.session_id.clone());
            let mut workspace = focused.info;
            workspace.pty_count = ptys.len() as u32;
            send_msg(
                sink,
                &Message::WorkspaceSwitched {
                    workspace,
                    tabs: focused.tabs,
                    active_tab: focused.active_tab,
                    ptys,
                    explorer_expanded: focused.explorer_expanded,
                    extra: focused.extra,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send WorkspaceSwitched".into(),
            })?;
            for msg in replay {
                send_msg(sink, &msg).await.map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to replay scrollback".into(),
                })?;
            }
            Ok(())
        }
        Message::WorkspaceLayoutSet {
            workspace_id,
            tabs,
            active_tab,
            explorer_expanded,
            extra,
        } => {
            require_auth(*authed)?;
            let (session_for_layout, layout) = state
                .workspaces
                .set_layout(&workspace_id, tabs, active_tab, explorer_expanded, extra)
                .await
                .map_err(|err| Message::Error {
                    code: "workspace_layout_failed".into(),
                    message: err.to_string(),
                })?;
            state
                .sessions
                .set_layout(&session_for_layout, layout)
                .await
                .map_err(|err| Message::Error {
                    code: "workspace_layout_failed".into(),
                    message: err.to_string(),
                })?;
            Ok(())
        }
        other => {
            warn!(?other, "unexpected client message");
            Ok(())
        }
    }
}

async fn workspace_list(
    state: &AppState,
) -> (Vec<fresh_gui_protocol::WorkspaceInfo>, Option<String>) {
    let (mut workspaces, focused) = state.workspaces.list().await;
    for workspace in &mut workspaces {
        workspace.pty_count = state.sessions.pty_count(&workspace.session_id).await;
    }
    (workspaces, focused)
}

async fn resolve_workspace_root(
    state: &AppState,
    root: Option<String>,
    code: &str,
) -> Result<String, Message> {
    let Some(raw) = root
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
    else {
        return Ok(state.fs_root.root_display());
    };
    validate_workspace_root(&raw).map_err(|message| Message::Error {
        code: code.into(),
        message,
    })?;
    let canon = state
        .fs_root
        .authorize(&raw)
        .await
        .map_err(|err| Message::Error {
            code: code.into(),
            message: err.to_string(),
        })?;
    Ok(canon.display().to_string())
}

fn validate_workspace_root(raw: &str) -> Result<(), String> {
    #[cfg(unix)]
    if raw.starts_with("\\\\")
        || raw.starts_with("//")
        || (raw.as_bytes().len() >= 2
            && raw.as_bytes()[0].is_ascii_alphabetic()
            && raw.as_bytes()[1] == b':')
    {
        return Err(
            "This is a Windows path; the remote daemon is Unix. Use an absolute Unix path like /home/user/project"
                .into(),
        );
    }
    if !std::path::Path::new(raw).is_absolute() {
        return Err(format!("workspace root must be an absolute path, got {raw}"));
    }
    Ok(())
}

#[cfg(test)]
mod workspace_root_tests {
    use super::validate_workspace_root;

    #[test]
    fn paths_follow_daemon_os() {
        #[cfg(unix)]
        {
            assert!(validate_workspace_root("/home/me/project").is_ok());
            assert!(validate_workspace_root("home/me/project")
                .unwrap_err()
                .contains("absolute"));
            assert!(validate_workspace_root(r"C:\work\project")
                .unwrap_err()
                .contains("Windows path"));
            assert!(validate_workspace_root("C:/work/project")
                .unwrap_err()
                .contains("Windows path"));
            assert!(validate_workspace_root(r"\\server\share")
                .unwrap_err()
                .contains("Windows path"));
        }
        #[cfg(windows)]
        {
            assert!(validate_workspace_root(r"C:\work\project").is_ok());
            assert!(validate_workspace_root("relative\\project").is_err());
        }
    }
}

async fn mirror_workspace_layout(state: &AppState, update: Option<(String, String)>) {
    let Some((session_id, layout)) = update else {
        return;
    };
    if let Err(err) = state.sessions.set_layout(&session_id, layout).await {
        debug!(%err, "workspace layout mirror failed");
    }
}

/// Resolve an editor open: settings `config.json` is always allowed (created
/// on first open, and hydrated with any missing default keys); everything else
/// uses Fresh path/`cwd` resolution inside the FS sandbox (+ authorized
/// terminal cwds).
async fn resolve_editor_open(
    state: &AppState,
    defaults_path: &Path,
    path: &str,
    cwd: Option<&str>,
    line: Option<u32>,
    column: Option<u32>,
) -> anyhow::Result<crate::path_open::ResolvedOpen> {
    let (path_part, parsed_line, parsed_col) = fresh::input::quick_open::parse_path_line_col(path);
    let line = line.or(parsed_line.map(|n| n as u32));
    let column = column.or(parsed_col.map(|n| n as u32));

    if Path::new(&path_part) == defaults_path {
        std::fs::write(defaults_path, crate::config::DEFAULT_CONFIG_TEMPLATE)?;
        return Ok(crate::path_open::ResolvedOpen {
            path: defaults_path.to_path_buf(),
            line,
            column,
        });
    }

    if Config::path_matches(&state.config_path, &path_part)
        || path_part == state.config_path.display().to_string()
        || std::path::Path::new(&path_part) == state.config_path.as_path()
    {
        // Create the file if needed, and fill in newly added default keys
        // without overriding the user's existing values.
        if Config::ensure_file(&state.config_path)? {
            match Config::load_from_path(&state.config_path) {
                Ok(cfg) => {
                    *state.config.write().expect("config lock") = cfg;
                }
                Err(err) => {
                    warn!(
                        path = %state.config_path.display(),
                        %err,
                        "hydrated config on disk but reload failed"
                    );
                }
            }
        }
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
    if crate::binary::is_binary_file(&path).unwrap_or(false) {
        return Err(Message::Error {
            code: "binary_file".into(),
            message: format!("{request_id}: {}", path.display()),
        });
    }
    let opened = editor.open(path, preview).await.map_err(|err| {
        let binary = err
            .chain()
            .any(|cause| cause.is::<crate::binary::BinaryFile>());
        Message::Error {
            code: if binary {
                "binary_file".into()
            } else {
                "editor_open_failed".into()
            },
            message: format!("{request_id}: {err:#}"),
        }
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

async fn workspace_dir(state: &AppState, workspace_id: &str) -> Result<PathBuf, Message> {
    let stored = if workspace_id.is_empty() {
        String::new()
    } else {
        state
            .workspaces
            .root_of(workspace_id)
            .await
            .ok_or_else(|| Message::Error {
                code: "git_failed".into(),
                message: format!("unknown workspace {workspace_id}"),
            })?
    };
    if stored.is_empty() {
        return Ok(state.fs_root.root_path().to_path_buf());
    }
    let path = PathBuf::from(&stored);
    if !path.is_dir() {
        return Err(Message::Error {
            code: "git_failed".into(),
            message: format!("{} is not a directory", path.display()),
        });
    }
    Ok(path)
}

async fn git_dir(state: &AppState, workspace_id: &str, directory: &str) -> Result<PathBuf, Message> {
    if directory.is_empty() {
        return workspace_dir(state, workspace_id).await;
    }
    state
        .fs_root
        .authorize(directory)
        .await
        .map_err(|err| Message::Error {
            code: "git_failed".into(),
            message: err.to_string(),
        })
}

fn git_err(request_id: &str, err: impl std::fmt::Display) -> Message {
    Message::Error {
        code: "git_failed".into(),
        message: format!("{request_id}: {err}"),
    }
}

async fn git_status(
    state: &AppState,
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    request_id: String,
    workspace_id: String,
    directory: String,
) -> Result<(), Message> {
    let dir = git_dir(state, &workspace_id, &directory).await?;
    let request = request_id.clone();
    let status = tokio::task::spawn_blocking(move || crate::git::status(&dir))
        .await
        .map_err(|err| git_err(&request, err))?
        .unwrap_or_else(|err| crate::git::Status {
            repo: false,
            root: String::new(),
            branch: String::new(),
            upstream: None,
            ahead: 0,
            behind: 0,
            files: Vec::new(),
            detail: Some(err.to_string()),
        });
    send_msg(
        sink,
        &Message::GitStatusResult {
            request_id,
            repo: status.repo,
            root: status.root,
            branch: status.branch,
            upstream: status.upstream,
            ahead: status.ahead,
            behind: status.behind,
            files: status.files,
            detail: status.detail,
        },
    )
    .await
    .map_err(|_| Message::Error {
        code: "send_failed".into(),
        message: "failed to send GitStatusResult".into(),
    })?;
    Ok(())
}

async fn git_diff(
    state: &AppState,
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    request_id: String,
    workspace_id: String,
    directory: String,
    path: String,
) -> Result<(), Message> {
    let dir = git_dir(state, &workspace_id, &directory).await?;
    let request = request_id.clone();
    let rel = path.clone();
    let sides = tokio::task::spawn_blocking(move || crate::git::diff(&dir, &rel))
        .await
        .map_err(|err| git_err(&request, err))?
        .map_err(|err| git_err(&request_id, err))?;
    send_msg(
        sink,
        &Message::GitDiffResult {
            request_id,
            path,
            old_text: sides.old_text,
            new_text: sides.new_text,
            binary: sides.binary,
            truncated: sides.truncated,
        },
    )
    .await
    .map_err(|_| Message::Error {
        code: "send_failed".into(),
        message: "failed to send GitDiffResult".into(),
    })?;
    Ok(())
}

async fn git_op<F>(
    state: &AppState,
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    request_id: String,
    workspace_id: String,
    directory: String,
    op: F,
) -> Result<(), Message>
where
    F: FnOnce(PathBuf) -> anyhow::Result<crate::git::Op> + Send + 'static,
{
    let dir = git_dir(state, &workspace_id, &directory).await?;
    let request = request_id.clone();
    let result = tokio::task::spawn_blocking(move || op(dir))
        .await
        .map_err(|err| git_err(&request, err))?
        .map_err(|err| git_err(&request_id, err))?;
    send_msg(
        sink,
        &Message::GitOpResult {
            request_id,
            ok: result.ok,
            output: result.output,
        },
    )
    .await
    .map_err(|_| Message::Error {
        code: "send_failed".into(),
        message: "failed to send GitOpResult".into(),
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
    sink.send(WsMessage::Text(json.into()))
        .await
        .map_err(|_| ())
}

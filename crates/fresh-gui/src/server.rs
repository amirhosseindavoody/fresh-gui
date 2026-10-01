//! WebSocket ADE server (JSON frames) with detachable sessions.

#![allow(clippy::result_large_err)] // ADE `Message` is the shared error envelope.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::ws::{Message as WsMessage, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use base64::Engine;
use fresh_gui_protocol::{
    ByteSelection, CAP_EDITOR, CAP_EDITOR_DRAFT_RECOVERY, CAP_EDITOR_EXTERNAL_CHANGES,
    CAP_EDITOR_PAGED_READS, CAP_EDITOR_RANGE_EDITS, CAP_EDITOR_SEARCH, CAP_PROJECT_SEARCH, CAP_LSP, CAP_LSP_NAVIGATION, CAP_LSP_REQUESTS,
    CAP_SCENE, CAP_SETTINGS_EDITOR, EditorDraftInfo, ExternalResolution, Hello, HelloUi,
    MAX_PAGE_BYTES, Message, PROTOCOL_VERSION,
};
use futures_util::{SinkExt, StreamExt};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::Config;
use crate::editor_worker::EditorHandle;
use crate::fs::FsRoot;
use crate::fs_watch::FsWatchStore;
use crate::memory_monitor::MemoryMonitor;
use crate::session::SessionStore;
use crate::workspace::WorkspaceStore;

const MAX_SNAPSHOT_BYTES: u64 = 2 * 1024 * 1024;
static NEXT_LSP_WIRE_ID: AtomicU64 = AtomicU64::new(1);

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

fn hello_ui(cfg: &Config) -> HelloUi {
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
        editor_line_wrap: cfg.editor.line_wrap.unwrap_or(cfg.ui.editor_line_wrap),
    }
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
    let defaults_path =
        std::env::temp_dir().join(format!("fresh-gui-defaults-{}.jsonc", uuid::Uuid::new_v4()));

    let mut caps = Hello::default_backend_caps();
    if state.editor.is_none() {
        caps.retain(|c| {
            c != CAP_EDITOR
                && c != CAP_EDITOR_RANGE_EDITS
                && c != CAP_EDITOR_PAGED_READS
                && c != CAP_EDITOR_DRAFT_RECOVERY
                && c != CAP_EDITOR_EXTERNAL_CHANGES
                && c != CAP_EDITOR_SEARCH
                && c != CAP_PROJECT_SEARCH
                && c != CAP_LSP
                && c != CAP_LSP_REQUESTS
                && c != CAP_LSP_NAVIGATION
                && c != CAP_SCENE
        });
    }
    let ui = hello_ui(&state.config.read().expect("config lock"));
    let mut hello = Hello::backend(format!("fresh-gui/{}", env!("CARGO_PKG_VERSION")), caps);
    hello.config_path = Some(state.config_path.display().to_string());
    hello.defaults_path = Some(defaults_path.display().to_string());
    hello.ui = Some(ui);
    hello.shortkeys = state
        .config
        .read()
        .expect("config lock")
        .shortkeys
        .iter()
        .map(|key| fresh_gui_protocol::Shortkey {
            action: key.action.clone(),
            shortkey: key.shortkey.clone(),
            when: key.when.clone(),
        })
        .collect();
    let hello = Message::Hello(hello);
    if send_msg(&mut sink, &hello).await.is_err() {
        return;
    }

    let mut authed = !state.require_auth;
    let mut client_range_edits = false;
    let mut client_paged_reads = false;
    let mut client_draft_recovery = false;
    let mut client_external_changes = false;
    let mut client_settings_editor = false;
    let mut client_lsp_requests = false;
    let mut client_editor_search = false;
    let mut client_lsp_navigation = false;
    let mut client_project_search = false;
    let project = crate::project_session::ProjectSession::default();
    let (project_tx, mut project_rx) = mpsc::channel::<Message>(8);
    let mut session_id: Option<String> = None;
    let socket_id = uuid::Uuid::new_v4().to_string();
    let mut lsp_request_map: HashMap<u64, (u64, String, String)> = HashMap::new();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<Message>();
    let mut external_rx = state.editor.as_ref().map(EditorHandle::subscribe_external);
    let mut lsp_rx = state.editor.as_ref().map(EditorHandle::subscribe_lsp);

    loop {
        tokio::select! {
            project_message = project_rx.recv() => {
                if let Some(message) = project_message {
                    let id = match &message { Message::ProjectSearchFile { request_id, .. } | Message::ProjectSearchDone { request_id, .. } => request_id, _ => continue };
                    if !project.is_current(id) { continue; }
                    if send_msg(&mut sink, &message).await.is_err() { break; }
                }
            }
            lsp = async { match lsp_rx.as_mut() { Some(rx) => rx.recv().await, None => std::future::pending().await } }, if authed && client_lsp_requests => {
                match lsp {
                    Ok(result) => if let Some((client_id, buffer_id, view_id)) = lsp_request_map.remove(&result.request_id) {
                        let mut result = result;
                        let owned = if let Some(editor) = state.editor.as_ref() {
                            ensure_editor_workspace(editor, &state, &session_id, &buffer_id, &client_id.to_string()).await.is_ok()
                        } else { false };
                        if !owned {
                            if let Some(editor) = state.editor.as_ref() { let _ = editor.cancel_lsp(result.request_id, buffer_id, format!("{socket_id}:{view_id}")); }
                            continue;
                        }
                        result.request_id = client_id;
                        result.buffer_id = buffer_id;
                        result.view_id = view_id;
                        if send_msg(&mut sink, &Message::BufferLspResult { result }).await.is_err() { break; }
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        cancel_socket_lsp(state.editor.as_ref(), &socket_id, &mut lsp_request_map);
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => lsp_rx = None,
                }
            }
            external = async {
                match external_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            }, if authed && client_external_changes => {
                match external {
                    Ok(change) => {
                        let owned_here = if let Some(editor) = state.editor.as_ref() {
                            ensure_editor_workspace(editor, &state, &session_id, &change.buffer_id, "external change").await.is_ok()
                        } else { false };
                        if owned_here {
                            let msg = external_changed_message(change);
                            if send_msg(&mut sink, &msg).await.is_err() { break; }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => external_rx = None,
                }
            }
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

                let previous_session = session_id.clone();
                if let Err(resp) = handle_client_msg(
                    msg,
                    &state,
                    &defaults_path,
                    &mut authed,
                    &mut client_range_edits,
                    &mut client_paged_reads,
                    &mut client_draft_recovery,
                    &mut client_external_changes,
                    &mut client_settings_editor,
                    &mut client_lsp_requests,
                    &mut client_lsp_navigation,
                    &mut lsp_request_map,
                    &socket_id,
                    &mut client_editor_search,
                    &mut client_project_search,
                    &project,
                    project_tx.clone(),
                    &mut session_id,
                    out_tx.clone(),
                    &mut sink,
                )
                .await
                {
                    let _ = send_msg(&mut sink, &resp).await;
                }
                if previous_session != session_id {
                    project.clear();
                    cancel_socket_lsp(state.editor.as_ref(), &socket_id, &mut lsp_request_map);
                }
            }
        }
    }

    if let Some(sid) = session_id {
        state.sessions.detach_subscriber(&sid).await;
    }
    cancel_socket_lsp(state.editor.as_ref(), &socket_id, &mut lsp_request_map);
    let _ = std::fs::remove_file(&defaults_path);
    info!("websocket client disconnected");
}

async fn handle_client_msg(
    msg: Message,
    state: &AppState,
    defaults_path: &Path,
    authed: &mut bool,
    client_range_edits: &mut bool,
    client_paged_reads: &mut bool,
    client_draft_recovery: &mut bool,
    client_external_changes: &mut bool,
    client_settings_editor: &mut bool,
    client_lsp_requests: &mut bool,
    client_lsp_navigation: &mut bool,
    lsp_request_map: &mut HashMap<u64, (u64, String, String)>,
    socket_id: &str,
    client_editor_search: &mut bool,
    client_project_search: &mut bool,
    project: &crate::project_session::ProjectSession,
    project_tx: mpsc::Sender<Message>,
    session_id: &mut Option<String>,
    out_tx: mpsc::UnboundedSender<Message>,
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
) -> Result<(), Message> {
    match msg {
        Message::Hello(client_hello) => {
            *client_range_edits = client_hello
                .capabilities
                .iter()
                .any(|cap| cap == CAP_EDITOR_RANGE_EDITS);
            *client_paged_reads = client_hello
                .capabilities
                .iter()
                .any(|cap| cap == CAP_EDITOR_PAGED_READS);
            *client_draft_recovery = client_hello
                .capabilities
                .iter()
                .any(|cap| cap == CAP_EDITOR_DRAFT_RECOVERY);
            *client_external_changes = client_hello
                .capabilities
                .iter()
                .any(|cap| cap == CAP_EDITOR_EXTERNAL_CHANGES);
            *client_settings_editor = client_hello
                .capabilities
                .iter()
                .any(|cap| cap == CAP_SETTINGS_EDITOR);
            *client_lsp_requests = client_hello.capabilities.iter().any(|cap| cap == CAP_LSP_REQUESTS);
            *client_editor_search = client_hello
                .capabilities
                .iter()
                .any(|cap| cap == CAP_EDITOR_SEARCH);
            *client_lsp_navigation = client_hello
                .capabilities
                .iter()
                .any(|cap| cap == CAP_LSP_NAVIGATION);
            *client_project_search = client_hello.capabilities.iter().any(|cap| cap == CAP_PROJECT_SEARCH);
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
        Message::ConfigReload => {
            require_auth(*authed)?;
            let cfg = Config::load_from_path(&state.config_path).map_err(|err| Message::Error {
                code: "config_reload_failed".into(),
                message: format!("{err:#}"),
            })?;
            if let Some(editor) = state.editor.as_ref() {
                editor
                    .reconfigure(cfg.clone())
                    .await
                    .map_err(|err| Message::Error {
                        code: "config_reload_failed".into(),
                        message: format!("failed to apply editor config: {err:#}"),
                    })?;
            }
            let shortkeys = cfg
                .shortkeys
                .iter()
                .map(|key| fresh_gui_protocol::Shortkey {
                    action: key.action.clone(),
                    shortkey: key.shortkey.clone(),
                    when: key.when.clone(),
                })
                .collect();
            let ui = hello_ui(&cfg);
            *state.config.write().expect("config lock") = cfg;
            send_msg(
                sink,
                &Message::ConfigUpdated {
                    shortkeys,
                    ui: Some(ui),
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send ConfigUpdated".into(),
            })?;
            Ok(())
        }
        Message::SettingsRead {
            request_id,
            workspace_id,
        } => {
            require_auth(*authed)?;
            require_settings_cap(*client_settings_editor, &request_id)?;
            let path = crate::settings::target_path(
                &state.config_path,
                &state.workspaces,
                workspace_id.as_deref(),
            )
            .await
            .map_err(|err| settings_error("settings_read_failed", &request_id, err))?;
            let mut snapshot =
                crate::settings::read_snapshot(&path, workspace_id.clone(), request_id.clone())
                    .map_err(|err| settings_error("settings_read_failed", &request_id, err))?;
            if let Message::SettingsSnapshot { defaults, .. } = &mut snapshot {
                *defaults =
                    crate::settings::layer_defaults(&state.config_path, workspace_id.is_some())
                        .map_err(|err| settings_error("settings_read_failed", &request_id, err))?;
            }
            send_msg(sink, &snapshot)
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send settings snapshot".into(),
                })?;
            Ok(())
        }
        Message::SettingsPatch {
            request_id,
            workspace_id,
            base_text,
            path: parts,
            value,
        } => {
            require_auth(*authed)?;
            require_settings_cap(*client_settings_editor, &request_id)?;
            let path = crate::settings::target_path(
                &state.config_path,
                &state.workspaces,
                workspace_id.as_deref(),
            )
            .await
            .map_err(|err| settings_error("settings_patch_failed", &request_id, err))?;
            let text = crate::settings::apply_patch(
                &path,
                &base_text,
                &parts,
                value,
                workspace_id.is_some(),
            )
            .map_err(|err| settings_error("settings_patch_failed", &request_id, err))?;
            if workspace_id.is_none() {
                let cfg = Config::load_from_path(&state.config_path)
                    .map_err(|err| settings_error("settings_patch_failed", &request_id, err))?;
                if matches!(parts.first().map(String::as_str), Some("lsp" | "languages"))
                    && let Some(editor) = state.editor.as_ref()
                {
                    editor
                        .reconfigure(cfg.clone())
                        .await
                        .map_err(|err| settings_error("settings_patch_failed", &request_id, err))?;
                }
                let shortkeys = cfg
                    .shortkeys
                    .iter()
                    .map(|key| fresh_gui_protocol::Shortkey {
                        action: key.action.clone(),
                        shortkey: key.shortkey.clone(),
                        when: key.when.clone(),
                    })
                    .collect();
                let ui = hello_ui(&cfg);
                *state.config.write().expect("config lock") = cfg;
                let _ = out_tx.send(Message::ConfigUpdated {
                    shortkeys,
                    ui: Some(ui),
                });
            }
            let defaults =
                crate::settings::layer_defaults(&state.config_path, workspace_id.is_some())
                    .map_err(|err| settings_error("settings_patch_failed", &request_id, err))?;
            let snapshot = Message::SettingsSnapshot {
                request_id,
                workspace_id,
                path: path.display().to_string(),
                text,
                defaults,
            };
            send_msg(sink, &snapshot)
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send settings snapshot".into(),
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
                    let entries = crate::fs::visible_entries(entries, show_dotfiles, show_git_dirs);
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
                    Err(err) => {
                        return Err(Message::Error {
                            code: "fs_delete_failed".into(),
                            message: format!("{request_id}: {err}"),
                        });
                    }
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
            if !*client_paged_reads && is_large_file(&resolved.path) {
                return Err(paged_reads_unavailable(&request_id));
            }
            let workspace_id = current_workspace_id(state, session_id).await?;
            reply_editor_opened(
                sink,
                editor,
                request_id,
                resolved.path,
                preview,
                resolved.line,
                resolved.column,
                workspace_id,
                *client_paged_reads,
            )
            .await
        }
        Message::EditorOpenLocation {
            request_id,
            uri,
            line,
            character,
        } => {
            require_auth(*authed)?;
            if !*client_lsp_navigation {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!("{request_id}: client did not negotiate {CAP_LSP_NAVIGATION}"),
                });
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let target_path = lsp_file_uri_to_path(&uri).map_err(|error| Message::Error {
                code: "editor_open_failed".into(),
                message: format!("{request_id}: {error}"),
            })?;
            let path = crate::path_open::resolve_exact_file(&state.fs_root, &target_path)
            .await
            .map_err(|error| Message::Error {
                code: "editor_open_failed".into(),
                message: format!("{request_id}: {error:#}"),
            })?;
            if !*client_paged_reads && is_large_file(&path) {
                return Err(paged_reads_unavailable(&request_id));
            }
            let workspace_id = current_workspace_id(state, session_id).await?;
            let (opened, offset) = editor
                .open_location_in_workspace(path, workspace_id, line, character)
                .await
                .map_err(|error| Message::Error {
                    code: "editor_open_failed".into(),
                    message: format!("{request_id}: {error:#}"),
                })?;
            if opened.total_bytes.is_some() && !*client_paged_reads {
                return Err(paged_reads_unavailable(&request_id));
            }
            let buffer_id = opened.buffer_id.clone();
            let path = opened.path.clone();
            send_editor_opened_snapshot(sink, request_id.clone(), opened, None, None).await?;
            send_msg(
                sink,
                &Message::EditorLocationOpened {
                    request_id,
                    buffer_id,
                    path,
                    offset,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send EditorLocationOpened".into(),
            })
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
            if !*client_paged_reads && is_large_file(&resolved.path) {
                return Err(paged_reads_unavailable(&request_id));
            }
            // Settings config.json is still openable by explicit path; link
            // opens stay inside the FS sandbox / authorized cwds.
            let workspace_id = current_workspace_id(state, session_id).await?;
            reply_editor_opened(
                sink,
                editor,
                request_id,
                resolved.path,
                preview,
                resolved.line,
                resolved.column,
                workspace_id,
                *client_paged_reads,
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
            let workspace_id = current_workspace_id(state, session_id).await?;
            let opened = editor
                .new_buffer_in_workspace(workspace_id)
                .await
                .map_err(|err| Message::Error {
                    code: "editor_new_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::EditorOpened {
                    request_id,
                    buffer_id: opened.buffer_id.clone(),
                    draft_id: Some(opened.draft_id.clone()),
                    path: opened.path.clone(),
                    language: opened.language.clone(),
                    line: None,
                    column: None,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send EditorOpened".into(),
            })?;
            send_msg(sink, &opened_content_message(opened))
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send BufferSnapshot".into(),
                })?;
            Ok(())
        }
        Message::EditorDraftList { request_id } => {
            require_auth(*authed)?;
            if !*client_draft_recovery {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!(
                        "{request_id}: client did not negotiate {CAP_EDITOR_DRAFT_RECOVERY}"
                    ),
                });
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let workspace_id = current_workspace_id(state, session_id).await?;
            let drafts = editor
                .draft_list(workspace_id)
                .await
                .map_err(|err| Message::Error {
                    code: "draft_list_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            let drafts = drafts
                .into_iter()
                .map(|draft| {
                    let source_changed = crate::drafts::DraftStore::source_changed(&draft);
                    EditorDraftInfo {
                        draft_id: draft.draft_id,
                        path: draft.path,
                        source_changed,
                    }
                })
                .collect();
            send_msg(sink, &Message::EditorDrafts { request_id, drafts })
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send EditorDrafts".into(),
                })?;
            Ok(())
        }
        Message::EditorDraftRestore {
            request_id,
            draft_id,
        } => {
            require_auth(*authed)?;
            if !*client_draft_recovery {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!(
                        "{request_id}: client did not negotiate {CAP_EDITOR_DRAFT_RECOVERY}"
                    ),
                });
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            let workspace_id = current_workspace_id(state, session_id).await?;
            if !*client_paged_reads {
                let drafts = editor
                    .draft_list(workspace_id.clone())
                    .await
                    .map_err(|err| Message::Error {
                        code: "draft_restore_failed".into(),
                        message: format!("{request_id}: {err:#}"),
                    })?;
                if drafts
                    .iter()
                    .any(|draft| draft.draft_id == draft_id && draft.paged.is_some())
                {
                    return Err(paged_reads_unavailable(&request_id));
                }
            }
            let (opened, source_changed) = editor
                .draft_restore(workspace_id, draft_id)
                .await
                .map_err(|err| Message::Error {
                    code: "draft_restore_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            let restored_buffer_id = opened.buffer_id.clone();
            if opened.total_bytes.is_some() && !*client_paged_reads {
                return Err(paged_reads_unavailable(&request_id));
            }
            send_editor_opened_snapshot(sink, request_id, opened, None, None).await?;
            if source_changed {
                send_msg(sink, &Message::EditorDraftWarning { buffer_id: restored_buffer_id, message: "The source file changed or is missing; review this recovered draft before saving.".into() }).await.map_err(|_| Message::Error { code: "send_failed".into(), message: "failed to send EditorDraftWarning".into() })?;
            }
            Ok(())
        }
        Message::EditorDraftDiscard {
            request_id,
            buffer_id,
        } => {
            require_auth(*authed)?;
            if !*client_draft_recovery {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!(
                        "{request_id}: client did not negotiate {CAP_EDITOR_DRAFT_RECOVERY}"
                    ),
                });
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            editor
                .draft_discard(buffer_id.clone())
                .await
                .map_err(|err| Message::Error {
                    code: "draft_discard_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::EditorDraftDiscarded {
                    request_id,
                    buffer_id,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send EditorDraftDiscarded".into(),
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
            let workspace_id = current_workspace_id(state, session_id).await?;
            editor
                .close_in_workspace(buffer_id, workspace_id)
                .await
                .map_err(|err| Message::Error {
                    code: "editor_close_failed".into(),
                    message: err.to_string(),
                })?;
            Ok(())
        }
        Message::ProjectSearch { request_id, search } => {
            require_auth(*authed)?;
            require_project_cap(*client_project_search, state.editor.is_some(), &request_id)?;
            let workspace = current_workspace_id(state, session_id).await?;
            let root = project_root(state, &workspace).await?;
            project.start(
                request_id,
                workspace,
                root,
                search,
                state.editor.as_ref().expect("checked").clone(),
                project_tx,
            );
            Ok(())
        }
        Message::ProjectSearchCancel { request_id } => {
            require_auth(*authed)?;
            require_project_cap(*client_project_search, state.editor.is_some(), &request_id)?;
            project.cancel(&request_id);
            Ok(())
        }
        Message::ProjectReplace {
            request_id,
            search_id,
            selections,
        } => {
            require_auth(*authed)?;
            require_project_cap(*client_project_search, state.editor.is_some(), &request_id)?;
            let workspace = current_workspace_id(state, session_id).await?;
            let root = project_root(state, &workspace).await?;
            let replaced = project
                .replace(
                    &search_id,
                    &workspace,
                    &root,
                    selections,
                    state.editor.as_ref().expect("checked"),
                )
                .await;
            let mut files = Vec::new();
            match replaced {
                Ok(results) => {
                    for (mut file, applied) in results {
                        if let Some(applied) = applied.filter(|result| !result.saved) {
                            file.buffer = Some(fresh_gui_protocol::ProjectBufferUpdate {
                                buffer_id: applied.buffer_id,
                                base_rev: applied.base_rev,
                                rev: applied.rev,
                                text: applied.text,
                                path: applied.path,
                                dirty: applied.dirty,
                            });
                        }
                        files.push(file);
                    }
                }
                Err(error) => files.push(fresh_gui_protocol::ProjectReplaceFileResult {
                    file_id: search_id,
                    error: Some(error),
                    buffer: None,
                }),
            }
            send_msg(
                sink,
                &Message::ProjectReplaceResult {
                    request_id: request_id.clone(),
                    files,
                },
            )
            .await
            .map_err(|_| settings_error("send_failed", &request_id, "socket closed"))?;
            Ok(())
        }

        Message::BufferSearch {
            request_id,
            text,
            query,
            replacement,
            options,
            scope,
        } => {
            require_auth(*authed)?;
            if !*client_editor_search {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!("{request_id}: client did not negotiate {CAP_EDITOR_SEARCH}"),
                });
            }
            if state.editor.is_none() {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!("{request_id}: server did not advertise {CAP_EDITOR_SEARCH}"),
                });
            }
            let preview = tokio::task::spawn_blocking(move || {
                crate::search::preview(&text, &query, &replacement, &options, scope)
            })
            .await;
            let (matches, capped, error) = match preview {
                Ok(Ok((matches, capped))) => (matches, capped, None),
                Ok(Err(error)) => (Vec::new(), false, Some(error)),
                Err(error) => (Vec::new(), false, Some(format!("search preview failed: {error}"))),
            };
            send_msg(
                sink,
                &Message::BufferSearchResult {
                    request_id,
                    matches,
                    error,
                    capped,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferSearchResult".into(),
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
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
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
        Message::BufferRangeEdit {
            request_id,
            buffer_id,
            view_id,
            base_rev,
            edits,
            viewport,
            selection,
        } => {
            require_auth(*authed)?;
            if !*client_range_edits {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!(
                        "{request_id}: client did not negotiate {CAP_EDITOR_RANGE_EDITS}"
                    ),
                });
            }
            if viewport.is_some() && !*client_paged_reads {
                return Err(paged_reads_unavailable(&request_id));
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            let result = editor
                .range_edit(
                    buffer_id.clone(),
                    view_id.clone(),
                    base_rev,
                    edits,
                    viewport,
                    selection,
                )
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_edit_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            let response = if let Some(page) = result.page {
                Message::BufferPage {
                    request_id,
                    buffer_id,
                    view_id,
                    rev: page.rev,
                    start: page.start,
                    total_bytes: page.total_bytes,
                    text: page.text,
                    selection: result.selection,
                    accepted: result.accepted,
                    dirty: page.dirty,
                }
            } else {
                Message::BufferEditResult {
                    request_id,
                    buffer_id,
                    view_id,
                    rev: result.rev,
                    text: result.text,
                    selection: result.selection,
                    dirty: result.dirty,
                    accepted: result.accepted,
                }
            };
            send_msg(sink, &response)
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send BufferEditResult".into(),
                })?;
            Ok(())
        }
        Message::BufferRead {
            request_id,
            buffer_id,
            view_id,
            start,
            len,
        } => {
            require_auth(*authed)?;
            if !*client_paged_reads {
                return Err(paged_reads_unavailable(&request_id));
            }
            if len > MAX_PAGE_BYTES {
                return Err(Message::Error {
                    code: "invalid_buffer_range".into(),
                    message: format!("{request_id}: page length {len} exceeds {MAX_PAGE_BYTES}"),
                });
            }
            if start.checked_add(len).is_none() {
                return Err(Message::Error {
                    code: "invalid_buffer_range".into(),
                    message: format!("{request_id}: byte range overflow"),
                });
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            let page = editor
                .read_page(buffer_id.clone(), start, len)
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_read_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::BufferPage {
                    request_id,
                    buffer_id,
                    view_id,
                    rev: page.rev,
                    start: page.start,
                    total_bytes: page.total_bytes,
                    text: page.text,
                    selection: ByteSelection { anchor: 0, head: 0 },
                    accepted: true,
                    dirty: page.dirty,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferPage".into(),
            })?;
            Ok(())
        }
        Message::BufferAction {
            request_id,
            buffer_id,
            view_id,
            base_rev,
            action,
            selection,
        } => {
            require_auth(*authed)?;
            if !*client_range_edits {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!(
                        "{request_id}: client did not negotiate {CAP_EDITOR_RANGE_EDITS}"
                    ),
                });
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            let result = editor
                .action(
                    buffer_id.clone(),
                    view_id.clone(),
                    base_rev,
                    action,
                    selection,
                )
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_action_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::BufferEditResult {
                    request_id,
                    buffer_id,
                    view_id,
                    rev: result.rev,
                    text: result.text,
                    selection: result.selection,
                    dirty: result.dirty,
                    accepted: result.accepted,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferEditResult".into(),
            })?;
            Ok(())
        }
        Message::BufferSync {
            request_id,
            buffer_id,
            view_id,
        } => {
            require_auth(*authed)?;
            if !*client_range_edits {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!(
                        "{request_id}: client did not negotiate {CAP_EDITOR_RANGE_EDITS}"
                    ),
                });
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: format!("{request_id}: editor capability not available"),
                });
            };
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            let result = editor
                .sync(buffer_id.clone())
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_sync_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::BufferEditResult {
                    request_id,
                    buffer_id,
                    view_id,
                    rev: result.rev,
                    text: result.text,
                    selection: result.selection,
                    dirty: result.dirty,
                    accepted: true,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferEditResult".into(),
            })?;
            Ok(())
        }

        Message::BufferExternalCheck {
            request_id,
            buffer_id,
        } => {
            require_auth(*authed)?;
            require_external_changes(*client_external_changes, &request_id)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: "editor capability not available".into(),
                });
            };
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            let change = editor
                .check_external(buffer_id.clone())
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_external_check_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            let response = match change {
                Some(change) => external_checked_message(request_id, change),
                None => Message::BufferExternalChecked {
                    request_id,
                    buffer_id,
                    found: false,
                    path: String::new(),
                    rev: 0,
                    generation: String::new(),
                    text: String::new(),
                    disk_text: None,
                    dirty: false,
                },
            };
            send_msg(sink, &response)
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send external check result".into(),
                })?;
            Ok(())
        }
        Message::BufferExternalResolve {
            request_id,
            buffer_id,
            base_rev,
            generation,
            resolution,
        } => {
            require_auth(*authed)?;
            require_external_changes(*client_external_changes, &request_id)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: "editor capability not available".into(),
                });
            };
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            let response_resolution = resolution;
            let worker_resolution = match resolution {
                ExternalResolution::Reload => crate::editor_worker::ExternalResolution::Reload,
                ExternalResolution::Keep => crate::editor_worker::ExternalResolution::Keep,
                ExternalResolution::Overwrite => {
                    crate::editor_worker::ExternalResolution::Overwrite
                }
            };
            let result = editor
                .resolve_external(
                    buffer_id.clone(),
                    base_rev,
                    generation.clone(),
                    worker_resolution,
                )
                .await
                .map_err(|err| Message::Error {
                    code: "buffer_external_resolve_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
            let response = Message::BufferExternalResolved {
                request_id,
                buffer_id,
                rev: result.rev,
                generation,
                text: result.text,
                selection: result.selection,
                accepted: result.accepted,
                dirty: result.dirty,
                resolution: response_resolution,
            };
            send_msg(sink, &response)
                .await
                .map_err(|_| Message::Error {
                    code: "send_failed".into(),
                    message: "failed to send external resolution result".into(),
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
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            let dest =
                if path.is_empty() {
                    None
                } else {
                    Some(state.fs_root.resolve_new_file(&path).await.map_err(|err| {
                        Message::Error {
                            code: "buffer_save_failed".into(),
                            message: format!("{request_id}: {err:#}"),
                        }
                    })?)
                };
            let (path, rev) = match editor.save(buffer_id.clone(), base_rev, dest).await {
                Ok(saved) => saved,
                Err(err) => {
                    // A disk-generation conflict is returned as a save error to
                    // preserve existing request semantics. Send the structured
                    // external state first so capable clients can offer a choice.
                    if *client_external_changes
                        && let Ok(Some(change)) = editor.check_external(buffer_id.clone()).await
                    {
                        send_msg(sink, &external_changed_message(change))
                            .await
                            .map_err(|_| Message::Error {
                                code: "send_failed".into(),
                                message: "failed to send save conflict state".into(),
                            })?;
                    }
                    return Err(Message::Error {
                        code: "buffer_save_failed".into(),
                        message: format!("{request_id}: {err:#}"),
                    });
                }
            };
            if Config::path_matches(&state.config_path, &path) {
                match Config::load_from_path(&state.config_path) {
                    Ok(cfg) => {
                        info!(
                            path = %state.config_path.display(),
                            shell = %cfg.resolve_shell().0,
                            theme = %cfg.ui.theme,
                            "reloaded config after save"
                        );
                        let shortkeys = cfg
                            .shortkeys
                            .iter()
                            .map(|key| fresh_gui_protocol::Shortkey {
                                action: key.action.clone(),
                                shortkey: key.shortkey.clone(),
                                when: key.when.clone(),
                            })
                            .collect();
                        let ui = hello_ui(&cfg);
                        if let Some(editor) = state.editor.as_ref()
                            && let Err(err) = editor.reconfigure(cfg.clone()).await
                        {
                            warn!(%err, "failed to apply language-server config to Fresh editor");
                        }
                        *state.config.write().expect("config lock") = cfg;
                        send_msg(
                            sink,
                            &Message::ConfigUpdated {
                                shortkeys,
                                ui: Some(ui),
                            },
                        )
                        .await
                        .map_err(|_| Message::Error {
                            code: "send_failed".into(),
                            message: "failed to send ConfigUpdated".into(),
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
        Message::BufferLspGet {
            buffer_id,
            known_rev,
        } => {
            require_auth(*authed)?;
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: "editor capability not available".into(),
                });
            };
            let lsp = editor
                .lsp_get(buffer_id.clone(), known_rev)
                .await
                .map_err(|err| Message::Error {
                    code: "lsp_failed".into(),
                    message: format!("{buffer_id}: {err:#}"),
                })?;
            send_msg(
                sink,
                &Message::BufferLspState {
                    buffer_id,
                    rev: lsp.rev,
                    text: lsp.text,
                    diagnostics: lsp.diagnostics,
                    status: lsp.status,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferLspState".into(),
            })?;
            Ok(())
        }
        Message::BufferLspRequest { mut request } => {
            require_auth(*authed)?;
            if !*client_lsp_requests {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: "LSP requests require lsp.requests capability".into(),
                });
            }
            if matches!(
                request.feature,
                fresh_gui_protocol::LspRequestFeature::Definition
                    | fresh_gui_protocol::LspRequestFeature::Declaration
                    | fresh_gui_protocol::LspRequestFeature::TypeDefinition
                    | fresh_gui_protocol::LspRequestFeature::Implementation
                    | fresh_gui_protocol::LspRequestFeature::References
                    | fresh_gui_protocol::LspRequestFeature::DocumentSymbols
                    | fresh_gui_protocol::LspRequestFeature::WorkspaceSymbols
            ) && !*client_lsp_navigation
            {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: format!("LSP navigation requires {CAP_LSP_NAVIGATION} capability"),
                });
            }
            if lsp_request_map.len() >= 64 {
                return Err(Message::Error {
                    code: "too_many_lsp_requests".into(),
                    message: "connection already has 64 outstanding LSP requests".into(),
                });
            }
            let Some(editor) = state.editor.as_ref() else {
                return Err(Message::Error {
                    code: "editor_unavailable".into(),
                    message: "editor capability not available".into(),
                });
            };
            ensure_editor_workspace(
                editor,
                state,
                session_id,
                &request.buffer_id,
                &request.request_id.to_string(),
            )
            .await?;
            let client_id = request.request_id;
            let internal_id = NEXT_LSP_WIRE_ID.fetch_add(1, Ordering::Relaxed).max(1);
            let client_view_id = request.view_id.clone();
            request.request_id = internal_id;
            request.view_id = format!("{socket_id}:{client_view_id}");
            // The socket's receiver was subscribed before this request is queued.
            lsp_request_map.insert(
                internal_id,
                (client_id, request.buffer_id.clone(), client_view_id),
            );
            if let Err(error) = editor.request_lsp(request) {
                lsp_request_map.remove(&internal_id);
                return Err(Message::Error {
                    code: "lsp_failed".into(),
                    message: error.to_string(),
                });
            }
            Ok(())
        }
        Message::BufferLspCancel {
            request_id,
            buffer_id,
            view_id,
        } => {
            require_auth(*authed)?;
            if !*client_lsp_requests {
                return Err(Message::Error {
                    code: "capability_unavailable".into(),
                    message: "LSP requests require lsp.requests capability".into(),
                });
            }
            if let Some((internal_id, _)) = lsp_request_map
                .iter()
                .find(|(_, (client_id, buffer, view))| {
                    *client_id == request_id && *buffer == buffer_id && *view == view_id
                })
                .map(|(id, value)| (*id, value.clone()))
            {
                lsp_request_map.remove(&internal_id);
                if let Some(editor) = state.editor.as_ref() {
                    let _ =
                        editor.cancel_lsp(internal_id, buffer_id, format!("{socket_id}:{view_id}"));
                }
            }
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
            ensure_editor_workspace(editor, state, session_id, &buffer_id, &request_id).await?;
            let formatted = editor
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
                    rev: formatted.rev,
                    text: formatted.text,
                    status: formatted.status,
                },
            )
            .await
            .map_err(|_| Message::Error {
                code: "send_failed".into(),
                message: "failed to send BufferFormatted".into(),
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
        Message::GitRestore {
            request_id,
            workspace_id,
            directory,
            paths,
        } => {
            require_auth(*authed)?;
            git_op(
                state,
                sink,
                request_id,
                workspace_id,
                directory,
                move |dir| crate::git::restore(&dir, &paths),
            )
            .await
        }
        Message::GitStage {
            request_id,
            workspace_id,
            directory,
            paths,
            stage,
        } => {
            require_auth(*authed)?;
            git_op(
                state,
                sink,
                request_id,
                workspace_id,
                directory,
                move |dir| crate::git::stage(&dir, &paths, stage),
            )
            .await
        }
        Message::GitCommit {
            request_id,
            workspace_id,
            directory,
            message,
        } => {
            require_auth(*authed)?;
            git_op(
                state,
                sink,
                request_id,
                workspace_id,
                directory,
                move |dir| crate::git::commit(&dir, &message),
            )
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
            project.clear();
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
            if let Some(editor) = state.editor.as_ref()
                && !editor
                    .draft_list(workspace_id.clone())
                    .await
                    .map_err(|err| Message::Error {
                        code: "workspace_close_failed".into(),
                        message: format!("cannot verify draft recovery: {err:#}"),
                    })?
                    .is_empty()
            {
                return Err(Message::Error {
                    code: "workspace_close_failed".into(),
                    message: "workspace has recoverable editor drafts; save or discard them before closing the workspace".into(),
                });
            }
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

fn require_project_cap(client: bool, editor: bool, request_id: &str) -> Result<(), Message> {
    if client && editor { Ok(()) } else { Err(settings_error("capability_unavailable", request_id, "project.search.v1 capability not negotiated")) }
}

async fn project_root(state: &AppState, workspace: &str) -> Result<PathBuf, Message> {
    let root = state.workspaces.root_of(workspace).await.unwrap_or_else(|| state.fs_root.root_display());
    state.fs_root.resolve(&root).await.map_err(|e| settings_error("project_search_failed", workspace, e))
}

fn require_settings_cap(enabled: bool, request_id: &str) -> Result<(), Message> {
    if enabled {
        Ok(())
    } else {
        Err(Message::Error {
            code: "capability_unavailable".into(),
            message: format!("{request_id}: {CAP_SETTINGS_EDITOR} capability not negotiated"),
        })
    }
}

fn settings_error(code: &str, request_id: &str, error: impl std::fmt::Display) -> Message {
    Message::Error {
        code: code.into(),
        message: format!("{request_id}: {error}"),
    }
}

#[cfg(test)]
mod settings_capability_tests {
    use super::{require_settings_cap, settings_error};
    use fresh_gui_protocol::Message;

    #[test]
    fn fresh_line_wrap_maps_to_live_native_ui_and_reset_uses_host_default() {
        let cfg = crate::config::Config::parse(r#"{"editor":{"line_wrap":false}}"#).unwrap();
        assert!(!super::hello_ui(&cfg).editor_line_wrap);
        assert!(super::hello_ui(&crate::config::Config::default()).editor_line_wrap);
    }

    #[test]
    fn settings_capability_is_required_and_errors_keep_request_id() {
        assert!(require_settings_cap(true, "settings-1").is_ok());
        assert!(matches!(
            require_settings_cap(false, "settings-1"),
            Err(Message::Error { code, message }) if code == "capability_unavailable" && message.starts_with("settings-1:")
        ));
        assert!(matches!(
            settings_error("settings_patch_failed", "settings-1", "stale config"),
            Message::Error { code, message } if code == "settings_patch_failed" && message.starts_with("settings-1:")
        ));
    }
}

fn cancel_socket_lsp(
    editor: Option<&EditorHandle>,
    socket_id: &str,
    requests: &mut HashMap<u64, (u64, String, String)>,
) {
    if let Some(editor) = editor {
        for (internal_id, (_, buffer_id, view_id)) in requests.drain() {
            let _ = editor.cancel_lsp(internal_id, buffer_id, format!("{socket_id}:{view_id}"));
        }
    } else {
        requests.clear();
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
        return Err(format!(
            "workspace root must be an absolute path, got {raw}"
        ));
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
            assert!(
                validate_workspace_root("home/me/project")
                    .unwrap_err()
                    .contains("absolute")
            );
            assert!(
                validate_workspace_root(r"C:\work\project")
                    .unwrap_err()
                    .contains("Windows path")
            );
            assert!(
                validate_workspace_root("C:/work/project")
                    .unwrap_err()
                    .contains("Windows path")
            );
            assert!(
                validate_workspace_root(r"\\server\share")
                    .unwrap_err()
                    .contains("Windows path")
            );
        }
        #[cfg(windows)]
        {
            assert!(validate_workspace_root(r"C:\work\project").is_ok());
            assert!(validate_workspace_root("relative\\project").is_err());
        }
    }
}

#[cfg(test)]
mod external_change_capability_tests {
    use super::require_external_changes;
    use fresh_gui_protocol::Message;

    #[test]
    fn external_change_requests_require_negotiated_capability() {
        let error = require_external_changes(false, "check-1").unwrap_err();
        assert!(matches!(error, Message::Error { code, .. } if code == "capability_unavailable"));
        assert!(require_external_changes(true, "check-2").is_ok());
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

fn lsp_file_uri_to_path(uri: &str) -> anyhow::Result<PathBuf> {
    let wire = fresh::app::types::LspUri::from_wire(
        serde_json::from_value(serde_json::Value::String(uri.to_owned()))
            .context("invalid LSP URI")?,
    );
    // Fresh's LspManager runs under the daemon's local Authority. SSH transport
    // translates the connection, while file URIs already name daemon paths.
    let path = wire.to_host_path(None).context("LSP target is not a file URI")?;
    anyhow::ensure!(path.is_absolute(), "LSP file URI path is not absolute");
    Ok(path)
}

async fn reply_editor_opened(
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    editor: &EditorHandle,
    request_id: String,
    path: std::path::PathBuf,
    preview: bool,
    line: Option<u32>,
    column: Option<u32>,
    workspace_id: String,
    paged_reads: bool,
) -> Result<(), Message> {
    if crate::binary::is_binary_file(&path).unwrap_or(false) {
        return Err(Message::Error {
            code: "binary_file".into(),
            message: format!("{request_id}: {}", path.display()),
        });
    }
    let prior_buffers = if !paged_reads {
        Some(
            editor
                .scene()
                .await
                .map_err(|err| Message::Error {
                    code: "editor_open_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?
                .buffers,
        )
    } else {
        None
    };
    let opened = editor
        .open_in_workspace(path, preview, workspace_id.clone())
        .await
        .map_err(|err| {
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
    if opened.total_bytes.is_some() && !paged_reads {
        // A stat/open race can cross the threshold. Remove only a new clean
        // open; an existing dirty buffer belongs to other connected views.
        if !opened.dirty
            && prior_buffers.as_ref().is_some_and(|buffers| {
                !buffers
                    .iter()
                    .any(|buffer| buffer.buffer_id == opened.buffer_id)
            })
        {
            editor
                .close_in_workspace(opened.buffer_id.clone(), workspace_id)
                .await
                .map_err(|err| Message::Error {
                    code: "editor_close_failed".into(),
                    message: format!("{request_id}: {err:#}"),
                })?;
        }
        return Err(paged_reads_unavailable(&request_id));
    }
    send_editor_opened_snapshot(sink, request_id, opened, line, column).await?;
    Ok(())
}

async fn send_editor_opened_snapshot(
    sink: &mut futures_util::stream::SplitSink<WebSocket, WsMessage>,
    request_id: String,
    opened: crate::editor_worker::OpenedBuffer,
    line: Option<u32>,
    column: Option<u32>,
) -> Result<(), Message> {
    send_msg(
        sink,
        &Message::EditorOpened {
            request_id,
            buffer_id: opened.buffer_id.clone(),
            draft_id: Some(opened.draft_id.clone()),
            path: opened.path.clone(),
            language: opened.language.clone(),
            line,
            column,
        },
    )
    .await
    .map_err(|_| Message::Error {
        code: "send_failed".into(),
        message: "failed to send EditorOpened".into(),
    })?;
    send_msg(sink, &opened_content_message(opened))
        .await
        .map_err(|_| Message::Error {
            code: "send_failed".into(),
            message: "failed to send BufferSnapshot".into(),
        })?;
    Ok(())
}

fn opened_content_message(opened: crate::editor_worker::OpenedBuffer) -> Message {
    if let Some(total_bytes) = opened.total_bytes {
        Message::BufferPaged {
            buffer_id: opened.buffer_id,
            rev: opened.rev,
            total_bytes,
            path: opened.path,
            dirty: opened.dirty,
        }
    } else {
        Message::BufferSnapshot {
            buffer_id: opened.buffer_id,
            rev: opened.rev,
            text: opened.text,
            path: opened.path,
        }
    }
}

fn is_large_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|metadata| metadata.len() > MAX_SNAPSHOT_BYTES)
        .unwrap_or(false)
}

fn paged_reads_unavailable(request_id: &str) -> Message {
    Message::Error {
        code: "capability_unavailable".into(),
        message: format!("{request_id}: client did not negotiate {CAP_EDITOR_PAGED_READS}"),
    }
}

async fn current_workspace_id(
    state: &AppState,
    session_id: &Option<String>,
) -> Result<String, Message> {
    let Some(session_id) = session_id else {
        return Ok("default".into());
    };
    Ok(state
        .workspaces
        .id_for_session(session_id)
        .await
        .unwrap_or_else(|| "default".into()))
}

async fn ensure_editor_workspace(
    editor: &EditorHandle,
    state: &AppState,
    session_id: &Option<String>,
    buffer_id: &str,
    request_id: &str,
) -> Result<(), Message> {
    let workspace_id = current_workspace_id(state, session_id).await?;
    editor
        .check_workspace(buffer_id.to_owned(), workspace_id)
        .await
        .map_err(|err| Message::Error {
            code: "editor_workspace_mismatch".into(),
            message: format!("{request_id}: {err:#}"),
        })
}

fn external_changed_message(change: crate::editor_worker::ExternalChange) -> Message {
    Message::BufferExternalChanged {
        buffer_id: change.buffer_id,
        path: change.path,
        rev: change.rev,
        generation: change.generation,
        text: change.text,
        disk_text: change.disk_text,
        dirty: change.dirty,
    }
}

fn external_checked_message(
    request_id: String,
    change: crate::editor_worker::ExternalChange,
) -> Message {
    Message::BufferExternalChecked {
        request_id,
        buffer_id: change.buffer_id,
        found: true,
        path: change.path,
        rev: change.rev,
        generation: change.generation,
        text: change.text,
        disk_text: change.disk_text,
        dirty: change.dirty,
    }
}

fn require_external_changes(enabled: bool, request_id: &str) -> Result<(), Message> {
    if enabled {
        Ok(())
    } else {
        Err(Message::Error {
            code: "capability_unavailable".into(),
            message: format!(
                "{request_id}: client did not negotiate {CAP_EDITOR_EXTERNAL_CHANGES}"
            ),
        })
    }
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

async fn git_dir(
    state: &AppState,
    workspace_id: &str,
    directory: &str,
) -> Result<PathBuf, Message> {
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

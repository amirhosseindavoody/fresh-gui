//! Tokio ADE worker thread. GPUI talks to it through channels.

use std::thread;
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{
    CAP_WORKSPACE, FsEntry, GitFile, Hello, Message, PtyInfo, WorkspaceInfo, WorkspaceTab,
};

use super::connect::ConnectTarget;
use super::paths::{daemon_uses_unix_paths, workspace_root_for_daemon};

#[derive(Debug, Clone)]
pub enum AdeCmd {
    OpenPty {
        cols: u16,
        rows: u16,
        cwd: Option<String>,
    },
    WritePty {
        id: String,
        data: Vec<u8>,
    },
    ResizePty {
        id: String,
        cols: u16,
        rows: u16,
    },
    ClosePty {
        id: String,
    },
    ListDir {
        request_id: String,
        path: String,
    },
    AuthorizeDir {
        request_id: String,
        path: String,
    },
    OpenEditor {
        request_id: String,
        path: String,
        preview: bool,
        line: Option<u32>,
        column: Option<u32>,
    },
    EditBuffer {
        request_id: String,
        buffer_id: String,
        base_rev: u64,
        text: String,
    },
    SaveBuffer {
        request_id: String,
        buffer_id: String,
        base_rev: u64,
    },
    CloseEditor {
        buffer_id: String,
    },
    DeletePaths {
        request_id: String,
        paths: Vec<String>,
    },
    CopyPaths {
        request_id: String,
        sources: Vec<String>,
        destination: String,
    },
    MovePaths {
        request_id: String,
        sources: Vec<String>,
        destination: String,
    },
    RenamePath {
        request_id: String,
        path: String,
        name: String,
    },
    CreatePath {
        request_id: String,
        parent: String,
        name: String,
        kind: fresh_gui_protocol::FsKind,
    },
    CreateWorkspace {
        name: String,
        root: String,
    },
    RenameWorkspace {
        id: String,
        name: String,
    },
    SetWorkspaceRoot {
        id: String,
        root: String,
    },
    CloseWorkspace {
        id: String,
    },
    SwitchWorkspace {
        id: String,
        /// Workspace whose tab list should be saved before the subscriber moves.
        from: Option<String>,
        tabs: Vec<WorkspaceTab>,
        active_tab: u32,
        explorer_expanded: Vec<String>,
        extra: fresh_gui_protocol::WorkspaceLayoutExtra,
    },
    SetWorkspaceLayout {
        id: String,
        tabs: Vec<WorkspaceTab>,
        active_tab: u32,
        explorer_expanded: Vec<String>,
        extra: fresh_gui_protocol::WorkspaceLayoutExtra,
    },
    GitStatus {
        request_id: String,
        workspace_id: String,
        directory: String,
    },
    GitDiff {
        request_id: String,
        workspace_id: String,
        directory: String,
        path: String,
    },
    GitStage {
        request_id: String,
        workspace_id: String,
        directory: String,
        paths: Vec<String>,
        stage: bool,
    },
    GitCommit {
        request_id: String,
        workspace_id: String,
        directory: String,
        message: String,
    },
    GitPull {
        request_id: String,
        workspace_id: String,
        directory: String,
    },
    GitPush {
        request_id: String,
        workspace_id: String,
        directory: String,
    },
    OpenExternal {
        request_id: String,
        path: String,
    },
    /// Acknowledged once every command queued before it has been written.
    Flush(std::sync::mpsc::Sender<()>),
    Disconnect,
}

/// Workspace the connection is attached to, plus the tab metadata needed to
/// rebuild the host strip before scrollback arrives.
#[derive(Debug, Clone)]
pub struct AttachedWorkspace {
    pub info: WorkspaceInfo,
    pub tabs: Vec<WorkspaceTab>,
    pub active_tab: u32,
    pub ptys: Vec<PtyInfo>,
    pub explorer_expanded: Vec<String>,
    pub extra: fresh_gui_protocol::WorkspaceLayoutExtra,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AdeEvent {
    Connecting,
    // Boxed: every PTY chunk crosses this channel as an `AdeEvent`, so the
    // rare connect/switch payloads should not set the size of each one.
    Connected {
        hello: Box<Hello>,
        session_id: String,
        workspaces: Vec<WorkspaceInfo>,
        /// Present when the daemon advertises `workspace` and a workspace is attached.
        attached: Option<Box<AttachedWorkspace>>,
    },
    ConfigUpdated { shortkeys: Vec<fresh_gui_protocol::Shortkey> },
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    WorkspaceRenamed {
        workspace: WorkspaceInfo,
    },
    WorkspaceRootSet {
        workspace: WorkspaceInfo,
    },
    WorkspaceClosed {
        id: String,
        focused_id: Option<String>,
    },
    WorkspaceSwitched {
        attached: Box<AttachedWorkspace>,
    },
    Disconnected {
        reason: String,
    },
    PtyOpened {
        id: String,
        cols: u16,
        rows: u16,
    },
    PtyData {
        id: String,
        bytes: Vec<u8>,
    },
    PtyClosed {
        id: String,
        reason: Option<String>,
    },
    FsListed {
        request_id: String,
        path: String,
        entries: Vec<FsEntry>,
    },
    EditorOpened {
        request_id: String,
        buffer_id: String,
        path: String,
        language: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
    },
    BufferSnapshot {
        buffer_id: String,
        rev: u64,
        text: String,
        path: String,
    },
    BufferChanged {
        request_id: String,
        buffer_id: String,
        rev: u64,
    },
    BufferSaved {
        request_id: String,
        buffer_id: String,
        path: String,
        rev: u64,
    },
    FsCopied {
        request_id: String,
        entries: Vec<FsEntry>,
    },
    FsMoved {
        request_id: String,
        entries: Vec<FsEntry>,
    },
    FsRenamed {
        request_id: String,
        entry: FsEntry,
    },
    FsCreated {
        request_id: String,
        entry: FsEntry,
    },
    Error {
        code: String,
        message: String,
    },
    GitStatus {
        request_id: String,
        repo: bool,
        root: String,
        branch: String,
        upstream: Option<String>,
        ahead: u32,
        behind: u32,
        files: Vec<GitFile>,
        detail: Option<String>,
    },
    GitDiff {
        request_id: String,
        path: String,
        old_text: String,
        new_text: String,
        binary: bool,
        truncated: bool,
    },
    GitOp {
        request_id: String,
        ok: bool,
        output: String,
    },
    FsOpened {
        request_id: String,
        message: String,
    },
}

#[derive(Clone)]
pub struct AdeHandle {
    tx: async_channel::Sender<AdeCmd>,
}

impl AdeHandle {
    pub fn send(&self, cmd: AdeCmd) {
        let _ = self.tx.try_send(cmd);
    }

    /// Block until queued commands reach the socket, or `timeout` passes.
    /// For window close: the process can exit before the worker thread runs.
    pub fn flush_blocking(&self, timeout: Duration) -> bool {
        let (ack_tx, ack_rx) = std::sync::mpsc::channel();
        if self.tx.try_send(AdeCmd::Flush(ack_tx)).is_err() {
            return false;
        }
        ack_rx.recv_timeout(timeout).is_ok()
    }
}

pub fn spawn(target: ConnectTarget) -> (AdeHandle, async_channel::Receiver<AdeEvent>) {
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<AdeCmd>();
    let (evt_tx, evt_rx) = async_channel::unbounded::<AdeEvent>();

    thread::Builder::new()
        .name("fresh-gui-ade".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = evt_tx.send_blocking(AdeEvent::Disconnected {
                        reason: format!("tokio runtime: {err}"),
                    });
                    return;
                }
            };
            rt.block_on(ade_loop(target, cmd_rx, evt_tx));
        })
        .expect("spawn ADE thread");

    (AdeHandle { tx: cmd_tx }, evt_rx)
}

async fn ade_loop(
    target: ConnectTarget,
    cmd_rx: async_channel::Receiver<AdeCmd>,
    evt_tx: async_channel::Sender<AdeEvent>,
) {
    let _ = evt_tx.send(AdeEvent::Connecting).await;
    let mut opts = ConnectOptions::new(&target.ws_url);
    if let Some(token) = &target.token {
        opts = opts.with_token(token.clone());
    }

    let mut client = match Client::connect(opts).await {
        Ok(c) => c,
        Err(err) => {
            let _ = evt_tx
                .send(AdeEvent::Disconnected {
                    reason: format!("{err:#}"),
                })
                .await;
            return;
        }
    };

    let hello = client.backend_hello.clone();
    let boot = match bootstrap_workspaces(&mut client, target.preferred_root.as_deref()).await {
        Ok(boot) => boot,
        Err(err) => {
            let _ = evt_tx
                .send(AdeEvent::Disconnected {
                    reason: format!("workspace: {err:#}"),
                })
                .await;
            return;
        }
    };

    let _ = evt_tx
        .send(AdeEvent::Connected {
            hello: Box::new(hello),
            session_id: boot.session_id,
            workspaces: boot.workspaces,
            attached: boot.attached.map(Box::new),
        })
        .await;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Ok(AdeCmd::Disconnect) | Err(_) => break,
                    Ok(cmd) => {
                        if let Err(err) = dispatch_cmd(&mut client, cmd).await {
                            let _ = evt_tx.send(AdeEvent::Error {
                                code: "ade".into(),
                                message: format!("{err:#}"),
                            }).await;
                        }
                    }
                }
            }
            msg = client.recv() => {
                match msg {
                    Ok(message) => {
                        if let Message::WorkspaceSwitched { ref workspace, .. } = message {
                            client.session_id = Some(workspace.session_id.clone());
                        }
                        if let Some(ev) = event_from_message(message)
                            && evt_tx.send(ev).await.is_err() {
                                break;
                            }
                    }
                    Err(err) => {
                        let _ = evt_tx.send(AdeEvent::Disconnected {
                            reason: format!("{err:#}"),
                        }).await;
                        break;
                    }
                }
            }
        }
    }
}

async fn dispatch_cmd(client: &mut Client, cmd: AdeCmd) -> anyhow::Result<()> {
    match cmd {
        AdeCmd::OpenPty { cols, rows, cwd } => {
            client
                .send(Message::PtyOpen {
                    cols,
                    rows,
                    cwd,
                    shell: None,
                })
                .await?;
        }
        AdeCmd::WritePty { id, data } => {
            client.write_pty(&id, &data).await?;
        }
        AdeCmd::ResizePty { id, cols, rows } => {
            client.resize_pty(&id, cols, rows).await?;
        }
        AdeCmd::ClosePty { id } => {
            client.close_pty(&id).await?;
        }
        AdeCmd::ListDir { request_id, path } => {
            client.send(Message::FsList { request_id, path }).await?;
        }
        AdeCmd::AuthorizeDir { request_id, path } => {
            client.send(Message::FsAuthorize { request_id, path }).await?;
        }
        AdeCmd::OpenEditor {
            request_id,
            path,
            preview,
            line,
            column,
        } => {
            client
                .send(Message::EditorOpen {
                    request_id,
                    path,
                    preview,
                    cwd: None,
                    line,
                    column,
                })
                .await?;
        }
        AdeCmd::EditBuffer {
            request_id,
            buffer_id,
            base_rev,
            text,
        } => {
            client
                .send(Message::BufferEdit {
                    request_id,
                    buffer_id,
                    base_rev,
                    text,
                })
                .await?;
        }
        AdeCmd::SaveBuffer {
            request_id,
            buffer_id,
            base_rev,
        } => {
            client
                .send(Message::BufferSave {
                    request_id,
                    buffer_id,
                    base_rev,
                })
                .await?;
        }
        AdeCmd::CloseEditor { buffer_id } => {
            client.send(Message::EditorClose { buffer_id }).await?;
        }
        AdeCmd::DeletePaths { request_id, paths } => {
            client.send(Message::FsDelete { request_id, paths }).await?;
        }
        AdeCmd::CopyPaths {
            request_id,
            sources,
            destination,
        } => {
            client
                .send(Message::FsCopy {
                    request_id,
                    sources,
                    destination,
                })
                .await?;
        }
        AdeCmd::MovePaths {
            request_id,
            sources,
            destination,
        } => {
            client
                .send(Message::FsMove {
                    request_id,
                    sources,
                    destination,
                })
                .await?;
        }
        AdeCmd::RenamePath {
            request_id,
            path,
            name,
        } => {
            client
                .send(Message::FsRename {
                    request_id,
                    path,
                    name,
                })
                .await?;
        }
        AdeCmd::CreatePath {
            request_id,
            parent,
            name,
            kind,
        } => {
            client
                .send(Message::FsCreate {
                    request_id,
                    parent,
                    name,
                    kind,
                })
                .await?;
        }
        AdeCmd::CreateWorkspace { name, root } => {
            let name = if name.trim().is_empty() {
                None
            } else {
                Some(name)
            };
            let root = if root.trim().is_empty() {
                None
            } else {
                let unix = daemon_uses_unix_paths(client.backend_hello.config_path.as_deref(), &[]);
                Some(workspace_root_for_daemon(&root, unix).map_err(anyhow::Error::msg)?)
            };
            client.send(Message::WorkspaceCreate { name, root }).await?;
        }
        AdeCmd::RenameWorkspace { id, name } => {
            client
                .send(Message::WorkspaceRename {
                    workspace_id: id,
                    name,
                })
                .await?;
        }
        AdeCmd::SetWorkspaceRoot { id, root } => {
            let unix = daemon_uses_unix_paths(client.backend_hello.config_path.as_deref(), &[]);
            let root = workspace_root_for_daemon(&root, unix).map_err(anyhow::Error::msg)?;
            client
                .send(Message::WorkspaceSetRoot {
                    workspace_id: id,
                    root,
                })
                .await?;
        }
        AdeCmd::CloseWorkspace { id } => {
            client
                .send(Message::WorkspaceClose { workspace_id: id })
                .await?;
        }
        AdeCmd::SwitchWorkspace {
            id,
            from,
            tabs,
            active_tab,
            explorer_expanded,
            extra,
        } => {
            if let Some(from) = from {
                client
                    .send(Message::WorkspaceLayoutSet {
                        workspace_id: from,
                        tabs,
                        active_tab,
                        explorer_expanded,
                        extra,
                    })
                    .await?;
            }
            client
                .send(Message::WorkspaceSwitch { workspace_id: id })
                .await?;
        }
        AdeCmd::SetWorkspaceLayout {
            id,
            tabs,
            active_tab,
            explorer_expanded,
            extra,
        } => {
            client
                .send(Message::WorkspaceLayoutSet {
                    workspace_id: id,
                    tabs,
                    active_tab,
                    explorer_expanded,
                    extra,
                })
                .await?;
        }
        AdeCmd::Flush(ack) => {
            let _ = ack.send(());
        }
        AdeCmd::Disconnect => {}
        AdeCmd::GitStatus {
            request_id,
            workspace_id,
            directory,
        } => {
            client
                .send(Message::GitStatus {
                    request_id,
                    workspace_id,
                    directory,
                })
                .await?;
        }
        AdeCmd::GitDiff {
            request_id,
            workspace_id,
            directory,
            path,
        } => {
            client
                .send(Message::GitDiff {
                    request_id,
                    workspace_id,
                    directory,
                    path,
                })
                .await?;
        }
        AdeCmd::GitStage {
            request_id,
            workspace_id,
            directory,
            paths,
            stage,
        } => {
            client
                .send(Message::GitStage {
                    request_id,
                    workspace_id,
                    directory,
                    paths,
                    stage,
                })
                .await?;
        }
        AdeCmd::GitCommit {
            request_id,
            workspace_id,
            directory,
            message,
        } => {
            client
                .send(Message::GitCommit {
                    request_id,
                    workspace_id,
                    directory,
                    message,
                })
                .await?;
        }
        AdeCmd::GitPull {
            request_id,
            workspace_id,
            directory,
        } => {
            client
                .send(Message::GitPull {
                    request_id,
                    workspace_id,
                    directory,
                })
                .await?;
        }
        AdeCmd::GitPush {
            request_id,
            workspace_id,
            directory,
        } => {
            client
                .send(Message::GitPush {
                    request_id,
                    workspace_id,
                    directory,
                })
                .await?;
        }
        AdeCmd::OpenExternal { request_id, path } => {
            client
                .send(Message::FsOpenExternal { request_id, path })
                .await?;
        }
    }
    Ok(())
}

struct Boot {
    session_id: String,
    workspaces: Vec<WorkspaceInfo>,
    attached: Option<AttachedWorkspace>,
}

async fn bootstrap_workspaces(
    client: &mut Client,
    preferred_root: Option<&str>,
) -> anyhow::Result<Boot> {
    let cap = client
        .backend_hello
        .capabilities
        .iter()
        .any(|c| c == CAP_WORKSPACE);
    if !cap {
        let session_id = client.create_session(None).await?;
        return Ok(Boot {
            session_id,
            workspaces: Vec::new(),
            attached: None,
        });
    }

    client.send(Message::WorkspaceList).await?;
    let (mut workspaces, focused) = recv_workspace_list(client).await?;
    let unix = daemon_uses_unix_paths(
        client.backend_hello.config_path.as_deref(),
        &workspaces
            .iter()
            .map(|ws| ws.root.as_str())
            .collect::<Vec<_>>(),
    );
    let preferred = preferred_root
        .map(|root| workspace_root_for_daemon(root, unix))
        .transpose()
        .map_err(anyhow::Error::msg)?;
    let action =
        super::connect::plan_workspace_boot(&workspaces, focused.as_deref(), preferred.as_deref());
    let id = match action {
        super::connect::BootAction::Switch { id } => id,
        super::connect::BootAction::Create { root } => {
            client
                .send(Message::WorkspaceCreate { name: None, root })
                .await?;
            let created = recv_workspace_created(client).await?;
            let id = created.id.clone();
            workspaces.push(created);
            id
        }
    };
    client
        .send(Message::WorkspaceSwitch { workspace_id: id })
        .await?;
    let attached = recv_workspace_switched(client).await?;
    client.session_id = Some(attached.info.session_id.clone());
    if let Some(slot) = workspaces
        .iter_mut()
        .find(|workspace| workspace.id == attached.info.id)
    {
        *slot = attached.info.clone();
    }
    Ok(Boot {
        session_id: attached.info.session_id.clone(),
        workspaces,
        attached: Some(attached),
    })
}

async fn recv_workspace_list(
    client: &mut Client,
) -> anyhow::Result<(Vec<WorkspaceInfo>, Option<String>)> {
    loop {
        match client.recv().await? {
            Message::WorkspaceListed {
                workspaces,
                focused_id,
            } => return Ok((workspaces, focused_id)),
            Message::Error { code, message } => {
                anyhow::bail!("workspace list failed: {code}: {message}")
            }
            other if bootstrap_skip(&other) => continue,
            other => anyhow::bail!("unexpected while listing workspaces: {other:?}"),
        }
    }
}

async fn recv_workspace_created(client: &mut Client) -> anyhow::Result<WorkspaceInfo> {
    loop {
        match client.recv().await? {
            Message::WorkspaceCreated { workspace } => return Ok(workspace),
            Message::Error { code, message } => {
                anyhow::bail!("workspace create failed: {code}: {message}")
            }
            other if bootstrap_skip(&other) => continue,
            other => anyhow::bail!("unexpected while creating workspace: {other:?}"),
        }
    }
}

async fn recv_workspace_switched(client: &mut Client) -> anyhow::Result<AttachedWorkspace> {
    loop {
        match client.recv().await? {
            Message::WorkspaceSwitched {
                workspace,
                tabs,
                active_tab,
                ptys,
                explorer_expanded,
                extra,
            } => {
                return Ok(AttachedWorkspace {
                    info: workspace,
                    tabs,
                    active_tab,
                    ptys,
                    explorer_expanded,
                    extra,
                });
            }
            Message::Error { code, message } => {
                anyhow::bail!("workspace switch failed: {code}: {message}")
            }
            other if bootstrap_skip(&other) => continue,
            other => anyhow::bail!("unexpected while switching workspace: {other:?}"),
        }
    }
}

fn bootstrap_skip(msg: &Message) -> bool {
    matches!(
        msg,
        Message::Pong { .. } | Message::Ping { .. } | Message::AuthOk
    )
}

fn event_from_message(msg: Message) -> Option<AdeEvent> {
    match msg {
        Message::PtyOpened { id, cols, rows } => Some(AdeEvent::PtyOpened { id, cols, rows }),
        Message::PtyData { id, data } => {
            let bytes = Client::decode_pty_data(&data).ok()?;
            Some(AdeEvent::PtyData { id, bytes })
        }
        Message::PtyClosed { id, reason } => Some(AdeEvent::PtyClosed { id, reason }),
        Message::FsListed {
            request_id,
            path,
            entries,
        } => Some(AdeEvent::FsListed {
            request_id,
            path,
            entries,
        }),
        Message::EditorOpened {
            request_id,
            buffer_id,
            path,
            language,
            line,
            column,
        } => Some(AdeEvent::EditorOpened {
            request_id,
            buffer_id,
            path,
            language,
            line,
            column,
        }),
        Message::BufferSnapshot {
            buffer_id,
            rev,
            text,
            path,
        } => Some(AdeEvent::BufferSnapshot {
            buffer_id,
            rev,
            text,
            path,
        }),
        Message::BufferChanged {
            request_id,
            buffer_id,
            rev,
        } => Some(AdeEvent::BufferChanged {
            request_id,
            buffer_id,
            rev,
        }),
        Message::ConfigUpdated { shortkeys } => Some(AdeEvent::ConfigUpdated { shortkeys }),
        Message::BufferSaved {
            request_id,
            buffer_id,
            path,
            rev,
        } => Some(AdeEvent::BufferSaved {
            request_id,
            buffer_id,
            path,
            rev,
        }),
        Message::FsCopied {
            request_id,
            entries,
        } => Some(AdeEvent::FsCopied {
            request_id,
            entries,
        }),
        Message::FsMoved {
            request_id,
            entries,
        } => Some(AdeEvent::FsMoved {
            request_id,
            entries,
        }),
        Message::FsRenamed { request_id, entry } => Some(AdeEvent::FsRenamed { request_id, entry }),
        Message::FsCreated { request_id, entry } => Some(AdeEvent::FsCreated { request_id, entry }),
        Message::Error { code, message } => Some(AdeEvent::Error { code, message }),
        Message::GitStatusResult {
            request_id,
            repo,
            root,
            branch,
            upstream,
            ahead,
            behind,
            files,
            detail,
        } => Some(AdeEvent::GitStatus {
            request_id,
            repo,
            root,
            branch,
            upstream,
            ahead,
            behind,
            files,
            detail,
        }),
        Message::GitDiffResult {
            request_id,
            path,
            old_text,
            new_text,
            binary,
            truncated,
        } => Some(AdeEvent::GitDiff {
            request_id,
            path,
            old_text,
            new_text,
            binary,
            truncated,
        }),
        Message::GitOpResult {
            request_id,
            ok,
            output,
        } => Some(AdeEvent::GitOp {
            request_id,
            ok,
            output,
        }),
        Message::FsOpened {
            request_id,
            message,
        } => Some(AdeEvent::FsOpened {
            request_id,
            message,
        }),
        Message::WorkspaceCreated { workspace } => Some(AdeEvent::WorkspaceCreated { workspace }),
        Message::WorkspaceRenamed { workspace } => Some(AdeEvent::WorkspaceRenamed { workspace }),
        Message::WorkspaceRootSet { workspace } => Some(AdeEvent::WorkspaceRootSet { workspace }),
        Message::WorkspaceClosed {
            workspace_id,
            focused_id,
        } => Some(AdeEvent::WorkspaceClosed {
            id: workspace_id,
            focused_id,
        }),
        Message::WorkspaceSwitched {
            workspace,
            tabs,
            active_tab,
            ptys,
            explorer_expanded,
            extra,
        } => Some(AdeEvent::WorkspaceSwitched {
            attached: Box::new(AttachedWorkspace {
                info: workspace,
                tabs,
                active_tab,
                ptys,
                explorer_expanded,
                extra,
            }),
        }),
        Message::Pong { .. } | Message::Ping { .. } | Message::AuthOk => None,
        _ => None,
    }
}

/// Used by tests so the unused import of Duration stays meaningful if we add
/// reconnect backoff later.
#[allow(dead_code)]
pub fn reconnect_delay() -> Duration {
    Duration::from_secs(2)
}

//! Shared protocol types for fresh-gui (host ↔ remote).
//!
//! PTY-first ADE protocol.
//! Phase 2: sessions. Phase 3a: editor open/snapshot.
//! Phase 3b: edit/save. Phase 3c: fs_watch + thin scene.

use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

/// Protocol version negotiated in [`Hello`].
pub const PROTOCOL_VERSION: &str = "0.4.0";

pub const CAP_PING: &str = "ping";
pub const CAP_PTY: &str = "pty";
pub const CAP_FS: &str = "fs";
pub const CAP_SESSION: &str = "session";
pub const CAP_WORKSPACE: &str = "workspace";
/// Changing the root of an existing workspace.
pub const CAP_WORKSPACE_SET_ROOT: &str = "workspace_set_root";
pub const CAP_EDITOR: &str = "editor";
pub const CAP_LSP: &str = "lsp";
pub const CAP_SCENE: &str = "scene";
/// Workspace git status, diff, and stage/commit/pull/push. Absent on older daemons.
pub const CAP_GIT: &str = "git";

/// First message after WebSocket connect. Client sends; backend replies with its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Hello {
    pub protocol_version: String,
    pub role: PeerRole,
    /// Free-form implementation id, e.g. `fresh-gui/2026.728.1`.
    pub implementation: String,
    pub capabilities: Vec<String>,
    /// Absolute path to the backend `config.json` (settings file). Backend only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_path: Option<String>,
    /// Per-connection temporary path for the documented defaults view. Backend only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defaults_path: Option<String>,
    /// Host UI prefs snapshot from that config (theme / fonts / webgl). Backend only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<HelloUi>,
    /// Effective user keybindings. Older peers omit this field.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub shortkeys: Vec<Shortkey>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shortkey {
    pub action: String,
    pub shortkey: String,
    #[serde(default)]
    pub when: Option<String>,
}

/// Optional native client layout details. Missing fields read as defaults for
/// workspaces saved by older clients and daemons.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceLayoutExtra {
    #[serde(default)]
    pub explorer_scroll: u32,
    #[serde(default)]
    pub sidebar_collapsed: bool,
    #[serde(default)]
    pub pinned: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub center: Option<LayoutNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LayoutNode {
    Split { axis: String, children: Vec<LayoutNode>, sizes: Vec<Option<f32>> },
    Tabs { tabs: Vec<u32>, active: u32 },
}

/// UI section mirrored from `config.json` → `Hello.ui`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelloUi {
    #[serde(default = "hello_ui_theme")]
    pub theme: String,
    #[serde(default = "hello_ui_palette")]
    pub palette: String,
    #[serde(default = "hello_ui_font", rename = "terminalFontSize")]
    pub terminal_font_size: u32,
    #[serde(default = "hello_ui_font", rename = "editorFontSize")]
    pub editor_font_size: u32,
    #[serde(default = "hello_ui_font_weight", rename = "fontWeight")]
    pub font_weight: u32,
    #[serde(default = "hello_ui_font_weight", rename = "monoFontWeight")]
    pub mono_font_weight: u32,
    #[serde(default, rename = "fontFamily")]
    pub font_family: String,
    #[serde(default, rename = "monoFontFamily")]
    pub mono_font_family: String,
    #[serde(default = "hello_ui_webgl")]
    pub webgl: bool,
    #[serde(default, rename = "showDotfiles")]
    pub show_dotfiles: bool,
    #[serde(default, rename = "showGitDirs")]
    pub show_git_dirs: bool,
    /// VS Code–style editor document map (minimap). Default off.
    #[serde(default, rename = "editorMinimap")]
    pub editor_minimap: bool,
    /// Soft-wrap long lines in the host editor (Fresh `editor.line_wrap`). Default on.
    #[serde(default = "hello_ui_line_wrap", rename = "editorLineWrap")]
    pub editor_line_wrap: bool,
}

fn hello_ui_theme() -> String {
    "system".to_owned()
}

fn hello_ui_palette() -> String {
    "primer".to_owned()
}

fn hello_ui_font() -> u32 {
    14
}

fn hello_ui_font_weight() -> u32 {
    400
}

fn hello_ui_webgl() -> bool {
    true
}

fn hello_ui_line_wrap() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PeerRole {
    Client,
    Backend,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FsKind {
    File,
    Dir,
    Symlink,
    Other,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FsEntry {
    pub name: String,
    /// Absolute path on the remote host (within the backend FS root).
    pub path: String,
    pub kind: FsKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// For a `symlink`, what it resolves to when the target is inside the FS
    /// sandbox. `dir` means the host may expand it like a folder. Absent for
    /// other kinds, dangling links, and links that leave the sandbox.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_kind: Option<FsKind>,
}

/// Summary of a live PTY inside a session (sent on attach).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyInfo {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
}

/// One open tab inside a workspace. The daemon stores this list; the host
/// restores its tab strip from it when the workspace is focused.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTab {
    pub kind: WorkspaceTabKind,
    pub title: String,
    /// Live PTY id when `kind` is `terminal`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_id: Option<String>,
    /// Absolute file path when `kind` is `editor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceTabKind {
    Terminal,
    Editor,
}

/// Workspace summary. Tab contents travel on switch / layout, not on every list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceInfo {
    pub id: String,
    pub name: String,
    /// Canonical project directory. Explorer re-roots here on switch.
    pub root: String,
    /// ADE session that owns this workspace's PTYs and layout blob.
    pub session_id: String,
    pub pty_count: u32,
    pub tab_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfo {
    pub id: String,
    pub pty_count: u32,
}

/// Open buffer summary for the thin ADE `scene` capability (not Fresh web-ui scene).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SceneBuffer {
    pub buffer_id: String,
    pub path: String,
    pub rev: u64,
    pub dirty: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
}

/// LSP diagnostic positions are zero-based UTF-16 columns, as in LSP.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BufferDiagnostic {
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub severity: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One changed path from `git status --porcelain`.
///
/// `path` is relative to the repository root. `xy` is the two-character
/// porcelain code (` M` unstaged, `M ` staged, `??` untracked, …).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GitFile {
    pub path: String,
    pub xy: String,
}

/// Top-level JSON envelope (one WebSocket text frame per message).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    /// Backend → client after a successful settings save.
    ConfigUpdated { shortkeys: Vec<Shortkey> },
    /// Client → backend. Required before PTY/FS ops when the backend demands a token.
    Auth {
        token: String,
    },
    AuthOk,
    AuthError {
        message: String,
    },
    Ping {
        nonce: u64,
    },
    Pong {
        nonce: u64,
    },
    /// Client → backend: create a detachable session.
    SessionCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layout: Option<String>,
    },
    /// Backend → client.
    SessionCreated {
        session_id: String,
    },
    /// Client → backend: attach to an existing session (PTYs keep running while detached).
    SessionAttach {
        session_id: String,
    },
    /// Backend → client after attach (includes live PTYs; scrollback follows as `pty_data`).
    SessionAttached {
        session_id: String,
        ptys: Vec<PtyInfo>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        layout: Option<String>,
    },
    /// Client → backend.
    SessionList,
    /// Backend → client.
    SessionListed {
        sessions: Vec<SessionInfo>,
    },
    /// Client → backend: persist UI layout JSON with the session.
    LayoutSet {
        layout: String,
    },
    /// Client → backend: list workspaces owned by this daemon.
    WorkspaceList,
    /// Backend → client.
    WorkspaceListed {
        workspaces: Vec<WorkspaceInfo>,
        /// Last workspace a client focused. Absent when none exist yet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focused_id: Option<String>,
    },
    /// Client → backend: create a workspace and its ADE session.
    ///
    /// Empty `name` uses the root directory's basename. Empty `root` uses the
    /// daemon FS root. Creating does not steal the connection's attached session.
    WorkspaceCreate {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        root: Option<String>,
    },
    /// Backend → client.
    WorkspaceCreated {
        workspace: WorkspaceInfo,
    },
    /// Client → backend.
    WorkspaceRename {
        workspace_id: String,
        name: String,
    },
    /// Backend → client.
    WorkspaceRenamed {
        workspace: WorkspaceInfo,
    },
    /// Client → backend: point an existing workspace at an existing directory.
    WorkspaceSetRoot {
        workspace_id: String,
        root: String,
    },
    /// Backend → client.
    WorkspaceRootSet {
        workspace: WorkspaceInfo,
    },
    /// Client → backend: drop a workspace and kill its PTYs.
    ///
    /// The last workspace cannot be closed.
    WorkspaceClose {
        workspace_id: String,
    },
    /// Backend → client. `focused_id` is the workspace the daemon suggests next
    /// when the closed one was focused.
    WorkspaceClosed {
        workspace_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        focused_id: Option<String>,
    },
    /// Client → backend: attach this connection to the workspace's session.
    ///
    /// Idle workspaces stay in the daemon; only the subscriber moves.
    WorkspaceSwitch {
        workspace_id: String,
    },
    /// Backend → client. Scrollback follows as `pty_data`, same as session attach.
    WorkspaceSwitched {
        workspace: WorkspaceInfo,
        tabs: Vec<WorkspaceTab>,
        #[serde(default)]
        active_tab: u32,
        ptys: Vec<PtyInfo>,
        /// Explorer directories the host last had open in this workspace.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        explorer_expanded: Vec<String>,
        #[serde(default)]
        extra: WorkspaceLayoutExtra,
    },
    /// Client → backend: replace the workspace's tab list (any workspace id,
    /// not only the one attached on this connection).
    WorkspaceLayoutSet {
        workspace_id: String,
        tabs: Vec<WorkspaceTab>,
        #[serde(default)]
        active_tab: u32,
        /// Open explorer directories (absolute paths). Replaces the stored set.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        explorer_expanded: Vec<String>,
        #[serde(default)]
        extra: WorkspaceLayoutExtra,
    },
    /// Client → backend: open a PTY in the attached session.
    PtyOpen {
        cols: u16,
        rows: u16,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<String>,
    },
    /// Backend → client: PTY ready.
    PtyOpened {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// Either direction: base64-encoded bytes (stdin ↔ stdout/stderr merged).
    PtyData {
        id: String,
        /// Standard base64 (no newlines).
        data: String,
    },
    /// Client → backend.
    PtyResize {
        id: String,
        cols: u16,
        rows: u16,
    },
    /// Client → backend: request close.
    PtyClose {
        id: String,
    },
    /// Backend → client: PTY ended.
    PtyClosed {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Client → backend: list a directory (read-only). Empty/`"."` → FS root.
    FsList {
        request_id: String,
        path: String,
    },
    /// Backend → client.
    FsListed {
        request_id: String,
        path: String,
        entries: Vec<FsEntry>,
    },
    /// Client → backend: authorize a directory for FS list/open (terminal cwd sync outside `--root`).
    FsAuthorize {
        request_id: String,
        path: String,
    },
    /// Backend → client after a successful authorize.
    FsAuthorized {
        request_id: String,
        path: String,
    },
    /// Client → backend: stat a path (read-only).
    FsStat {
        request_id: String,
        path: String,
    },
    /// Backend → client.
    FsStatResult {
        request_id: String,
        entry: FsEntry,
    },
    /// Client → backend: watch a path under the FS root (capability `fs`).
    FsWatch {
        request_id: String,
        path: String,
        #[serde(default)]
        recursive: bool,
    },
    /// Backend → client.
    FsWatchStarted {
        request_id: String,
        watch_id: String,
        path: String,
    },
    /// Client → backend.
    FsUnwatch {
        watch_id: String,
    },
    /// Backend → client: filesystem change under a watch.
    FsChanged {
        watch_id: String,
        paths: Vec<String>,
    },
    /// Client → backend: create an empty file or directory under `parent`.
    ///
    /// `name` must be a single path segment (no separators). `kind` is `file` or `dir`.
    FsCreate {
        request_id: String,
        parent: String,
        name: String,
        kind: FsKind,
    },
    /// Backend → client after a successful create.
    FsCreated {
        request_id: String,
        entry: FsEntry,
    },
    /// Client → backend: copy one or more paths into a destination directory.
    FsCopy {
        request_id: String,
        sources: Vec<String>,
        destination: String,
    },
    /// Backend → client after a successful copy.
    FsCopied {
        request_id: String,
        entries: Vec<FsEntry>,
    },
    /// Client → backend: move (cut+paste) one or more paths into a destination directory.
    FsMove {
        request_id: String,
        sources: Vec<String>,
        destination: String,
    },
    /// Backend → client after a successful move.
    FsMoved {
        request_id: String,
        entries: Vec<FsEntry>,
    },
    /// Client → backend: rename one file or folder within its parent directory.
    FsRename {
        request_id: String,
        path: String,
        name: String,
    },
    /// Backend → client after a successful rename.
    FsRenamed {
        request_id: String,
        entry: FsEntry,
    },
    /// Client → backend: permanently delete one or more paths under the FS sandbox.
    FsDelete {
        request_id: String,
        paths: Vec<String>,
    },
    /// Backend → client after a successful delete.
    FsDeleted {
        request_id: String,
        paths: Vec<String>,
    },
    /// Client → backend: open a path in the Fresh editor (capability `editor`).
    ///
    /// `path` may include a Fresh-style `:line` / `:line:col` suffix. Optional
    /// `cwd` resolves relative paths (terminal OSC 7 cwd), matching Fresh
    /// terminal-link resolution order.
    EditorOpen {
        request_id: String,
        path: String,
        #[serde(default)]
        preview: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<u32>,
    },
    /// Client → backend: Ctrl+click open — detect a path in `line_text` at
    /// `column` via Fresh `path_link::detect_link_at`, then open it.
    EditorOpenLink {
        request_id: String,
        /// Full line of terminal / editor text containing the path.
        line_text: String,
        /// 0-based character offset of the click within `line_text`.
        column: u32,
        #[serde(default)]
        preview: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
    },
    /// Backend → client: file opened; full text follows in [`Message::BufferSnapshot`].
    EditorOpened {
        request_id: String,
        buffer_id: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
        /// 1-based line to reveal in the host editor (from path suffix or link).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        /// 1-based column to reveal in the host editor.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        column: Option<u32>,
    },
    /// Backend → client: full buffer text.
    BufferSnapshot {
        buffer_id: String,
        rev: u64,
        text: String,
        path: String,
    },
    /// Client → backend: replace full buffer text when `base_rev` matches (CAS).
    BufferEdit {
        request_id: String,
        buffer_id: String,
        base_rev: u64,
        text: String,
    },
    /// Backend → client: edit applied (or conflict via `error`).
    BufferChanged {
        request_id: String,
        buffer_id: String,
        rev: u64,
    },
    /// Client → backend: save buffer to disk when `base_rev` matches.
    BufferSave {
        request_id: String,
        buffer_id: String,
        base_rev: u64,
    },
    /// Backend → client.
    BufferSaved {
        request_id: String,
        buffer_id: String,
        path: String,
        rev: u64,
    },
    /// Client → backend: fetch the latest LSP state for an open buffer.
    /// The backend also includes a snapshot when asynchronous formatting changed it.
    BufferLspGet {
        buffer_id: String,
        known_rev: u64,
    },
    /// Backend → client. An empty diagnostic list clears earlier problems.
    BufferLspState {
        buffer_id: String,
        rev: u64,
        diagnostics: Vec<BufferDiagnostic>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
    /// Client → backend: format a clean, revision-matched document.
    BufferFormat {
        request_id: String,
        buffer_id: String,
        base_rev: u64,
    },
    /// Backend → client: formatting completed or timed out; changed text is authoritative.
    BufferFormatted {
        request_id: String,
        buffer_id: String,
        rev: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        text: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<String>,
    },
    /// Client → backend: close an editor buffer.
    EditorClose {
        buffer_id: String,
    },
    /// Client → backend: `git status` for a workspace root (capability `git`).
    GitStatus {
        request_id: String,
        /// Daemon workspace id. Empty uses the daemon FS root.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        workspace_id: String,
        /// Active terminal cwd or selected file directory, when available.
        #[serde(default, skip_serializing_if = "String::is_empty")]
        directory: String,
    },
    /// Backend → client.
    GitStatusResult {
        request_id: String,
        repo: bool,
        root: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        branch: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        upstream: Option<String>,
        #[serde(default)]
        ahead: u32,
        #[serde(default)]
        behind: u32,
        #[serde(default)]
        files: Vec<GitFile>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    /// Client → backend: whole-file diff of `path` against `HEAD`.
    ///
    /// `path` is relative to the repository root (the `path` from [`GitFile`]).
    GitDiff {
        request_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        workspace_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        directory: String,
        path: String,
    },
    /// Backend → client: both sides of the file, or `binary` when either side
    /// is not text. `truncated` means a side was cut to the snapshot limit.
    GitDiffResult {
        request_id: String,
        path: String,
        #[serde(default)]
        old_text: String,
        #[serde(default)]
        new_text: String,
        #[serde(default)]
        binary: bool,
        #[serde(default)]
        truncated: bool,
    },
    /// Client → backend: `git add` (`stage`) or `git restore --staged`.
    GitStage {
        request_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        workspace_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        directory: String,
        paths: Vec<String>,
        stage: bool,
    },
    /// Client → backend: `git commit -m`.
    GitCommit {
        request_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        workspace_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        directory: String,
        message: String,
    },
    /// Client → backend: `git pull --no-edit`.
    GitPull {
        request_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        workspace_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        directory: String,
    },
    /// Client → backend: `git push`.
    GitPush {
        request_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        workspace_id: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        directory: String,
    },
    /// Backend → client: result of stage, commit, pull, or push.
    GitOpResult {
        request_id: String,
        ok: bool,
        output: String,
    },
    /// Client → backend: open a sandboxed path with the OS file handler.
    FsOpenExternal {
        request_id: String,
        path: String,
    },
    /// Backend → client after the opener was spawned.
    FsOpened {
        request_id: String,
        message: String,
    },
    /// Client → backend: thin ADE scene snapshot (capability `scene`).
    SceneGet {
        request_id: String,
    },
    /// Backend → client: open-buffer chrome (not Fresh `--web` cell scene).
    SceneSnapshot {
        request_id: String,
        buffers: Vec<SceneBuffer>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        active_buffer_id: Option<String>,
        /// Opaque extension bag for future fields.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        extra: Option<JsonValue>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(String),
    #[error("missing capability: {0}")]
    MissingCapability(String),
    #[error("invalid message json: {0}")]
    Json(#[from] serde_json::Error),
}

impl Hello {
    pub fn backend(implementation: impl Into<String>, capabilities: Vec<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            role: PeerRole::Backend,
            implementation: implementation.into(),
            capabilities,
            config_path: None,
            defaults_path: None,
            ui: None,
            shortkeys: Vec::new(),
        }
    }

    pub fn client(implementation: impl Into<String>, capabilities: Vec<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            role: PeerRole::Client,
            implementation: implementation.into(),
            capabilities,
            config_path: None,
            defaults_path: None,
            ui: None,
            shortkeys: Vec::new(),
        }
    }

    pub fn default_backend_caps() -> Vec<String> {
        vec![
            CAP_PING.to_owned(),
            CAP_PTY.to_owned(),
            CAP_FS.to_owned(),
            CAP_SESSION.to_owned(),
            CAP_WORKSPACE.to_owned(),
            CAP_WORKSPACE_SET_ROOT.to_owned(),
            CAP_EDITOR.to_owned(),
            CAP_LSP.to_owned(),
            CAP_SCENE.to_owned(),
            CAP_GIT.to_owned(),
        ]
    }

    pub fn default_client_caps() -> Vec<String> {
        vec![
            CAP_PING.to_owned(),
            CAP_PTY.to_owned(),
            CAP_FS.to_owned(),
            CAP_SESSION.to_owned(),
            CAP_WORKSPACE.to_owned(),
            CAP_EDITOR.to_owned(),
            CAP_LSP.to_owned(),
            CAP_SCENE.to_owned(),
            CAP_GIT.to_owned(),
        ]
    }
}

impl Message {
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    pub fn from_json(s: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_includes_editor_and_scene() {
        let hello = Hello::backend("fresh-gui/test", Hello::default_backend_caps());
        let json = Message::Hello(hello).to_json().unwrap();
        assert!(json.contains("\"editor\""));
        assert!(json.contains("\"scene\""));
        assert!(json.contains("\"workspace\""));
        assert!(json.contains("0.4.0"));
    }

    #[test]
    fn workspace_messages_roundtrip() {
        let info = WorkspaceInfo {
            id: "w1".into(),
            name: "alpha".into(),
            root: "/tmp/alpha".into(),
            session_id: "s1".into(),
            pty_count: 1,
            tab_count: 2,
        };
        let set_root = Message::WorkspaceSetRoot {
            workspace_id: info.id.clone(),
            root: "/tmp/beta".into(),
        };
        assert_eq!(Message::from_json(&set_root.to_json().unwrap()).unwrap(), set_root);
        let root_set = Message::WorkspaceRootSet { workspace: info.clone() };
        assert_eq!(Message::from_json(&root_set.to_json().unwrap()).unwrap(), root_set);
        let listed = Message::WorkspaceListed {
            workspaces: vec![info.clone()],
            focused_id: Some("w1".into()),
        };
        assert_eq!(
            Message::from_json(&listed.to_json().unwrap()).unwrap(),
            listed
        );

        let switched = Message::WorkspaceSwitched {
            workspace: info,
            tabs: vec![
                WorkspaceTab {
                    kind: WorkspaceTabKind::Terminal,
                    title: "Terminal 1".into(),
                    pty_id: Some("p1".into()),
                    path: None,
                },
                WorkspaceTab {
                    kind: WorkspaceTabKind::Editor,
                    title: "main.rs".into(),
                    pty_id: None,
                    path: Some("/tmp/alpha/main.rs".into()),
                },
            ],
            active_tab: 1,
            ptys: vec![PtyInfo {
                id: "p1".into(),
                cols: 80,
                rows: 24,
            }],
            explorer_expanded: vec!["/tmp/alpha/src".into()],
            extra: WorkspaceLayoutExtra::default(),
        };
        let json = switched.to_json().unwrap();
        assert!(json.contains("\"workspace_switched\""));
        assert_eq!(Message::from_json(&json).unwrap(), switched);

        let layout = Message::WorkspaceLayoutSet {
            workspace_id: "w1".into(),
            tabs: vec![WorkspaceTab {
                kind: WorkspaceTabKind::Editor,
                title: "lib.rs".into(),
                pty_id: None,
                path: Some("/tmp/alpha/lib.rs".into()),
            }],
            active_tab: 0,
            explorer_expanded: Vec::new(),
            extra: WorkspaceLayoutExtra::default(),
        };
        let json = layout.to_json().unwrap();
        assert!(!json.contains("explorer_expanded"));
        assert_eq!(Message::from_json(&json).unwrap(), layout);

        // A 0.4.0 peer that predates the field still parses.
        let old = r#"{"type":"workspace_layout_set","workspace_id":"w1","tabs":[],"active_tab":0}"#;
        assert!(matches!(
            Message::from_json(old).unwrap(),
            Message::WorkspaceLayoutSet { explorer_expanded, .. } if explorer_expanded.is_empty()
        ));
    }

    #[test]
    fn buffer_edit_save_roundtrips() {
        let edit = Message::BufferEdit {
            request_id: "r1".into(),
            buffer_id: "1".into(),
            base_rev: 0,
            text: "hello\n".into(),
        };
        assert_eq!(Message::from_json(&edit.to_json().unwrap()).unwrap(), edit);

        let saved = Message::BufferSaved {
            request_id: "r2".into(),
            buffer_id: "1".into(),
            path: "/tmp/a.rs".into(),
            rev: 1,
        };
        assert_eq!(
            Message::from_json(&saved.to_json().unwrap()).unwrap(),
            saved
        );
    }

    #[test]
    fn scene_snapshot_roundtrips() {
        let msg = Message::SceneSnapshot {
            request_id: "s1".into(),
            buffers: vec![SceneBuffer {
                buffer_id: "1".into(),
                path: "/tmp/a.rs".into(),
                rev: 2,
                dirty: true,
                language: Some("rust".into()),
            }],
            active_buffer_id: Some("1".into()),
            extra: None,
        };
        assert_eq!(Message::from_json(&msg.to_json().unwrap()).unwrap(), msg);
    }

    #[test]
    fn fs_watch_roundtrips() {
        let start = Message::FsWatch {
            request_id: "w1".into(),
            path: "".into(),
            recursive: true,
        };
        assert_eq!(
            Message::from_json(&start.to_json().unwrap()).unwrap(),
            start
        );
        let changed = Message::FsChanged {
            watch_id: "w".into(),
            paths: vec!["/tmp/a".into()],
        };
        assert_eq!(
            Message::from_json(&changed.to_json().unwrap()).unwrap(),
            changed
        );
    }

    #[test]
    fn fs_mutate_roundtrips() {
        let create = Message::FsCreate {
            request_id: "c1".into(),
            parent: "".into(),
            name: "a.txt".into(),
            kind: FsKind::File,
        };
        assert_eq!(
            Message::from_json(&create.to_json().unwrap()).unwrap(),
            create
        );
        let copy = Message::FsCopy {
            request_id: "c2".into(),
            sources: vec!["/tmp/a.txt".into()],
            destination: "/tmp/out".into(),
        };
        assert_eq!(Message::from_json(&copy.to_json().unwrap()).unwrap(), copy);
        let mv = Message::FsMove {
            request_id: "c3".into(),
            sources: vec!["/tmp/a.txt".into()],
            destination: "/tmp/out".into(),
        };
        assert_eq!(Message::from_json(&mv.to_json().unwrap()).unwrap(), mv);
        let rename = Message::FsRename {
            request_id: "r1".into(),
            path: "/tmp/a.txt".into(),
            name: "b.txt".into(),
        };
        assert_eq!(Message::from_json(&rename.to_json().unwrap()).unwrap(), rename);
        let del = Message::FsDelete {
            request_id: "c4".into(),
            paths: vec!["/tmp/a.txt".into()],
        };
        assert_eq!(Message::from_json(&del.to_json().unwrap()).unwrap(), del);
    }

    #[test]
    fn editor_open_link_roundtrips() {
        let open = Message::EditorOpen {
            request_id: "e1".into(),
            path: "src/main.rs:10:2".into(),
            preview: true,
            cwd: Some("/tmp/proj".into()),
            line: None,
            column: None,
        };
        assert_eq!(Message::from_json(&open.to_json().unwrap()).unwrap(), open);

        let link = Message::EditorOpenLink {
            request_id: "e2".into(),
            line_text: "error: src/lib.rs:1:1: boom".into(),
            column: 7,
            preview: true,
            cwd: Some("/tmp/proj".into()),
        };
        let json = link.to_json().unwrap();
        assert!(json.contains("\"editor_open_link\""));
        assert_eq!(Message::from_json(&json).unwrap(), link);

        let opened = Message::EditorOpened {
            request_id: "e2".into(),
            buffer_id: "1".into(),
            path: "/tmp/proj/src/lib.rs".into(),
            language: Some("rust".into()),
            line: Some(1),
            column: Some(1),
        };
        assert_eq!(
            Message::from_json(&opened.to_json().unwrap()).unwrap(),
            opened
        );
    }

    #[test]
    fn git_status_roundtrips() {
        let status = Message::GitStatusResult {
            request_id: "g1".into(),
            repo: true,
            root: "/tmp/proj".into(),
            branch: "main".into(),
            upstream: Some("origin/main".into()),
            ahead: 1,
            behind: 0,
            files: vec![GitFile {
                path: "src/main.rs".into(),
                xy: " M".into(),
            }],
            detail: None,
        };
        assert_eq!(
            Message::from_json(&status.to_json().unwrap()).unwrap(),
            status
        );
        let request = Message::GitStatus {
            request_id: "g2".into(),
            workspace_id: "w1".into(),
            directory: "/tmp/other-repo".into(),
        };
        assert_eq!(
            Message::from_json(&request.to_json().unwrap()).unwrap(),
            request
        );
        assert_eq!(
            Message::from_json(r#"{"type":"git_status","request_id":"g3","workspace_id":"w1"}"#).unwrap(),
            Message::GitStatus {
                request_id: "g3".into(),
                workspace_id: "w1".into(),
                directory: String::new()
            }
        );
    }

    #[test]
    fn lsp_diagnostics_and_format_roundtrip() {
        let state = Message::BufferLspState {
            buffer_id: "42".into(),
            rev: 3,
            diagnostics: vec![BufferDiagnostic {
                start_line: 1,
                start_character: 3,
                end_line: 1,
                end_character: 5,
                severity: "error".into(),
                message: "bad value".into(),
                source: Some("Ruff".into()),
            }],
            status: None,
            text: None,
        };
        assert_eq!(Message::from_json(&state.to_json().unwrap()).unwrap(), state);
        let formatted = Message::BufferFormatted {
            request_id: "format-1".into(),
            buffer_id: "42".into(),
            rev: 4,
            text: Some("value = 1\n".into()),
            status: None,
        };
        assert_eq!(Message::from_json(&formatted.to_json().unwrap()).unwrap(), formatted);
    }
}

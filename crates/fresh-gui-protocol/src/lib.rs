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
pub const CAP_EDITOR: &str = "editor";
pub const CAP_SCENE: &str = "scene";

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
    /// Host UI prefs snapshot from that config (theme / fonts / webgl). Backend only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui: Option<HelloUi>,
}

/// UI section mirrored from `config.json` → `Hello.ui`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HelloUi {
    #[serde(default = "hello_ui_theme")]
    pub theme: String,
    #[serde(default = "hello_ui_font", rename = "terminalFontSize")]
    pub terminal_font_size: u32,
    #[serde(default = "hello_ui_font", rename = "editorFontSize")]
    pub editor_font_size: u32,
    #[serde(default = "hello_ui_webgl")]
    pub webgl: bool,
    #[serde(default, rename = "showDotfiles")]
    pub show_dotfiles: bool,
    #[serde(default, rename = "showGitDirs")]
    pub show_git_dirs: bool,
}

fn hello_ui_theme() -> String {
    "system".to_owned()
}

fn hello_ui_font() -> u32 {
    14
}

fn hello_ui_webgl() -> bool {
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
}

/// Summary of a live PTY inside a session (sent on attach).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PtyInfo {
    pub id: String,
    pub cols: u16,
    pub rows: u16,
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

/// Top-level JSON envelope (one WebSocket text frame per message).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
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
    /// Client → backend: close an editor buffer.
    EditorClose {
        buffer_id: String,
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
            ui: None,
        }
    }

    pub fn client(implementation: impl Into<String>, capabilities: Vec<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            role: PeerRole::Client,
            implementation: implementation.into(),
            capabilities,
            config_path: None,
            ui: None,
        }
    }

    pub fn default_backend_caps() -> Vec<String> {
        vec![
            CAP_PING.to_owned(),
            CAP_PTY.to_owned(),
            CAP_FS.to_owned(),
            CAP_SESSION.to_owned(),
            CAP_EDITOR.to_owned(),
            CAP_SCENE.to_owned(),
        ]
    }

    pub fn default_client_caps() -> Vec<String> {
        vec![
            CAP_PING.to_owned(),
            CAP_PTY.to_owned(),
            CAP_FS.to_owned(),
            CAP_SESSION.to_owned(),
            CAP_EDITOR.to_owned(),
            CAP_SCENE.to_owned(),
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
        assert!(json.contains("0.4.0"));
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
        assert_eq!(Message::from_json(&saved.to_json().unwrap()).unwrap(), saved);
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
        assert_eq!(Message::from_json(&start.to_json().unwrap()).unwrap(), start);
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
}

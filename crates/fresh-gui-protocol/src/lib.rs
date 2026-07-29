//! Shared protocol types for fresh-gui (host ↔ remote).
//!
//! PTY-first ADE protocol. Phase 2: sessions. Phase 3a: optional `editor` open/snapshot.

use serde::{Deserialize, Serialize};

/// Protocol version negotiated in [`Hello`].
pub const PROTOCOL_VERSION: &str = "0.3.0";

pub const CAP_PING: &str = "ping";
pub const CAP_PTY: &str = "pty";
pub const CAP_FS: &str = "fs";
pub const CAP_SESSION: &str = "session";
pub const CAP_EDITOR: &str = "editor";

/// First message after WebSocket connect. Client sends; backend replies with its own.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hello {
    pub protocol_version: String,
    pub role: PeerRole,
    /// Free-form implementation id, e.g. `fresh-gui-backend/2026.728.1`.
    pub implementation: String,
    pub capabilities: Vec<String>,
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

/// Top-level JSON envelope (one WebSocket text frame per message).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
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
    /// Client → backend: open a path in the Fresh editor (capability `editor`).
    EditorOpen {
        request_id: String,
        path: String,
        #[serde(default)]
        preview: bool,
    },
    /// Backend → client: file opened; full text follows in [`Message::BufferSnapshot`].
    EditorOpened {
        request_id: String,
        buffer_id: String,
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        language: Option<String>,
    },
    /// Backend → client: full buffer text (Phase 3a MVP; diffs later).
    BufferSnapshot {
        buffer_id: String,
        rev: u64,
        text: String,
        path: String,
    },
    /// Client → backend: close an editor buffer.
    EditorClose {
        buffer_id: String,
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
        }
    }

    pub fn client(implementation: impl Into<String>, capabilities: Vec<String>) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            role: PeerRole::Client,
            implementation: implementation.into(),
            capabilities,
        }
    }

    pub fn default_backend_caps() -> Vec<String> {
        vec![
            CAP_PING.to_owned(),
            CAP_PTY.to_owned(),
            CAP_FS.to_owned(),
            CAP_SESSION.to_owned(),
            CAP_EDITOR.to_owned(),
        ]
    }

    pub fn default_client_caps() -> Vec<String> {
        vec![
            CAP_PING.to_owned(),
            CAP_PTY.to_owned(),
            CAP_FS.to_owned(),
            CAP_SESSION.to_owned(),
            CAP_EDITOR.to_owned(),
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
    fn hello_includes_editor_cap() {
        let hello = Hello::backend("fresh-gui-backend/test", Hello::default_backend_caps());
        let json = Message::Hello(hello).to_json().unwrap();
        assert!(json.contains("\"editor\""));
        assert!(json.contains("\"session\""));
        assert!(json.contains("0.3.0"));
    }

    #[test]
    fn session_attached_roundtrips() {
        let msg = Message::SessionAttached {
            session_id: "s1".into(),
            ptys: vec![PtyInfo {
                id: "p1".into(),
                cols: 80,
                rows: 24,
            }],
            layout: Some(r#"{"split":"vertical"}"#.into()),
        };
        let back = Message::from_json(&msg.to_json().unwrap()).unwrap();
        assert_eq!(back, msg);
    }

    #[test]
    fn editor_open_snapshot_roundtrips() {
        let open = Message::EditorOpen {
            request_id: "r1".into(),
            path: "/tmp/a.rs".into(),
            preview: false,
        };
        assert_eq!(Message::from_json(&open.to_json().unwrap()).unwrap(), open);

        let snap = Message::BufferSnapshot {
            buffer_id: "1".into(),
            rev: 0,
            text: "fn main() {}\n".into(),
            path: "/tmp/a.rs".into(),
        };
        assert_eq!(Message::from_json(&snap.to_json().unwrap()).unwrap(), snap);
    }

    #[test]
    fn pty_opened_includes_size() {
        let msg = Message::PtyOpened {
            id: "p1".into(),
            cols: 120,
            rows: 40,
        };
        let back = Message::from_json(&msg.to_json().unwrap()).unwrap();
        assert_eq!(back, msg);
    }
}

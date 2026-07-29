//! Shared protocol types for fresh-gui (host ↔ remote).
//!
//! PTY-first ADE protocol (D1 = B). Wire: JSON text frames over WebSocket.
//! Fresh Editor/scene is a later optional capability — see `docs/DESIGN.md`.

use serde::{Deserialize, Serialize};

/// Protocol version negotiated in [`Hello`].
pub const PROTOCOL_VERSION: &str = "0.1.0";

pub const CAP_PING: &str = "ping";
pub const CAP_PTY: &str = "pty";

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

/// Top-level JSON envelope (one WebSocket text frame per message).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    /// Client → backend. Required before PTY ops when the backend demands a token.
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
    /// Client → backend: open a PTY.
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
        vec![CAP_PING.to_owned(), CAP_PTY.to_owned()]
    }

    pub fn default_client_caps() -> Vec<String> {
        vec![CAP_PING.to_owned(), CAP_PTY.to_owned()]
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
    fn hello_roundtrips_json() {
        let hello = Hello::backend("fresh-gui-backend/test", Hello::default_backend_caps());
        let msg = Message::Hello(hello.clone());
        let json = msg.to_json().unwrap();
        let back = Message::from_json(&json).unwrap();
        assert_eq!(back, Message::Hello(hello));
    }

    #[test]
    fn pty_data_roundtrips() {
        let msg = Message::PtyData {
            id: "p1".into(),
            data: "aGVsbG8=".into(),
        };
        let back = Message::from_json(&msg.to_json().unwrap()).unwrap();
        assert_eq!(back, msg);
    }
}

//! Shared protocol types for fresh-gui (host ↔ remote).
//!
//! PTY-first ADE protocol (D1 = B). Fresh Editor/scene is a later optional
//! capability — see `docs/DESIGN.md` §5 / §10.

use serde::{Deserialize, Serialize};

/// Protocol major.minor negotiated in [`Hello`].
pub const PROTOCOL_VERSION: &str = "0.1.0";

/// First message after connect (either direction may send; backend replies).
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

/// Top-level envelope (placeholder until D1 is decided).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Hello(Hello),
    Ping { nonce: u64 },
    Pong { nonce: u64 },
    Error { code: String, message: String },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(String),
    #[error("missing capability: {0}")]
    MissingCapability(String),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hello_roundtrips_json() {
        let hello = Hello::backend("fresh-gui-backend/test", vec!["ping".into()]);
        let msg = Message::Hello(hello.clone());
        let json = serde_json::to_string(&msg).unwrap();
        let back: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(back, Message::Hello(hello));
    }
}

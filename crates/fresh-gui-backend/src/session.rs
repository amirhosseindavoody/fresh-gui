//! Detachable sessions: PTYs survive WebSocket disconnect.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use fresh_gui_protocol::{Message, PtyInfo, SessionInfo};
use tokio::sync::{mpsc, Mutex};
use tracing::debug;
use uuid::Uuid;

use crate::pty::PtySession;

const SCROLLBACK_MAX: usize = 64 * 1024;

struct PtySlot {
    session: PtySession,
    cols: u16,
    rows: u16,
    scrollback: VecDeque<u8>,
}

pub struct Session {
    pub id: String,
    layout: Option<String>,
    ptys: HashMap<String, PtySlot>,
    /// Live subscriber for outbound protocol messages (one attached client).
    subscriber: Option<mpsc::UnboundedSender<Message>>,
}

impl Session {
    fn new(id: String, layout: Option<String>) -> Self {
        Self {
            id,
            layout,
            ptys: HashMap::new(),
            subscriber: None,
        }
    }

    fn info(&self) -> SessionInfo {
        SessionInfo {
            id: self.id.clone(),
            pty_count: self.ptys.len() as u32,
        }
    }

    fn pty_infos(&self) -> Vec<PtyInfo> {
        self.ptys
            .values()
            .map(|p| PtyInfo {
                id: p.session.id().to_owned(),
                cols: p.cols,
                rows: p.rows,
            })
            .collect()
    }

    fn push_scrollback(slot: &mut PtySlot, bytes: &[u8]) {
        for &b in bytes {
            if slot.scrollback.len() >= SCROLLBACK_MAX {
                slot.scrollback.pop_front();
            }
            slot.scrollback.push_back(b);
        }
    }

    fn emit(&self, msg: Message) {
        if let Some(tx) = &self.subscriber {
            let _ = tx.send(msg);
        }
    }
}

#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<Mutex<HashMap<String, Session>>>,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn create(&self, layout: Option<String>) -> String {
        let id = Uuid::new_v4().to_string();
        let mut guard = self.inner.lock().await;
        guard.insert(id.clone(), Session::new(id.clone(), layout));
        id
    }

    pub async fn list(&self) -> Vec<SessionInfo> {
        let guard = self.inner.lock().await;
        let mut v: Vec<_> = guard.values().map(|s| s.info()).collect();
        v.sort_by(|a, b| a.id.cmp(&b.id));
        v
    }

    /// Attach `out_tx` as the sole subscriber. Returns pty infos + layout, and
    /// queued scrollback replay messages (caller should deliver after SessionAttached).
    pub async fn attach(
        &self,
        session_id: &str,
        out_tx: mpsc::UnboundedSender<Message>,
    ) -> Result<(Vec<PtyInfo>, Option<String>, Vec<Message>)> {
        let mut guard = self.inner.lock().await;
        let session = guard
            .get_mut(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        session.subscriber = Some(out_tx);
        let ptys = session.pty_infos();
        let layout = session.layout.clone();
        let mut replay = Vec::new();
        for slot in session.ptys.values() {
            if slot.scrollback.is_empty() {
                continue;
            }
            let bytes: Vec<u8> = slot.scrollback.iter().copied().collect();
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            replay.push(Message::PtyData {
                id: slot.session.id().to_owned(),
                data,
            });
        }
        Ok((ptys, layout, replay))
    }

    pub async fn detach_subscriber(&self, session_id: &str) {
        let mut guard = self.inner.lock().await;
        if let Some(session) = guard.get_mut(session_id) {
            session.subscriber = None;
            debug!(%session_id, "session detached");
        }
    }

    pub async fn set_layout(&self, session_id: &str, layout: String) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let session = guard
            .get_mut(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        session.layout = Some(layout);
        Ok(())
    }

    pub async fn open_pty(
        &self,
        session_id: &str,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
        shell: Option<String>,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pty = PtySession::spawn(id.clone(), cols, rows, cwd, shell, raw_tx)?;

        {
            let mut guard = self.inner.lock().await;
            let session = guard
                .get_mut(session_id)
                .with_context(|| format!("unknown session {session_id}"))?;
            session.ptys.insert(
                id.clone(),
                PtySlot {
                    session: pty,
                    cols,
                    rows,
                    scrollback: VecDeque::new(),
                },
            );
        }

        let store = self.clone();
        let sid = session_id.to_owned();
        let pid = id.clone();
        tokio::spawn(async move {
            while let Some(bytes) = raw_rx.recv().await {
                store.on_pty_output(&sid, &pid, &bytes).await;
            }
            store.on_pty_eof(&sid, &pid).await;
        });

        Ok(id)
    }

    async fn on_pty_output(&self, session_id: &str, pty_id: &str, bytes: &[u8]) {
        let mut guard = self.inner.lock().await;
        let Some(session) = guard.get_mut(session_id) else {
            return;
        };
        let Some(slot) = session.ptys.get_mut(pty_id) else {
            return;
        };
        Session::push_scrollback(slot, bytes);
        let data = base64::engine::general_purpose::STANDARD.encode(bytes);
        session.emit(Message::PtyData {
            id: pty_id.to_owned(),
            data,
        });
    }

    async fn on_pty_eof(&self, session_id: &str, pty_id: &str) {
        let mut guard = self.inner.lock().await;
        let Some(session) = guard.get_mut(session_id) else {
            return;
        };
        session.ptys.remove(pty_id);
        session.emit(Message::PtyClosed {
            id: pty_id.to_owned(),
            reason: Some("eof".into()),
        });
    }

    pub async fn write_pty(&self, session_id: &str, pty_id: &str, data: &[u8]) -> Result<()> {
        let guard = self.inner.lock().await;
        let session = guard
            .get(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let slot = session
            .ptys
            .get(pty_id)
            .with_context(|| format!("unknown pty {pty_id}"))?;
        slot.session.write_all(data)
    }

    pub async fn resize_pty(
        &self,
        session_id: &str,
        pty_id: &str,
        cols: u16,
        rows: u16,
    ) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let session = guard
            .get_mut(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        let slot = session
            .ptys
            .get_mut(pty_id)
            .with_context(|| format!("unknown pty {pty_id}"))?;
        slot.session.resize(cols, rows)?;
        slot.cols = cols;
        slot.rows = rows;
        Ok(())
    }

    pub async fn close_pty(&self, session_id: &str, pty_id: &str) -> Result<()> {
        let mut guard = self.inner.lock().await;
        let session = guard
            .get_mut(session_id)
            .with_context(|| format!("unknown session {session_id}"))?;
        session.ptys.remove(pty_id);
        session.emit(Message::PtyClosed {
            id: pty_id.to_owned(),
            reason: Some("client_close".into()),
        });
        Ok(())
    }
}

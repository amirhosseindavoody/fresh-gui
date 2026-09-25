//! Detachable sessions: PTYs survive WebSocket disconnect.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine;
use fresh_gui_protocol::{Message, PtyInfo, SessionInfo};
use tokio::sync::{Mutex, mpsc};
use tracing::debug;
use uuid::Uuid;

use crate::pty::PtySession;

const SCROLLBACK_MAX: usize = 64 * 1024;

struct PtySlot {
    session: PtySession,
    cols: u16,
    rows: u16,
    scrollback: VecDeque<u8>,
    /// DEC private modes currently set by the foreground application. Scrollback
    /// can be truncated before the mode-setting escape, so keep these across
    /// client detach/reattach alongside the state at the replay boundary.
    active_modes: std::collections::BTreeSet<u16>,
    mode_carry: Vec<u8>,
    /// Mode state immediately before the oldest retained scrollback byte.
    scrollback_modes: std::collections::BTreeSet<u16>,
    scrollback_mode_carry: Vec<u8>,
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
        let mut evicted = Vec::new();
        for &b in bytes {
            if slot.scrollback.len() >= SCROLLBACK_MAX {
                if let Some(evicted_byte) = slot.scrollback.pop_front() {
                    evicted.push(evicted_byte);
                }
            }
            slot.scrollback.push_back(b);
        }
        if !evicted.is_empty() {
            track_dec_modes(
                &mut slot.scrollback_mode_carry,
                &mut slot.scrollback_modes,
                &evicted,
            );
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

    pub async fn pty_count(&self, session_id: &str) -> u32 {
        let guard = self.inner.lock().await;
        guard
            .get(session_id)
            .map(|s| s.ptys.len() as u32)
            .unwrap_or(0)
    }

    /// Kill every PTY and drop the session. Used when a workspace is closed.
    pub async fn destroy(&self, session_id: &str) -> Result<()> {
        let session = {
            let mut guard = self.inner.lock().await;
            guard
                .remove(session_id)
                .with_context(|| format!("unknown session {session_id}"))?
        };
        for slot in session.ptys.into_values() {
            slot.session.kill();
        }
        Ok(())
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
            if slot.scrollback.is_empty() && slot.scrollback_modes.is_empty() {
                continue;
            }
            let mut bytes = Vec::new();
            // Re-establish modes active at the replay boundary before replaying
            // retained output. This reconstructs the alternate screen without
            // resetting it after its contents have been replayed.
            for mode in &slot.scrollback_modes {
                bytes.extend_from_slice(format!("\x1b[?{mode}h").as_bytes());
            }
            // If the eviction boundary split a DECSET/DECRST sequence, put
            // its evicted prefix back so the retained suffix remains parseable.
            bytes.extend_from_slice(&slot.scrollback_mode_carry);
            bytes.extend(slot.scrollback.iter().copied());
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
        config: &crate::config::Config,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let (raw_tx, mut raw_rx) = mpsc::unbounded_channel::<Vec<u8>>();
        let pty = PtySession::spawn(id.clone(), cols, rows, cwd, shell, config, raw_tx)?;

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
                    active_modes: Default::default(),
                    mode_carry: Vec::new(),
                    scrollback_modes: Default::default(),
                    scrollback_mode_carry: Vec::new(),
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
        track_dec_modes(&mut slot.mode_carry, &mut slot.active_modes, bytes);
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
        // Already removed by `close_pty` (client kill) — don't emit a second closed event.
        if session.ptys.remove(pty_id).is_none() {
            return;
        }
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
        let Some(slot) = session.ptys.remove(pty_id) else {
            anyhow::bail!("unknown pty {pty_id}");
        };
        // Kill the shell before dropping the slot (Fresh TerminalManager::close → shutdown).
        slot.session.kill();
        session.emit(Message::PtyClosed {
            id: pty_id.to_owned(),
            reason: Some("client_close".into()),
        });
        Ok(())
    }
}

/// Track DEC private modes independently of the client's terminal grid. The
/// parser is deliberately limited to CSI ?...h/l sequences and retains a short
/// incomplete suffix so escape sequences split between PTY reads are handled.
fn track_dec_modes(carry: &mut Vec<u8>, modes: &mut std::collections::BTreeSet<u16>, bytes: &[u8]) {
    let mut input = std::mem::take(carry);
    input.extend_from_slice(bytes);
    let mut i = 0;
    while i < input.len() {
        const PREFIX: &[u8] = b"\x1b[?";
        let remaining = &input[i..];
        if remaining.len() < PREFIX.len() && PREFIX.starts_with(remaining) {
            carry.extend_from_slice(remaining);
            break;
        }
        if !remaining.starts_with(PREFIX) {
            i += 1;
            continue;
        }
        let start = i + 3;
        let Some(end) = input[start..].iter().position(|byte| (0x40..=0x7e).contains(byte)) else {
            // A valid CSI is short. Keep only the unfinished suffix.
            if input.len() - i <= 128 {
                carry.extend_from_slice(&input[i..]);
            }
            break;
        };
        let final_index = start + end;
        let final_byte = input[final_index];
        if matches!(final_byte, b'h' | b'l')
            && let Ok(params) = std::str::from_utf8(&input[start..final_index])
        {
            for mode in params.split(';').filter_map(|value| value.parse::<u16>().ok()) {
                if matches!(mode, 1 | 6 | 47 | 1000 | 1002 | 1003 | 1005 | 1006 | 1007 | 1015 | 1047 | 1049 | 2004 | 2026) {
                    if matches!(mode, 47 | 1047 | 1049) {
                        // These are aliases for the same alternate screen in
                        // the client terminal implementation. Keep one bit so
                        // replay cannot enter the alternate grid multiple times.
                        modes.remove(&47);
                        modes.remove(&1047);
                        modes.remove(&1049);
                        if final_byte == b'h' {
                            modes.insert(1049);
                        }
                    } else if final_byte == b'h' {
                        modes.insert(mode);
                    } else {
                        modes.remove(&mode);
                    }
                }
            }
        }
        i = final_index + 1;
    }
}

#[cfg(test)]
mod terminal_mode_tests {
    use super::track_dec_modes;
    use std::collections::BTreeSet;

    #[test]
    fn tracks_modes_across_output_chunks_and_replays_current_state() {
        let mut carry = Vec::new();
        let mut modes = BTreeSet::new();
        track_dec_modes(&mut carry, &mut modes, b"hello\x1b[?100");
        assert!(modes.is_empty());
        track_dec_modes(&mut carry, &mut modes, b"6h\x1b[?1049h");
        assert!(modes.contains(&1006));
        assert!(modes.contains(&1049));
        track_dec_modes(&mut carry, &mut modes, b"\x1b[?1006l");
        assert!(!modes.contains(&1006));
        assert!(modes.contains(&1049));
    }
}

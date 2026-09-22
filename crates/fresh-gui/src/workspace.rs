//! Named workspaces inside the singleton daemon.
//!
//! One daemon process (one user) holds every workspace. Each workspace owns an
//! ADE session: PTYs, scrollback, and the tab list the host restores. Switching
//! moves the WebSocket subscriber; it does not stop the other sessions.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use fresh_gui_protocol::{WorkspaceInfo, WorkspaceTab, WorkspaceTabKind};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::session::SessionStore;

const MAX_TABS: usize = 64;
const MAX_NAME_CHARS: usize = 64;
const MAX_TITLE_CHARS: usize = 120;

struct Record {
    id: String,
    name: String,
    root: String,
    session_id: String,
    tabs: Vec<WorkspaceTab>,
    active_tab: u32,
}

impl Record {
    fn info(&self, pty_count: u32) -> WorkspaceInfo {
        WorkspaceInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            root: self.root.clone(),
            session_id: self.session_id.clone(),
            pty_count,
            tab_count: self.tabs.len() as u32,
        }
    }
}

struct Inner {
    order: Vec<String>,
    by_id: HashMap<String, Record>,
    by_session: HashMap<String, String>,
    focused_id: Option<String>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            order: Vec::new(),
            by_id: HashMap::new(),
            by_session: HashMap::new(),
            focused_id: None,
        }
    }
}

/// What [`WorkspaceStore::focus`] returns for a switch.
#[derive(Debug)]
pub struct FocusedWorkspace {
    pub info: WorkspaceInfo,
    pub session_id: String,
    pub tabs: Vec<WorkspaceTab>,
    pub active_tab: u32,
}

/// What [`WorkspaceStore::close`] returns so the server can destroy the session.
#[derive(Debug)]
pub struct ClosedWorkspace {
    pub session_id: String,
    pub focused_id: Option<String>,
}

#[derive(Clone, Default)]
pub struct WorkspaceStore {
    inner: Arc<Mutex<Inner>>,
}

impl WorkspaceStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn list(&self) -> (Vec<WorkspaceInfo>, Option<String>) {
        let guard = self.inner.lock().await;
        let workspaces = guard
            .order
            .iter()
            .filter_map(|id| guard.by_id.get(id).map(|rec| rec.info(0)))
            .collect();
        (workspaces, guard.focused_id.clone())
    }

    /// Create a workspace and a fresh ADE session. Does not change focus when
    /// another workspace is already focused, so creating one does not detach
    /// the caller's current session.
    pub async fn create(
        &self,
        sessions: &SessionStore,
        name: Option<String>,
        root: String,
    ) -> WorkspaceInfo {
        let session_id = sessions.create(None).await;
        let id = Uuid::new_v4().to_string();
        let name = normalize_name(name, &root);
        let rec = Record {
            id: id.clone(),
            name,
            root,
            session_id: session_id.clone(),
            tabs: Vec::new(),
            active_tab: 0,
        };
        let info = rec.info(0);
        let mut guard = self.inner.lock().await;
        if guard.focused_id.is_none() {
            guard.focused_id = Some(id.clone());
        }
        guard.by_session.insert(session_id, id.clone());
        guard.order.push(id.clone());
        guard.by_id.insert(id, rec);
        info
    }

    pub async fn rename(&self, id: &str, name: &str) -> Result<WorkspaceInfo> {
        let name = name.trim();
        if name.is_empty() {
            bail!("workspace name is empty");
        }
        let name: String = name.chars().take(MAX_NAME_CHARS).collect();
        let mut guard = self.inner.lock().await;
        let rec = guard
            .by_id
            .get_mut(id)
            .with_context(|| format!("unknown workspace {id}"))?;
        rec.name = name;
        Ok(rec.info(0))
    }

    /// Replace the tab list. Returns `(session_id, layout_json)` so the server
    /// can mirror it onto the ADE session blob.
    pub async fn set_layout(
        &self,
        id: &str,
        tabs: Vec<WorkspaceTab>,
        active_tab: u32,
    ) -> Result<(String, String)> {
        let tabs = sanitize_tabs(tabs);
        let active_tab = clamp_active(active_tab, tabs.len());
        let mut guard = self.inner.lock().await;
        let rec = guard
            .by_id
            .get_mut(id)
            .with_context(|| format!("unknown workspace {id}"))?;
        rec.tabs = tabs;
        rec.active_tab = active_tab;
        let json = layout_json(&rec.tabs, rec.active_tab);
        Ok((rec.session_id.clone(), json))
    }

    pub async fn focus(&self, id: &str) -> Result<FocusedWorkspace> {
        let mut guard = self.inner.lock().await;
        if !guard.by_id.contains_key(id) {
            bail!("unknown workspace {id}");
        }
        guard.focused_id = Some(id.to_owned());
        let rec = guard.by_id.get(id).expect("workspace present");
        Ok(FocusedWorkspace {
            info: rec.info(0),
            session_id: rec.session_id.clone(),
            tabs: rec.tabs.clone(),
            active_tab: rec.active_tab,
        })
    }

    /// Remove the workspace record. Caller destroys the ADE session.
    /// Refuses to remove the last workspace.
    pub async fn close(&self, id: &str) -> Result<ClosedWorkspace> {
        let mut guard = self.inner.lock().await;
        if guard.order.len() <= 1 {
            bail!("cannot close the last workspace");
        }
        let rec = guard
            .by_id
            .remove(id)
            .with_context(|| format!("unknown workspace {id}"))?;
        guard.order.retain(|existing| existing != id);
        guard.by_session.remove(&rec.session_id);
        if guard.focused_id.as_deref() == Some(id) {
            guard.focused_id = guard.order.first().cloned();
        }
        Ok(ClosedWorkspace {
            session_id: rec.session_id,
            focused_id: guard.focused_id.clone(),
        })
    }

    /// Remember a PTY opened in this session, if the session belongs to a workspace.
    ///
    /// Returns `(session_id, layout_json)` when the tab list changed so the
    /// server can mirror it onto the ADE session blob.
    pub async fn note_terminal(&self, session_id: &str, pty_id: &str) -> Option<(String, String)> {
        let mut guard = self.inner.lock().await;
        let id = guard.by_session.get(session_id)?.clone();
        let rec = guard.by_id.get_mut(&id)?;
        if rec.tabs.iter().any(|tab| {
            tab.kind == WorkspaceTabKind::Terminal && tab.pty_id.as_deref() == Some(pty_id)
        }) {
            return None;
        }
        let n = rec
            .tabs
            .iter()
            .filter(|tab| tab.kind == WorkspaceTabKind::Terminal)
            .count()
            + 1;
        rec.tabs.push(WorkspaceTab {
            kind: WorkspaceTabKind::Terminal,
            title: format!("Terminal {n}"),
            pty_id: Some(pty_id.to_owned()),
            path: None,
        });
        if rec.tabs.len() > MAX_TABS {
            let overflow = rec.tabs.len() - MAX_TABS;
            rec.tabs.drain(0..overflow);
            rec.active_tab = clamp_active(rec.active_tab, rec.tabs.len());
        }
        let session_id = rec.session_id.clone();
        Some((session_id, layout_json(&rec.tabs, rec.active_tab)))
    }

    pub async fn note_terminal_closed(
        &self,
        session_id: &str,
        pty_id: &str,
    ) -> Option<(String, String)> {
        let mut guard = self.inner.lock().await;
        let id = guard.by_session.get(session_id)?.clone();
        let rec = guard.by_id.get_mut(&id)?;
        let before = rec.tabs.len();
        rec.tabs.retain(|tab| {
            !(tab.kind == WorkspaceTabKind::Terminal && tab.pty_id.as_deref() == Some(pty_id))
        });
        if rec.tabs.len() == before {
            return None;
        }
        rec.active_tab = clamp_active(rec.active_tab, rec.tabs.len());
        let session_id = rec.session_id.clone();
        Some((session_id, layout_json(&rec.tabs, rec.active_tab)))
    }
}

fn normalize_name(name: Option<String>, root: &str) -> String {
    let trimmed = name.unwrap_or_default();
    let trimmed = trimmed.trim();
    if !trimmed.is_empty() {
        return trimmed.chars().take(MAX_NAME_CHARS).collect();
    }
    root.rsplit(['/', '\\'])
        .find(|seg| !seg.is_empty())
        .unwrap_or("Workspace")
        .chars()
        .take(MAX_NAME_CHARS)
        .collect()
}

fn clamp_active(active: u32, len: usize) -> u32 {
    if len == 0 {
        0
    } else {
        active.min((len - 1) as u32)
    }
}

fn sanitize_tabs(tabs: Vec<WorkspaceTab>) -> Vec<WorkspaceTab> {
    tabs.into_iter()
        .filter_map(|mut tab| {
            tab.title = tab.title.chars().take(MAX_TITLE_CHARS).collect();
            match tab.kind {
                WorkspaceTabKind::Terminal => {
                    let pty_id = tab
                        .pty_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|id| !id.is_empty())?
                        .to_owned();
                    if tab.title.is_empty() {
                        tab.title = "Terminal".to_owned();
                    }
                    tab.pty_id = Some(pty_id);
                    tab.path = None;
                    Some(tab)
                }
                WorkspaceTabKind::Editor => {
                    let path = tab
                        .path
                        .as_deref()
                        .map(str::trim)
                        .filter(|path| !path.is_empty())?
                        .to_owned();
                    if tab.title.is_empty() {
                        tab.title = path
                            .rsplit(['/', '\\'])
                            .find(|seg| !seg.is_empty())
                            .unwrap_or("file")
                            .to_owned();
                    }
                    tab.path = Some(path);
                    tab.pty_id = None;
                    Some(tab)
                }
            }
        })
        .take(MAX_TABS)
        .collect()
}

pub fn layout_json(tabs: &[WorkspaceTab], active_tab: u32) -> String {
    serde_json::json!({
        "version": 5,
        "host": "gpui",
        "tabs": tabs,
        "active_tab": active_tab,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term(title: &str, pty: &str) -> WorkspaceTab {
        WorkspaceTab {
            kind: WorkspaceTabKind::Terminal,
            title: title.to_owned(),
            pty_id: Some(pty.to_owned()),
            path: None,
        }
    }

    fn editor(path: &str) -> WorkspaceTab {
        WorkspaceTab {
            kind: WorkspaceTabKind::Editor,
            title: "file".into(),
            pty_id: None,
            path: Some(path.to_owned()),
        }
    }

    #[tokio::test]
    async fn tabs_do_not_cross_workspaces() {
        let sessions = SessionStore::new();
        let store = WorkspaceStore::new();
        let alpha = store
            .create(&sessions, Some("alpha".into()), "/work/alpha".into())
            .await;
        let beta = store
            .create(&sessions, Some("  ".into()), "/work/beta".into())
            .await;
        assert_eq!(beta.name, "beta");
        assert_ne!(alpha.session_id, beta.session_id);

        store
            .set_layout(
                &alpha.id,
                vec![term("alpha-term", "pty-a"), editor("/work/alpha/a.rs")],
                0,
            )
            .await
            .unwrap();
        store
            .set_layout(&beta.id, vec![editor("/work/beta/b.rs")], 0)
            .await
            .unwrap();

        store.note_terminal(&alpha.session_id, "pty-extra").await;
        store.note_terminal(&beta.session_id, "pty-b").await;

        let focused = store.focus(&alpha.id).await.unwrap();
        let pty_ids: Vec<_> = focused
            .tabs
            .iter()
            .filter_map(|tab| tab.pty_id.clone())
            .collect();
        assert_eq!(pty_ids, vec!["pty-a".to_owned(), "pty-extra".to_owned()]);
        assert!(
            focused
                .tabs
                .iter()
                .all(|tab| { tab.path.as_deref() != Some("/work/beta/b.rs") })
        );

        let beta_focus = store.focus(&beta.id).await.unwrap();
        assert!(
            beta_focus
                .tabs
                .iter()
                .all(|tab| tab.pty_id.as_deref() != Some("pty-a"))
        );
        assert!(
            beta_focus
                .tabs
                .iter()
                .any(|tab| tab.pty_id.as_deref() == Some("pty-b"))
        );
        assert!(
            beta_focus
                .tabs
                .iter()
                .any(|tab| tab.path.as_deref() == Some("/work/beta/b.rs"))
        );

        let renamed = store.rename(&alpha.id, "Alpha Project").await.unwrap();
        assert_eq!(renamed.name, "Alpha Project");

        let (listed, focused_id) = store.list().await;
        assert_eq!(listed.len(), 2);
        assert_eq!(focused_id.as_deref(), Some(beta.id.as_str()));

        store.close(&alpha.id).await.unwrap();
        let err = store.close(&beta.id).await.unwrap_err();
        assert!(err.to_string().contains("last workspace"));
    }
}

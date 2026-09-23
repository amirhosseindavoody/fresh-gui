//! Named workspaces inside the singleton daemon.
//!
//! One daemon process (one user) holds every workspace. Each workspace owns an
//! ADE session: PTYs, scrollback, and the tab list the host restores. Switching
//! moves the WebSocket subscriber; it does not stop the other sessions.
//!
//! With a state file, the workspace list (names, roots, tabs, active tab,
//! explorer open folders, focus) is written after each change and loaded on
//! the next start. PTYs do not survive a restart: terminal tabs keep their
//! title and a dead `pty_id`, and the host starts a new shell for them.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use fresh_gui_protocol::{WorkspaceInfo, WorkspaceTab, WorkspaceTabKind};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::session::SessionStore;

const MAX_TABS: usize = 64;
const MAX_NAME_CHARS: usize = 64;
const MAX_TITLE_CHARS: usize = 120;
const MAX_EXPANDED: usize = 512;
const STATE_VERSION: u32 = 1;
/// Coalesces bursts (tab activation, folder clicks) into one write.
const SAVE_DEBOUNCE: Duration = Duration::from_millis(300);

struct Record {
    id: String,
    name: String,
    root: String,
    session_id: String,
    tabs: Vec<WorkspaceTab>,
    active_tab: u32,
    explorer_expanded: Vec<String>,
}

/// On-disk shape of the workspace list.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct SavedState {
    version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    focused_id: Option<String>,
    #[serde(default)]
    workspaces: Vec<SavedWorkspace>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct SavedWorkspace {
    id: String,
    name: String,
    root: String,
    #[serde(default)]
    tabs: Vec<WorkspaceTab>,
    #[serde(default)]
    active_tab: u32,
    #[serde(default)]
    explorer_expanded: Vec<String>,
}

struct Persist {
    path: PathBuf,
    dirty: Notify,
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

#[derive(Default)]
struct Inner {
    order: Vec<String>,
    by_id: HashMap<String, Record>,
    by_session: HashMap<String, String>,
    focused_id: Option<String>,
}

impl Inner {
    fn snapshot(&self) -> SavedState {
        SavedState {
            version: STATE_VERSION,
            focused_id: self.focused_id.clone(),
            workspaces: self
                .order
                .iter()
                .filter_map(|id| self.by_id.get(id))
                .map(|rec| SavedWorkspace {
                    id: rec.id.clone(),
                    name: rec.name.clone(),
                    root: rec.root.clone(),
                    tabs: rec.tabs.clone(),
                    active_tab: rec.active_tab,
                    explorer_expanded: rec.explorer_expanded.clone(),
                })
                .collect(),
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
    pub explorer_expanded: Vec<String>,
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
    persist: Option<Arc<Persist>>,
}

impl WorkspaceStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// A store that saves to `path`. Call [`Self::load`] to restore the
    /// previous list and [`Self::spawn_saver`] to start writing.
    pub fn persistent(path: PathBuf) -> Self {
        Self {
            inner: Arc::default(),
            persist: Some(Arc::new(Persist {
                path,
                dirty: Notify::new(),
            })),
        }
    }

    pub fn state_path(&self) -> Option<&Path> {
        self.persist.as_ref().map(|persist| persist.path.as_path())
    }

    /// Recreate saved workspaces, each with a fresh ADE session. Returns the
    /// roots so the caller can re-authorize them in the FS sandbox. A missing
    /// file is an empty list; an unreadable one is logged and set aside so
    /// the daemon still starts.
    pub async fn load(&self, sessions: &SessionStore) -> Vec<String> {
        let Some(persist) = &self.persist else {
            return Vec::new();
        };
        let saved = match read_state(&persist.path) {
            Ok(Some(saved)) => saved,
            Ok(None) => return Vec::new(),
            Err(err) => {
                let aside = persist.path.with_extension("json.bad");
                warn!(
                    path = %persist.path.display(),
                    moved_to = %aside.display(),
                    "workspace state unreadable, starting empty: {err:#}"
                );
                let _ = std::fs::rename(&persist.path, &aside);
                return Vec::new();
            }
        };
        let mut roots = Vec::new();
        let mut guard = self.inner.lock().await;
        for ws in saved.workspaces {
            if ws.id.trim().is_empty() || guard.by_id.contains_key(&ws.id) {
                continue;
            }
            let session_id = sessions.create(None).await;
            let tabs = sanitize_tabs(ws.tabs);
            let active_tab = clamp_active(ws.active_tab, tabs.len());
            let json = layout_json(&tabs, active_tab);
            let _ = sessions.set_layout(&session_id, json).await;
            roots.push(ws.root.clone());
            let rec = Record {
                id: ws.id.clone(),
                name: normalize_name(Some(ws.name), &ws.root),
                root: ws.root,
                session_id: session_id.clone(),
                tabs,
                active_tab,
                explorer_expanded: sanitize_expanded(ws.explorer_expanded),
            };
            guard.by_session.insert(session_id, ws.id.clone());
            guard.order.push(ws.id.clone());
            guard.by_id.insert(ws.id, rec);
        }
        guard.focused_id = saved
            .focused_id
            .filter(|id| guard.by_id.contains_key(id))
            .or_else(|| guard.order.first().cloned());
        info!(
            path = %persist.path.display(),
            workspaces = guard.order.len(),
            "restored workspaces"
        );
        roots
    }

    /// Background writer: waits for a change, lets a burst settle, then
    /// writes one snapshot. No-op without a state file.
    pub fn spawn_saver(&self) {
        let Some(persist) = self.persist.clone() else {
            return;
        };
        let store = self.clone();
        tokio::spawn(async move {
            loop {
                persist.dirty.notified().await;
                tokio::time::sleep(SAVE_DEBOUNCE).await;
                if let Err(err) = store.save_now().await {
                    warn!(path = %persist.path.display(), "save workspaces: {err:#}");
                }
            }
        });
    }

    /// Write the current list immediately (shutdown, tests).
    pub async fn save_now(&self) -> Result<()> {
        let Some(persist) = &self.persist else {
            return Ok(());
        };
        let snapshot = self.inner.lock().await.snapshot();
        let path = persist.path.clone();
        tokio::task::spawn_blocking(move || write_state(&path, &snapshot))
            .await
            .context("join workspace save")??;
        debug!(path = %persist.path.display(), "saved workspaces");
        Ok(())
    }

    fn mark_dirty(&self) {
        if let Some(persist) = &self.persist {
            persist.dirty.notify_one();
        }
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
            explorer_expanded: Vec::new(),
        };
        let info = rec.info(0);
        let mut guard = self.inner.lock().await;
        if guard.focused_id.is_none() {
            guard.focused_id = Some(id.clone());
        }
        guard.by_session.insert(session_id, id.clone());
        guard.order.push(id.clone());
        guard.by_id.insert(id, rec);
        drop(guard);
        self.mark_dirty();
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
        let info = rec.info(0);
        drop(guard);
        self.mark_dirty();
        Ok(info)
    }

    /// Persist a new canonical root. The server authorizes the directory first.
    pub async fn set_root(&self, id: &str, root: String) -> Result<WorkspaceInfo> {
        let mut guard = self.inner.lock().await;
        let rec = guard
            .by_id
            .get_mut(id)
            .with_context(|| format!("unknown workspace {id}"))?;
        rec.root = root;
        // Expanded folders belonged to the previous tree.
        rec.explorer_expanded.clear();
        let info = rec.info(0);
        drop(guard);
        self.mark_dirty();
        Ok(info)
    }

    /// Replace the tab list and open explorer folders. Returns
    /// `(session_id, layout_json)` so the server can mirror it onto the ADE
    /// session blob.
    pub async fn set_layout(
        &self,
        id: &str,
        tabs: Vec<WorkspaceTab>,
        active_tab: u32,
        explorer_expanded: Vec<String>,
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
        rec.explorer_expanded = sanitize_expanded(explorer_expanded);
        let json = layout_json(&rec.tabs, rec.active_tab);
        let out = (rec.session_id.clone(), json);
        drop(guard);
        self.mark_dirty();
        Ok(out)
    }

    /// Project root stored for `id`. Empty means the daemon FS root.
    pub async fn root_of(&self, id: &str) -> Option<String> {
        let guard = self.inner.lock().await;
        guard.by_id.get(id).map(|rec| rec.root.clone())
    }

    /// Project root of the workspace that owns this ADE session.
    pub async fn root_for_session(&self, session_id: &str) -> Option<String> {
        let guard = self.inner.lock().await;
        let id = guard.by_session.get(session_id)?.clone();
        guard.by_id.get(&id).map(|rec| rec.root.clone())
    }

    pub async fn focus(&self, id: &str) -> Result<FocusedWorkspace> {
        let mut guard = self.inner.lock().await;
        if !guard.by_id.contains_key(id) {
            bail!("unknown workspace {id}");
        }
        let changed = guard.focused_id.as_deref() != Some(id);
        guard.focused_id = Some(id.to_owned());
        let rec = guard.by_id.get(id).expect("workspace present");
        let focused = FocusedWorkspace {
            info: rec.info(0),
            session_id: rec.session_id.clone(),
            tabs: rec.tabs.clone(),
            active_tab: rec.active_tab,
            explorer_expanded: rec.explorer_expanded.clone(),
        };
        drop(guard);
        if changed {
            self.mark_dirty();
        }
        Ok(focused)
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
        let closed = ClosedWorkspace {
            session_id: rec.session_id,
            focused_id: guard.focused_id.clone(),
        };
        drop(guard);
        self.mark_dirty();
        Ok(closed)
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
        let out = (
            rec.session_id.clone(),
            layout_json(&rec.tabs, rec.active_tab),
        );
        drop(guard);
        self.mark_dirty();
        Some(out)
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
        let out = (
            rec.session_id.clone(),
            layout_json(&rec.tabs, rec.active_tab),
        );
        drop(guard);
        self.mark_dirty();
        Some(out)
    }
}

fn read_state(path: &Path) -> Result<Option<SavedState>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).with_context(|| format!("read {}", path.display())),
    };
    let saved: SavedState =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    if saved.version > STATE_VERSION {
        bail!(
            "state version {} is newer than this daemon ({STATE_VERSION})",
            saved.version
        );
    }
    Ok(Some(saved))
}

/// Write to a sibling temp file, then rename, so a crash mid-write leaves the
/// previous list intact. The file is private (0600 on Unix): it names
/// project roots and open file paths.
fn write_state(path: &Path, state: &SavedState) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let json = serde_json::to_vec_pretty(state)?;
    let tmp = path.with_extension("json.tmp");
    {
        use std::io::Write as _;
        let mut file = crate::daemon::open_private_write(&tmp)
            .with_context(|| format!("open {}", tmp.display()))?;
        file.write_all(&json)?;
        file.sync_all()?;
    }
    crate::daemon::chmod_file_private(&tmp);
    std::fs::rename(&tmp, path)
        .with_context(|| format!("rename {} -> {}", tmp.display(), path.display()))?;
    Ok(())
}

fn sanitize_expanded(paths: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .map(|path| path.trim().to_owned())
        .filter(|path| !path.is_empty() && !path.ends_with("/.") && seen.insert(path.clone()))
        .take(MAX_EXPANDED)
        .collect()
}

/// Directory a new shell should start in.
///
/// An explicit cwd (the user already `cd`'d, or the host asked) wins. Otherwise
/// the workspace root, then the daemon FS root. An empty string is not a
/// directory: remote daemons are often started from `$HOME`, and that must not
/// override a session root.
pub fn shell_working_directory(
    requested: Option<&str>,
    workspace_root: Option<&str>,
    fs_root: &str,
) -> Option<String> {
    fn nonempty(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }
    nonempty(requested)
        .or_else(|| nonempty(workspace_root))
        .or_else(|| nonempty(Some(fs_root)))
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

    #[test]
    fn shell_cwd_prefers_an_explicit_directory_then_the_workspace_root() {
        assert_eq!(
            shell_working_directory(Some("/tmp/here"), Some("/work/proj"), "/home/me").as_deref(),
            Some("/tmp/here")
        );
        assert_eq!(
            shell_working_directory(Some("  "), Some("/work/proj"), "/home/me").as_deref(),
            Some("/work/proj")
        );
        assert_eq!(
            shell_working_directory(None, Some(""), "/srv/app").as_deref(),
            Some("/srv/app")
        );
        assert_eq!(shell_working_directory(None, None, "  "), None);
    }

    #[tokio::test]
    async fn session_root_is_the_workspace_directory() {
        let sessions = SessionStore::new();
        let store = WorkspaceStore::new();
        let info = store
            .create(&sessions, None, "/work/remote-root".into())
            .await;
        assert_eq!(
            store.root_for_session(&info.session_id).await.as_deref(),
            Some("/work/remote-root")
        );
        assert_eq!(store.root_for_session("missing").await, None);
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
                Vec::new(),
            )
            .await
            .unwrap();
        store
            .set_layout(&beta.id, vec![editor("/work/beta/b.rs")], 0, Vec::new())
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

    fn temp_state(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-ws-state-{tag}-{}-{}",
            std::process::id(),
            Uuid::new_v4().simple()
        ));
        dir.join(crate::daemon::WORKSPACES_NAME)
    }

    #[tokio::test]
    async fn changing_root_persists_and_clears_old_explorer_folders() {
        let path = temp_state("set-root");
        let sessions = SessionStore::new();
        let store = WorkspaceStore::persistent(path.clone());
        let created = store
            .create(&sessions, Some("project".into()), "/old".into())
            .await;
        store
            .set_layout(&created.id, vec![], 0, vec!["/old/src".into()])
            .await
            .unwrap();
        let changed = store.set_root(&created.id, "/new".into()).await.unwrap();
        assert_eq!(changed.root, "/new");
        assert_eq!(changed.name, "project");
        assert!(store.focus(&created.id).await.unwrap().explorer_expanded.is_empty());
        store.save_now().await.unwrap();
        let restored = WorkspaceStore::persistent(path.clone());
        restored.load(&SessionStore::new()).await;
        assert_eq!(restored.list().await.0[0].root, "/new");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn saved_workspaces_come_back_after_restart() {
        let path = temp_state("roundtrip");
        let sessions = SessionStore::new();
        let store = WorkspaceStore::persistent(path.clone());
        assert!(store.load(&sessions).await.is_empty());
        let alpha = store
            .create(&sessions, Some("alpha".into()), "/work/alpha".into())
            .await;
        let beta = store
            .create(&sessions, Some("beta".into()), "/work/beta".into())
            .await;
        store
            .set_layout(
                &beta.id,
                vec![term("2", "pty-b"), editor("/work/beta/lib.rs")],
                1,
                vec!["/work/beta/src".into(), "/work/beta/src/.".into()],
            )
            .await
            .unwrap();
        store.focus(&beta.id).await.unwrap();
        store.rename(&alpha.id, "Alpha").await.unwrap();
        store.save_now().await.unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "state file must be private");
        }

        let sessions = SessionStore::new();
        let restored = WorkspaceStore::persistent(path.clone());
        let roots = restored.load(&sessions).await;
        assert_eq!(
            roots,
            vec!["/work/alpha".to_string(), "/work/beta".to_string()]
        );
        let (listed, focused) = restored.list().await;
        assert_eq!(
            listed.iter().map(|ws| ws.name.as_str()).collect::<Vec<_>>(),
            ["Alpha", "beta"]
        );
        assert_eq!(listed[0].id, alpha.id);
        assert_ne!(listed[0].session_id, alpha.session_id, "sessions are new");
        assert_eq!(focused.as_deref(), Some(beta.id.as_str()));

        let beta_now = restored.focus(&beta.id).await.unwrap();
        assert_eq!(beta_now.active_tab, 1);
        assert_eq!(beta_now.tabs[0].title, "2");
        assert_eq!(beta_now.tabs[0].pty_id.as_deref(), Some("pty-b"));
        assert_eq!(beta_now.tabs[1].path.as_deref(), Some("/work/beta/lib.rs"));
        assert_eq!(
            beta_now.explorer_expanded,
            vec!["/work/beta/src".to_string()]
        );

        // The restored store keeps saving: closing a workspace sticks.
        restored.close(&alpha.id).await.unwrap();
        restored.save_now().await.unwrap();
        let again = WorkspaceStore::persistent(path.clone());
        again.load(&SessionStore::new()).await;
        assert_eq!(again.list().await.0.len(), 1);

        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn corrupt_state_is_set_aside_and_the_daemon_starts_empty() {
        let path = temp_state("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{not json").unwrap();
        let store = WorkspaceStore::persistent(path.clone());
        assert!(store.load(&SessionStore::new()).await.is_empty());
        assert!(store.list().await.0.is_empty());
        assert!(!path.exists());
        assert!(path.with_extension("json.bad").exists());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[tokio::test]
    async fn saver_writes_after_a_change() {
        let path = temp_state("saver");
        let store = WorkspaceStore::persistent(path.clone());
        store.spawn_saver();
        store
            .create(&SessionStore::new(), Some("one".into()), "/work/one".into())
            .await;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !path.exists() && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let saved = read_state(&path).unwrap().expect("saver wrote the file");
        assert_eq!(saved.workspaces.len(), 1);
        assert_eq!(saved.workspaces[0].name, "one");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }
}

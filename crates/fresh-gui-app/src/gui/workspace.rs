//! Zed/VS Code-like ADE workspace: activity bar, explorer, docked tabs, status, palette.
//!
//! Editor and terminal surfaces are dock panels. Dragging a tab to a pane edge
//! splits; dropping it on a tab merges; dropping it in the strip reorders.
//! gpui-component refuses to drag the last remaining tab, so a split needs two
//! tabs. Terminal titles are herdr-style numbers inside the focused workspace.
//! A left spaces rail lists daemon workspaces (name and project root);
//! switching swaps this dock for that workspace's session without closing
//! its PTYs.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use fresh_gui_protocol::{
    CAP_WORKSPACE, FsEntry, FsKind, Hello, PtyInfo, WorkspaceInfo, WorkspaceTab, WorkspaceTabKind,
};
use gpui_kit::component::dock::{
    DockArea, DockPlacement, DockSkin, PanelId, PanelStyle, panel_handle,
};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Root, Selectable, Sizable, StyledExt, TitleBar,
    button::{Button, ButtonVariants as _},
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    input::{Input, InputEvent, InputState},
    list::ListItem,
    status_bar::StatusBar,
    tree::{TreeEvent, TreeItem, TreeState, tree},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{
    CloseTab, CloseWorkspace, CopyExplorer, Disconnect, GoToFile, NewTerminal, NewWorkspace,
    NextTab, OpenSettings, PasteExplorer, PrevTab, Reconnect, RenameWorkspace, SaveBuffer,
    ToggleCommandPalette, ToggleSidebar,
};
use super::ade::AttachedWorkspace;
use super::ade::{AdeCmd, AdeEvent, AdeHandle};
use super::connect::{ConnectTarget, parse_goto_spec};
use super::explorer::{
    absolute_paths_text, apply_selection, copyable_sources, drag_paths, gesture_from_modifiers,
    is_placeholder, movable_sources, parent_dir, real_ids,
};
use super::pane::{EditorPanel, TerminalPanel};
use super::rail::{
    WORKSPACE_RAIL_W, WORKSPACE_ROW_H, empty_workspace_name_hint, user_home, workspace_rail_hint,
    workspace_root_label,
};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Dense ribbon sizes. gpui-component medium controls (32px icon buttons,
/// 32px tabs, `py_2` rails) leave the shell airier than VS Code / Zed.
/// The dock tab strip itself stays at the skin's default 32px; that chrome
/// is owned by `DockSkin`, not this host.
const TITLE_BAR_H: f32 = 30.;
const ACTIVITY_RAIL_W: f32 = 36.;
const SIDEBAR_HEADER_H: f32 = 26.;
const TREE_ROW_H: f32 = 22.;
const STATUS_BAR_H: f32 = 22.;

fn next_id(prefix: &str) -> String {
    format!("{prefix}-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activity {
    Explorer,
}

enum ConnectionState {
    Connecting,
    Online,
    Offline { reason: String },
}

enum ActiveSurface {
    Terminal(String),
    Editor(String),
}

struct PendingFs {
    sources: Vec<String>,
    destination: String,
}

#[derive(Clone)]
struct ExplorerDrag {
    paths: Vec<String>,
}

struct ExplorerDragPreview {
    count: usize,
}

impl Render for ExplorerDragPreview {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = if self.count == 1 {
            "Move".to_string()
        } else {
            format!("Move {}", self.count)
        };
        div()
            .px_2()
            .py_1()
            .text_sm()
            .rounded(cx.theme().radius)
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .child(label)
    }
}

pub struct Workspace {
    target: ConnectTarget,
    ade: AdeHandle,
    connection: ConnectionState,
    session_id: Option<String>,
    workspace_cap: bool,
    workspaces: Vec<WorkspaceInfo>,
    active_workspace_id: Option<String>,
    /// `pty_open` requests this view is still waiting on. `pty_opened` for any
    /// other id is ignored so a late open from workspace A cannot appear in B.
    pty_opens_pending: u32,
    /// Editor opens issued by this view. Unsolicited `editor_opened` does not
    /// add a tab.
    pending_editors: HashMap<String, bool>,
    restoring: bool,
    /// Workspace whose name is being edited in the rail. Any row, not only the
    /// active one. `None` when the inline field is closed.
    renaming_id: Option<String>,
    /// Row under the pointer, so Rename / Close stay off the resting layout.
    rail_hover: Option<String>,
    create_open: bool,
    capabilities: Vec<String>,
    config_path: Option<String>,
    status: SharedString,
    sidebar_collapsed: bool,
    activity: Activity,
    dock: Entity<DockArea>,
    terminals: HashMap<String, Entity<TerminalPanel>>,
    editors: HashMap<String, Entity<EditorPanel>>,
    next_terminal_number: u32,
    active: Option<ActiveSurface>,
    last_cwd: Option<String>,
    explorer: Entity<TreeState>,
    explorer_root: String,
    explorer_cache: HashMap<String, Vec<FsEntry>>,
    explorer_focus: FocusHandle,
    /// Selected absolute paths. The last entry is the primary row.
    selection: Vec<String>,
    anchor: Option<String>,
    /// In-app file clipboard for explorer paste (`fs_copy`). Absolute paths.
    file_clipboard: Option<Vec<String>>,
    pending_fs: HashMap<String, PendingFs>,
    command_state: Entity<CommandState>,
    palette_open: bool,
    goto_open: bool,
    goto_input: Entity<InputState>,
    create_name: Entity<InputState>,
    create_root: Entity<InputState>,
    ws_rename_input: Entity<InputState>,
    rename_pty: Option<String>,
    rename_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    _recv_task: Task<()>,
}

impl Workspace {
    pub fn new(target: ConnectTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (ade, evt_rx) = super::ade::spawn(target.clone());
        let (dock, skin) = DockSkin::dock_area("workspace", Some(1), window, cx);
        skin.set_panel_style(PanelStyle::TabBar, cx);
        skin.set_toggle_button_visible(false, cx);
        let explorer = cx.new(|cx| TreeState::new(cx));
        let command_state = cx.new(|cx| CommandState::new(window, cx));
        let goto_input = cx.new(|cx| InputState::new(window, cx).placeholder("path[:line[:col]]"));
        let create_name = cx.new(|cx| InputState::new(window, cx).placeholder("Folder name"));
        let create_root = cx.new(|cx| InputState::new(window, cx).placeholder("/absolute/path"));
        let ws_rename_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Workspace name"));
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Terminal name"));
        let tree_sub = cx.subscribe(&explorer, |this, _, ev: &TreeEvent, _cx| {
            if let TreeEvent::Expanded(id) = ev {
                let path = id.to_string();
                if !is_placeholder(&path) && !this.explorer_cache.contains_key(&path) {
                    this.ade.send(AdeCmd::ListDir {
                        request_id: next_id("ex"),
                        path,
                    });
                }
            }
        });
        let rename_sub = cx.subscribe(&rename_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) && this.rename_pty.is_some() {
                this.confirm_rename(cx);
            }
        });
        let ws_rename_sub = cx.subscribe(&ws_rename_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) && this.renaming_id.is_some() {
                this.confirm_workspace_rename(cx);
            }
        });
        let create_name_sub = cx.subscribe(&create_name, |this, _, ev: &InputEvent, cx| {
            if !this.create_open {
                return;
            }
            match ev {
                InputEvent::PressEnter { .. } => this.confirm_create(cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });
        let create_root_sub = cx.subscribe(&create_root, |this, _, ev: &InputEvent, cx| {
            if !this.create_open {
                return;
            }
            match ev {
                InputEvent::PressEnter { .. } => this.confirm_create(cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });

        let recv_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(ev) = evt_rx.recv().await {
                if cx
                    .update(|window, app| {
                        this.update(app, |this, cx| this.handle_event(ev, window, cx))
                    })
                    .is_err()
                {
                    break;
                }
            }
        });

        Self {
            target,
            ade,
            connection: ConnectionState::Connecting,
            session_id: None,
            workspace_cap: false,
            workspaces: Vec::new(),
            active_workspace_id: None,
            pty_opens_pending: 0,
            pending_editors: HashMap::new(),
            restoring: false,
            renaming_id: None,
            rail_hover: None,
            create_open: false,
            capabilities: Vec::new(),
            config_path: None,
            status: "Connecting…".into(),
            sidebar_collapsed: false,
            activity: Activity::Explorer,
            dock,
            terminals: HashMap::new(),
            editors: HashMap::new(),
            next_terminal_number: 1,
            active: None,
            last_cwd: None,
            explorer,
            explorer_root: String::new(),
            explorer_cache: HashMap::new(),
            explorer_focus: cx.focus_handle(),
            selection: Vec::new(),
            anchor: None,
            file_clipboard: None,
            pending_fs: HashMap::new(),
            command_state,
            palette_open: false,
            goto_open: false,
            goto_input,
            create_name,
            create_root,
            ws_rename_input,
            rename_pty: None,
            rename_input,
            _subscriptions: vec![
                tree_sub,
                rename_sub,
                ws_rename_sub,
                create_name_sub,
                create_root_sub,
            ],
            _recv_task: recv_task,
        }
    }

    fn handle_event(&mut self, ev: AdeEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            AdeEvent::Connecting => {
                self.connection = ConnectionState::Connecting;
                self.status = "Connecting…".into();
            }
            AdeEvent::Connected {
                hello,
                session_id,
                workspaces,
                attached,
            } => {
                self.apply_hello(&hello);
                self.workspace_cap = hello.capabilities.iter().any(|cap| cap == CAP_WORKSPACE);
                self.workspaces = workspaces;
                self.session_id = Some(session_id);
                self.connection = ConnectionState::Online;
                self.status = "Online".into();
                self.pty_opens_pending = 0;
                self.pending_editors.clear();
                if let Some(attached) = attached {
                    self.restore_workspace(attached, window, cx);
                } else {
                    self.active_workspace_id = None;
                    self.pty_opens_pending = 1;
                    self.ade.send(AdeCmd::OpenPty {
                        cols: 80,
                        rows: 24,
                        cwd: None,
                    });
                    self.ade.send(AdeCmd::ListDir {
                        request_id: next_id("ex"),
                        path: String::new(),
                    });
                }
            }
            AdeEvent::WorkspaceCreated { workspace } => {
                let id = workspace.id.clone();
                self.upsert_workspace(workspace);
                self.switch_to(id, cx);
            }
            AdeEvent::WorkspaceRenamed { workspace } => {
                self.status = format!("Renamed to {}", workspace.name).into();
                self.upsert_workspace(workspace);
            }
            AdeEvent::WorkspaceClosed { id, focused_id } => {
                let was_active = self.active_workspace_id.as_deref() == Some(id.as_str());
                self.workspaces.retain(|workspace| workspace.id != id);
                if was_active {
                    self.release_dock(window, cx);
                    self.active_workspace_id = None;
                    self.session_id = None;
                    self.pty_opens_pending = 0;
                    self.pending_editors.clear();
                    self.restoring = false;
                    if let Some(next) =
                        focused_id.or_else(|| self.workspaces.first().map(|w| w.id.clone()))
                    {
                        self.switch_to(next, cx);
                    }
                }
            }
            AdeEvent::WorkspaceSwitched { attached } => {
                self.restore_workspace(attached, window, cx);
            }
            AdeEvent::Disconnected { reason } => {
                self.connection = ConnectionState::Offline {
                    reason: reason.clone(),
                };
                self.status = format!("Disconnected: {reason}").into();
            }
            AdeEvent::PtyOpened { id, .. } => {
                if self.pty_opens_pending > 0 {
                    self.pty_opens_pending -= 1;
                    self.add_terminal_tab(id, window, cx);
                    self.publish_layout(cx);
                }
            }
            AdeEvent::PtyData { id, bytes } => self.on_pty_data(&id, &bytes, cx),
            AdeEvent::PtyClosed { id, reason } => {
                if let Some(reason) = reason {
                    self.status = format!("PTY closed: {reason}").into();
                }
                if let Some(panel) = self.terminals.get(&id).cloned() {
                    self.dock.update(cx, |dock, cx| {
                        dock.remove_panel(panel, window, cx);
                    });
                }
            }
            AdeEvent::FsListed { path, entries, .. } => {
                if self.explorer_root.is_empty() {
                    self.explorer_root = path.clone();
                }
                self.explorer_cache.insert(path, entries);
                self.rebuild_tree(cx);
            }
            AdeEvent::FsMoved {
                request_id,
                entries,
            } => {
                self.finish_fs(&request_id, entries, true, cx);
            }
            AdeEvent::FsCopied {
                request_id,
                entries,
            } => {
                self.finish_fs(&request_id, entries, false, cx);
            }
            AdeEvent::EditorOpened {
                request_id,
                buffer_id,
                path,
                language,
                line,
                column,
            } => {
                if let Some(activate) = self.pending_editors.remove(&request_id) {
                    self.begin_editor_tab(
                        buffer_id, path, language, line, column, activate, window, cx,
                    );
                    self.finish_restore_if_idle(cx);
                }
            }
            AdeEvent::BufferSnapshot {
                buffer_id,
                rev,
                text,
                path,
            } => self.apply_snapshot(buffer_id, rev, text, path, window, cx),
            AdeEvent::BufferChanged { buffer_id, rev, .. } => {
                if let Some(panel) = self.editor_by_buffer(&buffer_id, cx) {
                    panel.update(cx, |panel, cx| panel.set_rev(rev, cx));
                }
            }
            AdeEvent::BufferSaved {
                buffer_id,
                path,
                rev,
                ..
            } => self.on_buffer_saved(&buffer_id, path, rev, cx),
            AdeEvent::Error { code, message } => {
                self.pending_fs.clear();
                if code == "pty_open_failed" {
                    self.pty_opens_pending = self.pty_opens_pending.saturating_sub(1);
                }
                if self.restoring {
                    self.restoring = false;
                    self.pending_editors.clear();
                }
                self.status = format!("{code}: {message}").into();
            }
        }
        cx.notify();
    }

    fn apply_hello(&mut self, hello: &Hello) {
        self.capabilities = hello.capabilities.clone();
        self.config_path = hello.config_path.clone();
    }

    fn add_terminal_tab(&mut self, pty_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let number = self.next_terminal_number;
        self.next_terminal_number = self.next_terminal_number.saturating_add(1);
        let workspace = cx.weak_entity();
        let ade = self.ade.clone();
        let panel = cx.new(|cx| TerminalPanel::new(pty_id.clone(), number, ade, workspace, cx));
        let workspace_id = self.active_workspace_id.clone();
        panel.update(cx, |panel, _| panel.bind_workspace(workspace_id));
        self.dock.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            );
        });
        self.terminals.insert(pty_id, panel);
    }

    fn on_pty_data(&mut self, pty_id: &str, bytes: &[u8], cx: &mut Context<Self>) {
        let Some(panel) = self.terminals.get(pty_id).cloned() else {
            return;
        };
        let cwd = panel.update(cx, |panel, cx| panel.push_bytes(bytes, cx));
        if let Some(cwd) = cwd {
            self.last_cwd = Some(cwd);
        }
    }

    fn begin_editor_tab(
        &mut self,
        buffer_id: String,
        path: String,
        language: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
        activate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.editors.get(&path).cloned() {
            panel.update(cx, |panel, cx| {
                panel.note_reopen(buffer_id, line, column, cx);
            });
            if activate {
                self.select_entity(&panel, window, cx);
            }
            return;
        }
        let workspace = cx.weak_entity();
        let ade = self.ade.clone();
        let panel = cx.new(|cx| {
            EditorPanel::new(
                buffer_id,
                path.clone(),
                language,
                line,
                column,
                ade,
                workspace,
                window,
                cx,
            )
        });
        self.dock.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            );
        });
        self.editors.insert(path, panel.clone());
        if activate {
            self.select_entity(&panel, window, cx);
        }
    }

    fn apply_snapshot(
        &mut self,
        buffer_id: String,
        rev: u64,
        text: String,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let panel = self
            .editor_by_buffer(&buffer_id, cx)
            .or_else(|| self.editors.get(&path).cloned());
        let Some(panel) = panel else {
            return;
        };
        panel.update(cx, |panel, cx| {
            if panel.buffer_id() != buffer_id {
                panel.note_reopen(buffer_id, None, None, cx);
            }
            panel.apply_snapshot(rev, text, path, window, cx);
        });
        if !self.restoring {
            self.select_entity(&panel, window, cx);
        }
    }

    fn on_buffer_saved(&mut self, buffer_id: &str, path: String, rev: u64, cx: &mut Context<Self>) {
        let Some(panel) = self.editor_by_buffer(buffer_id, cx) else {
            return;
        };
        let previous = panel.update(cx, |panel, cx| panel.mark_saved(path.clone(), rev, cx));
        if let Some(previous) = previous
            && let Some(entity) = self.editors.remove(&previous)
        {
            self.editors.insert(path.clone(), entity);
            if matches!(&self.active, Some(ActiveSurface::Editor(current)) if current == &previous)
            {
                self.active = Some(ActiveSurface::Editor(path));
            }
        }
        self.status = "Saved".into();
    }

    fn editor_by_buffer(&self, buffer_id: &str, cx: &App) -> Option<Entity<EditorPanel>> {
        self.editors
            .values()
            .find(|panel| panel.read(cx).buffer_id() == buffer_id)
            .cloned()
    }

    fn select_entity<P: 'static>(
        &mut self,
        panel: &Entity<P>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let id = PanelId::from(panel.entity_id());
        self.dock
            .update(cx, |dock, cx| dock.select_panel(id, window, cx));
    }

    fn panel_order(&self, cx: &App) -> Vec<PanelId> {
        self.dock
            .read(cx)
            .layout(DockPlacement::Center)
            .map(|tree| tree.panels().collect())
            .unwrap_or_default()
    }

    fn active_panel_id(&self) -> Option<PanelId> {
        match &self.active {
            Some(ActiveSurface::Terminal(id)) => self
                .terminals
                .get(id)
                .map(|panel| PanelId::from(panel.entity_id())),
            Some(ActiveSurface::Editor(path)) => self
                .editors
                .get(path)
                .map(|panel| PanelId::from(panel.entity_id())),
            None => None,
        }
    }

    pub(crate) fn note_terminal_active(
        &mut self,
        pty_id: &str,
        active: bool,
        cx: &mut Context<Self>,
    ) {
        if active {
            self.active = Some(ActiveSurface::Terminal(pty_id.to_string()));
            if !self.restoring {
                // The dock calls `Panel::set_active` from inside
                // `Entity::update` on this terminal. `publish_layout` reads
                // that same entity for the tab title (`capture_layout`), and
                // GPUI panics on a read while the update is still on the
                // stack. Defer until the outermost update has returned it.
                let this = cx.entity();
                cx.defer(move |cx| {
                    this.update(cx, |this, cx| this.publish_layout(cx));
                });
            }
        } else if matches!(&self.active, Some(ActiveSurface::Terminal(id)) if id == pty_id) {
            self.active = None;
        }
        cx.notify();
    }

    pub(crate) fn note_editor_active(&mut self, path: &str, active: bool, cx: &mut Context<Self>) {
        if active {
            self.active = Some(ActiveSurface::Editor(path.to_string()));
            if !self.restoring {
                self.publish_layout(cx);
            }
        } else if matches!(&self.active, Some(ActiveSurface::Editor(current)) if current == path) {
            self.active = None;
        }
        cx.notify();
    }

    pub(crate) fn forget_terminal(&mut self, pty_id: &str, cx: &mut Context<Self>) {
        self.terminals.remove(pty_id);
        if matches!(&self.active, Some(ActiveSurface::Terminal(id)) if id == pty_id) {
            self.active = None;
        }
        if !self.restoring {
            self.publish_layout(cx);
        }
        cx.notify();
    }

    pub(crate) fn forget_editor(&mut self, path: &str, cx: &mut Context<Self>) {
        self.editors.remove(path);
        if matches!(&self.active, Some(ActiveSurface::Editor(current)) if current == path) {
            self.active = None;
        }
        cx.notify();
    }

    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        let root = self.explorer_root.clone();
        if root.is_empty() {
            return;
        }
        let items = build_tree_items(&root, &self.explorer_cache);
        self.explorer.update(cx, |state, cx| {
            state.set_items(items, cx);
        });
        self.sync_tree_highlight(None, cx);
    }

    pub(crate) fn new_terminal(&mut self, cx: &App) {
        if !matches!(self.connection, ConnectionState::Online) {
            self.status = "Not connected".into();
            return;
        }
        let cwd = self.active_cwd(cx);
        self.pty_opens_pending = self.pty_opens_pending.saturating_add(1);
        self.ade.send(AdeCmd::OpenPty {
            cols: 80,
            rows: 24,
            cwd,
        });
    }

    fn active_cwd(&self, cx: &App) -> Option<String> {
        if let Some(ActiveSurface::Terminal(id)) = &self.active
            && let Some(cwd) = self
                .terminals
                .get(id)
                .and_then(|panel| panel.read(cx).cwd())
        {
            return Some(cwd);
        }
        self.last_cwd.clone()
    }

    fn upsert_workspace(&mut self, info: WorkspaceInfo) {
        if let Some(slot) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == info.id)
        {
            *slot = info;
        } else {
            self.workspaces.push(info);
        }
    }

    /// Remove dock panels without `pty_close` / `editor_close`. Idle sessions
    /// stay on the daemon.
    fn release_dock(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let terminals: Vec<_> = self.terminals.values().cloned().collect();
        let editors: Vec<_> = self.editors.values().cloned().collect();
        for panel in terminals {
            panel.update(cx, |panel, _| panel.release());
            self.dock
                .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
        }
        for panel in editors {
            panel.update(cx, |panel, _| panel.release());
            self.dock
                .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
        }
        self.terminals.clear();
        self.editors.clear();
        self.active = None;
        self.next_terminal_number = 1;
    }

    fn capture_layout(&self, cx: &App) -> (Vec<WorkspaceTab>, u32) {
        let mut tabs = Vec::new();
        let mut seen_pty = HashSet::new();
        let mut seen_path = HashSet::new();
        for id in self.panel_order(cx) {
            if let Some((pty_id, panel)) = self
                .terminals
                .iter()
                .find(|(_, panel)| PanelId::from(panel.entity_id()) == id)
            {
                seen_pty.insert(pty_id.clone());
                tabs.push(WorkspaceTab {
                    kind: WorkspaceTabKind::Terminal,
                    title: panel.read(cx).label().to_string(),
                    pty_id: Some(pty_id.clone()),
                    path: None,
                });
            } else if let Some((path, _)) = self
                .editors
                .iter()
                .find(|(_, panel)| PanelId::from(panel.entity_id()) == id)
            {
                seen_path.insert(path.clone());
                let title = path
                    .rsplit(['/', '\\'])
                    .find(|seg| !seg.is_empty())
                    .unwrap_or(path)
                    .to_owned();
                tabs.push(WorkspaceTab {
                    kind: WorkspaceTabKind::Editor,
                    title,
                    pty_id: None,
                    path: Some(path.clone()),
                });
            }
        }
        for (pty_id, panel) in &self.terminals {
            if seen_pty.insert(pty_id.clone()) {
                tabs.push(WorkspaceTab {
                    kind: WorkspaceTabKind::Terminal,
                    title: panel.read(cx).label().to_string(),
                    pty_id: Some(pty_id.clone()),
                    path: None,
                });
            }
        }
        for path in self.editors.keys() {
            if seen_path.insert(path.clone()) {
                let title = path
                    .rsplit(['/', '\\'])
                    .find(|seg| !seg.is_empty())
                    .unwrap_or(path)
                    .to_owned();
                tabs.push(WorkspaceTab {
                    kind: WorkspaceTabKind::Editor,
                    title,
                    pty_id: None,
                    path: Some(path.clone()),
                });
            }
        }
        let active_id = self.active_panel_id();
        let active_tab = active_id
            .and_then(|id| {
                self.panel_order(cx)
                    .iter()
                    .position(|item| *item == id)
                    .map(|ix| ix as u32)
            })
            .unwrap_or(0);
        (tabs, active_tab)
    }

    fn publish_layout(&mut self, cx: &App) {
        if self.restoring || !self.workspace_cap {
            return;
        }
        let Some(id) = self.active_workspace_id.clone() else {
            return;
        };
        let (tabs, active_tab) = self.capture_layout(cx);
        if let Some(workspace) = self
            .workspaces
            .iter_mut()
            .find(|workspace| workspace.id == id)
        {
            workspace.tab_count = tabs.len() as u32;
            workspace.pty_count = tabs
                .iter()
                .filter(|tab| tab.kind == WorkspaceTabKind::Terminal)
                .count() as u32;
        }
        self.ade.send(AdeCmd::SetWorkspaceLayout {
            id,
            tabs,
            active_tab,
        });
    }

    fn finish_restore_if_idle(&mut self, cx: &App) {
        if self.restoring && self.pending_editors.is_empty() {
            self.restoring = false;
            self.publish_layout(cx);
        }
    }

    fn switch_to(&mut self, id: String, cx: &mut Context<Self>) {
        if !self.workspace_cap || !matches!(self.connection, ConnectionState::Online) {
            return;
        }
        if self.active_workspace_id.as_deref() == Some(id.as_str()) {
            return;
        }
        let from = if self.restoring {
            None
        } else {
            self.active_workspace_id.clone()
        };
        let (tabs, active_tab) = if from.is_some() {
            self.capture_layout(cx)
        } else {
            (Vec::new(), 0)
        };
        self.restoring = true;
        self.pty_opens_pending = 0;
        self.pending_editors.clear();
        self.renaming_id = None;
        self.rename_pty = None;
        // The window is only available from event handlers. `switch_to` is
        // called from those, but `Context` does not hand us a window here.
        // Panels are released when `workspace_switched` restores. Until then
        // the old dock stays on screen and is replaced in `restore_workspace`.
        self.ade.send(AdeCmd::SwitchWorkspace {
            id,
            from,
            tabs,
            active_tab,
        });
    }

    fn restore_workspace(
        &mut self,
        attached: AttachedWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let AttachedWorkspace {
            info,
            tabs,
            active_tab,
            ptys,
        } = attached;
        self.restoring = true;
        self.pty_opens_pending = 0;
        self.pending_editors.clear();
        self.release_dock(window, cx);
        self.session_id = Some(info.session_id.clone());
        self.active_workspace_id = Some(info.id.clone());
        self.explorer_cache.clear();
        self.selection.clear();
        self.anchor = None;
        self.explorer_root = info.root.clone();
        let root = info.root.clone();
        self.upsert_workspace(info);
        self.rebuild_tree(cx);
        self.ade.send(AdeCmd::ListDir {
            request_id: next_id("ex"),
            path: root,
        });

        let live: HashSet<String> = ptys.iter().map(|pty| pty.id.clone()).collect();
        let mut seen = HashSet::new();
        let mut editor_jobs = Vec::new();
        for (index, tab) in tabs.into_iter().enumerate() {
            match tab.kind {
                WorkspaceTabKind::Terminal => {
                    let Some(pty_id) = tab.pty_id else {
                        continue;
                    };
                    if !live.contains(&pty_id) || !seen.insert(pty_id.clone()) {
                        continue;
                    }
                    self.attach_terminal(pty_id, tab.title, window, cx);
                }
                WorkspaceTabKind::Editor => {
                    if let Some(path) = tab.path {
                        editor_jobs.push((path, index as u32 == active_tab));
                    }
                }
            }
        }
        for PtyInfo { id, .. } in ptys {
            if seen.insert(id.clone()) {
                let n = self.next_terminal_number;
                self.attach_terminal(id, n.to_string(), window, cx);
            }
        }

        let expect_editors = !editor_jobs.is_empty();
        if self.terminals.is_empty() && !expect_editors {
            self.restoring = false;
            self.new_terminal(cx);
        } else if !self.terminals.is_empty()
            && (active_tab as usize) < self.terminals.len()
            && editor_jobs.is_empty()
        {
            let order = self.panel_order(cx);
            if let Some(id) = order.get(active_tab as usize).cloned() {
                self.dock
                    .update(cx, |dock, cx| dock.select_panel(id, window, cx));
            }
        }
        for (path, activate) in editor_jobs {
            self.open_editor(path, false, activate);
        }
        if !expect_editors {
            self.restoring = false;
            if !self.panes_empty() {
                self.publish_layout(cx);
            }
        }
    }

    fn attach_terminal(
        &mut self,
        pty_id: String,
        title: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let parsed = title.parse::<u32>().ok().filter(|n| *n > 0);
        let number = parsed.unwrap_or(self.next_terminal_number.max(1));
        if number >= self.next_terminal_number {
            self.next_terminal_number = number.saturating_add(1);
        }
        let workspace = cx.weak_entity();
        let ade = self.ade.clone();
        let panel = cx.new(|cx| TerminalPanel::new(pty_id.clone(), number, ade, workspace, cx));
        let workspace_id = self.active_workspace_id.clone();
        let custom = parsed.is_none() && !title.is_empty() && title != number.to_string();
        panel.update(cx, |panel, cx| {
            panel.bind_workspace(workspace_id);
            if custom {
                panel.set_custom_title(title, cx);
            }
        });
        self.dock.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            );
        });
        self.terminals.insert(pty_id, panel);
    }

    fn open_editor(&mut self, path: String, preview: bool, activate: bool) {
        let (path, line, column) = parse_goto_spec(&path);
        if path.is_empty() {
            return;
        }
        let request_id = next_id("ed");
        self.pending_editors.insert(request_id.clone(), activate);
        self.ade.send(AdeCmd::OpenEditor {
            request_id,
            path,
            preview,
            line,
            column,
        });
    }

    fn open_create_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.workspace_cap {
            self.status = "This daemon has no workspace capability".into();
            return;
        }
        self.create_open = true;
        self.palette_open = false;
        self.goto_open = false;
        self.rename_pty = None;
        self.renaming_id = None;
        self.create_name.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        self.create_root
            .update(cx, |state, cx| state.set_value("", window, cx));
    }

    fn confirm_create(&mut self, cx: &mut Context<Self>) {
        let name = self.create_name.read(cx).value().to_string();
        let root = self.create_root.read(cx).value().to_string();
        self.create_open = false;
        self.ade.send(AdeCmd::CreateWorkspace { name, root });
    }

    fn begin_workspace_rename(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(id) = self.active_workspace_id.clone() else {
            self.status = "No workspace to rename".into();
            return;
        };
        self.begin_workspace_rename_for(&id, window, cx);
    }

    fn begin_workspace_rename_for(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let name = self
            .workspaces
            .iter()
            .find(|workspace| workspace.id == id)
            .map(|workspace| workspace.name.clone());
        let Some(name) = name else {
            self.status = "No workspace to rename".into();
            return;
        };
        self.renaming_id = Some(id.to_string());
        self.rename_pty = None;
        self.palette_open = false;
        self.create_open = false;
        self.ws_rename_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
            state.focus(window, cx);
        });
    }

    fn confirm_workspace_rename(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.renaming_id.take() else {
            return;
        };
        let name = self.ws_rename_input.read(cx).value().to_string();
        if name.trim().is_empty() {
            self.status = "Workspace name is empty".into();
            return;
        }
        self.ade.send(AdeCmd::RenameWorkspace { id, name });
    }

    fn close_workspace(&mut self, id: String) {
        if !self.workspace_cap {
            return;
        }
        if self.workspaces.len() <= 1 {
            self.status = "Cannot close the last workspace".into();
            return;
        }
        self.ade.send(AdeCmd::CloseWorkspace { id });
    }

    fn close_active_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.active {
            Some(ActiveSurface::Terminal(id)) => {
                if let Some(panel) = self.terminals.get(id).cloned() {
                    self.dock
                        .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                }
            }
            Some(ActiveSurface::Editor(path)) => {
                if let Some(panel) = self.editors.get(path).cloned() {
                    self.dock
                        .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
                }
            }
            None => {}
        }
    }

    fn cycle_tab(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let order = self.panel_order(cx);
        if order.is_empty() {
            return;
        }
        let current = self.active_panel_id();
        let ix = current
            .and_then(|id| order.iter().position(|item| *item == id))
            .unwrap_or(0);
        let len = order.len() as isize;
        let next = (ix as isize + delta).rem_euclid(len) as usize;
        let id = order[next];
        self.dock
            .update(cx, |dock, cx| dock.select_panel(id, window, cx));
    }

    fn save_active(&mut self, cx: &mut Context<Self>) {
        let Some(ActiveSurface::Editor(path)) = &self.active else {
            return;
        };
        let Some(panel) = self.editors.get(path).cloned() else {
            return;
        };
        let (buffer_id, rev, text, dirty) = {
            let panel = panel.read(cx);
            (
                panel.buffer_id().to_string(),
                panel.rev(),
                panel.editor().read(cx).value().to_string(),
                panel.is_dirty(),
            )
        };
        if !dirty {
            self.status = "No changes".into();
            cx.notify();
            return;
        }
        self.ade.send(AdeCmd::EditBuffer {
            request_id: next_id("ed"),
            buffer_id: buffer_id.clone(),
            base_rev: rev,
            text,
        });
        self.ade.send(AdeCmd::SaveBuffer {
            request_id: next_id("sv"),
            buffer_id,
            base_rev: rev + 1,
        });
        self.status = "Saving…".into();
        cx.notify();
    }

    fn open_path(&mut self, path: String, preview: bool) {
        self.open_editor(path, preview, true);
    }

    fn open_settings(&mut self) {
        if let Some(path) = self.config_path.clone() {
            self.open_path(path, false);
        } else {
            self.status = "Backend did not send config_path".into();
        }
    }

    pub(crate) fn begin_terminal_rename(
        &mut self,
        pty_id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.terminals.get(pty_id).cloned() else {
            return;
        };
        let current = panel.read(cx).label().to_string();
        self.renaming_id = None;
        self.rename_pty = Some(pty_id.to_string());
        self.palette_open = false;
        self.goto_open = false;
        self.rename_input.update(cx, |state, cx| {
            state.set_value(current, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn confirm_rename(&mut self, cx: &mut Context<Self>) {
        let Some(pty) = self.rename_pty.clone() else {
            return;
        };
        let title = self.rename_input.read(cx).value().to_string();
        let title = title.trim().to_string();
        if title.is_empty() {
            self.status = "Title cannot be empty".into();
            cx.notify();
            return;
        }
        self.rename_pty = None;
        if let Some(panel) = self.terminals.get(&pty).cloned() {
            panel.update(cx, |panel, cx| panel.set_custom_title(title, cx));
            self.status = "Renamed terminal".into();
            self.publish_layout(cx);
        }
        cx.notify();
    }

    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ade.send(AdeCmd::Disconnect);
        let (ade, evt_rx) = super::ade::spawn(self.target.clone());
        self.ade = ade;
        self.connection = ConnectionState::Connecting;
        self.status = "Reconnecting…".into();
        self.release_dock(window, cx);
        self.workspaces.clear();
        self.active_workspace_id = None;
        self.session_id = None;
        self.pty_opens_pending = 0;
        self.pending_editors.clear();
        self.restoring = false;
        self.renaming_id = None;
        self.rail_hover = None;
        self.create_open = false;
        self.explorer_cache.clear();
        self.explorer_root.clear();
        self.selection.clear();
        self.anchor = None;
        self._recv_task = cx.spawn_in(window, async move |this, cx| {
            while let Ok(ev) = evt_rx.recv().await {
                if cx
                    .update(|window, app| {
                        this.update(app, |this, cx| this.handle_event(ev, window, cx))
                    })
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    fn visible_tree_ids(&self, cx: &App) -> Vec<String> {
        let state = self.explorer.read(cx);
        let mut raw = Vec::new();
        let mut ix = 0;
        while let Some(entry) = state.entry(ix) {
            raw.push(entry.item().id.to_string());
            ix += 1;
        }
        real_ids(raw.iter().map(String::as_str))
    }

    /// Highlight `highlight` when set, otherwise the last selected path.
    ///
    /// Deferred so a context menu — invoked while the tree entity is already
    /// updating — can still move the built-in row highlight.
    fn sync_tree_highlight(&mut self, highlight: Option<String>, cx: &mut Context<Self>) {
        let primary = highlight.or_else(|| self.selection.last().cloned());
        let explorer = self.explorer.clone();
        cx.defer(move |app| {
            explorer.update(app, |state, cx| {
                let ix = primary.as_ref().and_then(|path| {
                    let id = SharedString::from(path.clone());
                    state.index_of(&id)
                });
                state.set_selected_index(ix, cx);
            });
        });
    }

    fn apply_tree_click(
        &mut self,
        path: &str,
        is_folder: bool,
        gesture: super::explorer::SelectGesture,
        cx: &mut Context<Self>,
    ) {
        if is_placeholder(path) {
            return;
        }
        let visible = self.visible_tree_ids(cx);
        let (next, anchor) = apply_selection(
            &self.selection,
            self.anchor.as_deref(),
            &visible,
            path,
            gesture,
        );
        self.selection = next;
        self.anchor = anchor;
        let highlight =
            (gesture == super::explorer::SelectGesture::Range).then(|| path.to_string());
        self.sync_tree_highlight(highlight, cx);
        if is_folder {
            if !self.explorer_cache.contains_key(path) {
                self.ade.send(AdeCmd::ListDir {
                    request_id: next_id("ex"),
                    path: path.to_string(),
                });
            }
        } else if gesture == super::explorer::SelectGesture::Replace {
            self.open_path(path.to_string(), true);
        }
    }

    fn ensure_context_selection(&mut self, path: &str, cx: &mut Context<Self>) {
        if is_placeholder(path) || self.selection.iter().any(|item| item == path) {
            return;
        }
        self.selection = vec![path.to_string()];
        self.anchor = Some(path.to_string());
        self.sync_tree_highlight(None, cx);
    }

    fn copy_path_text(&mut self, paths: &[String], cx: &mut Context<Self>) {
        if paths.is_empty() {
            self.status = "No selection".into();
            cx.notify();
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(absolute_paths_text(paths)));
        self.status = if paths.len() == 1 {
            "Copied path".into()
        } else {
            format!("Copied {} paths", paths.len()).into()
        };
        cx.notify();
    }

    fn arm_file_copy(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            self.status = "No selection".into();
            cx.notify();
            return;
        }
        let text = absolute_paths_text(&paths);
        let path_bufs: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
        cx.write_to_clipboard(ClipboardItem {
            entries: vec![
                ClipboardEntry::String(ClipboardString::new(text)),
                ClipboardEntry::ExternalPaths(ExternalPaths(path_bufs.into())),
            ],
        });
        let count = paths.len();
        self.file_clipboard = Some(paths);
        self.status = format!("Copied {count} item(s) — paste in the explorer").into();
        cx.notify();
    }

    fn selected_or_primary(&self) -> Vec<String> {
        self.selection.clone()
    }

    fn copy_explorer_selection(&mut self, cx: &mut Context<Self>) {
        self.arm_file_copy(self.selected_or_primary(), cx);
    }

    fn is_dir(&self, path: &str) -> bool {
        if self.explorer_cache.contains_key(path) {
            return true;
        }
        self.explorer_cache.values().any(|entries| {
            entries
                .iter()
                .any(|entry| entry.path == path && entry.kind == FsKind::Dir)
        })
    }

    fn paste_destination(&self) -> Option<String> {
        let primary = self
            .selection
            .last()
            .cloned()
            .or_else(|| self.anchor.clone());
        if let Some(path) = primary {
            if self.is_dir(&path) {
                return Some(path);
            }
            if let Some(parent) = parent_dir(&path) {
                return Some(parent);
            }
        }
        if self.explorer_root.is_empty() {
            None
        } else {
            Some(self.explorer_root.clone())
        }
    }

    fn paste_explorer(&mut self, cx: &mut Context<Self>) {
        let Some(sources) = self.file_clipboard.clone() else {
            self.status = "Nothing to paste".into();
            cx.notify();
            return;
        };
        let Some(destination) = self.paste_destination() else {
            self.status = "Explorer is not ready".into();
            cx.notify();
            return;
        };
        let sources = copyable_sources(&sources, &destination);
        if sources.is_empty() {
            self.status = "Cannot paste into that folder".into();
            cx.notify();
            return;
        }
        self.send_fs(false, sources, destination, cx);
    }

    fn move_into_folder(
        &mut self,
        sources: Vec<String>,
        destination: String,
        cx: &mut Context<Self>,
    ) {
        let sources = movable_sources(&sources, &destination);
        if sources.is_empty() {
            self.status = "Cannot move into that folder".into();
            cx.notify();
            return;
        }
        self.send_fs(true, sources, destination, cx);
    }

    fn send_fs(
        &mut self,
        move_files: bool,
        sources: Vec<String>,
        destination: String,
        cx: &mut Context<Self>,
    ) {
        let request_id = next_id(if move_files { "mv" } else { "cp" });
        self.pending_fs.insert(
            request_id.clone(),
            PendingFs {
                sources: sources.clone(),
                destination: destination.clone(),
            },
        );
        if move_files {
            self.ade.send(AdeCmd::MovePaths {
                request_id,
                sources,
                destination,
            });
            self.status = "Moving…".into();
        } else {
            self.ade.send(AdeCmd::CopyPaths {
                request_id,
                sources,
                destination,
            });
            self.status = "Copying…".into();
        }
        cx.notify();
    }

    fn finish_fs(
        &mut self,
        request_id: &str,
        entries: Vec<FsEntry>,
        moved: bool,
        cx: &mut Context<Self>,
    ) {
        let pending = self.pending_fs.remove(request_id);
        let mut dirs = Vec::new();
        if let Some(pending) = &pending {
            for src in &pending.sources {
                if let Some(parent) = parent_dir(src) {
                    dirs.push(parent);
                }
            }
            dirs.push(pending.destination.clone());
        }
        for entry in &entries {
            if let Some(parent) = parent_dir(&entry.path) {
                dirs.push(parent);
            }
        }
        dirs.sort();
        dirs.dedup();
        for dir in dirs {
            self.relist(&dir);
        }
        let new_paths: Vec<String> = entries.iter().map(|entry| entry.path.clone()).collect();
        if !new_paths.is_empty() {
            self.anchor = new_paths.last().cloned();
            self.selection = new_paths;
        }
        self.status = if moved {
            format!("Moved {} item(s)", entries.len()).into()
        } else {
            format!("Copied {} item(s)", entries.len()).into()
        };
        cx.notify();
    }

    fn relist(&mut self, dir: &str) {
        if dir.is_empty() {
            return;
        }
        let prefix = format!("{dir}/");
        self.explorer_cache
            .retain(|key, _| key != dir && !key.starts_with(&prefix));
        self.ade.send(AdeCmd::ListDir {
            request_id: next_id("ex"),
            path: dir.to_string(),
        });
    }

    fn on_new_terminal(&mut self, _: &NewTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal(cx);
        cx.notify();
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_active_tab(window, cx);
    }

    fn on_save(&mut self, _: &SaveBuffer, _: &mut Window, cx: &mut Context<Self>) {
        self.save_active(cx);
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        cx.notify();
    }

    fn on_toggle_palette(
        &mut self,
        _: &ToggleCommandPalette,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.palette_open = !self.palette_open;
        if self.palette_open {
            self.goto_open = false;
            self.rename_pty = None;
            self.command_state.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn on_goto_file(&mut self, _: &GoToFile, window: &mut Window, cx: &mut Context<Self>) {
        self.goto_open = !self.goto_open;
        if self.goto_open {
            self.palette_open = false;
            self.rename_pty = None;
            self.goto_input.update(cx, |state, cx| {
                state.focus(window, cx);
            });
        }
        cx.notify();
    }

    fn confirm_goto(&mut self, cx: &mut Context<Self>) {
        let query = self.goto_input.read(cx).value().to_string();
        self.goto_open = false;
        self.open_path(query, false);
        cx.notify();
    }

    fn on_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.open_settings();
        cx.notify();
    }

    fn on_reconnect(&mut self, _: &Reconnect, window: &mut Window, cx: &mut Context<Self>) {
        self.reconnect(window, cx);
        cx.notify();
    }

    fn on_disconnect(&mut self, _: &Disconnect, _: &mut Window, cx: &mut Context<Self>) {
        self.ade.send(AdeCmd::Disconnect);
        self.connection = ConnectionState::Offline {
            reason: "Disconnected".into(),
        };
        self.status = "Disconnected".into();
        cx.notify();
    }

    fn on_next_tab(&mut self, _: &NextTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(1, window, cx);
    }

    fn on_prev_tab(&mut self, _: &PrevTab, window: &mut Window, cx: &mut Context<Self>) {
        self.cycle_tab(-1, window, cx);
    }

    fn on_copy_explorer(&mut self, _: &CopyExplorer, _: &mut Window, cx: &mut Context<Self>) {
        self.copy_explorer_selection(cx);
    }

    fn on_paste_explorer(&mut self, _: &PasteExplorer, _: &mut Window, cx: &mut Context<Self>) {
        self.paste_explorer(cx);
    }

    fn on_new_workspace(&mut self, _: &NewWorkspace, window: &mut Window, cx: &mut Context<Self>) {
        self.open_create_dialog(window, cx);
        cx.notify();
    }

    fn on_rename_workspace(
        &mut self,
        _: &RenameWorkspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_workspace_rename(window, cx);
        cx.notify();
    }

    fn on_close_workspace(&mut self, _: &CloseWorkspace, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_workspace_id.clone() {
            self.close_workspace(id);
        } else {
            self.status = "No workspace to close".into();
        }
        cx.notify();
    }

    fn note_rail_hover(&mut self, id: &str, hovered: bool, cx: &mut Context<Self>) {
        if hovered {
            if self.rail_hover.as_deref() != Some(id) {
                self.rail_hover = Some(id.to_string());
                cx.notify();
            }
        } else if self.rail_hover.as_deref() == Some(id) {
            self.rail_hover = None;
            cx.notify();
        }
    }

    fn render_workspace_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let home = user_home();
        let hint = if matches!(self.connection, ConnectionState::Online) {
            workspace_rail_hint(self.workspaces.len())
        } else {
            None
        };
        let rows = self
            .workspaces
            .iter()
            .map(|workspace| self.render_workspace_row(workspace, home.as_deref(), cx))
            .collect::<Vec<_>>();
        let muted = cx.theme().muted_foreground;

        v_flex()
            .id("workspace-rail")
            .w(px(WORKSPACE_RAIL_W))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .h(px(SIDEBAR_HEADER_H))
                    .px_2()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_xs()
                            .font_semibold()
                            .text_color(muted)
                            .child("Workspaces"),
                    )
                    .child(
                        Button::new("workspace-create")
                            .ghost()
                            .xsmall()
                            .icon(IconName::Plus)
                            .tooltip("New Workspace")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_create_dialog(window, cx);
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .id("workspace-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows),
            )
            .when_some(hint, |rail, hint| {
                rail.child(
                    div()
                        .id("workspace-rail-hint")
                        .w_full()
                        .flex_shrink_0()
                        .px_2()
                        .py_2()
                        .border_t_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .text_color(muted)
                        .child(hint),
                )
            })
    }

    fn render_workspace_row(
        &self,
        workspace: &WorkspaceInfo,
        home: Option<&str>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = workspace.id.clone();
        let active = self.active_workspace_id.as_deref() == Some(workspace.id.as_str());
        let hovered = self.rail_hover.as_deref() == Some(workspace.id.as_str());
        let renaming = self.renaming_id.as_deref() == Some(workspace.id.as_str());
        let can_close = self.workspaces.len() > 1;
        let name = workspace.name.clone();
        let root_label = workspace_root_label(&workspace.root, home);
        let row_id = format!("ws-row-{}", workspace.id);
        let fill = if active && hovered {
            Some(cx.theme().accent.opacity(0.28))
        } else if active {
            Some(cx.theme().accent.opacity(0.20))
        } else if hovered {
            Some(cx.theme().foreground.opacity(0.06))
        } else {
            None
        };
        let accent = cx.theme().accent.opacity(if active { 1. } else { 0. });
        let view = cx.entity();
        let menu_id = id.clone();
        let hover_id = id.clone();

        h_flex()
            .id(row_id)
            .w_full()
            .min_h(px(WORKSPACE_ROW_H))
            .items_stretch()
            .cursor_pointer()
            .when_some(fill, |row, color| row.bg(color))
            .on_hover(cx.listener(move |this, hovered, _, cx| {
                this.note_rail_hover(&hover_id, *hovered, cx);
            }))
            .on_click({
                let id = id.clone();
                cx.listener(move |this, _, _, cx| {
                    this.switch_to(id.clone(), cx);
                    cx.notify();
                })
            })
            .child(
                div()
                    .w(px(3.))
                    .min_h(px(WORKSPACE_ROW_H))
                    .flex_shrink_0()
                    .bg(accent),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .py(px(4.))
                    .pl(px(8.))
                    .pr_1()
                    .justify_center()
                    .gap(px(1.))
                    .child(if renaming {
                        self.render_workspace_rename(&id, cx).into_any_element()
                    } else {
                        self.render_workspace_name_line(
                            &id,
                            &name,
                            active,
                            hovered && !renaming,
                            can_close,
                            cx,
                        )
                        .into_any_element()
                    })
                    .child(
                        div()
                            .w_full()
                            .min_w_0()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(root_label),
                    ),
            )
            .context_menu(move |menu, _, _| {
                let rename_id = menu_id.clone();
                let close_id = menu_id.clone();
                let rename_view = view.clone();
                let close_view = view.clone();
                menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                    rename_view.update(cx, |this, cx| {
                        this.begin_workspace_rename_for(&rename_id, window, cx);
                        cx.notify();
                    });
                }))
                .item(
                    PopupMenuItem::new("Close")
                        .disabled(!can_close)
                        .on_click(move |_, _, cx| {
                            close_view.update(cx, |this, cx| {
                                this.close_workspace(close_id.clone());
                                cx.notify();
                            });
                        }),
                )
            })
            .into_any_element()
    }

    fn render_workspace_name_line(
        &self,
        id: &str,
        name: &str,
        active: bool,
        show_actions: bool,
        can_close: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        h_flex()
            .w_full()
            .min_w_0()
            .items_center()
            .gap_1()
            .child(
                div()
                    .id(format!("ws-name-{id}"))
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .when(active, |label| label.font_semibold())
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(name.to_string()),
            )
            .when(show_actions, |line| {
                line.child(self.render_workspace_actions(id, can_close, cx))
            })
    }

    fn render_workspace_actions(
        &self,
        id: &str,
        can_close: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let rename_id = id.to_string();
        let close_id = id.to_string();
        h_flex()
            .flex_shrink_0()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                Button::new(format!("ws-rename-{id}"))
                    .ghost()
                    .xsmall()
                    .label("Rename")
                    .tooltip("Rename workspace")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.begin_workspace_rename_for(&rename_id, window, cx);
                        cx.notify();
                    })),
            )
            .when(can_close, |line| {
                line.child(
                    Button::new(format!("ws-close-{close_id}"))
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip("Close workspace")
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.close_workspace(close_id.clone());
                            cx.notify();
                        })),
                )
            })
    }

    fn render_workspace_rename(&self, id: &str, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .gap_1()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.ws_rename_input)),
            )
            .child(
                Button::new(format!("ws-rename-ok-{id}"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Check)
                    .tooltip("Rename")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_workspace_rename(cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(format!("ws-rename-cancel-{id}"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.renaming_id = None;
                        cx.notify();
                    })),
            )
    }

    fn render_create_workspace(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let name_value = self.create_name.read(cx).value().to_string();
        let root_value = self.create_root.read(cx).value().to_string();
        let name_hint = if name_value.trim().is_empty() {
            empty_workspace_name_hint(&root_value)
        } else {
            "Display name in the spaces list.".to_string()
        };
        let muted = cx.theme().muted_foreground;

        div()
            .id("workspace-create-overlay")
            .absolute()
            .inset_0()
            .bg(cx.theme().background.opacity(0.35))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.create_open = false;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .absolute()
                    .left(px(8.))
                    .top(px(TITLE_BAR_H + SIDEBAR_HEADER_H + 4.))
                    .w(px(320.))
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_sm().font_semibold().child("New Workspace"))
                    .child(div().text_xs().font_semibold().child("Name"))
                    .child(Input::new(&self.create_name))
                    .child(div().text_xs().text_color(muted).child(name_hint))
                    .child(div().text_xs().font_semibold().child("Project root"))
                    .child(Input::new(&self.create_root))
                    .child(
                        div().text_xs().text_color(muted).child(
                            "Absolute path on the daemon. Empty uses the daemon project root.",
                        ),
                    )
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("ws-create-cancel")
                                    .ghost()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.create_open = false;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("ws-create-confirm")
                                    .primary()
                                    .label("Create")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.confirm_create(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
    }

    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(ACTIVITY_RAIL_W))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .gap(px(0.))
            .py_1()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("act-explorer")
                    .ghost()
                    .small()
                    .icon(IconName::Folder)
                    .tooltip("Explorer")
                    .selected(self.activity == Activity::Explorer && !self.sidebar_collapsed)
                    .on_click(cx.listener(|this, _, _, cx| {
                        if this.activity == Activity::Explorer {
                            this.sidebar_collapsed = !this.sidebar_collapsed;
                        } else {
                            this.activity = Activity::Explorer;
                            this.sidebar_collapsed = false;
                        }
                        cx.notify();
                    })),
            )
            .child(
                Button::new("act-settings")
                    .ghost()
                    .small()
                    .icon(IconName::Settings)
                    .tooltip("Settings")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.open_settings();
                        cx.notify();
                    })),
            )
    }

    fn render_explorer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let root_label = if self.explorer_root.is_empty() {
            "Explorer".to_string()
        } else {
            self.explorer_root
                .rsplit('/')
                .next()
                .unwrap_or("Explorer")
                .to_string()
        };
        let selected: HashSet<String> = self.selection.iter().cloned().collect();

        v_flex()
            .id("explorer-pane")
            .key_context("Explorer")
            .track_focus(&self.explorer_focus)
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .h(px(SIDEBAR_HEADER_H))
                    .px_2()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().font_semibold().child(root_label))
                    .child(
                        Button::new("collapse-sidebar")
                            .ghost()
                            .xsmall()
                            .icon(IconName::PanelLeftClose)
                            .tooltip("Hide Explorer (Ctrl+B)")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_collapsed = true;
                                cx.notify();
                            })),
                    ),
            )
            .child({
                let row_view = view.clone();
                let menu_view = view;
                tree(&self.explorer, move |ix, entry, _selected, _window, cx| {
                    let item = entry.item();
                    let is_folder = entry.is_folder();
                    let icon = if !is_folder {
                        IconName::File
                    } else if entry.is_expanded() {
                        IconName::FolderOpen
                    } else {
                        IconName::Folder
                    };
                    let path = item.id.to_string();
                    let label = item.label.clone();
                    let placeholder = is_placeholder(&path);
                    let in_selection = selected.contains(&path);
                    let view = row_view.clone();
                    let drag_view = view.clone();
                    let drop_view = view.clone();
                    let grabbed = path.clone();
                    let selected_now: Vec<String> = selected.iter().cloned().collect();
                    ListItem::new(ix)
                        .w_full()
                        .h(px(TREE_ROW_H))
                        .text_sm()
                        .rounded(cx.theme().radius)
                        .py_0()
                        .px_1()
                        .pl(px(8.) * entry.depth() + px(4.))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .items_center()
                                .when(in_selection, |this| {
                                    this.bg(cx.theme().accent.opacity(0.28))
                                })
                                .child(Icon::new(icon).small())
                                .child(label),
                        )
                        .when(!placeholder, |this| {
                            let path_for_drag = grabbed.clone();
                            this.on_drag(
                                ExplorerDrag {
                                    paths: drag_paths(&selected_now, &path_for_drag),
                                },
                                |drag, _, _, cx| {
                                    let count = drag.paths.len();
                                    cx.new(|_| ExplorerDragPreview { count })
                                },
                            )
                        })
                        .when(is_folder && !placeholder, |this| {
                            let dest = path.clone();
                            let drop_view = drop_view.clone();
                            this.drag_over::<ExplorerDrag>(|style, _, _, cx| {
                                style.bg(cx.theme().accent.opacity(0.35))
                            })
                            .on_drop(
                                move |drag: &ExplorerDrag, _, cx| {
                                    let paths = drag.paths.clone();
                                    let dest = dest.clone();
                                    drop_view.update(cx, |this, cx| {
                                        this.move_into_folder(paths, dest, cx);
                                    });
                                },
                            )
                        })
                        .on_click({
                            let path = path.clone();
                            move |event, window, cx| {
                                let mods = event.modifiers();
                                let gesture = gesture_from_modifiers(
                                    mods.shift,
                                    mods.control || mods.platform,
                                );
                                let focus = drag_view.update(cx, |this, cx| {
                                    this.apply_tree_click(&path, is_folder, gesture, cx);
                                    this.explorer_focus.clone()
                                });
                                window.focus(&focus, cx);
                            }
                        })
                })
                .context_menu({
                    let view = menu_view;
                    move |_ix, entry, menu, _window, cx| {
                        let path = entry.item().id.to_string();
                        if is_placeholder(&path) {
                            return menu;
                        }
                        let paths = view.update(cx, |this, cx| {
                            this.ensure_context_selection(&path, cx);
                            this.selection.clone()
                        });
                        let can_paste = view.read(cx).file_clipboard.is_some();
                        let copy_paths = paths.clone();
                        let file_paths = paths;
                        menu.item(PopupMenuItem::new("Copy Path").on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                view.update(cx, |this, cx| this.copy_path_text(&copy_paths, cx));
                            }
                        }))
                        .item(PopupMenuItem::new("Copy").on_click({
                            let view = view.clone();
                            let file_paths = file_paths.clone();
                            move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.arm_file_copy(file_paths.clone(), cx)
                                });
                            }
                        }))
                        .item(
                            PopupMenuItem::new("Paste").disabled(!can_paste).on_click({
                                let view = view.clone();
                                move |_, _, cx| {
                                    view.update(cx, |this, cx| this.paste_explorer(cx));
                                }
                            }),
                        )
                    }
                })
                .text_sm()
                .p_0()
                .flex_1()
                .min_h_0()
            })
    }

    fn render_empty(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().background)
            .child(
                v_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_lg().font_bold().child("fresh-gui"))
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(match &self.connection {
                                ConnectionState::Connecting => {
                                    "Connecting to the ADE daemon…".to_string()
                                }
                                ConnectionState::Offline { reason } => {
                                    format!("Offline — {reason}")
                                }
                                ConnectionState::Online => {
                                    "Open a terminal with Ctrl+T, or a file from the explorer."
                                        .to_string()
                                }
                            }),
                    )
                    .when(matches!(self.connection, ConnectionState::Online), |this| {
                        this.child(
                            Button::new("empty-new-term")
                                .small()
                                .label("New Terminal")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.new_terminal(cx);
                                    cx.notify();
                                })),
                        )
                    }),
            )
    }

    fn render_palette(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let confirm = cx.entity();
        let cancel = cx.entity();
        let items = vec![
            ("New Terminal", Box::new(NewTerminal) as Box<dyn Action>),
            ("New Workspace", Box::new(NewWorkspace)),
            ("Rename Workspace", Box::new(RenameWorkspace)),
            ("Close Workspace", Box::new(CloseWorkspace)),
            ("Close Tab", Box::new(CloseTab)),
            ("Save", Box::new(SaveBuffer)),
            ("Toggle Sidebar", Box::new(ToggleSidebar)),
            ("Go to File…", Box::new(GoToFile)),
            ("Open Settings", Box::new(OpenSettings)),
            ("Reconnect", Box::new(Reconnect)),
            ("Disconnect", Box::new(Disconnect)),
        ];
        Command::new(&self.command_state)
            .placeholder("Type a command…")
            .bordered(true)
            .w(px(520.))
            .group(
                CommandGroup::new().label("Commands").items(
                    items
                        .into_iter()
                        .map(|(label, action)| CommandItem::new().label(label).action(action)),
                ),
            )
            .on_confirm(move |_, _, cx| {
                confirm.update(cx, |this, cx| {
                    this.palette_open = false;
                    cx.notify();
                });
            })
            .on_cancel(move |_, cx| {
                cancel.update(cx, |this, cx| {
                    this.palette_open = false;
                    cx.notify();
                });
            })
    }

    fn render_goto(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("goto-overlay")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt(px(80.))
            .bg(cx.theme().background.opacity(0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.goto_open = false;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .w(px(480.))
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_sm().font_bold().child("Go to File"))
                    .child(Input::new(&self.goto_input))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("goto-cancel").ghost().label("Cancel").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.goto_open = false;
                                    cx.notify();
                                }),
                            ))
                            .child(Button::new("goto-open").primary().label("Open").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.confirm_goto(cx);
                                }),
                            )),
                    ),
            )
    }

    fn render_rename(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("rename-overlay")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt(px(80.))
            .bg(cx.theme().background.opacity(0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.rename_pty = None;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .w(px(420.))
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_sm().font_bold().child("Rename Terminal"))
                    .child(Input::new(&self.rename_input))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("rename-cancel")
                                    .ghost()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.rename_pty = None;
                                        cx.notify();
                                    })),
                            )
                            .child(Button::new("rename-ok").primary().label("Rename").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.confirm_rename(cx);
                                }),
                            )),
                    ),
            )
    }

    fn connection_label(&self) -> String {
        match &self.connection {
            ConnectionState::Connecting => "connecting".into(),
            ConnectionState::Online => "online".into(),
            ConnectionState::Offline { .. } => "offline".into(),
        }
    }

    fn panes_empty(&self) -> bool {
        self.terminals.is_empty() && self.editors.is_empty()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let dialog_layer = Root::render_dialog_layer(window, cx);
        let notification_layer = Root::render_notification_layer(window, cx);
        let session = self
            .session_id
            .as_deref()
            .map(|s| {
                if s.len() > 8 {
                    format!("session {}", &s[..8])
                } else {
                    format!("session {s}")
                }
            })
            .unwrap_or_else(|| "no session".into());
        let workspace_label = self
            .workspaces
            .iter()
            .find(|workspace| Some(&workspace.id) == self.active_workspace_id.as_ref())
            .map(|workspace| workspace.name.clone())
            .unwrap_or_else(|| "no workspace".into());
        let caps = if self.capabilities.is_empty() {
            "—".into()
        } else {
            self.capabilities.join(" · ")
        };
        let dock = self.dock.clone();

        div()
            .id("workspace")
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_goto_file))
            .on_action(cx.listener(Self::on_settings))
            .on_action(cx.listener(Self::on_reconnect))
            .on_action(cx.listener(Self::on_disconnect))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_copy_explorer))
            .on_action(cx.listener(Self::on_paste_explorer))
            .on_action(cx.listener(Self::on_new_workspace))
            .on_action(cx.listener(Self::on_rename_workspace))
            .on_action(cx.listener(Self::on_close_workspace))
            .child(
                TitleBar::new().h(px(TITLE_BAR_H)).child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(div().text_sm().font_semibold().child("fresh-gui"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(self.target.chrome_label()),
                        ),
                ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .when(self.workspace_cap, |this| {
                        this.child(self.render_workspace_rail(cx))
                    })
                    .child(self.render_activity_bar(cx))
                    .when(!self.sidebar_collapsed, |this| {
                        this.child(self.render_explorer(cx))
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .relative()
                            .child(dock)
                            .when(self.panes_empty(), |this| this.child(self.render_empty(cx))),
                    ),
            )
            .child(
                StatusBar::new()
                    .h(px(STATUS_BAR_H))
                    .py_0()
                    .px_2()
                    .gap_1()
                    .left(self.status.clone())
                    .child(self.connection_label())
                    .right(caps)
                    .right(workspace_label)
                    .right(session),
            )
            .when(self.palette_open, |this| {
                this.child(
                    div()
                        .id("palette-overlay")
                        .absolute()
                        .inset_0()
                        .flex()
                        .justify_center()
                        .pt(px(80.))
                        .bg(cx.theme().background.opacity(0.45))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                this.palette_open = false;
                                cx.notify();
                            }),
                        )
                        .child(
                            div()
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation();
                                })
                                .child(self.render_palette(cx)),
                        ),
                )
            })
            .when(self.goto_open, |this| this.child(self.render_goto(cx)))
            .when(self.create_open, |this| {
                this.child(self.render_create_workspace(cx))
            })
            .when(self.rename_pty.is_some(), |this| {
                this.child(self.render_rename(cx))
            })
            .children(dialog_layer)
            .children(notification_layer)
    }
}

fn build_tree_items(root: &str, cache: &HashMap<String, Vec<FsEntry>>) -> Vec<TreeItem> {
    let Some(entries) = cache.get(root) else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|entry| {
            if entry.kind == FsKind::Dir {
                let children = if cache.contains_key(&entry.path) {
                    build_tree_items(&entry.path, cache)
                } else {
                    Vec::new()
                };
                TreeItem::new(entry.path.clone(), entry.name.clone())
                    .children(if children.is_empty() && !cache.contains_key(&entry.path) {
                        vec![TreeItem::new(format!("{}/.", entry.path), "…")]
                    } else {
                        children
                    })
                    .expanded(cache.contains_key(&entry.path))
            } else {
                TreeItem::new(entry.path.clone(), entry.name.clone())
            }
        })
        .collect()
}

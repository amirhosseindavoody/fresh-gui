//! Zed/VS Code-like ADE workspace: activity bar, explorer, docked tabs, status, palette.
//!
//! Editor and terminal surfaces are dock panels. Dragging a tab to a pane edge
//! splits; dropping it on a tab merges; dropping it in the strip reorders.
//! gpui-component refuses to drag the last remaining tab, so a split needs two
//! tabs. Terminal titles are herdr-style numbers for this client session.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use fresh_gui_protocol::{FsEntry, FsKind, Hello};
use gpui_kit::component::dock::{
    DockArea, DockPlacement, DockSkin, PanelId, PanelStyle, panel_handle,
};
use gpui_kit::component::menu::PopupMenuItem;
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
    CloseTab, CopyExplorer, Disconnect, GoToFile, NewTerminal, NextTab, OpenSettings,
    PasteExplorer, PrevTab, Reconnect, SaveBuffer, ToggleCommandPalette, ToggleSidebar,
};
use super::ade::{AdeCmd, AdeEvent, AdeHandle};
use super::connect::{ConnectTarget, parse_goto_spec};
use super::explorer::{
    absolute_paths_text, apply_selection, copyable_sources, drag_paths, gesture_from_modifiers,
    is_placeholder, movable_sources, parent_dir, real_ids,
};
use super::pane::{EditorPanel, TerminalPanel};

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
            rename_pty: None,
            rename_input,
            _subscriptions: vec![tree_sub, rename_sub],
            _recv_task: recv_task,
        }
    }

    fn handle_event(&mut self, ev: AdeEvent, window: &mut Window, cx: &mut Context<Self>) {
        match ev {
            AdeEvent::Connecting => {
                self.connection = ConnectionState::Connecting;
                self.status = "Connecting…".into();
            }
            AdeEvent::Connected { hello, session_id } => {
                self.apply_hello(&hello);
                self.session_id = Some(session_id);
                self.connection = ConnectionState::Online;
                self.status = "Online".into();
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
            AdeEvent::Disconnected { reason } => {
                self.connection = ConnectionState::Offline {
                    reason: reason.clone(),
                };
                self.status = format!("Disconnected: {reason}").into();
            }
            AdeEvent::PtyOpened { id, .. } => self.add_terminal_tab(id, window, cx),
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
                buffer_id,
                path,
                language,
                line,
                column,
                ..
            } => {
                self.begin_editor_tab(buffer_id, path, language, line, column, window, cx);
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.editors.get(&path).cloned() {
            panel.update(cx, |panel, cx| {
                panel.note_reopen(buffer_id, line, column, cx);
            });
            self.select_entity(&panel, window, cx);
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
        self.editors.insert(path, panel);
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
        if let Some(panel) = self.editor_by_buffer(&buffer_id, cx) {
            panel.update(cx, |panel, cx| {
                panel.apply_snapshot(rev, text, path, window, cx);
            });
            self.select_entity(&panel, window, cx);
            return;
        }
        if let Some(panel) = self.editors.get(&path).cloned() {
            panel.update(cx, |panel, cx| {
                panel.note_reopen(buffer_id.clone(), None, None, cx);
                panel.apply_snapshot(rev, text, path, window, cx);
            });
            self.select_entity(&panel, window, cx);
            return;
        }
        self.begin_editor_tab(buffer_id, path.clone(), None, None, None, window, cx);
        if let Some(panel) = self.editors.get(&path).cloned() {
            panel.update(cx, |panel, cx| {
                panel.apply_snapshot(rev, text, path, window, cx);
            });
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
        } else if matches!(&self.active, Some(ActiveSurface::Terminal(id)) if id == pty_id) {
            self.active = None;
        }
        cx.notify();
    }

    pub(crate) fn note_editor_active(&mut self, path: &str, active: bool, cx: &mut Context<Self>) {
        if active {
            self.active = Some(ActiveSurface::Editor(path.to_string()));
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
        let (path, line, column) = parse_goto_spec(&path);
        if path.is_empty() {
            return;
        }
        self.ade.send(AdeCmd::OpenEditor {
            request_id: next_id("ed"),
            path,
            preview,
            line,
            column,
        });
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
        }
        cx.notify();
    }

    fn reconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.ade.send(AdeCmd::Disconnect);
        let (ade, evt_rx) = super::ade::spawn(self.target.clone());
        self.ade = ade;
        self.connection = ConnectionState::Connecting;
        self.status = "Reconnecting…".into();
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

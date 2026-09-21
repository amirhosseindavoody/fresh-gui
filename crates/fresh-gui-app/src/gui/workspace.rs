//! Zed/VS Code-like ADE workspace: activity bar, explorer, tabs, status, palette.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use fresh_gui_protocol::{FsEntry, FsKind, Hello};
use gpui_kit::component::{
    ActiveTheme, Icon, IconName, Root, Selectable, Sizable, StyledExt, TitleBar,
    button::{Button, ButtonVariants as _},
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    input::{Editor, EditorState, Input, InputEvent, InputState},
    list::ListItem,
    status_bar::StatusBar,
    tab::{Tab, TabBar},
    tree::{TreeEvent, TreeItem, TreeState, tree},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{
    CloseTab, Disconnect, GoToFile, NewTerminal, NextTab, OpenSettings, PrevTab, Reconnect,
    SaveBuffer, ToggleCommandPalette, ToggleSidebar,
};
use super::ade::{AdeCmd, AdeEvent, AdeHandle};
use super::connect::{ConnectTarget, parse_goto_spec};
use super::osc7::feed_osc7_chunk;
use super::terminal::{TermScreen, keystroke_to_bytes};

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id(prefix: &str) -> String {
    format!("{prefix}-{}", NEXT_ID.fetch_add(1, Ordering::Relaxed))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activity {
    Explorer,
}

enum ShellTab {
    Terminal {
        #[allow(dead_code)]
        id: String,
        pty_id: String,
        title: String,
        cwd: Option<String>,
        osc_carry: String,
        screen: TermScreen,
        focus: FocusHandle,
    },
    Editor {
        #[allow(dead_code)]
        id: String,
        buffer_id: String,
        path: String,
        dirty: bool,
        rev: u64,
        pending: Option<EditorPending>,
        editor: Entity<EditorState>,
    },
}

#[derive(Clone)]
struct EditorPending {
    #[allow(dead_code)]
    path: String,
    #[allow(dead_code)]
    language: Option<String>,
    line: Option<u32>,
    column: Option<u32>,
}

enum ConnectionState {
    Connecting,
    Online,
    Offline { reason: String },
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
    tabs: Vec<ShellTab>,
    active_tab: usize,
    explorer: Entity<TreeState>,
    explorer_root: String,
    explorer_cache: HashMap<String, Vec<FsEntry>>,
    command_state: Entity<CommandState>,
    palette_open: bool,
    goto_open: bool,
    goto_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    _recv_task: Task<()>,
}

impl Workspace {
    pub fn new(target: ConnectTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (ade, evt_rx) = super::ade::spawn(target.clone());
        let explorer = cx.new(|cx| TreeState::new(cx));
        let command_state = cx.new(|cx| CommandState::new(window, cx));
        let goto_input = cx.new(|cx| InputState::new(window, cx).placeholder("path[:line[:col]]"));
        let tree_sub = cx.subscribe(&explorer, |this, _, ev: &TreeEvent, _cx| {
            if let TreeEvent::Expanded(id) = ev {
                let path = id.to_string();
                if !this.explorer_cache.contains_key(&path) {
                    this.ade.send(AdeCmd::ListDir {
                        request_id: next_id("ex"),
                        path,
                    });
                }
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
            tabs: Vec::new(),
            active_tab: 0,
            explorer,
            explorer_root: String::new(),
            explorer_cache: HashMap::new(),
            command_state,
            palette_open: false,
            goto_open: false,
            goto_input,
            _subscriptions: vec![tree_sub],
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
                if let Some(ix) = self.tabs.iter().position(|t| match t {
                    ShellTab::Terminal { pty_id, .. } => pty_id == &id,
                    _ => false,
                }) {
                    self.tabs.remove(ix);
                    if self.active_tab >= self.tabs.len() {
                        self.active_tab = self.tabs.len().saturating_sub(1);
                    }
                }
            }
            AdeEvent::FsListed { path, entries, .. } => {
                if self.explorer_root.is_empty() {
                    self.explorer_root = path.clone();
                }
                self.explorer_cache.insert(path, entries);
                self.rebuild_tree(cx);
            }
            AdeEvent::EditorOpened {
                buffer_id,
                path,
                language,
                line,
                column,
                ..
            } => {
                if let Some(ShellTab::Editor { pending, .. }) =
                    self.tabs.iter_mut().find(|t| match t {
                        ShellTab::Editor { buffer_id: bid, .. } => bid == &buffer_id,
                        _ => false,
                    })
                {
                    *pending = Some(EditorPending {
                        path,
                        language,
                        line,
                        column,
                    });
                } else {
                    self.begin_editor_tab(buffer_id, path, language, line, column, window, cx);
                }
            }
            AdeEvent::BufferSnapshot {
                buffer_id,
                rev,
                text,
                path,
            } => self.apply_snapshot(buffer_id, rev, text, path, window, cx),
            AdeEvent::BufferChanged { buffer_id, rev, .. } => {
                if let Some(ShellTab::Editor {
                    buffer_id: bid,
                    rev: r,
                    ..
                }) = self.find_editor_mut(&buffer_id)
                {
                    if bid == &buffer_id {
                        *r = rev;
                    }
                }
            }
            AdeEvent::BufferSaved {
                buffer_id,
                path,
                rev,
                ..
            } => {
                if let Some(ShellTab::Editor {
                    dirty,
                    rev: r,
                    path: p,
                    ..
                }) = self.find_editor_mut(&buffer_id)
                {
                    *dirty = false;
                    *r = rev;
                    *p = path;
                    self.status = "Saved".into();
                }
            }
            AdeEvent::Error { code, message } => {
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
        let n = self
            .tabs
            .iter()
            .filter(|t| matches!(t, ShellTab::Terminal { .. }))
            .count()
            + 1;
        let focus = cx.focus_handle();
        let tab = ShellTab::Terminal {
            id: next_id("tab"),
            pty_id,
            title: format!("Terminal {n}"),
            cwd: None,
            osc_carry: String::new(),
            screen: TermScreen::default(),
            focus: focus.clone(),
        };
        self.tabs.push(tab);
        self.active_tab = self.tabs.len() - 1;
        window.focus(&focus, cx);
    }

    fn on_pty_data(&mut self, pty_id: &str, bytes: &[u8], _cx: &mut Context<Self>) {
        if let Some(ShellTab::Terminal {
            screen,
            cwd,
            osc_carry,
            title,
            ..
        }) = self.tabs.iter_mut().find(|t| match t {
            ShellTab::Terminal { pty_id: id, .. } => id == pty_id,
            _ => false,
        }) {
            screen.feed(bytes);
            let chunk = String::from_utf8_lossy(bytes);
            if let Some(new_cwd) = feed_osc7_chunk(osc_carry, &chunk) {
                *cwd = Some(new_cwd.clone());
                if let Some(name) = new_cwd.rsplit('/').next() {
                    if !name.is_empty() {
                        *title = name.to_string();
                    }
                }
            }
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
        if let Some(ix) = self.tabs.iter().position(|t| match t {
            ShellTab::Editor { path: p, .. } => p == &path,
            _ => false,
        }) {
            self.active_tab = ix;
            return;
        }
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .line_number(true)
                .placeholder("Loading…");
            if let Some(lang) = language_from_path(&path, language.as_deref()) {
                state = state.language(lang);
            }
            state
        });
        let sub = cx.subscribe(&editor, |this, editor, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                if let Some(ShellTab::Editor {
                    editor: e, dirty, ..
                }) = this.tabs.iter_mut().find(|t| match t {
                    ShellTab::Editor { editor: ent, .. } => ent == &editor,
                    _ => false,
                }) {
                    if e == &editor {
                        *dirty = true;
                    }
                }
                cx.notify();
            }
        });
        self._subscriptions.push(sub);
        self.tabs.push(ShellTab::Editor {
            id: next_id("tab"),
            buffer_id,
            path,
            dirty: false,
            rev: 0,
            pending: Some(EditorPending {
                path: String::new(),
                language,
                line,
                column,
            }),
            editor,
        });
        self.active_tab = self.tabs.len() - 1;
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
        if let Some(ix) = self.tabs.iter().position(|t| match t {
            ShellTab::Editor { buffer_id: bid, .. } => bid == &buffer_id,
            _ => false,
        }) {
            if let ShellTab::Editor {
                editor,
                rev: r,
                path: p,
                pending,
                dirty,
                ..
            } = &mut self.tabs[ix]
            {
                *r = rev;
                *p = path;
                *dirty = false;
                let jump = pending.take();
                editor.update(cx, |state, cx| {
                    state.set_value(&text, window, cx);
                    if let Some(j) = jump {
                        if let Some(line) = j.line {
                            let pos = gpui_kit::component::input::Position::new(
                                line.saturating_sub(1),
                                j.column.unwrap_or(1).saturating_sub(1),
                            );
                            state.set_cursor_position(pos, window, cx);
                        }
                    }
                });
            }
            self.active_tab = ix;
            return;
        }
        self.begin_editor_tab(
            buffer_id.clone(),
            path.clone(),
            None,
            None,
            None,
            window,
            cx,
        );
        if let Some(ShellTab::Editor { editor, rev: r, .. }) = self.tabs.last_mut() {
            *r = rev;
            editor.update(cx, |state, cx| {
                state.set_value(&text, window, cx);
            });
        }
    }

    fn find_editor_mut(&mut self, buffer_id: &str) -> Option<&mut ShellTab> {
        self.tabs.iter_mut().find(|t| match t {
            ShellTab::Editor { buffer_id: bid, .. } => bid == buffer_id,
            _ => false,
        })
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
    }

    fn new_terminal(&mut self) {
        if !matches!(self.connection, ConnectionState::Online) {
            self.status = "Not connected".into();
            return;
        }
        let cwd = self.active_cwd();
        self.ade.send(AdeCmd::OpenPty {
            cols: 80,
            rows: 24,
            cwd,
        });
    }

    fn active_cwd(&self) -> Option<String> {
        self.tabs.iter().rev().find_map(|t| match t {
            ShellTab::Terminal { cwd, .. } => cwd.clone(),
            _ => None,
        })
    }

    fn close_active_tab(&mut self, cx: &mut Context<Self>) {
        if self.tabs.is_empty() {
            return;
        }
        let ix = self.active_tab.min(self.tabs.len() - 1);
        match &self.tabs[ix] {
            ShellTab::Terminal { pty_id, .. } => {
                self.ade.send(AdeCmd::ClosePty { id: pty_id.clone() });
            }
            ShellTab::Editor { buffer_id, .. } => {
                self.ade.send(AdeCmd::CloseEditor {
                    buffer_id: buffer_id.clone(),
                });
            }
        }
        self.tabs.remove(ix);
        if self.active_tab >= self.tabs.len() {
            self.active_tab = self.tabs.len().saturating_sub(1);
        }
        cx.notify();
    }

    fn save_active(&mut self, cx: &mut Context<Self>) {
        let Some(ShellTab::Editor {
            buffer_id,
            rev,
            editor,
            dirty,
            ..
        }) = self.tabs.get(self.active_tab)
        else {
            return;
        };
        if !*dirty {
            self.status = "No changes".into();
            cx.notify();
            return;
        }
        let text = editor.read(cx).value().to_string();
        let buffer_id = buffer_id.clone();
        let rev = *rev;
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

    fn on_new_terminal(&mut self, _: &NewTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal();
        cx.notify();
    }

    fn on_close_tab(&mut self, _: &CloseTab, _: &mut Window, cx: &mut Context<Self>) {
        self.close_active_tab(cx);
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

    fn on_next_tab(&mut self, _: &NextTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + 1) % self.tabs.len();
            cx.notify();
        }
    }

    fn on_prev_tab(&mut self, _: &PrevTab, _: &mut Window, cx: &mut Context<Self>) {
        if !self.tabs.is_empty() {
            self.active_tab = (self.active_tab + self.tabs.len() - 1) % self.tabs.len();
            cx.notify();
        }
    }

    fn tab_label(tab: &ShellTab) -> String {
        match tab {
            ShellTab::Terminal { title, .. } => title.clone(),
            ShellTab::Editor { path, dirty, .. } => {
                let name = path.rsplit('/').next().unwrap_or(path);
                if *dirty {
                    format!("• {name}")
                } else {
                    name.to_string()
                }
            }
        }
    }

    fn render_activity_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(48.))
            .h_full()
            .flex_shrink_0()
            .items_center()
            .gap_1()
            .py_2()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                Button::new("act-explorer")
                    .ghost()
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

        v_flex()
            .w(px(260.))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .items_center()
                    .justify_between()
                    .child(div().text_sm().font_bold().child(root_label))
                    .child(
                        Button::new("collapse-sidebar")
                            .ghost()
                            .icon(IconName::PanelLeftClose)
                            .tooltip("Hide Explorer (Ctrl+B)")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.sidebar_collapsed = true;
                                cx.notify();
                            })),
                    ),
            )
            .child(
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
                    let view = view.clone();
                    ListItem::new(ix)
                        .w_full()
                        .rounded(cx.theme().radius)
                        .py_0p5()
                        .px_2()
                        .pl(px(12.) * entry.depth() + px(8.))
                        .child(h_flex().gap_2().child(Icon::new(icon).small()).child(label))
                        .on_click(move |_, _, cx| {
                            view.update(cx, |this, cx| {
                                if is_folder {
                                    if !this.explorer_cache.contains_key(&path) {
                                        this.ade.send(AdeCmd::ListDir {
                                            request_id: next_id("ex"),
                                            path: path.clone(),
                                        });
                                    }
                                } else if !path.ends_with("/.") {
                                    this.open_path(path.clone(), true);
                                }
                                cx.notify();
                            });
                        })
                })
                .text_sm()
                .p_1()
                .flex_1()
                .min_h_0(),
            )
    }

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut bar = TabBar::new("workspace-tabs")
            .selected_index(self.active_tab)
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.active_tab = *ix;
                cx.notify();
            }));
        for tab in &self.tabs {
            let icon = match tab {
                ShellTab::Terminal { .. } => IconName::SquareTerminal,
                ShellTab::Editor { .. } => IconName::File,
            };
            bar = bar.child(Tab::new().label(Self::tab_label(tab)).icon(icon));
        }
        h_flex()
            .w_full()
            .items_center()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(bar.flex_1())
            .child(
                Button::new("new-term")
                    .ghost()
                    .icon(IconName::Plus)
                    .tooltip("New Terminal (Ctrl+T)")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.new_terminal();
                        cx.notify();
                    })),
            )
    }

    fn render_main(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.tabs.is_empty() {
            return div()
                .flex_1()
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
                        ),
                )
                .into_any_element();
        }

        let ix = self.active_tab.min(self.tabs.len() - 1);
        match &self.tabs[ix] {
            ShellTab::Terminal {
                screen,
                focus,
                pty_id,
                ..
            } => {
                let pty_id = pty_id.clone();
                let lines = screen.visible_lines();
                let focused = focus.is_focused(window);
                div()
                    .id("terminal-pane")
                    .flex_1()
                    .min_h_0()
                    .p_2()
                    .bg(cx.theme().background)
                    .font_family(cx.theme().mono_font_family.clone())
                    .text_sm()
                    .track_focus(focus)
                    .on_key_down(cx.listener(move |this, event: &KeyDownEvent, _, cx| {
                        let ks = &event.keystroke;
                        let bytes = keystroke_to_bytes(
                            ks.key.as_str(),
                            ks.key_char.as_deref(),
                            ks.modifiers.control,
                            ks.modifiers.alt,
                            ks.modifiers.shift,
                        );
                        if let Some(bytes) = bytes {
                            cx.stop_propagation();
                            this.ade.send(AdeCmd::WritePty {
                                id: pty_id.clone(),
                                data: bytes,
                            });
                        }
                    }))
                    .child(
                        v_flex()
                            .id("term-scroll")
                            .size_full()
                            .overflow_y_scroll()
                            .children(lines.into_iter().map(|line| {
                                div()
                                    .h(px(18.))
                                    .whitespace_nowrap()
                                    .child(if line.is_empty() {
                                        " ".to_string()
                                    } else {
                                        line
                                    })
                            }))
                            .when(focused, |this| this.opacity(1.))
                            .when(!focused, |this| this.opacity(0.85)),
                    )
                    .into_any_element()
            }
            ShellTab::Editor { editor, .. } => Editor::new(editor)
                .bordered(false)
                .p_0()
                .h(relative(1.))
                .font_family(cx.theme().mono_font_family.clone())
                .into_any_element(),
        }
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

    fn connection_label(&self) -> String {
        match &self.connection {
            ConnectionState::Connecting => "connecting".into(),
            ConnectionState::Online => "online".into(),
            ConnectionState::Offline { .. } => "offline".into(),
        }
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
            .child(
                TitleBar::new().child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(div().font_bold().child("fresh-gui"))
                        .child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(self.target.ws_url.clone()),
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
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(self.render_tabs(cx))
                            .child(self.render_main(window, cx)),
                    ),
            )
            .child(
                StatusBar::new()
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
            .children(dialog_layer)
            .children(notification_layer)
    }
}

fn language_from_path(path: &str, reported: Option<&str>) -> Option<String> {
    if let Some(lang) = reported.filter(|s| !s.is_empty()) {
        return Some(lang.to_string());
    }
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    let name = match ext.as_str() {
        "rs" => "rust",
        "ts" | "tsx" => "typescript",
        "js" | "jsx" | "mjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "toml" => "toml",
        "json" | "jsonc" => "json",
        "md" | "markdown" => "markdown",
        "sh" | "bash" | "zsh" => "bash",
        "css" => "css",
        "html" | "htm" => "html",
        "yml" | "yaml" => "yaml",
        _ => return None,
    };
    Some(name.into())
}

fn build_tree_items(root: &str, cache: &HashMap<String, Vec<FsEntry>>) -> Vec<TreeItem> {
    let Some(entries) = cache.get(root) else {
        return Vec::new();
    };
    entries
        .iter()
        .map(|e| {
            if e.kind == FsKind::Dir {
                let children = if cache.contains_key(&e.path) {
                    build_tree_items(&e.path, cache)
                } else {
                    Vec::new()
                };
                TreeItem::new(e.path.clone(), e.name.clone())
                    .children(if children.is_empty() && !cache.contains_key(&e.path) {
                        vec![TreeItem::new(format!("{}/.", e.path), "…")]
                    } else {
                        children
                    })
                    .expanded(cache.contains_key(&e.path))
            } else {
                TreeItem::new(e.path.clone(), e.name.clone())
            }
        })
        .collect()
}

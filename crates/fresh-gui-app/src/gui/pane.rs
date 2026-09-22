//! Dock panels for terminals and editors.
//!
//! Splits, tab reorder, and merging tabs back into one group are gpui-component's
//! dock (`DockArea` / `TabGroup`). These panels are the surfaces that dock hosts.
//! Tab titles are per workspace. `SessionTabTitle::workspace_id` is the workspace
//! that owns the panel.

use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent};
use gpui_kit::component::input::{Editor, EditorState, InputEvent, Position};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::ade::{AdeCmd, AdeHandle};
use super::osc7::feed_osc7_chunk;
use super::terminal::{TermScreen, keystroke_to_bytes};
use super::workspace::Workspace;

/// Label shown on a tab for this client session.
///
/// `workspace_id` is the daemon workspace that owns this tab. Numbers restart
/// inside each workspace.
#[derive(Clone, Debug)]
pub struct SessionTabTitle {
    pub label: String,
    pub custom: bool,
    pub workspace_id: Option<String>,
}

impl SessionTabTitle {
    pub fn numbered(n: u32) -> Self {
        Self {
            label: n.to_string(),
            custom: false,
            workspace_id: None,
        }
    }
}

/// Herdr-style terminal titles: the first terminal is `1`, the next is `2`, …
pub fn terminal_title(n: u32) -> String {
    n.to_string()
}

pub struct TerminalPanel {
    pty_id: String,
    title: SessionTabTitle,
    cwd: Option<String>,
    osc_carry: String,
    screen: TermScreen,
    focus: FocusHandle,
    ade: AdeHandle,
    workspace: WeakEntity<Workspace>,
    closed: bool,
}

impl TerminalPanel {
    pub fn new(
        pty_id: String,
        number: u32,
        ade: AdeHandle,
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            pty_id,
            title: SessionTabTitle::numbered(number),
            cwd: None,
            osc_carry: String::new(),
            screen: TermScreen::default(),
            focus: cx.focus_handle(),
            ade,
            workspace,
            closed: false,
        }
    }

    pub fn cwd(&self) -> Option<String> {
        self.cwd.clone()
    }

    pub fn label(&self) -> &str {
        &self.title.label
    }

    pub fn set_custom_title(&mut self, title: String, cx: &mut Context<Self>) {
        self.title.label = title;
        self.title.custom = true;
        cx.notify();
    }

    pub fn bind_workspace(&mut self, workspace_id: Option<String>) {
        self.title.workspace_id = workspace_id;
    }

    /// Drop the panel without sending `pty_close`. Used when the host swaps
    /// workspaces and the PTY must keep running on the daemon.
    pub fn release(&mut self) {
        self.closed = true;
    }

    /// Feed PTY bytes. Returns a new OSC 7 cwd when one was parsed.
    /// The numeric (or renamed) title is left alone.
    pub fn push_bytes(&mut self, bytes: &[u8], cx: &mut Context<Self>) -> Option<String> {
        let replies = self.screen.feed(bytes);
        if !replies.is_empty() {
            self.ade.send(AdeCmd::WritePty {
                id: self.pty_id.clone(),
                data: replies,
            });
        }
        let chunk = String::from_utf8_lossy(bytes);
        let cwd = feed_osc7_chunk(&mut self.osc_carry, &chunk);
        if let Some(cwd) = &cwd {
            self.cwd = Some(cwd.clone());
        }
        cx.notify();
        cwd
    }

    fn new_terminal_button(&self) -> impl IntoElement {
        let workspace = self.workspace.clone();
        Button::new(format!("new-term-{}", self.pty_id))
            .ghost()
            .xsmall()
            .icon(IconName::Plus)
            .tooltip("New Terminal (Ctrl+T)")
            .on_click(move |_, _, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.new_terminal(cx);
                        cx.notify();
                    })
                    .ok();
            })
    }
}

impl EventEmitter<PanelEvent> for TerminalPanel {}

impl Focusable for TerminalPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl BasePanel for TerminalPanel {
    fn panel_name(&self) -> &'static str {
        "Terminal"
    }

    fn zoomable(&self, _: &App) -> bool {
        false
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let pty = self.pty_id.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.note_terminal_active(&pty, active, cx);
            })
            .ok();
        if active {
            window.focus(&self.focus, cx);
        }
    }

    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.ade.send(AdeCmd::ClosePty {
            id: self.pty_id.clone(),
        });
        let pty = self.pty_id.clone();
        self.workspace
            .update(cx, |workspace, cx| workspace.forget_terminal(&pty, cx))
            .ok();
    }
}

impl DockPanel for TerminalPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let label = self.title.label.clone();
        let pty = self.pty_id.clone();
        let workspace = self.workspace.clone();
        h_flex()
            .id(format!("term-title-{}", self.pty_id))
            .gap_1()
            .items_center()
            .min_w_0()
            .child(Icon::new(IconName::SquareTerminal).small())
            .child(div().text_ellipsis().child(label))
            .context_menu(move |menu, _, _| {
                let pty = pty.clone();
                let workspace = workspace.clone();
                menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                    workspace
                        .update(cx, |workspace, cx| {
                            workspace.begin_terminal_rename(&pty, window, cx);
                        })
                        .ok();
                }))
            })
            .into_any_element()
    }

    fn title_suffix(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<impl IntoElement> {
        Some(self.new_terminal_button())
    }

    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> PopupMenu {
        let pty = self.pty_id.clone();
        let workspace = self.workspace.clone();
        menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.begin_terminal_rename(&pty, window, cx);
                })
                .ok();
        }))
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for TerminalPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let pty_id = self.pty_id.clone();
        let lines = self.screen.visible_lines();
        let focused = self.focus.is_focused(window);
        div()
            .id(format!("terminal-pane-{}", self.pty_id))
            .size_full()
            .p_2()
            .bg(cx.theme().background)
            .font_family(cx.theme().mono_font_family.clone())
            .text_sm()
            .track_focus(&self.focus)
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
                    .id(format!("term-scroll-{}", self.pty_id))
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
    }
}

struct EditorPending {
    line: Option<u32>,
    column: Option<u32>,
}

pub struct EditorPanel {
    buffer_id: String,
    path: String,
    dirty: bool,
    rev: u64,
    pending: Option<EditorPending>,
    editor: Entity<EditorState>,
    ade: AdeHandle,
    workspace: WeakEntity<Workspace>,
    closed: bool,
    _subscription: Subscription,
}

impl EditorPanel {
    pub fn new(
        buffer_id: String,
        path: String,
        language: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
        ade: AdeHandle,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx)
                .line_number(true)
                .placeholder("Loading…");
            if let Some(lang) = language_from_path(&path, language.as_deref()) {
                state = state.language(lang);
            }
            state
        });
        let subscription = cx.subscribe(&editor, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.dirty = true;
                cx.notify();
            }
        });
        Self {
            buffer_id,
            path,
            dirty: false,
            rev: 0,
            pending: Some(EditorPending { line, column }),
            editor,
            ade,
            workspace,
            closed: false,
            _subscription: subscription,
        }
    }

    pub fn buffer_id(&self) -> &str {
        &self.buffer_id
    }

    /// Drop the panel without sending `editor_close`. The Fresh buffer stays
    /// in the daemon worker so another workspace can reopen the same path.
    pub fn release(&mut self) {
        self.closed = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn editor(&self) -> &Entity<EditorState> {
        &self.editor
    }

    pub fn note_reopen(
        &mut self,
        buffer_id: String,
        line: Option<u32>,
        column: Option<u32>,
        cx: &mut Context<Self>,
    ) {
        self.buffer_id = buffer_id;
        self.pending = Some(EditorPending { line, column });
        cx.notify();
    }

    pub fn apply_snapshot(
        &mut self,
        rev: u64,
        text: String,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rev = rev;
        self.path = path;
        self.dirty = false;
        let jump = self.pending.take();
        self.editor.update(cx, |state, cx| {
            state.set_value(&text, window, cx);
            if let Some(jump) = jump
                && let Some(line) = jump.line
            {
                let pos = Position::new(
                    line.saturating_sub(1),
                    jump.column.unwrap_or(1).saturating_sub(1),
                );
                state.set_cursor_position(pos, window, cx);
            }
        });
        cx.notify();
    }

    pub fn set_rev(&mut self, rev: u64, cx: &mut Context<Self>) {
        self.rev = rev;
        cx.notify();
    }

    /// Returns the previous path when the saved path differs, so the workspace
    /// map can be re-keyed.
    pub fn mark_saved(&mut self, path: String, rev: u64, cx: &mut Context<Self>) -> Option<String> {
        let previous = (self.path != path).then(|| self.path.clone());
        self.path = path;
        self.rev = rev;
        self.dirty = false;
        cx.notify();
        previous
    }

    fn label(&self) -> String {
        let name = self.path.rsplit('/').next().unwrap_or(self.path.as_str());
        if self.dirty {
            format!("• {name}")
        } else {
            name.to_string()
        }
    }

    fn new_terminal_button(&self) -> impl IntoElement {
        let workspace = self.workspace.clone();
        Button::new(format!("new-term-ed-{}", self.buffer_id))
            .ghost()
            .xsmall()
            .icon(IconName::Plus)
            .tooltip("New Terminal (Ctrl+T)")
            .on_click(move |_, _, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.new_terminal(cx);
                        cx.notify();
                    })
                    .ok();
            })
    }
}

impl EventEmitter<PanelEvent> for EditorPanel {}

impl Focusable for EditorPanel {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.read(cx).focus_handle(cx)
    }
}

impl BasePanel for EditorPanel {
    fn panel_name(&self) -> &'static str {
        "Editor"
    }

    fn zoomable(&self, _: &App) -> bool {
        false
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path.clone();
        self.workspace
            .update(cx, |workspace, cx| {
                workspace.note_editor_active(&path, active, cx);
            })
            .ok();
        if active {
            self.editor.read(cx).focus_handle(cx).focus(window, cx);
        }
    }

    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.ade.send(AdeCmd::CloseEditor {
            buffer_id: self.buffer_id.clone(),
        });
        let path = self.path.clone();
        self.workspace
            .update(cx, |workspace, cx| workspace.forget_editor(&path, cx))
            .ok();
    }
}

impl DockPanel for EditorPanel {
    fn title(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .items_center()
            .min_w_0()
            .child(Icon::new(IconName::File).small())
            .child(div().text_ellipsis().child(self.label()))
    }

    fn title_suffix(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<impl IntoElement> {
        Some(self.new_terminal_button())
    }

    fn zoom_control(&self, _: &App) -> Option<PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for EditorPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(
            Editor::new(&self.editor)
                .bordered(false)
                .p_0()
                .h(relative(1.))
                .font_family(cx.theme().mono_font_family.clone()),
        )
    }
}

pub(crate) fn language_from_path(path: &str, reported: Option<&str>) -> Option<String> {
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

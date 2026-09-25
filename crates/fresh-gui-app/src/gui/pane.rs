//! Dock panels for terminals and editors.
//!
//! Splits, tab reorder, and merging tabs back into one group are gpui-component's
//! dock (`DockArea` / `TabGroup`). These panels are the surfaces that dock hosts.
//! Tab titles are per workspace. `SessionTabTitle::workspace_id` is the workspace
//! that owns the panel.

use std::cell::Cell;
use std::ops::Range;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::vte::ansi::CursorShape;
use fresh_gui_protocol::BufferDiagnostic;

use gpui_kit::base::ElementExt as _;
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent, PanelId};
use gpui_kit::component::input::{Editor, EditorState, InputEvent, Position, TextDecoration, TextDecorationCollection};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::base::Selectable as _;

use super::actions::{TerminalInputBacktab, TerminalInputTab, ZoomInUi};
use super::ade::{AdeCmd, AdeHandle};
use super::clipboard;
use super::osc7::feed_osc7_chunk;
use super::explorer::parent_dir;
use super::paths::display_path;
use super::rail::path_basename;
use super::tab_chrome::{TabCloseScope, TabStripMetrics};
use super::terminal::{
    TermMouseButton, TermMouseKind, TermMouseMods, TermRow, TermScreen, TermSpan, keystroke_to_bytes,
    paste_payload, readable_light_foreground, word_bounds_at_column,
};

/// `text_sm` monospace cell, matching [`super::terminal`] pixel reports.
const TERM_CELL_W: f32 = 8.;
const TERM_CELL_H: f32 = 18.;
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
#[cfg(test)]
pub fn terminal_title(n: u32) -> String {
    n.to_string()
}

pub struct TerminalPanel {
    pty_id: String,
    title: SessionTabTitle,
    cwd: Option<String>,
    osc_carry: String,
    screen: TermScreen,
    sync_deadline: Option<Instant>,
    sync_task: Option<Task<()>>,
    focus: FocusHandle,
    focus_subscription: Option<Subscription>,
    ade: AdeHandle,
    workspace: WeakEntity<Workspace>,
    metrics: TabStripMetrics,
    /// How far left the **+** sits from the suffix slot. Measured last frame.
    plus_shift: Rc<Cell<f32>>,
    /// Grid size measured after layout. Applied on the next frame.
    pending_grid: Rc<Cell<Option<(usize, usize)>>>,
    /// Top-left of the cell grid in window coordinates, from the last layout.
    grid_origin: Rc<Cell<Option<Point<Pixels>>>>,
    /// Left-button drag is selecting text on the host.
    selecting: bool,
    /// Button held for a report to the PTY.
    pressed: Option<TermMouseButton>,
    /// Last cell written as a move or drag, so a hover does not repeat.
    last_mouse_cell: Option<(usize, usize)>,
    /// Row and scalar bounds of a path under a held Ctrl/Cmd pointer.
    link_hover: Option<(usize, usize, usize)>,
    last_pointer: Option<Point<Pixels>>,
    /// Monospace cell, scaled with content and UI zoom. 8×18 at 14px.
    cell_w: f32,
    cell_h: f32,
    font_px: f32,
    restoring: bool,
    closed: bool,
    /// OSC 52 text that arrived without a window handle (sync-update flush).
    pending_clipboard: Vec<String>,
}

impl TerminalPanel {
    pub fn new(
        pty_id: String,
        number: u32,
        ade: AdeHandle,
        workspace: WeakEntity<Workspace>,
        metrics: TabStripMetrics,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            pty_id,
            title: SessionTabTitle::numbered(number),
            cwd: None,
            osc_carry: String::new(),
            screen: TermScreen::default(),
            sync_deadline: None,
            sync_task: None,
            focus: cx.focus_handle(),
            focus_subscription: None,
            ade,
            workspace,
            metrics,
            plus_shift: Rc::new(Cell::new(0.0)),
            pending_grid: Rc::new(Cell::new(None)),
            grid_origin: Rc::new(Cell::new(None)),
            selecting: false,
            pressed: None,
            last_mouse_cell: None,
            link_hover: None,
            last_pointer: None,
            cell_w: TERM_CELL_W,
            cell_h: TERM_CELL_H,
            font_px: 14.0,
            restoring: false,
            closed: false,
            pending_clipboard: Vec::new(),
        }
    }

    pub fn set_metrics(&mut self, cell_w: f32, cell_h: f32, font_px: f32, cx: &mut Context<Self>) {
        if (self.cell_w - cell_w).abs() < 0.1
            && (self.cell_h - cell_h).abs() < 0.1
            && (self.font_px - font_px).abs() < 0.1
        {
            return;
        }
        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.font_px = font_px;
        cx.notify();
    }

    pub fn cwd(&self) -> Option<String> {
        self.cwd.clone()
    }

    pub fn restore_cwd(&mut self, cwd: Option<String>) {
        self.cwd = cwd;
    }

    pub fn set_restoring(&mut self, restoring: bool) {
        self.restoring = restoring;
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
    pub fn push_bytes(
        &mut self,
        bytes: &[u8],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        let replies = self.screen.feed(bytes);
        if !replies.is_empty() {
            self.ade.send(AdeCmd::WritePty {
                id: self.pty_id.clone(),
                data: replies,
            });
        }
        let cwd = if self.osc_carry.is_empty() && !bytes.windows(2).any(|pair| pair == b"\x1b]") {
            None
        } else {
            let chunk = String::from_utf8_lossy(bytes);
            feed_osc7_chunk(&mut self.osc_carry, &chunk)
        };
        if let Some(cwd) = &cwd {
            self.cwd = Some(cwd.clone());
        }
        for text in self.screen.take_clipboard_stores() {
            if let Err(error) = clipboard::write_text(window, cx, &text) {
                tracing::warn!(%error, "terminal clipboard copy failed");
            } else {
                clipboard::notify_copied(window, cx);
            }
        }
        self.arm_sync_timeout(cx);
        cx.notify();
        cwd
    }

    fn arm_sync_timeout(&mut self, cx: &mut Context<Self>) {
        let deadline = self.screen.sync_deadline();
        if self.sync_deadline == deadline {
            return;
        }
        self.sync_deadline = deadline;
        self.sync_task = deadline.map(|deadline| {
            let timer = cx
                .background_executor()
                .timer(deadline.saturating_duration_since(Instant::now()));
            cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, cx| {
                    if this.sync_deadline != Some(deadline) {
                        return;
                    }
                    let replies = this.screen.stop_sync_if_expired();
                    this.write_pty(replies);
                    this.pending_clipboard
                        .extend(this.screen.take_clipboard_stores());
                    this.sync_deadline = None;
                    this.arm_sync_timeout(cx);
                    cx.notify();
                });
            })
        });
    }

    fn cell_at(&self, position: Point<Pixels>) -> Option<(usize, usize)> {
        let origin = self.grid_origin.get()?;
        let x = f32::from(position.x - origin.x);
        let y = f32::from(position.y - origin.y);
        let cols = self.screen.cols.max(1);
        let rows = self.screen.rows.max(1);
        let col = (x / self.cell_w).floor() as isize;
        let row = (y / self.cell_h).floor() as isize;
        let col = col.clamp(0, cols as isize - 1) as usize;
        let row = row.clamp(0, rows as isize - 1) as usize;
        Some((col, row))
    }

    fn write_pty(&self, data: Vec<u8>) {
        if data.is_empty() {
            return;
        }
        self.ade.send(AdeCmd::WritePty {
            id: self.pty_id.clone(),
            data,
        });
    }

    fn mouse_mods(modifiers: &Modifiers) -> TermMouseMods {
        TermMouseMods {
            shift: modifiers.shift,
            alt: modifiers.alt,
            control: modifiers.control || modifiers.platform,
        }
    }

    /// Host selection when the program is not tracking the mouse, or when
    /// Shift is held (the usual bypass).
    fn host_selects(&self, button: TermMouseButton, modifiers: &Modifiers) -> bool {
        button == TermMouseButton::Left
            && (modifiers.shift || !self.screen.mouse_tracking().active())
    }

    /// Dock splits finish on mouse-up over the pane. The terminal used to
    /// stop that event, so a terminal tab could not land in a split.
    fn host_pointer(
        &mut self,
        button: TermMouseButton,
        down: bool,
        position: Point<Pixels>,
        modifiers: &Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if cx.has_active_drag() {
            return;
        }
        self.on_mouse_button(button, down, position, modifiers, window, cx);
    }

    fn on_mouse_button(
        &mut self,
        button: TermMouseButton,
        down: bool,
        position: Point<Pixels>,
        modifiers: &Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        if down {
            window.focus(&self.focus, cx);
        }
        if down
            && button == TermMouseButton::Left
            && modifiers.secondary()
            && !modifiers.shift
            && let Some((col, row)) = self.cell_at(position)
            && let Some((line, char_col)) = self.screen.line_text_and_char_column(row, col)
        {
            let cwd = self.cwd();
            let workspace = self.workspace.clone();
            workspace
                .update(cx, |workspace, cx| {
                    workspace.open_path_link(line, char_col as u32, cwd, cx);
                })
                .ok();
            return;
        }
        if down && self.host_selects(button, modifiers) {
            self.pressed = None;
            self.pointer_select(position, true, cx);
            return;
        }
        if !down {
            self.release_mouse(button, position, modifiers, window, cx);
            return;
        }
        let tracking = self.screen.mouse_tracking();
        if !tracking.active() {
            return;
        }
        self.selecting = false;
        self.screen.clear_selection();
        self.pressed = Some(button);
        self.last_mouse_cell = self.cell_at(position);
        self.send_mouse(position, TermMouseKind::Down(button), modifiers, false);
        cx.notify();
    }

    fn release_mouse(
        &mut self,
        button: TermMouseButton,
        position: Point<Pixels>,
        modifiers: &Modifiers,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting && button == TermMouseButton::Left {
            self.finish_select(window, cx);
            return;
        }
        if self.pressed == Some(button) {
            self.pressed = None;
            self.last_mouse_cell = None;
            self.send_mouse(position, TermMouseKind::Up(button), modifiers, false);
        }
    }

    fn on_pointer_move(
        &mut self,
        position: Point<Pixels>,
        modifiers: &Modifiers,
        cx: &mut Context<Self>,
    ) {
        self.last_pointer = Some(position);
        if self.selecting {
            if self.link_hover.take().is_some() {
                cx.notify();
            }
            self.pointer_select(position, false, cx);
            return;
        }
        let hovered = self.link_hover_at(position, modifiers);
        if hovered != self.link_hover {
            self.link_hover = hovered;
            cx.notify();
        }
        let tracking = self.screen.mouse_tracking();
        let kind = if let Some(button) = self.pressed {
            if !tracking.reports_drag() {
                return;
            }
            TermMouseKind::Drag(button)
        } else if tracking.reports_move() {
            TermMouseKind::Move
        } else {
            return;
        };
        self.send_mouse(position, kind, modifiers, true);
    }

    fn link_hover_at(
        &self,
        position: Point<Pixels>,
        modifiers: &Modifiers,
    ) -> Option<(usize, usize, usize)> {
        let candidate = self.cell_at(position).and_then(|(col, row)| {
            self.screen
                .line_text_and_char_column(row, col)
                .and_then(|(line, char_col)| {
                    terminal_link_range(&line, char_col)
                        .map(|(start, end)| (row, start, end))
                })
        });
        terminal_link_hover(candidate, modifiers)
    }

    fn send_mouse(
        &mut self,
        position: Point<Pixels>,
        kind: TermMouseKind,
        modifiers: &Modifiers,
        dedup: bool,
    ) {
        let Some((col, row)) = self.cell_at(position) else {
            return;
        };
        if dedup && self.last_mouse_cell == Some((col, row)) {
            return;
        }
        let Some(bytes) =
            self.screen
                .mouse_tracking()
                .encode(col, row, kind, Self::mouse_mods(modifiers))
        else {
            return;
        };
        if dedup {
            self.last_mouse_cell = Some((col, row));
        }
        self.write_pty(bytes);
    }

    fn scroll_host(&mut self, lines: f32, cx: &mut Context<Self>) {
        if self.screen.alt_screen() {
            // Alternate-screen applications own their viewport. DEC mode 1007
            // opts into translating the host wheel to cursor keys; without it
            // the wheel must not move the host's primary-screen scrollback.
            if !self.screen.alternate_scroll() {
                return;
            }
            let steps = lines.abs().round().clamp(1., 8.) as usize;
            let seq: &[u8] = if lines > 0. { b"\x1b[A" } else { b"\x1b[B" };
            let mut data = Vec::with_capacity(seq.len() * steps);
            for _ in 0..steps {
                data.extend_from_slice(seq);
            }
            self.write_pty(data);
            return;
        }
        let steps = lines.round() as i32;
        if steps != 0 {
            self.screen.scroll_by(-steps);
            cx.notify();
        }
    }

    fn pointer_select(&mut self, position: Point<Pixels>, start: bool, cx: &mut Context<Self>) {
        let Some((col, row)) = self.cell_at(position) else {
            return;
        };
        if start {
            self.screen.begin_selection(col, row);
            self.selecting = true;
        } else if self.selecting {
            self.screen.update_selection(col, row);
        }
        cx.notify();
    }

    fn select_word_at(&mut self, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((col, row)) = self.cell_at(position) else { return };
        let Some((line, char_col)) = self.screen.line_text_and_char_column(row, col) else { return };
        let Some((start, end)) = word_bounds_at_column(&line, char_col) else { return };
        if !self.screen.select_char_range(row, start..end) {
            return;
        }
        self.selecting = true;
        self.finish_select(window, cx);
    }

    fn finish_select(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        if self.screen.selection_is_empty() {
            self.screen.clear_selection();
        } else {
            self.copy_selection(window, cx);
        }
        cx.notify();
    }

    fn copy_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.screen.selection_text() else {
            return;
        };
        if let Err(error) = clipboard::write_text(window, cx, &text) {
            tracing::warn!(%error, "terminal copy failed");
        } else {
            clipboard::notify_copied(window, cx);
        }
        cx.notify();
    }

    fn paste_clipboard(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = match clipboard::read_text(window, cx) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(%error, "terminal paste failed");
                return;
            }
        };
        let data = paste_payload(&text, self.screen.bracketed_paste());
        cx.stop_propagation();
        if self.screen.prepare_for_input() {
            cx.notify();
        }
        self.write_pty(data);
    }

    fn write_keys(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if self.screen.prepare_for_input() {
            cx.notify();
        }
        self.write_pty(bytes.to_vec());
    }

    pub fn copy_or_interrupt(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.screen.selection_text().is_some() {
            self.copy_selection(window, cx);
        } else {
            self.ade.send(AdeCmd::WritePty { id: self.pty_id.clone(), data: vec![3] });
        }
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
        let workspace = self.workspace.clone();
        // The dock delivers this from inside the panel update, and Ctrl+W
        // reaches it while `Workspace` is still on the stack. Defer the
        // workspace write until that update returns.
        if !self.restoring {
            cx.defer(move |cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.note_terminal_active(&pty, active, cx);
                    })
                    .ok();
            });
        }
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
        let workspace = self.workspace.clone();
        // `remove_panel` runs inside `Workspace::close_active_tab` (Ctrl+W
        // and the tab's ×). Updating Workspace here panics:
        // "cannot update Workspace while it is already being updated".
        cx.defer(move |cx| {
            workspace
                .update(cx, |workspace, cx| workspace.forget_terminal(&pty, cx))
                .ok();
        });
    }
}

impl DockPanel for TerminalPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let label = self.title.label.clone();
        let pty = self.pty_id.clone();
        let workspace = self.workspace.clone();
        let panel_id = PanelId::from(cx.entity().entity_id());
        let metrics = self.metrics.clone();
        h_flex()
            .id(format!("term-title-{}", self.pty_id))
            .gap_1()
            .items_center()
            .min_w_0()
            .on_prepaint(move |bounds, _, _| note_tab_edge(&metrics, bounds))
            .child(Icon::new(IconName::SquareTerminal).small())
            .child(div().text_ellipsis().child(label))
            .child(tab_close_button(
                format!("close-term-{}", self.pty_id),
                workspace.clone(),
                panel_id,
            ))
            .context_menu(move |menu, _, cx| {
                let pty = pty.clone();
                let workspace_rename = workspace.clone();
                let menu = menu
                    .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                        workspace_rename
                            .update(cx, |workspace, cx| {
                                workspace.begin_terminal_rename(&pty, window, cx);
                            })
                            .ok();
                    }))
                    .separator();
                with_close_items(menu, workspace.clone(), panel_id, true, cx)
            })
            .into_any_element()
    }

    fn title_suffix(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let panel_id = PanelId::from(cx.entity().entity_id());
        Some(new_terminal_button(
            format!("new-term-{}", self.pty_id),
            self.metrics.clone(),
            self.plus_shift.clone(),
            self.workspace.clone(),
            panel_id,
        ))
    }

    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let pty = self.pty_id.clone();
        let workspace = self.workspace.clone();
        let panel_id = PanelId::from(cx.entity().entity_id());
        let copy_from = cx.entity().downgrade();
        let menu = menu
            .item(PopupMenuItem::new("Copy").on_click({
                let copy_from = copy_from.clone();
                move |_, window, cx| {
                    copy_from
                        .update(cx, |panel, cx| panel.copy_selection(window, cx))
                        .ok();
                }
            }))
            .item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                workspace
                    .update(cx, |workspace, cx| {
                        workspace.begin_terminal_rename(&pty, window, cx);
                    })
                    .ok();
            }))
            .separator();
        // The dock already appends Close for a group that can lose a tab.
        with_close_items(menu, self.workspace.clone(), panel_id, false, cx)
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
        if self.focus_subscription.is_none() {
            let handle = self.focus.clone();
            self.focus_subscription = Some(cx.on_focus(&handle, window, |this, _, cx| {
                if this.restoring {
                    return;
                }
                let workspace = this.workspace.clone();
                let pty = this.pty_id.clone();
                cx.defer(move |cx| {
                    workspace.update(cx, |workspace, cx| workspace.note_terminal_active(&pty, true, cx)).ok();
                });
            }));
        }
        let pty_id = self.pty_id.clone();
        if let Some((cols, rows)) = self.pending_grid.get()
            && (cols != self.screen.cols || rows != self.screen.rows)
        {
            self.screen.resize(cols, rows);
            self.ade.send(AdeCmd::ResizePty {
                id: self.pty_id.clone(),
                cols: cols as u16,
                rows: rows as u16,
            });
        }
        let rows = self.screen.rows();
        let paint_rows = rows.clone();
        if !self.pending_clipboard.is_empty() {
            for text in std::mem::take(&mut self.pending_clipboard) {
                if let Err(error) = clipboard::write_text(window, cx, &text) {
                    tracing::warn!(%error, "terminal clipboard copy failed");
                } else {
                    clipboard::notify_copied(window, cx);
                }
            }
        }
        let focused = self.focus.is_focused(window);
        let cursor = focused.then(|| self.screen.cursor_cell()).flatten();
        let fg_default = cx.theme().foreground;
        let bg_default = cx.theme().background;
        let accent = cx.theme().accent;
        let light_theme = !cx.theme().is_dark();
        let pending_grid = Rc::clone(&self.pending_grid);
        let grid_origin = Rc::clone(&self.grid_origin);
        let cell_w = self.cell_w;
        let cell_h = self.cell_h;
        let entity_id = cx.entity().entity_id();
        let track_outside = self.selecting || self.pressed.is_some();
        let select_entity = cx.entity().downgrade();
        let reporting = self.screen.mouse_tracking().active();
        let link_hover = self.link_hover;
        let pane = div()
            .id(format!("terminal-pane-{}", self.pty_id))
            .key_context("Terminal")
            .role(Role::Terminal)
            .aria_label("Terminal")
            .size_full()
            .overflow_hidden()
            .p_2()
            .bg(bg_default)
            .font_family(cx.theme().mono_font_family.clone())
            .text_size(px(self.font_px))
            .track_focus(&self.focus)
            .when(link_hover.is_some(), |pane| pane.cursor_pointer())
            .on_mouse_exit(cx.listener(|this, _, _, cx| {
                this.last_pointer = None;
                if this.link_hover.take().is_some() {
                    cx.notify();
                }
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                let hovered = this.last_pointer.and_then(|position| {
                    this.link_hover_at(position, &event.modifiers)
                });
                if hovered != this.link_hover {
                    this.link_hover = hovered;
                    cx.notify();
                }
            }))
            // Child hitboxes can consume the bubble phase. TUI mouse reports
            // and modified path clicks must reach the terminal in capture.
            .capture_any_mouse_down(cx.listener(|this, event: &MouseDownEvent, window, cx| {
                if event.button == MouseButton::Left && event.modifiers.secondary()
                    && !event.modifiers.shift
                {
                    this.host_pointer(
                        TermMouseButton::Left,
                        true,
                        event.position,
                        &event.modifiers,
                        window,
                        cx,
                    );
                } else if event.button == MouseButton::Left && event.click_count >= 2 {
                    // A double-click is a host selection gesture even when a
                    // TUI enabled mouse reporting. The first ordinary press
                    // still reaches the PTY; this press is copied locally.
                    this.select_word_at(event.position, window, cx);
                    cx.stop_propagation();
                } else if this.screen.mouse_tracking().active() && !event.modifiers.shift {
                    let button = match event.button {
                        MouseButton::Left => TermMouseButton::Left,
                        MouseButton::Middle => TermMouseButton::Middle,
                        MouseButton::Right => TermMouseButton::Right,
                        _ => return,
                    };
                    this.host_pointer(button, true, event.position, &event.modifiers, window, cx);
                }
            }))
            .capture_any_mouse_up(cx.listener(|this, event: &MouseUpEvent, window, cx| {
                if !this.screen.mouse_tracking().active() || this.pressed.is_none() { return; }
                let button = match event.button {
                    MouseButton::Left => TermMouseButton::Left,
                    MouseButton::Middle => TermMouseButton::Middle,
                    MouseButton::Right => TermMouseButton::Right,
                    _ => return,
                };
                this.host_pointer(button, false, event.position, &event.modifiers, window, cx);
            }))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.host_pointer(
                        TermMouseButton::Left,
                        true,
                        event.position,
                        &event.modifiers,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_down(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.host_pointer(
                        TermMouseButton::Middle,
                        true,
                        event.position,
                        &event.modifiers,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_down(
                MouseButton::Right,
                cx.listener(|this, event: &MouseDownEvent, window, cx| {
                    this.host_pointer(
                        TermMouseButton::Right,
                        true,
                        event.position,
                        &event.modifiers,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if cx.has_active_drag() {
                    return;
                }
                this.on_pointer_move(event.position, &event.modifiers, cx);
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    this.host_pointer(
                        TermMouseButton::Left,
                        false,
                        event.position,
                        &event.modifiers,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_up(
                MouseButton::Middle,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    this.host_pointer(
                        TermMouseButton::Middle,
                        false,
                        event.position,
                        &event.modifiers,
                        window,
                        cx,
                    );
                }),
            )
            .on_mouse_up(
                MouseButton::Right,
                cx.listener(|this, event: &MouseUpEvent, window, cx| {
                    this.host_pointer(
                        TermMouseButton::Right,
                        false,
                        event.position,
                        &event.modifiers,
                        window,
                        cx,
                    );
                }),
            )
            .on_action(cx.listener(|this, _: &TerminalInputTab, _, cx| {
                cx.stop_propagation();
                this.write_keys(b"\t", cx);
            }))
            .on_action(cx.listener(|this, _: &TerminalInputBacktab, _, cx| {
                cx.stop_propagation();
                this.write_keys(b"\x1b[Z", cx);
            }))
            .on_key_down(cx.listener(move |this, event: &KeyDownEvent, window, cx| {
                let ks = &event.keystroke;
                let key = ks.key.to_lowercase();
                if is_ui_zoom_in_chord(&key, ks.key_char.as_deref(), &ks.modifiers) {
                    cx.stop_propagation();
                    window.dispatch_action(Box::new(ZoomInUi), cx);
                    return;
                }
                let copy = (ks.modifiers.control || ks.modifiers.platform)
                    && !ks.modifiers.alt
                    && key == "c"
                    && this.screen.selection_text().is_some();
                if copy {
                    cx.stop_propagation();
                    this.copy_selection(window, cx);
                    return;
                }
                let paste = !ks.modifiers.alt
                    && ((ks.modifiers.control || ks.modifiers.platform) && key == "v"
                        || ks.modifiers.shift && key == "insert");
                if paste {
                    this.paste_clipboard(window, cx);
                    return;
                }
                if ks.modifiers.shift && !ks.modifiers.control && !ks.modifiers.alt {
                    match key.as_str() {
                        "pageup" => {
                            cx.stop_propagation();
                            this.screen.scroll_page(true);
                            cx.notify();
                            return;
                        }
                        "pagedown" => {
                            cx.stop_propagation();
                            this.screen.scroll_page(false);
                            cx.notify();
                            return;
                        }
                        _ => {}
                    }
                }
                let bytes = keystroke_to_bytes(
                    ks.key.as_str(),
                    ks.key_char.as_deref(),
                    ks.modifiers.control,
                    ks.modifiers.alt,
                    ks.modifiers.shift,
                    this.screen.app_cursor(),
                );
                if let Some(bytes) = bytes {
                    cx.stop_propagation();
                    if this.screen.prepare_for_input() {
                        cx.notify();
                    }
                    this.ade.send(AdeCmd::WritePty {
                        id: pty_id.clone(),
                        data: bytes,
                    });
                }
            }))
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                cx.stop_propagation();
                let lines = scroll_lines(event.delta, this.cell_h);
                if lines == 0. {
                    return;
                }
                // Wheel is a mouse report while tracking is on. Shift keeps
                // the host's history / alternate-screen arrows.
                if this.screen.mouse_tracking().active() && !event.modifiers.shift {
                    let steps = lines.abs().round().clamp(1., 8.) as usize;
                    let kind = if lines > 0. {
                        TermMouseKind::ScrollUp
                    } else {
                        TermMouseKind::ScrollDown
                    };
                    let Some((col, row)) = this.cell_at(event.position) else {
                        return;
                    };
                    let mut data = Vec::new();
                    for _ in 0..steps {
                        if let Some(bytes) = this.screen.mouse_tracking().encode(
                            col,
                            row,
                            kind,
                            Self::mouse_mods(&event.modifiers),
                        ) {
                            data.extend(bytes);
                        }
                    }
                    this.write_pty(data);
                    return;
                }
                this.scroll_host(lines, cx);
            }))
            .child(
                v_flex()
                    .id(format!("term-scroll-{}", self.pty_id))
                    .size_full()
                    .relative()
                    .overflow_hidden()
                    .on_prepaint(move |bounds, window, app| {
                        grid_origin.set(Some(bounds.origin));
                        let next = grid_size(bounds.size, cell_w, cell_h);
                        if pending_grid.get() != Some(next) {
                            pending_grid.set(Some(next));
                            app.notify(entity_id);
                        }
                        if !track_outside {
                            return;
                        }
                        let moved = select_entity.clone();
                        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, app| {
                            if phase.capture() {
                                return;
                            }
                            moved
                                .update(app, |panel, cx| {
                                    panel.on_pointer_move(event.position, &event.modifiers, cx);
                                })
                                .ok();
                        });
                        let released = select_entity.clone();
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, app| {
                            if phase.capture() {
                                return;
                            }
                            let Some(button) = term_mouse_button(event.button) else {
                                return;
                            };
                            released
                                .update(app, |panel, cx| {
                                    panel.release_mouse(
                                        button,
                                        event.position,
                                        &event.modifiers,
                                        window,
                                        cx,
                                    );
                                })
                                .ok();
                        });
                    })
                    // GPUI text backgrounds use glyph/line bounds, which can leave
                    // fractional device-pixel gaps between terminal grid rows.
                    .child(
                        gpui::canvas(|_, _, _| {}, move |bounds, _, window, _| {
                            paint_terminal_cells(
                                bounds, &paint_rows, cell_w, cell_h,
                                fg_default, accent, light_theme, window,
                            );
                        })
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full(),
                    )
                    .children(rows.into_iter().enumerate().map(|(row_index, row)| {
                        let mut char_start = 0;
                        h_flex().h(px(cell_h)).items_center().children(
                            row.spans.into_iter().map(|span| {
                                let char_end = char_start + span.text.chars().count();
                                let underline = link_hover.is_some_and(|(hover_row, start, end)| {
                                    hover_row == row_index && start < char_end && char_start < end
                                });
                                char_start = char_end;
                                term_span_el(span, cell_w, fg_default, accent, light_theme, underline)
                            }),
                        )
                    }))
                    .when_some(cursor, |grid, (row, col, shape)| {
                        grid.child(
                            gpui::canvas(|_, _, _| {}, move |bounds, _, window, _| {
                                paint_terminal_cursor(
                                    bounds, row, col, shape, cell_w, cell_h, fg_default, window,
                                );
                            })
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full(),
                        )
                    })
                    .when(focused, |this| this.opacity(1.))
                    .when(!focused, |this| this.opacity(0.85)),
            );
        // Right-click Copy stays available until a program takes the pointer.
        // While tracking is on, that click belongs to the PTY; Copy remains
        // on the tab's ··· menu.
        if reporting {
            pane.into_any_element()
        } else {
            let menu_entity = cx.entity().downgrade();
            pane.context_menu(move |menu, _, _| {
                let menu_entity = menu_entity.clone();
                let paste_entity = menu_entity.clone();
                menu.item(PopupMenuItem::new("Copy").on_click(move |_, window, cx| {
                    menu_entity
                        .update(cx, |panel, cx| panel.copy_selection(window, cx))
                        .ok();
                }))
                .item(PopupMenuItem::new("Paste").on_click(move |_, window, cx| {
                    paste_entity
                        .update(cx, |panel, cx| panel.paste_clipboard(window, cx))
                        .ok();
                }))
            })
            .into_any_element()
        }
    }
}

fn grid_size(size: Size<Pixels>, cell_w: f32, cell_h: f32) -> (usize, usize) {
    let width = f32::from(size.width);
    let height = f32::from(size.height);
    if width < cell_w || height < cell_h {
        return (80, 24);
    }
    let cols = ((width / cell_w).floor() as usize).clamp(2, 500);
    let rows = ((height / cell_h).floor() as usize).clamp(1, 200);
    (cols, rows)
}

fn term_mouse_button(button: MouseButton) -> Option<TermMouseButton> {
    match button {
        MouseButton::Left => Some(TermMouseButton::Left),
        MouseButton::Middle => Some(TermMouseButton::Middle),
        MouseButton::Right => Some(TermMouseButton::Right),
        MouseButton::Navigate(_) => None,
    }
}

fn scroll_lines(delta: ScrollDelta, cell_h: f32) -> f32 {
    match delta {
        ScrollDelta::Lines(point) => point.y,
        ScrollDelta::Pixels(point) => f32::from(point.y) / cell_h,
    }
}

fn term_rgb(rgb: [u8; 3]) -> gpui::Rgba {
    gpui::rgb((u32::from(rgb[0]) << 16) | (u32::from(rgb[1]) << 8) | u32::from(rgb[2]))
}

fn snapped_edge(origin: f32, offset: f32, scale: f32) -> f32 {
    ((origin + offset) * scale).round() / scale
}

fn paint_cell_rect(window: &mut Window, x0: f32, y0: f32, x1: f32, y1: f32, color: gpui::Rgba) {
    if x1 > x0 && y1 > y0 {
        window.paint_quad(gpui::fill(gpui::Bounds {
            origin: gpui::point(px(x0), px(y0)),
            size: gpui::size(px(x1 - x0), px(y1 - y0)),
        }, color));
    }
}

/// Paint grid backgrounds and terminal graphics from shared, device-pixel-snapped
/// edges. GPUI's text/div background follows line and glyph bounds, and its
/// independently rounded fractional row rectangles exposed the pane color as
/// thin seams. Font box glyphs also have ascent/descent padding, so they do not
/// reach the edges of a terminal cell.
fn paint_terminal_cells(
    bounds: Bounds<Pixels>, rows: &[TermRow], cell_w: f32, cell_h: f32,
    fg_default: Hsla, accent: Hsla, light_theme: bool, window: &mut Window,
) {
    let scale = window.scale_factor();
    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    for (row, content) in rows.iter().enumerate() {
        let y0 = snapped_edge(oy, row as f32 * cell_h, scale);
        let y1 = snapped_edge(oy, (row + 1) as f32 * cell_h, scale);
        let mut col = 0;
        for span in &content.spans {
            let x0 = snapped_edge(ox, col as f32 * cell_w, scale);
            let x1 = snapped_edge(ox, (col + span.cells) as f32 * cell_w, scale);
            let bg = if span.selected { Some(accent.opacity(1.).to_rgb()) }
                else { span.bg.map(term_rgb) };
            if let Some(bg) = bg {
                paint_cell_rect(window, x0, y0, x1, y1, bg);
            }
            if let Some(ch) = span.text.chars().next().filter(|ch| is_procedural_cell(*ch)) {
                let fg = if span.selected { terminal_selection_foreground(accent) }
                    else if let Some(rgb) = span.fg {
                        term_rgb(if light_theme { readable_light_foreground(rgb, span.bg) } else { rgb })
                    } else { fg_default.to_rgb() };
                paint_terminal_graphic(window, ch, x0, y0, x1, y1, fg, scale);
            }
            col += span.cells;
        }
    }
}

fn is_procedural_cell(ch: char) -> bool {
    box_arms(ch).is_some() || ('\u{2580}'..='\u{259f}').contains(&ch)
}

// N, E, S, W. Mixed-weight box characters use the heavier common stroke.
fn box_arms(ch: char) -> Option<(u8, u8)> {
    let arms = match ch {
        '─' | '━' | '═' => 0b1010,
        '│' | '┃' | '║' => 0b0101,
        '┌' | '┍' | '┎' | '┏' | '╔' => 0b0110,
        '┐' | '┑' | '┒' | '┓' | '╗' => 0b1100,
        '└' | '┕' | '┖' | '┗' | '╚' => 0b0011,
        '┘' | '┙' | '┚' | '┛' | '╝' => 0b1001,
        '├' | '┝' | '┞' | '┟' | '┠' | '┡' | '┢' | '┣' | '╠' => 0b0111,
        '┤' | '┥' | '┦' | '┧' | '┨' | '┩' | '┪' | '┫' | '╣' => 0b1101,
        '┬' | '┭' | '┮' | '┯' | '┰' | '┱' | '┲' | '┳' | '╦' => 0b1110,
        '┴' | '┵' | '┶' | '┷' | '┸' | '┹' | '┺' | '┻' | '╩' => 0b1011,
        '┼' | '┽' | '┾' | '┿' | '╀' | '╁' | '╂' | '╃' | '╄' | '╅' | '╆' | '╇' | '╈' | '╉' | '╊' | '╋' | '╬' => 0b1111,
        '╴' | '╸' => 0b1000,
        '╵' | '╹' => 0b0001,
        '╶' | '╺' => 0b0010,
        '╷' | '╻' => 0b0100,
        '╼' => 0b1010,
        '╽' => 0b0101,
        _ => return None,
    };
    let weight = if matches!(ch, '═' | '║' | '╔' | '╗' | '╚' | '╝' | '╠' | '╣' | '╦' | '╩' | '╬') { 2 }
        else if matches!(ch, '━' | '┃' | '┏' | '┓' | '┗' | '┛' | '┣' | '┫' | '┳' | '┻' | '╋') { 1 }
        else { 0 };
    Some((arms, weight))
}

fn paint_terminal_graphic(
    window: &mut Window, ch: char, x0: f32, y0: f32, x1: f32, y1: f32,
    color: gpui::Rgba, scale: f32,
) {
    let w = x1 - x0;
    let h = y1 - y0;
    let edge = |x: f32, base: f32| (x * scale).round() / scale + base;
    let rect = |window: &mut Window, a: f32, b: f32, c: f32, d: f32| {
        paint_cell_rect(window, edge(a, x0), edge(b, y0), edge(c, x0), edge(d, y0), color);
    };
    if let Some((arms, weight)) = box_arms(ch) {
        let stroke = (if weight == 1 { 2. } else { 1. } / scale).min(w.min(h));
        let cx = w / 2.;
        let cy = h / 2.;
        let offsets: &[f32] = if weight == 2 { &[-1.5, 1.5] } else { &[0.] };
        for offset in offsets {
            let dx = *offset / scale;
            let dy = *offset / scale;
            let reach = if weight == 2 { 2. / scale } else { stroke / 2. };
            if arms & 0b0001 != 0 { rect(window, cx + dx - stroke/2., 0., cx + dx + stroke/2., cy + reach); }
            if arms & 0b0010 != 0 { rect(window, cx - reach, cy + dy - stroke/2., w, cy + dy + stroke/2.); }
            if arms & 0b0100 != 0 { rect(window, cx + dx - stroke/2., cy - reach, cx + dx + stroke/2., h); }
            if arms & 0b1000 != 0 { rect(window, 0., cy + dy - stroke/2., cx + reach, cy + dy + stroke/2.); }
        }
        return;
    }
    match ch {
        '▀' => rect(window, 0., 0., w, h/2.),
        '▄' => rect(window, 0., h/2., w, h),
        '█' => rect(window, 0., 0., w, h),
        '▌' => rect(window, 0., 0., w/2., h),
        '▐' => rect(window, w/2., 0., w, h),
        '▔' => rect(window, 0., 0., w, h/8.),
        '▕' => rect(window, w*7./8., 0., w, h),
        '▁'..='▇' => {
            let eighths = (ch as u32 - '▁' as u32 + 1) as f32;
            rect(window, 0., h*(8.-eighths)/8., w, h);
        }
        '▉'..='▏' => {
            let eighths = (8 - (ch as u32 - '▉' as u32)) as f32;
            rect(window, 0., 0., w*eighths/8., h);
        }
        '░' | '▒' | '▓' => {
            let density = match ch { '░' => 0.25, '▒' => 0.5, _ => 0.75 };
            paint_cell_rect(window, x0, y0, x1, y1,
                gpui::Rgba { a: color.a * density, ..color });
        }
        '▖'..='▟' => {
            let mask = match ch {
                '▖' => 0b0100, '▗' => 0b1000, '▘' => 0b0001,
                '▙' => 0b1101, '▚' => 0b1001, '▛' => 0b0111,
                '▜' => 0b1011, '▝' => 0b0010, '▞' => 0b0110,
                '▟' => 0b1110, _ => 0,
            };
            if mask & 1 != 0 { rect(window, 0., 0., w/2., h/2.); }
            if mask & 2 != 0 { rect(window, w/2., 0., w, h/2.); }
            if mask & 4 != 0 { rect(window, 0., h/2., w/2., h); }
            if mask & 8 != 0 { rect(window, w/2., h/2., w, h); }
        }
        _ => {}
    }
}

fn terminal_selection_foreground(background: Hsla) -> gpui::Rgba {
    let rgb = background.to_rgb();
    let linear = |channel: f32| {
        if channel <= 0.04045 {
            channel / 12.92
        } else {
            ((channel + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear(rgb.r) + 0.7152 * linear(rgb.g) + 0.0722 * linear(rgb.b);
    if luminance > 0.179 {
        term_rgb([0x10, 0x10, 0x10])
    } else {
        term_rgb([0xff, 0xff, 0xff])
    }
}

fn is_ui_zoom_in_chord(key: &str, key_char: Option<&str>, modifiers: &Modifiers) -> bool {
    modifiers.control
        && modifiers.shift
        && !modifiers.alt
        && (key == "=" || key == "+" || key_char == Some("+"))
}

fn terminal_link_hover(
    candidate: Option<(usize, usize, usize)>,
    modifiers: &Modifiers,
) -> Option<(usize, usize, usize)> {
    candidate.filter(|_| {
        !modifiers.shift
            && !modifiers.alt
            && modifiers.secondary()
    })
}

fn terminal_link_range(line: &str, column: usize) -> Option<(usize, usize)> {
    let (start, end) = word_bounds_at_column(line, column)?;
    let token: String = line.chars().skip(start).take(end - start).collect();
    let last_component = token
        .rsplit(|ch| ch == '/' || ch == '\\')
        .next()
        .unwrap_or(&token);
    let looks_like_path = token.contains('/')
        || token.contains('\\')
        || token.starts_with('~')
        || (last_component.contains('.') && last_component.chars().any(char::is_alphabetic));
    looks_like_path.then_some((start, end))
}

/// Candidate path tokens and their character columns in one editor line.
/// Ranges are UTF-8 byte offsets, as required by EditorState decorations.
fn editor_path_tokens(line: &str) -> Vec<(Range<usize>, u32)> {
    let is_word = |ch: char| ch.is_alphanumeric() || matches!(ch, '_' | '/' | '.' | '-' | '~' | ':' | '\\');
    let mut result = Vec::new();
    let mut start = None;
    let mut char_start = 0;
    let mut char_col = 0;
    for (byte, ch) in line.char_indices().chain(std::iter::once((line.len(), ' '))) {
        if is_word(ch) {
            if start.is_none() {
                start = Some(byte);
                char_start = char_col;
            }
        } else if let Some(begin) = start.take() {
            let token = &line[begin..byte];
            let leaf = token.rsplit(['/', '\\']).next().unwrap_or(token);
            if token.contains(['/', '\\']) || token.starts_with('~')
                || (leaf.contains('.') && leaf.chars().any(char::is_alphabetic))
            {
                result.push((begin..byte, char_start));
            }
        }
        char_col += 1;
    }
    result
}

fn editor_path_at_point(editor: &EditorState, text: &str, position: Point<Pixels>) -> Option<(String, u32, Range<usize>)> {
    let mut line_start = 0;
    for line_with_newline in text.split_inclusive('\n') {
        let line = line_with_newline.trim_end_matches(['\r', '\n']);
        for (range, column) in editor_path_tokens(line) {
            let absolute = (line_start + range.start)..(line_start + range.end);
            let hit = text[absolute.clone()].char_indices().any(|(offset, ch)| {
                let start = absolute.start + offset;
                editor.range_to_bounds(&(start..start + ch.len_utf8()))
                    .is_some_and(|bounds| bounds.contains(&position))
            });
            if hit {
                return Some((line.to_string(), column, absolute));
            }
        }
        line_start += line_with_newline.len();
    }
    None
}

#[cfg(test)]
mod editor_path_tests {
    use super::editor_path_tokens;

    #[test]
    fn extracts_remote_and_relative_paths_with_utf8_byte_offsets() {
        let line = "é src/my-file.rs:12 C:\\work\\other.log:3 plain";
        let tokens = editor_path_tokens(line);
        let values: Vec<_> = tokens.iter().map(|(range, _)| &line[range.clone()]).collect();
        assert_eq!(values, vec!["src/my-file.rs:12", "C:\\work\\other.log:3"]);
        assert_eq!(tokens[0].1, 2);
    }
}

#[cfg(test)]
mod terminal_link_hover_tests {
    use super::{terminal_link_hover, terminal_link_range};
    use gpui_kit::Modifiers;

    #[test]
    fn path_tokens_get_ctrl_hover_range_but_words_do_not() {
        let line = "edit src/main.rs and README";
        let path_col = line.find("main").unwrap();
        assert_eq!(terminal_link_range(line, path_col), Some((5, 16)));
        assert_eq!(terminal_link_range(line, line.find("README").unwrap()), None);
        assert_eq!(
            terminal_link_hover(Some((0, 5, 16)), &Modifiers::default()),
            None
        );
        #[cfg(not(target_os = "macos"))]
        let secondary = Modifiers {
            control: true,
            ..Modifiers::default()
        };
        #[cfg(target_os = "macos")]
        let secondary = Modifiers {
            platform: true,
            ..Modifiers::default()
        };
        assert_eq!(
            terminal_link_hover(Some((0, 5, 16)), &secondary),
            Some((0, 5, 16))
        );
    }

    #[test]
    fn path_extraction_keeps_line_suffix_and_windows_separators() {
        let line = r"open C:\work\my-file_2.rs:17:3 now";
        let col = line.find("my-file").unwrap();
        let (start, end) = terminal_link_range(line, col).unwrap();
        assert_eq!(&line[start..end], r"C:\work\my-file_2.rs:17:3");
        assert_eq!(terminal_link_range("open README now", 6), None);
    }
}

#[cfg(test)]
mod zoom_tests {
    use super::is_ui_zoom_in_chord;
    use gpui_kit::Modifiers;

    #[test]
    fn shifted_plus_and_equal_reach_ui_zoom() {
        let mods = Modifiers {
            control: true,
            shift: true,
            ..Modifiers::default()
        };
        assert!(is_ui_zoom_in_chord("=", Some("+"), &mods));
        assert!(is_ui_zoom_in_chord("+", None, &mods));
        assert!(!is_ui_zoom_in_chord("=", None, &Modifiers::default()));
    }
}

#[cfg(test)]
mod language_path_tests {
    use super::language_from_path;

    #[test]
    fn maps_common_editor_extensions_to_registered_language_names() {
        for (path, expected) in [
            ("src/lib.rs", "rust"),
            ("main.py", "python"),
            ("app.js", "javascript"),
            ("app.ts", "typescript"),
            ("view.tsx", "tsx"),
            ("data.json", "json"),
            ("Cargo.toml", "toml"),
            ("README.md", "markdown"),
            ("config.yaml", "yaml"),
            ("script.sh", "bash"),
            ("index.html", "html"),
            ("site.css", "css"),
            ("main.c", "c"),
            ("header.h", "c"),
            ("main.cpp", "cpp"),
            ("header.hpp", "cpp"),
            ("main.go", "go"),
            ("server.log", "log"),
            ("server.log.1", "log"),
            ("daemon.trace", "log"),
        ] {
            assert_eq!(language_from_path(path, None).as_deref(), Some(expected), "{path}");
        }
    }

    #[test]
    fn reported_language_takes_precedence_over_extension() {
        assert_eq!(
            language_from_path("file.txt", Some("custom-language")).as_deref(),
            Some("custom-language")
        );
    }
}

fn term_span_el(
    span: TermSpan,
    cell_w: f32,
    fg_default: Hsla,
    accent: Hsla,
    light_theme: bool,
    link_hover: bool,
) -> gpui::Div {
    let text = if span.text.chars().next().is_some_and(is_procedural_cell) {
        " ".to_string()
    } else if span.text.is_empty() {
        " ".to_string()
    } else {
        span.text
    };
    let bold = span.bold;
    let fg = span.fg.map(|rgb| {
        if light_theme && !span.selected {
            readable_light_foreground(rgb, span.bg)
        } else {
            rgb
        }
    });
    div()
        .w(px(span.cells as f32 * cell_w))
        .flex_shrink_0()
        // Each terminal cell has an explicit grid width. Center the glyph
        // within that box so fallback glyphs and fractional font advances do
        // not shift later characters away from the PTY's cursor columns.
        .text_center()
        .whitespace_nowrap()
        .when(link_hover, |el| el.underline())
        .when(bold, |el| el.font_semibold())
        .when(!span.selected, |el| match fg {
            Some(fg) => el.text_color(term_rgb(fg)),
            None => el.text_color(fg_default),
        })
        .when(span.selected, |el| {
            // Keep the selection background opaque. Semi-transparent accents
            // let terminal cell colors bleed through and made selected text
            // nearly indistinguishable in several themes.
            let selected_fg = terminal_selection_foreground(accent);
            el.text_color(selected_fg)
        })
        .child(text)
}

fn paint_terminal_cursor(
    bounds: Bounds<Pixels>,
    row: usize,
    col: usize,
    shape: CursorShape,
    cell_w: f32,
    cell_h: f32,
    color: Hsla,
    window: &mut Window,
) {
    let scale = window.scale_factor();
    let ox = f32::from(bounds.origin.x);
    let oy = f32::from(bounds.origin.y);
    let x0 = snapped_edge(ox, col as f32 * cell_w, scale);
    let x1 = snapped_edge(ox, (col + 1) as f32 * cell_w, scale);
    let y0 = snapped_edge(oy, row as f32 * cell_h, scale);
    let y1 = snapped_edge(oy, (row + 1) as f32 * cell_h, scale);
    let stroke = 1. / scale;
    let fg = color.to_rgb();
    match shape {
        CursorShape::Beam => paint_cell_rect(window, x0, y0, x0 + 2. * stroke, y1, fg),
        CursorShape::Underline => paint_cell_rect(window, x0, y1 - 2. * stroke, x1, y1, fg),
        CursorShape::Block | CursorShape::HollowBlock => {
            if shape == CursorShape::Block {
                paint_cell_rect(window, x0, y0, x1, y1, color.opacity(0.4).to_rgb());
            }
            paint_cell_rect(window, x0, y0, x1, y0 + stroke, fg);
            paint_cell_rect(window, x0, y1 - stroke, x1, y1, fg);
            paint_cell_rect(window, x0, y0, x0 + stroke, y1, fg);
            paint_cell_rect(window, x1 - stroke, y0, x1, y1, fg);
        }
        _ => {}
    }
}

struct EditorPending {
    line: Option<u32>,
    column: Option<u32>,
}

pub struct EditorPanel {
    buffer_id: String,
    path: String,
    /// Buffer has no file yet. `path` is a client key, not a disk path.
    unsaved: bool,
    /// Tab title while `unsaved` (`Untitled`, `Untitled 2`, …).
    unsaved_title: Option<String>,
    dirty: bool,
    diagnostics: Vec<BufferDiagnostic>,
    lsp_status: Option<String>,
    format_pending_text: Option<String>,
    rev: u64,
    pending: Option<EditorPending>,
    editor: Entity<EditorState>,
    ade: AdeHandle,
    workspace: WeakEntity<Workspace>,
    metrics: TabStripMetrics,
    plus_shift: Rc<Cell<f32>>,
    font_px: f32,
    closed: bool,
    word_wrap: bool,
    hover_link: Option<Range<usize>>,
    last_editor_pointer: Option<Point<Pixels>>,
    hover_decoration: TextDecorationCollection,
    markdown_preview: bool,
    markdown_preview_locked: bool,
    inline_markdown_edit: Option<MarkdownInlineEdit>,
    inline_markdown_subscription: Option<Subscription>,
    _subscription: Subscription,
}

struct MarkdownInlineEdit {
    line_index: usize,
    editor: Entity<EditorState>,
    prefix: String,
    suffix: String,
}

impl EditorPanel {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        buffer_id: String,
        path: String,
        language: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
        unsaved: bool,
        unsaved_title: Option<String>,
        ade: AdeHandle,
        workspace: WeakEntity<Workspace>,
        metrics: TabStripMetrics,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut state = EditorState::new(window, cx).line_number(true);
            // Untitled buffers are already empty — do not show a loading placeholder.
            if !unsaved {
                state = state.placeholder("Loading…");
            }
            if let Some(lang) = language_from_path(&path, language.as_deref()) {
                state = state.language(lang);
            }
            state
        });
        if language_from_path(&path, language.as_deref()).as_deref() == Some("log") {
            editor.update(cx, |state, cx| state.set_highlighter_factory(super::log_highlight::factory(), cx));
        }
        let hover_decoration = editor.update(cx, |state, cx| state.create_decorations_collection(Vec::new(), cx));
        let subscription = cx.subscribe(&editor, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.dirty = true;
                cx.notify();
            }
        });
        Self {
            buffer_id,
            path,
            unsaved,
            unsaved_title,
            dirty: false,
            diagnostics: Vec::new(),
            lsp_status: None,
            format_pending_text: None,
            rev: 0,
            pending: Some(EditorPending { line, column }),
            editor,
            ade,
            workspace,
            metrics,
            plus_shift: Rc::new(Cell::new(0.0)),
            font_px: 14.0,
            closed: false,
            word_wrap: true,
            hover_link: None,
            last_editor_pointer: None,
            hover_decoration,
            markdown_preview: false,
            markdown_preview_locked: false,
            inline_markdown_edit: None,
            inline_markdown_subscription: None,
            _subscription: subscription,
        }
    }

    pub fn set_font_px(&mut self, font_px: f32, cx: &mut Context<Self>) {
        if (self.font_px - font_px).abs() < 0.1 {
            return;
        }
        self.font_px = font_px;
        cx.notify();
    }

    pub fn set_word_wrap(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.word_wrap == enabled { return; }
        self.word_wrap = enabled;
        self.editor.update(cx, |editor, cx| editor.set_soft_wrap(enabled, window, cx));
        cx.notify();
    }

    pub fn toggle_word_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let enabled = !self.word_wrap;
        self.set_word_wrap(enabled, window, cx);
        enabled
    }

    pub fn buffer_id(&self) -> &str {
        &self.buffer_id
    }

    /// Compact file-scoped information for the workspace status bar.
    pub fn status_summary(&self) -> String {
        let mut parts = vec![format!("{} problems", self.diagnostics.len())];
        if let Some(status) = self.lsp_status.as_deref() {
            parts.push(status.to_string());
        }
        parts.join(" · ")
    }

    /// Drop the panel without sending `editor_close`. The Fresh buffer stays
    /// in the daemon worker so another workspace can reopen the same path.
    pub fn release(&mut self) {
        self.closed = true;
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn is_unsaved(&self) -> bool {
        self.unsaved
    }

    pub fn set_dirty(&mut self, dirty: bool, cx: &mut Context<Self>) {
        self.dirty = dirty;
        cx.notify();
    }

    fn link_at_pointer(&self, position: Point<Pixels>, cx: &App) -> Option<(String, u32, Range<usize>)> {
        let text = self.editor.read(cx).value().to_string();
        editor_path_at_point(self.editor.read(cx), &text, position)
    }

    fn hover_path(&mut self, position: Option<Point<Pixels>>, modifiers: &Modifiers, cx: &mut Context<Self>) {
        let next = if modifiers.secondary() && !modifiers.shift && !modifiers.alt {
            position.and_then(|position| self.link_at_pointer(position, cx)).map(|(_, _, range)| range)
        } else { None };
        if next == self.hover_link { return; }
        self.hover_link = next.clone();
        let decorations = next.map(|range| TextDecoration::new(range, gpui::HighlightStyle {
            underline: Some(gpui::UnderlineStyle { thickness: px(1.), ..Default::default() }),
            ..Default::default()
        })).into_iter().collect();
        self.hover_decoration.set(decorations, cx);
        cx.notify();
    }

    /// Resolve the pointer against rendered text, independently of caret and
    /// selection events that may run later in the mouse dispatch.
    fn open_link_at_pointer(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) -> bool {
        let Some((line, column, _)) = self.link_at_pointer(position, cx) else { return false; };
        let cwd = if self.unsaved {
            None
        } else {
            parent_dir(&self.path)
        };
        let workspace = self.workspace.clone();
        workspace
            .update(cx, |workspace, cx| {
                workspace.open_path_link(line, column, cwd, cx);
            })
            .ok();
        true
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// Full Markdown source, including an active rich-preview block edit.
    pub fn current_text(&self, cx: &App) -> String {
        let source = self.editor.read(cx).value().to_string();
        let Some(edit) = &self.inline_markdown_edit else {
            return source;
        };
        replace_markdown_line(
            &source,
            edit.line_index,
            &edit.prefix,
            &edit.editor.read(cx).value(),
            &edit.suffix,
        )
    }

    fn begin_markdown_inline_edit(
        &mut self,
        line_index: usize,
        prefix: String,
        suffix: String,
        content: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let editor = cx.new(|cx| EditorState::new(window, cx).line_number(false));
        editor.update(cx, |state, cx| state.set_value(&content, window, cx));
        let subscription = cx.subscribe(&editor, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                this.dirty = true;
                cx.notify();
            }
        });
        self.inline_markdown_edit = Some(MarkdownInlineEdit {
            line_index,
            editor,
            prefix,
            suffix,
        });
        self.inline_markdown_subscription = Some(subscription);
        cx.notify();
    }

    pub fn commit_markdown_inline_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.inline_markdown_edit.is_none() {
            return;
        }
        let text = self.current_text(cx);
        self.editor.update(cx, |state, cx| state.set_value(&text, window, cx));
        self.inline_markdown_edit = None;
        self.inline_markdown_subscription = None;
        self.dirty = true;
        cx.notify();
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
        if !path.is_empty() {
            self.path = path;
        }
        self.dirty = false;
        self.diagnostics.clear();
        self.inline_markdown_edit = None;
        self.inline_markdown_subscription = None;
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

    pub fn set_lsp_state(
        &mut self,
        rev: u64,
        text: Option<String>,
        diagnostics: Vec<BufferDiagnostic>,
        status: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(text) = text {
            if !self.dirty && rev > self.rev {
                self.editor.update(cx, |state, cx| state.set_value(&text, window, cx));
                self.rev = rev;
                self.dirty = true;
            } else if rev > self.rev {
                self.lsp_status = Some("Formatting changed the server buffer while local edits are open; save to resolve".into());
            }
        }
        self.diagnostics = diagnostics;
        if status.is_some() {
            self.lsp_status = status;
        } else if self.lsp_status.as_deref().is_some_and(|s| s.starts_with("LSP")) {
            self.lsp_status = None;
        }
        cx.notify();
    }

    pub fn apply_formatted(
        &mut self,
        rev: u64,
        text: Option<String>,
        status: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let safe = self.format_pending_text.take()
            .is_some_and(|expected| expected == self.current_text(cx));
        self.rev = rev;
        if let Some(text) = text {
            if safe {
                self.editor.update(cx, |state, cx| state.set_value(&text, window, cx));
                self.dirty = true;
                self.lsp_status = Some("Formatted; save to write changes".into());
            } else {
                self.lsp_status = Some("Formatting finished after further local edits; those edits were kept".into());
            }
        } else {
            self.lsp_status = status.or_else(|| Some("No formatting changes".into()));
        }
        cx.notify();
    }

    pub fn set_format_error(&mut self, message: String, cx: &mut Context<Self>) {
        self.format_pending_text = None;
        self.lsp_status = Some(message);
        cx.notify();
    }

    pub(crate) fn request_format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_markdown_inline_edit(window, cx);
        let text = self.current_text(cx);
        let base_rev = if self.dirty {
            self.ade.send(AdeCmd::EditBuffer {
                request_id: format!("fmt-edit-{}-{}", self.buffer_id, self.rev),
                buffer_id: self.buffer_id.clone(),
                base_rev: self.rev,
                text: text.clone(),
            });
            self.rev + 1
        } else {
            self.rev
        };
        self.format_pending_text = Some(text);
        self.ade.send(AdeCmd::FormatBuffer {
            request_id: format!("fmt-{}-{base_rev}", self.buffer_id),
            buffer_id: self.buffer_id.clone(),
            base_rev,
        });
        self.lsp_status = Some("Formatting…".into());
        cx.notify();
    }

    /// Returns the previous path when the saved path differs, so the workspace
    /// map can be re-keyed.
    pub fn mark_saved(&mut self, path: String, rev: u64, cx: &mut Context<Self>) -> Option<String> {
        let previous = (self.path != path).then(|| self.path.clone());
        self.path = path;
        self.unsaved = false;
        self.unsaved_title = None;
        self.rev = rev;
        self.dirty = false;
        cx.notify();
        previous
    }

    fn label(&self) -> String {
        let name = if self.unsaved {
            self.unsaved_title
                .clone()
                .unwrap_or_else(|| "Untitled".to_string())
        } else {
            let shown = display_path(&self.path);
            path_basename(&shown)
                .unwrap_or(shown.as_str())
                .to_string()
        };
        if self.dirty {
            format!("• {name}")
        } else {
            name
        }
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
        let workspace = self.workspace.clone();
        let focus = self.editor.read(cx).focus_handle(cx);
        cx.defer(move |cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.note_editor_active(&path, active, cx);
                })
                .ok();
        });
        if active {
            focus.focus(window, cx);
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
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            workspace
                .update(cx, |workspace, cx| workspace.forget_editor(&path, cx))
                .ok();
        });
    }
}

impl DockPanel for EditorPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let panel_id = PanelId::from(cx.entity().entity_id());
        let metrics = self.metrics.clone();
        h_flex()
            .id(format!("editor-title-{}", self.buffer_id))
            .gap_1()
            .items_center()
            .min_w_0()
            .on_prepaint(move |bounds, _, _| note_tab_edge(&metrics, bounds))
            .child(Icon::new(IconName::File).small())
            .child(div().text_ellipsis().child(self.label()))
            .child(tab_close_button(
                format!("close-ed-{}", self.buffer_id),
                workspace.clone(),
                panel_id,
            ))
            .context_menu(move |menu, _, cx| {
                with_close_items(menu, workspace.clone(), panel_id, true, cx)
            })
    }

    fn title_suffix(&mut self, _: &mut Window, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let panel_id = PanelId::from(cx.entity().entity_id());
        Some(new_terminal_button(
            format!("new-term-ed-{}", self.buffer_id),
            self.metrics.clone(),
            self.plus_shift.clone(),
            self.workspace.clone(),
            panel_id,
        ))
    }

    fn dropdown_menu(
        &mut self,
        menu: PopupMenu,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> PopupMenu {
        let panel_id = PanelId::from(cx.entity().entity_id());
        with_close_items(menu, self.workspace.clone(), panel_id, false, cx)
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
        let markdown = matches!(
            self.path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref(),
            Some("md" | "markdown")
        );
        let mut root = div().key_context("Editor").size_full().flex().flex_col();
        if markdown {
            let panel = cx.entity();
            root = root.child(
                h_flex()
                    .w_full()
                    .h_8()
                    .px_2()
                    .gap_1()
                    .items_center()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child(
                        if self.markdown_preview_locked {
                            "Markdown · preview locked (view only)"
                        } else {
                            "Markdown · click preview text to edit"
                        }
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("markdown-source")
                            .ghost()
                            .xsmall()
                            .label("Source")
                            .selected(!self.markdown_preview)
                            .on_click({
                                let panel = panel.clone();
                                move |_, window, cx| {
                                    panel.update(cx, |this, cx| {
                                        this.commit_markdown_inline_edit(window, cx);
                                        this.markdown_preview = false;
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        Button::new("markdown-preview")
                            .ghost()
                            .xsmall()
                            .label("Preview")
                            .selected(self.markdown_preview)
                            .on_click(move |_, _, cx| {
                                panel.update(cx, |this, cx| {
                                    this.markdown_preview = true;
                                    cx.notify();
                                });
                            }),
                    )
                    .child({
                        let panel = cx.entity().clone();
                        let locked = self.markdown_preview_locked;
                        Button::new("markdown-preview-lock")
                            .ghost()
                            .xsmall()
                            .icon(if locked {
                                gpui_kit::assets::IconName::Lock
                            } else {
                                gpui_kit::assets::IconName::LockOpen
                            })
                            .tooltip(if locked {
                                "Unlock preview editing"
                            } else {
                                "Lock preview (view only)"
                            })
                            .selected(locked)
                            .on_click(move |_, window, cx| {
                                panel.update(cx, |this, cx| {
                                    if !this.markdown_preview_locked {
                                        this.commit_markdown_inline_edit(window, cx);
                                    }
                                    this.markdown_preview_locked = !this.markdown_preview_locked;
                                    cx.notify();
                                });
                            })
                    })
            );
        }

        if markdown && self.markdown_preview {
            let content = self.editor.read(cx).value();
            let inline_edit = self
                .inline_markdown_edit
                .as_ref()
                .map(|edit| (edit.line_index, edit.editor.clone()));
            root = root.child(render_markdown_preview(
                &content,
                cx.theme().muted,
                cx.theme().muted_foreground,
                cx.entity(),
                inline_edit,
                self.markdown_preview_locked,
            ));
        } else {
            let panel = cx.entity();
            root = root.child(
                div()
                    .flex_1()
                    .size_full()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .capture_action::<gpui_kit::component::input::Copy>(
                        cx.listener(|this, _, window, cx| {
                            let has_selection =
                                !this.editor.read(cx).selected_value().is_empty();
                            if has_selection {
                                clipboard::notify_copied(window, cx);
                            }
                        }),
                    )
                    .when(self.hover_link.is_some(), |pane| pane.cursor_pointer())
                    .on_mouse_move({
                        let panel = panel.clone();
                        move |event: &MouseMoveEvent, _, cx| {
                            panel.update(cx, |this, cx| {
                                this.last_editor_pointer = Some(event.position);
                                this.hover_path(Some(event.position), &event.modifiers, cx);
                            });
                        }
                    })
                    .on_mouse_exit({
                        let panel = panel.clone();
                        move |_, _, cx| {
                            panel.update(cx, |this, cx| {
                                this.last_editor_pointer = None;
                                this.hover_path(None, &Modifiers::default(), cx);
                            });
                        }
                    })
                    .on_modifiers_changed({
                        let panel = panel.clone();
                        move |event: &ModifiersChangedEvent, _, cx| {
                            panel.update(cx, |this, cx| this.hover_path(this.last_editor_pointer, &event.modifiers, cx));
                        }
                    })
                    .capture_any_mouse_down(move |event: &MouseDownEvent, _, cx| {
                        if event.button != MouseButton::Left || event.modifiers.shift || event.modifiers.alt
                            || !event.modifiers.secondary() { return; }
                        if panel.update(cx, |this, cx| this.open_link_at_pointer(event.position, cx)) {
                            cx.stop_propagation();
                        }
                    })
                    .child(
                        Editor::new(&self.editor)
                            .bordered(false)
                            .p_0()
                            .flex_1()
                            .size_full()
                            .min_h_0()
                            .text_size(px(self.font_px))
                            .font_family(cx.theme().mono_font_family.clone()),
                    ),
            );
        }
        if !self.diagnostics.is_empty() {
            let mut problems = v_flex().id("editor-problems").w_full().h(px(112.))
                .overflow_y_scroll().border_t_1().border_color(cx.theme().border);
            for (index, diagnostic) in self.diagnostics.iter().enumerate() {
                let row_panel = cx.entity();
                let line = diagnostic.start_line;
                let utf16_col = diagnostic.start_character;
                let source = diagnostic.source.as_deref().unwrap_or("LSP");
                let label = format!("{}:{} {}: {}", line + 1, utf16_col + 1,
                    source, diagnostic.message.replace('\n', " "));
                problems = problems.child(
                    div().id(format!("problem-{index}")).px_2().py_1()
                        .text_xs().text_color(if diagnostic.severity == "error" {
                            cx.theme().danger
                        } else { cx.theme().muted_foreground })
                        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                            row_panel.update(cx, |this, cx| {
                                let source = this.current_text(cx);
                                let col = utf16_to_scalar_column(&source, line, utf16_col);
                                this.editor.update(cx, |state, cx| {
                                    state.set_cursor_position(Position::new(line, col), window, cx);
                                });
                            });
                        })
                        .child(label),
                );
            }
            root = root.child(problems);
        }
        root
    }
}

fn utf16_to_scalar_column(text: &str, line: u32, utf16_col: u32) -> u32 {
    let Some(content) = text.lines().nth(line as usize) else { return 0 };
    let mut units = 0;
    let mut scalars = 0;
    for ch in content.chars() {
        if units + ch.len_utf16() as u32 > utf16_col { break; }
        units += ch.len_utf16() as u32;
        scalars += 1;
    }
    scalars
}

#[cfg(test)]
mod lsp_position_tests {
    use super::utf16_to_scalar_column;

    #[test]
    fn diagnostic_columns_after_non_bmp_characters() {
        assert_eq!(utf16_to_scalar_column("first\na😀b\n", 1, 3), 2);
        assert_eq!(utf16_to_scalar_column("first\na😀b\n", 1, 4), 3);
    }
}

/// A lightweight native Markdown presentation for the first WYSIWYG pass.
/// Source remains authoritative and editable in Source mode; Preview reflects
/// live changes and gives block structure without a second document model.


fn markdown_inline_elements(text: &str) -> impl IntoElement {
    // Compose a single line from inline markdown parts without nested interactive editors.
    let mut row = h_flex().flex_wrap().gap_0();
    for part in parse_inline_markdown(text) {
        row = match part {
            InlinePart::Text(s) => row.child(s),
            InlinePart::Bold(s) => row.child(div().font_bold().child(s)),
            InlinePart::Italic(s) => row.child(div().italic().child(s)),
            InlinePart::Code(s) => row.child(
                div()
                    .font_family("monospace")
                    .px_1()
                    .child(s),
            ),
            InlinePart::Link { text, .. } => row.child(div().underline().child(text)),
            InlinePart::Image { alt, .. } => row.child(div().italic().child(format!("[image: {alt}]"))),
            InlinePart::Math(s) => row.child(div().font_family("monospace").italic().child(s)),
        };
    }
    row
}

#[derive(Debug, Clone)]
enum InlinePart {
    Text(String),
    Bold(String),
    Italic(String),
    Code(String),
    Link { text: String, href: String },
    Image { alt: String, src: String },
    Math(String),
}

fn parse_inline_markdown(input: &str) -> Vec<InlinePart> {
    let mut out = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;
    let mut buf = String::new();
    let flush = |buf: &mut String, out: &mut Vec<InlinePart>| {
        if !buf.is_empty() {
            out.push(InlinePart::Text(std::mem::take(buf)));
        }
    };
    while i < chars.len() {
        if chars[i] == '!' && i + 1 < chars.len() && chars[i + 1] == '[' {
            if let Some((alt, src, next)) = parse_link_like(&chars, i + 1) {
                flush(&mut buf, &mut out);
                out.push(InlinePart::Image { alt, src });
                i = next;
                continue;
            }
        }
        if chars[i] == '[' {
            if let Some((text, href, next)) = parse_link_like(&chars, i) {
                flush(&mut buf, &mut out);
                out.push(InlinePart::Link { text, href });
                i = next;
                continue;
            }
        }
        if chars[i] == '`' {
            if let Some(end) = chars[i + 1..].iter().position(|c| *c == '`') {
                flush(&mut buf, &mut out);
                let content: String = chars[i + 1..i + 1 + end].iter().collect();
                out.push(InlinePart::Code(content));
                i = i + 2 + end;
                continue;
            }
        }
        if chars[i] == '$' {
            let display = i + 1 < chars.len() && chars[i + 1] == '$';
            let start = if display { i + 2 } else { i + 1 };
            let delim_len = if display { 2 } else { 1 };
            let mut rel = None;
            let mut j = start;
            while j + delim_len - 1 < chars.len() {
                if (0..delim_len).all(|k| chars[j + k] == '$') {
                    rel = Some(j - start);
                    break;
                }
                j += 1;
            }
            if let Some(rel) = rel {
                flush(&mut buf, &mut out);
                let content: String = chars[start..start + rel].iter().collect();
                out.push(InlinePart::Math(content.trim().to_string()));
                i = start + rel + delim_len;
                continue;
            }
        }
        if (chars[i] == '*' || chars[i] == '_') && i + 1 < chars.len() && chars[i + 1] == chars[i] {
            let marker = chars[i];
            if let Some(end) = find_closing_double(&chars, i + 2, marker) {
                flush(&mut buf, &mut out);
                let content: String = chars[i + 2..end].iter().collect();
                out.push(InlinePart::Bold(content));
                i = end + 2;
                continue;
            }
        }
        if chars[i] == '*' || chars[i] == '_' {
            let marker = chars[i];
            if let Some(end) = chars[i + 1..].iter().position(|c| *c == marker) {
                if end > 0 {
                    flush(&mut buf, &mut out);
                    let content: String = chars[i + 1..i + 1 + end].iter().collect();
                    out.push(InlinePart::Italic(content));
                    i = i + 2 + end;
                    continue;
                }
            }
        }
        buf.push(chars[i]);
        i += 1;
    }
    flush(&mut buf, &mut out);
    out
}

fn find_closing_double(chars: &[char], start: usize, marker: char) -> Option<usize> {
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == marker && chars[i + 1] == marker {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn parse_link_like(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    if start >= chars.len() || chars[start] != '[' {
        return None;
    }
    let close = chars[start + 1..].iter().position(|c| *c == ']')? + start + 1;
    if close + 1 >= chars.len() || chars[close + 1] != '(' {
        return None;
    }
    let end = chars[close + 2..].iter().position(|c| *c == ')')? + close + 2;
    let text: String = chars[start + 1..close].iter().collect();
    let href: String = chars[close + 2..end].iter().collect();
    Some((text, href, end + 1))
}

fn markdown_task_item(line: &str) -> Option<(bool, &str)> {
    let t = line.trim_start();
    for prefix in ["- [ ] ", "- [x] ", "- [X] ", "* [ ] ", "* [x] ", "* [X] "] {
        if let Some(rest) = t.strip_prefix(prefix) {
            return Some((prefix.contains('x') || prefix.contains('X'), rest));
        }
    }
    None
}

fn markdown_table_row(line: &str) -> Option<Vec<String>> {
    let t = line.trim();
    if !t.starts_with('|') || !t.ends_with('|') {
        return None;
    }
    let cells: Vec<String> = t
        .trim_matches('|')
        .split('|')
        .map(|c| c.trim().to_string())
        .collect();
    if cells.is_empty() {
        return None;
    }
    if cells
        .iter()
        .all(|c| !c.is_empty() && c.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '))
    {
        return Some(vec![]);
    }
    Some(cells)
}

fn list_indent_px(line: &str) -> f32 {
    let spaces = line.chars().take_while(|c| *c == ' ').count();
    let tabs = line.chars().take_while(|c| *c == '\t').count();
    ((spaces / 2) + tabs) as f32 * 12.0
}

fn render_markdown_preview(
    source: &str,
    block_bg: Hsla,
    muted_fg: Hsla,
    panel: Entity<EditorPanel>,
    inline_edit: Option<(usize, Entity<EditorState>)>,
    locked: bool,
) -> impl IntoElement {
    let mut in_code = false;
    let mut body = v_flex().id("markdown-preview-content").w_full().h_full().overflow_y_scroll().p_6().gap_2();
    for (line_index, line) in source.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        let is_code = in_code;
        let (prefix, display_text, suffix) = markdown_edit_parts(line, is_code)
            .unwrap_or_else(|| (String::new(), line.to_string(), String::new()));
        if inline_edit.as_ref().is_some_and(|(index, _)| *index == line_index) {
            let editor = inline_edit.as_ref().unwrap().1.clone();
            body = body.child(
                Editor::new(&editor)
                    .bordered(false)
                    .p_1()
                    .w_full()
                    .text_size(px(15.)),
            );
            continue;
        }
        let rendered = if in_code {
            div()
                .w_full()
                .px_3()
                .py_1()
                .bg(block_bg)
                .font_family("monospace")
                .child(display_text.clone())
        } else if trimmed.is_empty() {
            div().h_2()
        } else if trimmed.starts_with("$$") && trimmed.ends_with("$$") && trimmed.len() > 4 {
            div()
                .w_full()
                .py_2()
                .text_center()
                .font_family("monospace")
                .italic()
                .child(trimmed.trim_matches('$').trim().to_string())
        } else if let Some((level, text)) = markdown_heading(trimmed) {
            let font = match level {
                1 => px(28.),
                2 => px(24.),
                3 => px(20.),
                _ => px(17.),
            };
            div()
                .font_semibold()
                .text_size(font)
                .child(markdown_inline_elements(text))
        } else if let Some(text) = trimmed.strip_prefix("> ") {
            h_flex()
                .gap_2()
                .child(div().w(px(3.)).h_full().bg(block_bg))
                .child(div().italic().text_color(muted_fg).child(markdown_inline_elements(text)))
        } else if let Some((done, text)) = markdown_task_item(line) {
            h_flex()
                .gap_2()
                .pl(px(16. + list_indent_px(line)))
                .child(if done { "☑" } else { "☐" })
                .child(markdown_inline_elements(text))
        } else if let Some(text) = markdown_list_item(trimmed) {
            h_flex()
                .gap_2()
                .pl(px(16. + list_indent_px(line)))
                .child("•")
                .child(markdown_inline_elements(text))
        } else if let Some(cells) = markdown_table_row(trimmed) {
            if cells.is_empty() {
                div().h_0()
            } else {
                h_flex()
                    .w_full()
                    .gap_2()
                    .border_b_1()
                    .border_color(block_bg)
                    .children(cells.into_iter().map(|c| {
                        div().flex_1().p_1().child(markdown_inline_elements(&c))
                    }))
            }
        } else if trimmed.starts_with("---") || trimmed.starts_with("***") {
            div().w_full().h(px(1.)).my_2().bg(block_bg)
        } else if trimmed.starts_with('<') && trimmed.ends_with('>') {
            // Best-effort HTML: strip tags for preview.
            let stripped = trimmed
                .replace("<br>", "\n")
                .replace("<br/>", "\n")
                .replace("<br />", "\n");
            let mut plain = String::new();
            let mut in_tag = false;
            for ch in stripped.chars() {
                match ch {
                    '<' => in_tag = true,
                    '>' => in_tag = false,
                    _ if !in_tag => plain.push(ch),
                    _ => {}
                }
            }
            div().text_size(px(15.)).child(markdown_inline_elements(plain.trim()))
        } else {
            div().text_size(px(15.)).child(markdown_inline_elements(trimmed))
        };
        let line_panel = panel.clone();
        let line_prefix = prefix.clone();
        let line_suffix = suffix.clone();
        let line_content = display_text.clone();
        body = body.child(
            div()
                .id(format!("markdown-preview-line-{line_index}"))
                .w_full()
                .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                    if locked {
                        return;
                    }
                    line_panel.update(cx, |this, cx| {
                        this.commit_markdown_inline_edit(window, cx);
                        this.begin_markdown_inline_edit(
                            line_index,
                            line_prefix.clone(),
                            line_suffix.clone(),
                            line_content.clone(),
                            window,
                            cx,
                        );
                    });
                })
                .child(rendered),
        );
    }
    body
}

fn markdown_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|ch| *ch == '#').count();
    (1..=6)
        .contains(&hashes)
        .then(|| (hashes, line[hashes..].trim_start()))
}

fn markdown_list_item(line: &str) -> Option<&str> {
    ["- ", "* ", "+ "]
        .iter()
        .find_map(|prefix| line.strip_prefix(prefix))
        .or_else(|| {
            let (number, rest) = line.split_once(". ")?;
            (!number.is_empty() && number.chars().all(|ch| ch.is_ascii_digit())).then_some(rest)
        })
}

fn markdown_edit_parts(line: &str, in_code: bool) -> Option<(String, String, String)> {
    if in_code || line.trim().is_empty() {
        return Some((String::new(), line.to_string(), String::new()));
    }

    let indent = line.len() - line.trim_start().len();
    let content = &line[indent..];
    let hashes = content.chars().take_while(|ch| *ch == '#').count();
    if (1..=6).contains(&hashes) {
        let rest = &content[hashes..];
        let spaces = rest.chars().take_while(|ch| ch.is_whitespace()).map(char::len_utf8).sum::<usize>();
        if spaces > 0 {
            let prefix_end = indent + hashes + spaces;
            return Some((line[..prefix_end].to_string(), line[prefix_end..].to_string(), String::new()));
        }
    }

    if content.starts_with("> ") {
        return Some((line[..indent + 2].to_string(), content[2..].to_string(), String::new()));
    }
    if content.len() >= 2
        && matches!(content.as_bytes()[0], b'-' | b'*' | b'+')
        && content.as_bytes()[1].is_ascii_whitespace()
    {
        let spaces = content[1..].chars().take_while(|ch| ch.is_whitespace()).map(char::len_utf8).sum::<usize>();
        let prefix_end = indent + 1 + spaces;
        return Some((line[..prefix_end].to_string(), line[prefix_end..].to_string(), String::new()));
    }
    if let Some((number, rest)) = content.split_once(". ")
        && !number.is_empty()
        && number.chars().all(|ch| ch.is_ascii_digit())
    {
        let prefix_end = indent + number.len() + 2;
        return Some((line[..prefix_end].to_string(), rest.to_string(), String::new()));
    }

    Some((String::new(), line.to_string(), String::new()))
}

fn replace_markdown_line(
    source: &str,
    line_index: usize,
    prefix: &str,
    content: &str,
    suffix: &str,
) -> String {
    let mut lines: Vec<String> = source.split_inclusive('\n').map(str::to_string).collect();
    if lines.is_empty() {
        lines.push(String::new());
    }
    let Some(line) = lines.get_mut(line_index) else {
        return source.to_string();
    };
    let body_len = line.trim_end_matches(['\r', '\n']).len();
    let ending = line[body_len..].to_string();
    *line = format!("{prefix}{content}{suffix}{ending}");
    lines.concat()
}

#[cfg(test)]
mod markdown_preview_tests {
    use super::{
        InlinePart, markdown_edit_parts, markdown_heading, markdown_list_item,
        markdown_task_item, parse_inline_markdown, replace_markdown_line,
    };

    
    #[test]
    fn parse_inline_markdown_bold_and_code() {
        let parts = parse_inline_markdown("a **b** and `c`");
        assert!(parts.iter().any(|p| matches!(p, InlinePart::Bold(s) if s == "b")));
        assert!(parts.iter().any(|p| matches!(p, InlinePart::Code(s) if s == "c")));
    }

    #[test]
    fn task_items_and_math_markers() {
        assert_eq!(markdown_task_item("- [x] done"), Some((true, "done")));
        let parts = parse_inline_markdown("energy $E=mc^2$ here");
        assert!(parts.iter().any(|p| matches!(p, InlinePart::Math(s) if s == "E=mc^2")));
    }

    #[test]
    fn recognizes_heading_depth_and_list_items() {
        assert_eq!(markdown_heading("### Details"), Some((3, "Details")));
        assert_eq!(markdown_heading("####### not a heading"), None);
        assert_eq!(markdown_list_item("- first"), Some("first"));
        assert_eq!(markdown_list_item("4. fourth"), Some("fourth"));
        assert_eq!(markdown_list_item("not a list"), None);
    }

    #[test]
    fn inline_preview_edits_preserve_markers_and_line_endings() {
        assert_eq!(
            markdown_edit_parts("### Old title", false),
            Some(("### ".into(), "Old title".into(), String::new()))
        );
        assert_eq!(
            replace_markdown_line("# old\r\n- item\r\n", 1, "- ", "updated", ""),
            "# old\r\n- updated\r\n"
        );
    }
}

pub(super) fn note_tab_edge(metrics: &TabStripMetrics, bounds: Bounds<Pixels>) {
    let top = f32::from(bounds.origin.y);
    let right = f32::from(bounds.origin.x + bounds.size.width);
    metrics.note_tab(top, right);
}

pub(super) fn tab_close_button(
    id: impl Into<ElementId>,
    workspace: WeakEntity<Workspace>,
    panel_id: PanelId,
) -> impl IntoElement {
    Button::new(id)
        .ghost()
        .xsmall()
        .icon(IconName::Close)
        .tooltip("Close (Ctrl+W)")
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, window, cx| {
            cx.stop_propagation();
            workspace
                .update(cx, |workspace, cx| {
                    workspace.close_panel_id(panel_id, window, cx);
                })
                .ok();
        })
}

pub(super) fn new_terminal_button(
    id: impl Into<ElementId>,
    metrics: TabStripMetrics,
    plus_shift: Rc<Cell<f32>>,
    workspace: WeakEntity<Workspace>,
    source_panel: PanelId,
) -> impl IntoElement {
    let shift = plus_shift.get();
    div().relative().w(px(20.)).h(px(20.)).child(
        div()
            .absolute()
            .top_0()
            .left(px(-shift))
            .on_prepaint(move |bounds, _, _| {
                let top = f32::from(bounds.origin.y);
                let anchor = f32::from(bounds.origin.x) + shift;
                plus_shift.set(metrics.plus_shift(top, anchor));
            })
            .child(
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .tooltip("New Terminal or New File")
                    .dropdown_menu(move |menu, _, _| {
                        let new_tab = workspace.clone();
                        let new_file = workspace.clone();
                        menu.item(PopupMenuItem::new("New Terminal").on_click(move |_, _, cx| {
                            new_tab
                                .update(cx, |workspace, cx| {
                                    workspace.new_terminal_in_group(source_panel, cx);
                                    cx.notify();
                                })
                                .ok();
                        }))
                        .item(PopupMenuItem::new("New File").on_click(move |_, _, cx| {
                            new_file
                                .update(cx, |workspace, cx| {
                                    workspace.new_file_in_group(source_panel, cx);
                                    cx.notify();
                                })
                                .ok();
                        }))
                    }),
            ),
    )
}

pub(super) fn with_close_items(
    menu: PopupMenu,
    workspace: WeakEntity<Workspace>,
    panel_id: PanelId,
    include_this: bool,
    cx: &App,
) -> PopupMenu {
    let (others_ok, right_ok) = workspace
        .read_with(cx, |workspace, cx| {
            workspace.tab_close_availability(panel_id, cx)
        })
        .unwrap_or((false, false));
    let mut menu = menu;
    if include_this {
        let workspace = workspace.clone();
        menu = menu.item(PopupMenuItem::new("Close").on_click(move |_, window, cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.close_panel_id(panel_id, window, cx)
                })
                .ok();
        }));
    }
    if let Some(pinned) = workspace
        .read_with(cx, |workspace, _| workspace.tab_pin_state(panel_id))
        .ok()
        .flatten()
    {
        let pin_workspace = workspace.clone();
        menu = menu.item(
            PopupMenuItem::new(if pinned { "Unpin Tab" } else { "Pin Tab" }).on_click(
                move |_, _, cx| {
                    pin_workspace
                        .update(cx, |workspace, cx| workspace.toggle_pin(panel_id, cx))
                        .ok();
                },
            ),
        );
    }
    let workspace_others = workspace.clone();
    let workspace_right = workspace.clone();
    menu.item(
        PopupMenuItem::new("Close Others")
            .disabled(!others_ok)
            .on_click(move |_, window, cx| {
                workspace_others
                    .update(cx, |workspace, cx| {
                        workspace.close_panel_scope(panel_id, TabCloseScope::Others, window, cx);
                    })
                    .ok();
            }),
    )
    .item(
        PopupMenuItem::new("Close to the Right")
            .disabled(!right_ok)
            .on_click(move |_, window, cx| {
                workspace_right
                    .update(cx, |workspace, cx| {
                        workspace.close_panel_scope(
                            panel_id,
                            TabCloseScope::ToTheRight,
                            window,
                            cx,
                        );
                    })
                    .ok();
            }),
    )
    .item({
        let workspace = workspace.clone();
        PopupMenuItem::new("Close All Editors").on_click(move |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.close_all_editors(window, cx)).ok();
        })
    })
    .item({
        let workspace = workspace.clone();
        PopupMenuItem::new("Close All Terminals").on_click(move |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.close_all_terminals(window, cx)).ok();
        })
    })
    .item({
        let workspace = workspace.clone();
        PopupMenuItem::new("Close All Other Terminals").on_click(move |_, window, cx| {
            workspace.update(cx, |workspace, cx| workspace.close_all_other_terminals(Some(panel_id), window, cx)).ok();
        })
    })
    .item(PopupMenuItem::new("Close All Other Tabs").on_click(move |_, window, cx| {
        workspace.update(cx, |workspace, cx| workspace.close_all_other_tabs(Some(panel_id), window, cx)).ok();
    }))
}

pub(crate) fn language_from_path(path: &str, reported: Option<&str>) -> Option<String> {
    let filename = path.rsplit(['/', '\\']).next()?.to_ascii_lowercase();
    let ext = filename.rsplit('.').next()?;
    if matches!(ext, "log" | "logs" | "trace")
        || (ext.chars().all(|ch| ch.is_ascii_digit())
            && filename.rsplit_once('.').is_some_and(|(base, _)| base.ends_with(".log") || base.ends_with(".trace")))
    {
        return Some("log".into());
    }
    if let Some(lang) = reported.filter(|s| !s.is_empty()) {
        return Some(lang.to_string());
    }
    let name = match ext {
        "rs" => "rust",
        "ts" => "typescript",
        "tsx" => "tsx",
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
        "c" | "h" => "c",
        "cc" | "cpp" | "cxx" | "hh" | "hpp" | "hxx" => "cpp",
        _ => return None,
    };
    Some(name.into())
}

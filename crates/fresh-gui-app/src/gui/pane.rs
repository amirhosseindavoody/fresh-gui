//! Dock panels for terminals and editors.
//!
//! Splits, tab reorder, and merging tabs back into one group are gpui-component's
//! dock (`DockArea` / `TabGroup`). These panels are the surfaces that dock hosts.
//! Tab titles are per workspace. `SessionTabTitle::workspace_id` is the workspace
//! that owns the panel.

use std::cell::Cell;
use std::collections::VecDeque;
use std::ops::Range;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use alacritty_terminal::vte::ansi::CursorShape;
use fresh_gui_client::edit_sync::{EditSync, SnapshotReconciliation, contiguous_diff, external_reconciliation, ExternalSnapshotReconciliation};
use fresh_gui_protocol::{BufferDiagnostic, ByteRange, ByteSelection, EditorAction, ExternalResolution, MAX_PAGE_BYTES};
use fresh_gui_protocol::{LspRequest, LspRequestFeature, LspResult};

use gpui_kit::base::ElementExt as _;
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent, PanelId};
use gpui_kit::component::input::{Editor, EditorState, Input, InputState, InputEvent, Position, TextDecoration, TextDecorationCollection};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, StyledExt as _,
    button::{Button, ButtonVariants as _},
    h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;
use gpui_kit::base::{Selectable as _, Disableable as _};

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
    resize_task: Option<Task<()>>,
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
            resize_task: None,
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
            // The dock paints all tab shells alike. Tint the terminal title
            // itself so terminal tabs remain distinct in light and dark themes.
            .rounded(cx.theme().radius)
            .px_1()
            .bg(cx.theme().accent.opacity(if cx.theme().is_dark() { 0.42 } else { 0.32 }))
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
            && self.resize_task.is_none()
        {
            // A short settle window coalesces the several intermediate sizes
            // GPUI reports while a window or split is being dragged.
            let timer = cx.background_executor().timer(std::time::Duration::from_millis(75));
            self.resize_task = Some(cx.spawn(async move |this, cx| {
                timer.await;
                let _ = this.update(cx, |this, cx| {
                    this.resize_task = None;
                    if let Some((cols, rows)) = this.pending_grid.get()
                        && (cols != this.screen.cols || rows != this.screen.rows)
                    {
                        this.screen.resize(cols, rows);
                        this.ade.send(AdeCmd::ResizePty {
                            id: this.pty_id.clone(),
                            cols: cols.clamp(1, u16::MAX as usize) as u16,
                            rows: rows.clamp(1, u16::MAX as usize) as u16,
                        });
                        cx.notify();
                    }
                });
            }));
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
                        let Some(next) = grid_size(bounds.size, cell_w, cell_h) else {
                            // Hidden/collapsed panes can temporarily have no
                            // usable area. Keep the last PTY size instead of
                            // issuing a zero or guessed 80x24 resize.
                            return;
                        };
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

fn grid_size(size: Size<Pixels>, cell_w: f32, cell_h: f32) -> Option<(usize, usize)> {
    let width = f32::from(size.width);
    let height = f32::from(size.height);
    if width < cell_w || height < cell_h {
        return None;
    }
    let cols = ((width / cell_w).floor() as usize).clamp(2, 500);
    let rows = ((height / cell_h).floor() as usize).clamp(1, 200);
    Some((cols, rows))
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

const PAGE_VIEW_BYTES: usize = MAX_PAGE_BYTES / 2;

struct PageView {
    start: usize,
    total_bytes: usize,
}

struct EditorPending {
    line: Option<u32>,
    column: Option<u32>,
}

static NEXT_EDITOR_VIEW: AtomicU64 = AtomicU64::new(1);
static NEXT_LSP_REQUEST: AtomicU64 = AtomicU64::new(1);

struct PendingLsp {
    request: LspRequest,
    text: String,
    sent: bool,
    reply: async_channel::Sender<LspResult>,
}

#[path = "editor_search.rs"]
mod editor_search;

pub struct EditorPanel {
    search: editor_search::SearchUi,
    buffer_id: String,
    path: String,
    /// Buffer has no file yet. `path` is a client key, not a disk path.
    unsaved: bool,
    /// Tab title while `unsaved` (`Untitled`, `Untitled 2`, …).
    unsaved_title: Option<String>,
    dirty: bool,
    diagnostics: Vec<BufferDiagnostic>,
    lsp_status: Option<String>,
    lsp_requests: bool,
    lsp_provider: Option<Rc<super::lsp::RemoteLsp>>,
    pending_lsp: Vec<PendingLsp>,
    lsp_request_tracker: fresh_gui_client::lsp_sync::LspRequestTracker,
    signature_triggers: Vec<String>,
    completion_plans: Vec<super::lsp::CompletionPlan>,
    signature_help: Option<String>,
    signature_active: bool,
    signature_snapshot: Option<(String, usize)>,
    recovery_warning: Option<String>,
    format_pending_text: Option<String>,
    format_sent_selection: Option<ByteSelection>,
    format_request_id: Option<String>,
    rev: u64,
    view_id: String,
    range_edits: bool,
    page: Option<PageView>,
    page_request: Option<String>,
    pending_page: Option<usize>,
    navigation_offset: Option<usize>,
    byte_offset_input: Entity<InputState>,
    draft_recovery: bool,
    transport_connected: bool,
    edit_sync: Option<EditSync>,
    edit_flush_scheduled: bool,
    edit_request_id: Option<String>,
    sync_request_id: Option<String>,
    edit_sent_selection: Option<ByteSelection>,
    legacy_sent_text: Option<String>,
    next_request: u64,
    pending_save: Option<String>,
    pending_format: bool,
    pending_format_range: Option<fresh_gui_protocol::ByteRange>,
    format_inflight: bool,
    pending_actions: VecDeque<EditorAction>,
    action_inflight: bool,
    action_sent_text: Option<String>,
    action_sent_selection: Option<ByteSelection>,
    conflict: bool,
    external_changes: bool,
    external: Option<ExternalNotice>,
    external_generation: Option<String>,
    external_pending: Option<ExternalResolution>,
    external_request: Option<(String, String, String)>,
    external_save_path: Option<String>,
    sync_paused: bool,
    save_request_id: Option<String>,
    save_sent_text: Option<String>,
    pending: Option<EditorPending>,
    project_reveal: Option<std::ops::Range<usize>>,
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
    _lsp_observer: Subscription,
}

fn reload_response_can_replace(sent_draft: &str, current_draft: &str) -> bool {
    sent_draft == current_draft
}

#[cfg(test)]
mod external_reload_tests {
    use super::reload_response_can_replace;

    #[test]
    fn reload_discards_only_the_reviewed_draft() {
        assert!(reload_response_can_replace("reviewed draft", "reviewed draft"));
        assert!(!reload_response_can_replace("reviewed draft", "new typing after reload"));
    }
}

#[derive(Clone)]
struct ExternalNotice {
    path: String,
    generation: String,
    disk_text: Option<String>,
    kept: bool,
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
        let byte_offset_input = cx.new(|cx| InputState::new(window, cx).placeholder("Byte offset"));
        let hover_decoration = editor.update(cx, |state, cx| state.create_decorations_collection(Vec::new(), cx));
        let search = editor_search::SearchUi::new(&editor, window, cx);
        let subscription = cx.subscribe(&editor, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                let completion_accepted = this.on_lsp_text_change(cx);
                this.dirty = true;
                if completion_accepted { this.flush_pending(cx); }
                else { this.schedule_edit_flush(cx); }
                this.refresh_search(cx);
                cx.notify();
            } else if matches!(ev, InputEvent::Blur) {
                this.cancel_lsp_requests();
                this.signature_help = None;
                this.signature_active = false;
                this.signature_snapshot = None;
            }
        });
        let lsp_observer = cx.observe(&editor, |this, editor, cx| {
            let state = editor.read(cx);
            let caret = state.cursor();
            let draft = state.value().to_string();
            let completion_caret_moved = this.completion_plans.iter().any(|plan|
                plan.before == draft) && this.completion_plans.iter().any(|plan| plan.request_offset != caret);
            if completion_caret_moved {
                this.lsp_request_tracker.cancel(LspRequestFeature::Completion);
                this.completion_plans.clear();
                editor.update(cx, |editor, cx| editor.dismiss_completion_overlay(cx));
            }
            let mut retained = Vec::new();
            for pending in this.pending_lsp.drain(..) {
                if !matches!(pending.request.feature, LspRequestFeature::Hover | LspRequestFeature::Capabilities)
                    && pending.request.offset != caret {
                    this.lsp_request_tracker.cancel(pending.request.feature);
                    if pending.sent { this.ade.send(AdeCmd::LspCancel { request_id: pending.request.request_id,
                        buffer_id: pending.request.buffer_id, view_id: pending.request.view_id }); }
                } else { retained.push(pending); }
            }
            this.pending_lsp = retained;
            if this.signature_snapshot.as_ref().is_some_and(|(text, offset)| text == &draft && *offset != caret) {
                this.signature_help = None;
                this.signature_active = false;
                this.signature_snapshot = None;
                this.lsp_request_tracker.cancel(LspRequestFeature::SignatureHelp);
                cx.notify();
            }
        });
        let view_id = format!("view-{}-{}-{}", std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default().as_nanos(),
            NEXT_EDITOR_VIEW.fetch_add(1, Ordering::Relaxed));
        Self {
            search,
            buffer_id,
            path,
            unsaved,
            unsaved_title,
            dirty: false,
            diagnostics: Vec::new(),
            lsp_status: None,
            lsp_requests: false,
            lsp_provider: None,
            pending_lsp: Vec::new(),
            lsp_request_tracker: Default::default(),
            signature_triggers: Vec::new(),
            completion_plans: Vec::new(),
            signature_help: None,
            signature_active: false,
            signature_snapshot: None,
            recovery_warning: None,
            format_pending_text: None,
            format_sent_selection: None,
            format_request_id: None,
            rev: 0,
            view_id,
            range_edits: false,
            page: None,
            page_request: None,
            pending_page: None,
            navigation_offset: None,
            byte_offset_input,
            draft_recovery: false,
            transport_connected: true,
            edit_sync: unsaved.then(|| EditSync::new(String::new(), 0)),
            edit_flush_scheduled: false,
            edit_request_id: None,
            sync_request_id: None,
            edit_sent_selection: None,
            legacy_sent_text: None,
            next_request: 1,
            pending_save: None,
            pending_format: false,
            pending_format_range: None,
            format_inflight: false,
            pending_actions: VecDeque::new(),
            action_inflight: false,
            action_sent_text: None,
            action_sent_selection: None,
            conflict: false,
            external_changes: false,
            external: None,
            external_generation: None,
            external_pending: None,
            external_request: None,
            external_save_path: None,
            sync_paused: false,
            save_request_id: None,
            save_sent_text: None,
            pending: Some(EditorPending { line, column }),
            project_reveal: None,
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
            _lsp_observer: lsp_observer,
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

    pub(crate) fn revision(&self) -> u64 { self.rev }
    pub(crate) fn navigation_selection(&self, cx: &App) -> ByteSelection { self.byte_selection(cx) }
    pub(crate) fn cancel_workspace_lsp(&mut self) {
        let features = [LspRequestFeature::PrepareRename, LspRequestFeature::Rename, LspRequestFeature::CodeActions, LspRequestFeature::CodeActionResolve, LspRequestFeature::ExecuteCommand];
        for feature in features {
            self.lsp_request_tracker.cancel(feature);
            let mut retained = Vec::new();
            for pending in self.pending_lsp.drain(..) {
                if pending.request.feature == feature {
                    if pending.sent { self.ade.send(AdeCmd::LspCancel { request_id: pending.request.request_id, buffer_id: pending.request.buffer_id, view_id: pending.request.view_id }); }
                } else { retained.push(pending); }
            }
            self.pending_lsp = retained;
        }
    }
    pub(crate) fn apply_workspace_edit_snapshot(&mut self, rev: u64, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let diagnostics = self.diagnostics.clone();
        self.set_lsp_state(rev, Some(text), diagnostics, None, window, cx);
    }

    pub fn begin_paged(&mut self, rev: u64, total_bytes: usize, path: String, dirty: bool, cx: &mut Context<Self>) {
        self.cancel_lsp_requests();
        self.editor.update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
        self.path = path;
        self.rev = rev;
        self.dirty |= dirty;
        self.pending = None; // Global line numbers are unknown until Fresh scans the file.
        if let Some(page) = self.page.as_mut() {
            page.total_bytes = total_bytes;
            if self.page_request.is_none() && self.sync_request_id.is_none() {
                self.request_page_sync(cx);
            }
        } else {
            self.page = Some(PageView { start: 0, total_bytes });
            self.edit_sync = None;
            self.request_page_now(0, cx);
        }
        cx.notify();
    }

    fn request_page_sync(&mut self, _cx: &mut Context<Self>) {
        let Some(page) = self.page.as_ref() else { return; };
        let start = page.start;
        let len = self.edit_sync.as_ref().map_or(PAGE_VIEW_BYTES, |sync| sync.acknowledged().0.len().max(1));
        let request_id = self.next_edit_request("page-sync");
        self.sync_request_id = Some(request_id.clone());
        self.ade.send(AdeCmd::ReadBuffer { request_id, buffer_id: self.buffer_id.clone(), view_id: self.view_id.clone(), start, len });
    }

    fn request_page_now(&mut self, start: usize, cx: &mut Context<Self>) {
        let request_id = self.next_edit_request("page");
        self.page_request = Some(request_id.clone());
        self.ade.send(AdeCmd::ReadBuffer { request_id, buffer_id: self.buffer_id.clone(), view_id: self.view_id.clone(), start, len: PAGE_VIEW_BYTES });
        cx.notify();
    }

    fn navigate_page(&mut self, start: usize, cx: &mut Context<Self>) {
        if self.page_request.is_some() || self.conflict || !self.transport_connected || self.sync_paused { return; }
        self.pending_page = Some(start);
        self.flush_pending(cx); // Preserve edits before replacing the visible window.
    }

    #[allow(clippy::too_many_arguments)]
    pub fn apply_page(&mut self, request_id: &str, view_id: &str, rev: u64, start: usize,
        total_bytes: usize, text: String, selection: ByteSelection, accepted: bool, dirty: bool,
        window: &mut Window, cx: &mut Context<Self>) {
        if view_id != self.view_id { return; }
        if self.page_request.as_deref() == Some(request_id) {
            self.page_request = None;
            self.reset_search_scope(cx);
            self.page = Some(PageView { start, total_bytes });
            self.rev = rev;
            let draft = self.current_text(cx);
            let had_local_changes = self.edit_sync.as_ref().map_or(!draft.is_empty(), |sync| sync.acknowledged().0 != draft);
            let (sync, outcome) = EditSync::from_initial_snapshot(text.clone(), rev, &draft, had_local_changes);
            self.edit_sync = Some(sync);
            self.conflict = outcome == SnapshotReconciliation::Conflict;
            self.pending = None;
            if !self.conflict {
                self.set_editor_text_and_selection(&text, Some(ByteSelection { anchor: 0, head: 0 }), window, cx);
            }
            self.editor.update(cx, |state, cx| state.set_scroll_offset(point(px(0.), px(0.)), cx));
            self.dirty = dirty || self.conflict;
            self.lsp_status = Some("Paged file · line numbers and Find refer to this page".into());
            self.apply_navigation_offset(window, cx);
            self.flush_pending(cx);
            cx.notify();
        } else if self.edit_request_id.as_deref() == Some(request_id) || self.sync_request_id.as_deref() == Some(request_id) {
            // A reply for another window must never rebase this page's draft.
            if self.page.as_ref().is_none_or(|page| page.start != start) {
                self.handle_request_error(request_id, "The server page moved; local edits retained", cx);
                return;
            }
            if let Some(page) = self.page.as_mut() { page.total_bytes = total_bytes; }
            let selection = ByteSelection { anchor: selection.anchor.saturating_sub(start).min(text.len()), head: selection.head.saturating_sub(start).min(text.len()) };
            self.apply_edit_result(request_id, view_id, rev, text, selection, accepted, dirty, window, cx);
        }
    }

    pub fn configure_lsp_requests(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.lsp_requests = enabled && self.range_edits;
        if !self.lsp_requests { self.cancel_lsp_requests(); }
        let provider = super::lsp::RemoteLsp::new(cx.weak_entity());
        self.lsp_provider = self.lsp_requests.then(|| provider.clone());
        self.editor.update(cx, |editor, cx| {
            editor.lsp_mut().completion_provider = self.lsp_requests.then(|| provider.clone() as Rc<dyn gpui_kit::component::input::CompletionProvider>);
            editor.lsp_mut().hover_provider = self.lsp_requests.then_some(provider as Rc<dyn gpui_kit::component::input::HoverProvider>);
            editor.refresh(cx);
        });
    }

    pub(crate) fn lsp_snapshot_matches(&self, text: &str, offset: Option<usize>, cx: &App) -> bool {
        self.lsp_requests && self.transport_connected && !self.closed && self.page.is_none() && !self.markdown_preview
            && !self.conflict && !self.sync_paused && self.current_text(cx) == text
            && offset.is_none_or(|offset| self.byte_selection(cx).head == offset)
    }

    pub(crate) fn lsp_result_matches(&self, result: &LspResult, text: &str, offset: Option<usize>, cx: &App) -> bool {
        !result.stale && result.rev == self.rev
            && self.lsp_request_tracker.is_current(result.feature, result.request_id)
            && self.lsp_snapshot_matches(text, offset, cx)
    }

    pub(crate) fn set_completion_plans(&mut self, plans: Vec<super::lsp::CompletionPlan>, cx: &mut Context<Self>) {
        let draft = self.current_text(cx);
        if plans.iter().all(|plan| plan.before == draft) { self.completion_plans = plans; }
    }

    fn cancel_lsp_requests(&mut self) {
        self.lsp_request_tracker.clear();
        self.signature_help = None;
        self.signature_active = false;
        self.signature_snapshot = None;
        for pending in self.pending_lsp.drain(..) {
            if pending.sent {
                self.ade.send(AdeCmd::LspCancel { request_id: pending.request.request_id,
                    buffer_id: pending.request.buffer_id, view_id: pending.request.view_id });
            }
            // Closing the channel also releases a provider waiting on a superseded request.
        }
        self.completion_plans.clear();
    }

    pub(crate) fn queue_lsp(&mut self, feature: LspRequestFeature, offset: usize,
        trigger_character: Option<String>, text: String, cx: &mut Context<Self>) -> async_channel::Receiver<LspResult> {
        self.queue_lsp_payload(feature, offset, trigger_character, text, None, cx)
    }

    pub(crate) fn queue_lsp_payload(&mut self, feature: LspRequestFeature, offset: usize,
        trigger_character: Option<String>, text: String, item: Option<serde_json::Value>,
        cx: &mut Context<Self>) -> async_channel::Receiver<LspResult> {
        self.queue_lsp_target(feature, offset, trigger_character, text, item, None, cx)
    }

    // Preserve the existing request bridge arguments while adding provider targeting.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn queue_lsp_target(&mut self, feature: LspRequestFeature, offset: usize,
        trigger_character: Option<String>, text: String, item: Option<serde_json::Value>, server: Option<String>,
        cx: &mut Context<Self>) -> async_channel::Receiver<LspResult> {
        let (reply, receiver) = async_channel::bounded(1);
        if !self.lsp_requests || !self.transport_connected || self.closed || self.page.is_some()
            || self.conflict || self.sync_paused || self.markdown_preview || self.current_text(cx) != text {
            return receiver;
        }
        let mut retained = Vec::new();
        for pending in self.pending_lsp.drain(..) {
            if pending.request.feature == feature {
                if pending.sent {
                    self.ade.send(AdeCmd::LspCancel { request_id: pending.request.request_id,
                        buffer_id: pending.request.buffer_id, view_id: pending.request.view_id });
                }
            } else { retained.push(pending); }
        }
        self.pending_lsp = retained;
        let request_id = NEXT_LSP_REQUEST.fetch_add(1, Ordering::Relaxed);
        self.lsp_request_tracker.start(feature, request_id);
        self.pending_lsp.push(PendingLsp {
            request: LspRequest { request_id, buffer_id: self.buffer_id.clone(), view_id: self.view_id.clone(),
                base_rev: self.rev, offset, feature, trigger_character, item, server },
            text, sent: false, reply,
        });
        // Flush typing first; the request is sent only after its exact draft is acknowledged.
        self.flush_pending(cx);
        let timer = cx.background_executor().timer(std::time::Duration::from_secs(7));
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, _| {
                if let Some(index) = this.pending_lsp.iter().position(|pending| pending.request.request_id == request_id) {
                    let pending = this.pending_lsp.remove(index);
                    if pending.sent { this.ade.send(AdeCmd::LspCancel { request_id,
                        buffer_id: pending.request.buffer_id, view_id: pending.request.view_id }); }
                }
            });
        }).detach();
        receiver
    }

    fn send_pending_lsp(&mut self, cx: &mut Context<Self>) {
        if self.edit_request_id.is_some() || self.save_request_id.is_some() || self.format_inflight { return; }
        let draft = self.current_text(cx);
        let Some(sync) = self.edit_sync.as_ref() else { return; };
        let (acknowledged, rev) = sync.acknowledged();
        if acknowledged != draft { return; }
        self.pending_lsp.retain(|pending| pending.text == draft && !pending.reply.is_closed());
        for pending in &mut self.pending_lsp {
            if !pending.sent {
                pending.request.base_rev = rev;
                pending.sent = true;
                self.ade.send(AdeCmd::LspRequest { request: pending.request.clone() });
            }
        }
    }

    pub fn apply_lsp_result(&mut self, mut result: LspResult, cx: &mut Context<Self>) {
        if result.view_id != self.view_id { return; }
        let Some(index) = self.pending_lsp.iter().position(|pending| pending.request.request_id == result.request_id) else { return; };
        let pending = self.pending_lsp.remove(index);
        let draft = self.current_text(cx);
        let caret_matches = matches!(result.feature, LspRequestFeature::Hover | LspRequestFeature::Capabilities)
            || self.byte_selection(cx).head == result.offset;
        result.stale |= result.rev != self.rev || pending.text != draft || !caret_matches
            || pending.request.base_rev != result.rev || pending.request.feature != result.feature;
        if !result.stale {
            if let Some(provider) = &self.lsp_provider { provider.update_triggers(&result); }
            self.signature_triggers = result.signature_triggers.clone();
            if let Some(status) = &result.status { self.lsp_status = Some(status.clone()); }
        }
        let _ = pending.reply.try_send(result);
    }

    fn on_lsp_text_change(&mut self, cx: &mut Context<Self>) -> bool {
        let draft = self.current_text(cx);
        self.diagnostics.clear();
        self.editor.update(cx, |editor, cx| { if let Some(set) = editor.diagnostics_mut() { set.clear(); } cx.notify(); });
        // Invalidate callbacks that received their reply before this change.
        self.lsp_request_tracker.clear();
        for pending in &self.pending_lsp {
            if pending.text == draft { self.lsp_request_tracker.start(pending.request.feature, pending.request.request_id); }
        }
        let accepted = self.completion_plans.iter().find(|plan| plan.after == draft);
        let completion_accepted = accepted.is_some();
        if let Some(plan) = accepted {
            let cursor = plan.cursor;
            self.editor.update(cx, |editor, cx| editor.set_selected_range(cursor..cursor, cx));
        }
        self.completion_plans.clear();
        let mut retained = Vec::new();
        for pending in self.pending_lsp.drain(..) {
            if pending.text != draft {
                if pending.sent { self.ade.send(AdeCmd::LspCancel { request_id: pending.request.request_id,
                    buffer_id: pending.request.buffer_id, view_id: pending.request.view_id }); }
            } else { retained.push(pending); }
        }
        self.pending_lsp = retained;
        self.signature_help = None;
        self.signature_snapshot = None;
        let offset = self.byte_selection(cx).head;
        if self.lsp_requests && self.page.is_none() && (self.signature_active || self.signature_triggers.iter().any(|trigger|
            !trigger.is_empty() && draft.get(..offset).is_some_and(|prefix| prefix.ends_with(trigger)))) {
            self.request_signature(cx);
        }
        completion_accepted
    }

    fn request_signature(&mut self, cx: &mut Context<Self>) {
        self.signature_active = true;
        let offset = self.byte_selection(cx).head;
        let text = self.current_text(cx);
        let trigger = self.signature_triggers.iter().find(|trigger| text.get(..offset).is_some_and(|prefix| prefix.ends_with(trigger.as_str()))).cloned();
        let receiver = self.queue_lsp(LspRequestFeature::SignatureHelp, offset, trigger, text.clone(), cx);
        cx.spawn(async move |this, cx| {
            if let Ok(result) = receiver.recv().await {
                let _ = this.update(cx, |this, cx| {
                    if this.signature_active && this.lsp_result_matches(&result, &text, Some(offset), cx) {
                        this.signature_help = super::lsp::signature_text(&result);
                        this.signature_active = this.signature_help.is_some();
                        this.signature_snapshot = this.signature_help.as_ref().map(|_| (text.clone(), offset));
                        cx.notify();
                    }
                });
            }
        }).detach();
    }

    pub(crate) fn request_language_help(&mut self, feature: LspRequestFeature, window: &mut Window, cx: &mut Context<Self>) {
        if !self.lsp_requests || self.page.is_some() || !self.transport_connected {
            self.lsp_status = Some(if self.page.is_some() { "Language help is unavailable for paged files" }
                else { "Language help requires a daemon with lsp.requests and editor.range-edits" }.into());
            cx.notify();
            return;
        }
        if feature == LspRequestFeature::SignatureHelp { self.request_signature(cx); return; }
        let offset = self.byte_selection(cx).head;
        let text = self.current_text(cx);
        let receiver = self.queue_lsp(feature, offset, None, text.clone(), cx);
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(result) = receiver.recv().await {
                let _ = this.update_in(cx, |this, _window, cx| {
                    if !this.lsp_result_matches(&result, &text, Some(offset), cx) { return; }
                    if feature == LspRequestFeature::Completion {
                        let (items, plans) = super::lsp::normalize_completions(&text, offset, &result);
                        this.completion_plans = plans;
                        this.editor.update(cx, |editor, cx| editor.present_completion_items(offset, "", items, cx));
                    } else if let Some(hover) = super::lsp::merge_hover(&result) {
                        this.editor.update(cx, |editor, cx| editor.present_hover(offset..offset, hover, cx));
                    }
                });
            }
        }).detach();
    }

    /// Absolute byte locations work for both normal buffers and paged views.
    pub(crate) fn navigation_location(&self, cx: &App) -> fresh_gui_client::navigation::EditorLocation {
        fresh_gui_client::navigation::EditorLocation {
            path: self.path.clone(), buffer_id: Some(self.buffer_id.clone()), view_id: self.view_id.clone(),
            offset: self.page.as_ref().map_or(0, |page| page.start) + self.byte_selection(cx).head,
        }
    }

    pub(crate) fn focus_navigation_source(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor.update(cx, |editor, cx| editor.focus(window, cx));
    }

    pub(crate) fn navigation_snapshot(&self, cx: &App) -> (String, usize) {
        (self.current_text(cx), self.byte_selection(cx).head)
    }

    pub(crate) fn restore_preview_position(&mut self, offset: usize, cx: &mut Context<Self>) {
        let text = self.current_text(cx);
        if offset <= text.len() && text.is_char_boundary(offset) {
            self.editor.update(cx, |state, cx| state.set_selected_range(offset..offset, cx));
        }
    }

    /// Preview only a fully loaded buffer; paged global line numbers require
    /// daemon indexing. This moves the cursor without stealing finder focus.
    pub(crate) fn preview_line(&mut self, query: &str, cx: &mut Context<Self>) -> Option<String> {
        if self.page.is_some() { return None; }
        let text = self.current_text(cx);
        let offset = fresh_gui_client::finder::line_position(&text, query)?;
        self.editor.update(cx, |state, cx| state.set_selected_range(offset..offset, cx));
        let start = text[..offset].rfind('\n').map_or(0, |pos| pos + 1);
        let end = text[offset..].find('\n').map_or(text.len(), |pos| offset + pos);
        Some(text[start..end].chars().take(200).collect())
    }

    pub(crate) fn cancel_navigation(&mut self) {
        let mut retained = Vec::new();
        for pending in self.pending_lsp.drain(..) {
            if super::workspace::navigation_feature(pending.request.feature) {
                self.lsp_request_tracker.cancel(pending.request.feature);
                if pending.sent { self.ade.send(AdeCmd::LspCancel {
                    request_id: pending.request.request_id, buffer_id: pending.request.buffer_id,
                    view_id: pending.request.view_id }); }
            } else { retained.push(pending); }
        }
        // Invalidate replies that have already arrived but await presentation.
        for feature in [LspRequestFeature::Definition, LspRequestFeature::Declaration,
            LspRequestFeature::TypeDefinition, LspRequestFeature::Implementation,
            LspRequestFeature::References, LspRequestFeature::DocumentSymbols, LspRequestFeature::WorkspaceSymbols] {
            self.lsp_request_tracker.cancel(feature);
        }
        self.pending_lsp = retained;
    }

    pub(crate) fn reveal_byte(&mut self, offset: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.navigation_offset = Some(offset);
        self.pending = None;
        self.markdown_preview = false;
        if let Some(page) = &self.page {
            let end = page.start + self.current_text(cx).len();
            if self.page_request.is_some() { return; }
            if offset < page.start || offset >= end {
                self.navigate_page(offset.saturating_sub(PAGE_VIEW_BYTES / 4), cx);
                return;
            }
        } else if self.edit_sync.is_none() {
            return; // Initial snapshot will reveal it, including on older daemons.
        }
        self.apply_navigation_offset(window, cx);
    }

    fn apply_navigation_offset(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(offset) = self.navigation_offset else { return; };
        let start = self.page.as_ref().map_or(0, |page| page.start);
        if self.page.is_some() && (offset < start || offset > start + self.current_text(cx).len()) {
            self.navigate_page(offset.saturating_sub(PAGE_VIEW_BYTES / 4), cx);
            return;
        }
        self.navigation_offset = None;
        let offset = offset.saturating_sub(start).min(self.current_text(cx).len());
        self.editor.update(cx, |editor, cx| {
            editor.set_selected_range(offset..offset, cx);
            editor.focus(window, cx);
        });
        cx.notify();
    }

    pub fn configure_range_edits(&mut self, enabled: bool, cx: &mut Context<Self>) {
        self.range_edits = enabled;
        // EditorOpen/New always supplies the initial BufferSnapshot. Do not
        // race it with a second snapshot that could undo its reveal position.
        cx.notify();
    }

    pub fn configure_draft_recovery(&mut self, enabled: bool) {
        self.draft_recovery = enabled;
    }

    pub fn configure_external_changes(&mut self, enabled: bool) {
        self.external_changes = enabled;
    }

    pub fn check_external(&self) {
        if self.external_changes && !self.unsaved && self.page.is_none() {
            self.ade.send(AdeCmd::CheckExternal {
                request_id: format!("external-{}", self.view_id),
                buffer_id: self.buffer_id.clone(),
            });
        }
    }

    pub fn apply_external_change(
        &mut self, path: String, rev: u64, generation: String, text: String, disk_text: Option<String>,
        server_dirty: bool, window: &mut Window, cx: &mut Context<Self>,
    ) {
        if self.page.is_some() {
            // Lazy pieces still use the source file. Never install the legacy
            // empty notice text as a page, or offer unsafe overwrite/reload.
            self.dirty |= server_dirty;
            self.sync_paused = true;
            self.lsp_status = Some("File changed on disk; paged reads, edits and save are blocked. Recovery journal retained; close and reopen after reviewing the disk change.".into());
            cx.notify();
            return;
        }
        if self.external.is_none() && self.external_generation.as_deref() == Some(generation.as_str()) {
            return;
        }
        let decision = external_reconciliation(self.dirty, self.rev, rev, disk_text.is_some(), server_dirty);
        if decision == ExternalSnapshotReconciliation::Ignore { return; }
        if decision == ExternalSnapshotReconciliation::Reload {
            self.external = None;
            self.external_generation = Some(generation);
            self.apply_snapshot(rev, text, self.path.clone(), window, cx);
            self.dirty = false;
        } else {
            let kept = self.external.as_ref().is_some_and(|notice| notice.generation == generation && notice.kept);
            self.external = Some(ExternalNotice { path, generation, disk_text, kept });
            // The snapshot is the daemon draft, never disk text. Reconcile only
            // when no edit acknowledgement is outstanding; that reply owns its base.
            if self.edit_request_id.is_none() && self.sync_request_id.is_none() {
                self.apply_snapshot(rev, text, self.path.clone(), window, cx);
            }
            self.dirty = true;
            cx.notify();
        }
    }

    fn request_external_resolution(&mut self, resolution: ExternalResolution, cx: &mut Context<Self>) {
        if self.external.is_none() || self.external_request.is_some() { return; }
        if self.conflict {
            // Keeping or explicitly discarding a divergent draft also resolves
            // its stale transaction base, before submitting the disk decision.
            self.resolve_keep_local(cx);
        }
        self.external_pending = Some(resolution);
        self.sync_paused = false;
        self.pending_actions.clear();
        self.pending_format = false;
        self.flush_pending(cx);
    }

    pub fn apply_external_resolution(
        &mut self, request_id: &str, rev: u64, generation: String, text: String,
        accepted: bool, server_dirty: bool, resolution: ExternalResolution,
        window: &mut Window, cx: &mut Context<Self>,
    ) {
        let Some((pending_id, sent_draft, sent_generation)) = self.external_request.take() else { return; };
        if pending_id != request_id {
            self.external_request = Some((pending_id, sent_draft, sent_generation));
            return;
        }
        if !accepted {
            self.external_save_path = None;
            self.lsp_status = Some("File or draft changed again; review the current version".into());
            self.check_external();
            cx.notify();
            return;
        }
        let current = self.current_text(cx);
        self.rev = rev;
        self.edit_sync = Some(EditSync::new(text.clone(), rev));
        self.conflict = false;
        if resolution == ExternalResolution::Reload {
            self.external_generation = Some(generation.clone());
            self.external = None;
            self.external_save_path = None;
            if reload_response_can_replace(&sent_draft, &current) {
                let selection = map_selection_through_edits(&current, &text, self.byte_selection(cx));
                self.set_editor_text_and_selection(&text, Some(selection), window, cx);
                self.dirty = server_dirty;
            } else {
                // Typing after clicking Reload belongs to a newer draft.
                self.dirty = true;
            }
        } else {
            if let Some(notice) = self.external.as_mut()
                && notice.generation == generation && sent_generation == generation {
                notice.kept = true;
            }
            self.dirty = server_dirty || current != text;
            if resolution == ExternalResolution::Overwrite {
                self.external_generation = Some(generation);
                self.external = None;
                self.pending_save = self.external_save_path.take();
            }
        }
        self.flush_pending(cx);
        cx.notify();
    }

    pub fn external_target_path(&self) -> Option<String> {
        self.external.as_ref().map(|notice| notice.path.clone())
    }

    pub fn reattach_reloaded_path(&mut self, path: String, cx: &mut Context<Self>) -> Option<String> {
        let previous = (self.path != path).then(|| self.path.clone());
        self.path = path;
        self.unsaved = false;
        self.unsaved_title = None;
        cx.notify();
        previous
    }

    fn compare_external(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(notice) = &self.external {
            let disk = notice.disk_text.clone().unwrap_or_default();
            let draft = self.current_text(cx);
            let path = self.path.clone();
            let deleted = notice.disk_text.is_none();
            let _ = self.workspace.update(cx, |workspace, cx| {
                workspace.compare_external(path, disk, draft, deleted, window, cx);
            });
        }
    }

    /// Retention is established by a durable daemon acknowledgement for exactly
    /// the visible text. Capability negotiation alone never authorizes closing.
    pub fn recovery_guaranteed(&self, cx: &App) -> bool {
        !self.search_has_accepted() && self.draft_recovery && self.transport_connected && !self.conflict
            && self.external_request.is_none() && self.external_pending.is_none()
            && !self.sync_paused && self.edit_request_id.is_none()
            && self.sync_request_id.is_none() && !self.action_inflight
            && self.save_request_id.is_none() && !self.format_inflight
            && self.edit_sync.as_ref().is_some_and(|sync| sync.acknowledged().0 == self.current_text(cx))
    }

    pub fn flush_for_recovery(&mut self, cx: &mut Context<Self>) {
        self.checkpoint_search_review(cx);
        self.flush_pending(cx);
    }

    pub fn save_in_progress(&self) -> bool {
        self.pending_save.is_some() || self.save_request_id.is_some() || self.edit_request_id.is_some()
    }

    pub fn discard_recovery(&mut self, _cx: &mut Context<Self>) {
        if self.draft_recovery && self.transport_connected {
            let request_id = self.next_edit_request("discard");
            self.ade.send(AdeCmd::DiscardDraft { request_id, buffer_id: self.buffer_id.clone() });
        }
    }

    pub fn note_recovery_warning(&mut self, message: String, cx: &mut Context<Self>) {
        self.recovery_warning = Some(message);
        self.dirty = true;
        cx.notify();
    }

    fn next_edit_request(&mut self, prefix: &str) -> String {
        let id = format!("{prefix}-{}-{}", self.view_id, self.next_request);
        self.next_request = self.next_request.wrapping_add(1);
        id
    }

    fn byte_selection(&self, cx: &App) -> ByteSelection {
        let (editor, offset) = if let Some(edit) = &self.inline_markdown_edit {
            let source = self.editor.read(cx).value();
            let offset = source
                .split_inclusive('\n')
                .take(edit.line_index)
                .map(str::len)
                .sum::<usize>()
                + edit.prefix.len();
            (edit.editor.read(cx), offset)
        } else {
            (self.editor.read(cx), 0)
        };
        let cursor = editor.cursor();
        let range = editor.selected_range();
        let anchor = if range.is_empty() {
            cursor
        } else if cursor == range.start {
            range.end
        } else {
            range.start
        };
        ByteSelection {
            anchor: offset + anchor,
            head: offset + cursor,
        }
    }

    fn schedule_edit_flush(&mut self, cx: &mut Context<Self>) {
        if self.edit_flush_scheduled || self.conflict {
            return;
        }
        self.edit_flush_scheduled = true;
        let timer = cx
            .background_executor()
            .timer(std::time::Duration::from_millis(75));
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                this.edit_flush_scheduled = false;
                this.flush_pending(cx);
            });
        })
        .detach();
    }

    fn flush_pending(&mut self, cx: &mut Context<Self>) {
        if !self.transport_connected
            || self.closed
            || self.page_request.is_some()
            || self.external_request.is_some()
            || self.conflict
            || self.sync_paused
            || self.edit_request_id.is_some()
            || self.action_inflight
            || self.save_request_id.is_some()
            || self.sync_request_id.is_some()
            || self.format_inflight
        {
            return;
        }
        let draft = self.current_text(cx);
        let Some(sync) = self.edit_sync.as_mut() else {
            if !self.range_edits {
                return;
            }
            let request_id = self.next_edit_request("sync");
            self.sync_request_id = Some(request_id.clone());
            self.ade.send(AdeCmd::SyncBuffer {
                request_id,
                buffer_id: self.buffer_id.clone(),
                view_id: self.view_id.clone(),
            });
            return;
        };
        let edit = sync.begin_edit(&draft);
        if let Some((base_rev, mut edits)) = edit {
            let viewport = self.page.as_ref().map(|page| ByteRange { start: page.start, len: self.edit_sync.as_ref().map_or(0, |sync| sync.acknowledged().0.len()) });
            if let Some(page) = self.page.as_ref() {
                for edit in &mut edits { edit.start += page.start; edit.end += page.start; }
            }
            let request_id = self.next_edit_request("edit");
            self.edit_request_id = Some(request_id.clone());
            self.edit_sent_selection = Some(self.byte_selection(cx));
            if self.range_edits {
                self.ade.send(AdeCmd::RangeEdit {
                    request_id,
                    buffer_id: self.buffer_id.clone(),
                    view_id: self.view_id.clone(),
                    base_rev,
                    edits,
                    viewport,
                    selection: {
                        let local = self.edit_sent_selection.expect("just recorded");
                        let start = self.page.as_ref().map_or(0, |page| page.start);
                        ByteSelection { anchor: local.anchor + start, head: local.head + start }
                    },
                });
            } else {
                self.legacy_sent_text = Some(draft.clone());
                self.ade.send(AdeCmd::EditBuffer {
                    request_id,
                    buffer_id: self.buffer_id.clone(),
                    base_rev,
                    text: draft,
                });
            }
            return;
        }
        self.drain_pending(cx);
    }

    fn drain_pending(&mut self, cx: &mut Context<Self>) {
        if !self.transport_connected
            || self.closed
            || self.page_request.is_some()
            || self.external_request.is_some()
            || self.conflict
            || self.sync_paused
            || self.edit_request_id.is_some()
            || self.action_inflight
            || self.save_request_id.is_some()
            || self.sync_request_id.is_some()
            || self.format_inflight
        {
            return;
        }
        let draft = self.current_text(cx);
        let dirty_against_server = self
            .edit_sync
            .as_ref()
            .is_some_and(|sync| sync.acknowledged().0 != draft);
        if dirty_against_server {
            self.flush_pending(cx);
            return;
        }
        if self.pending_save.is_none() && self.pending_actions.is_empty() && !self.pending_format
            && let Some(start) = self.pending_page.take() {
            self.request_page_now(start, cx);
            return;
        }
        if let Some(resolution) = self.external_pending.take() {
            if let Some(notice) = self.external.clone() {
                let request_id = self.next_edit_request("external-resolve");
                let base_rev = self.edit_sync.as_ref().map_or(self.rev, |sync| sync.acknowledged().1);
                self.external_request = Some((request_id.clone(), draft, notice.generation.clone()));
                self.ade.send(AdeCmd::ResolveExternal { request_id, buffer_id: self.buffer_id.clone(),
                    base_rev, generation: notice.generation, resolution });
            }
            return;
        }
        if let Some(action) = self.pending_actions.pop_front() {
            let base_rev = self
                .edit_sync
                .as_ref()
                .map_or(self.rev, |sync| sync.acknowledged().1);
            let request_id = self.next_edit_request("action");
            self.edit_request_id = Some(request_id.clone());
            self.action_inflight = true;
            self.action_sent_text = Some(draft);
            self.action_sent_selection = Some(self.byte_selection(cx));
            self.ade.send(AdeCmd::ActionBuffer {
                request_id,
                buffer_id: self.buffer_id.clone(),
                view_id: self.view_id.clone(),
                base_rev,
                action,
                selection: self.action_sent_selection.expect("just recorded"),
            });
            return;
        }
        if self.external.is_some() && self.pending_save.is_some() {
            self.external_save_path = self.pending_save.take();
            self.lsp_status = Some("Disk differs from this draft. Review, then choose Overwrite disk to save.".into());
            cx.notify();
            return;
        }
        if let Some(path) = self.pending_save.take() {
            let base_rev = self
                .edit_sync
                .as_ref()
                .map_or(self.rev, |sync| sync.acknowledged().1);
            let request_id = self.next_edit_request("save");
            self.save_request_id = Some(request_id.clone());
            self.save_sent_text = Some(draft.clone());
            self.ade.send(AdeCmd::SaveBuffer {
                request_id,
                buffer_id: self.buffer_id.clone(),
                base_rev,
                path,
            });
        } else if self.pending_format {
            self.pending_format = false;
            let base_rev = self
                .edit_sync
                .as_ref()
                .map_or(self.rev, |sync| sync.acknowledged().1);
            self.format_pending_text = Some(draft);
            self.format_sent_selection = Some(self.byte_selection(cx));
            let request_id = self.next_edit_request("format");
            self.format_request_id = Some(request_id.clone());
            self.ade.send(AdeCmd::FormatBuffer {
                request_id,
                buffer_id: self.buffer_id.clone(),
                base_rev,
                range: self.pending_format_range.take(),
            });
            self.format_inflight = true;
            self.lsp_status = Some("Formatting…".into());
        }
        self.send_pending_lsp(cx);
        cx.notify();
    }

    pub(crate) fn request_editor_action(&mut self, action: EditorAction, cx: &mut Context<Self>) -> bool {
        if self.page.is_some() {
            self.lsp_status = Some("Fresh editing commands across pages are not available; edit this page or restore your draft".into());
            cx.notify();
            return true;
        }
        if !self.range_edits {
            return false;
        }
        if self.conflict || !self.transport_connected {
            return true;
        }
        if self.sync_paused {
            return true;
        }
        self.pending_actions.push_back(action);
        self.flush_pending(cx);
        true
    }

    /// Compact file-scoped information for the workspace status bar.
    pub fn status_summary(&self) -> String {
        let mut parts = vec![format!("{} problems", self.diagnostics.len())];
        if let Some(warning) = self.recovery_warning.as_ref() {
            parts.push(warning.clone());
        }
        if let Some(status) = self.lsp_status.as_deref() {
            parts.push(status.to_string());
        }
        parts.join(" · ")
    }

    /// Drop the panel without sending `editor_close`. The Fresh buffer stays
    /// in the daemon worker so another workspace can reopen the same path.
    pub fn release(&mut self) {
        self.cancel_lsp_requests();
        self.closed = true;
    }

    pub fn is_dirty(&self) -> bool {
        // The legacy opening snapshot omits Fresh's modified state. Treat a
        // pending authoritative sync as potentially dirty until it replies.
        self.dirty || self.search_has_accepted() || self.sync_request_id.is_some()
    }

    pub fn is_unsaved(&self) -> bool {
        self.unsaved
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
                this.schedule_edit_flush(cx);
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

    pub fn apply_project_update(
        &mut self,
        update: &fresh_gui_protocol::ProjectBufferUpdate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let unchanged = self.edit_request_id.is_none()
            && self.sync_request_id.is_none()
            && self.edit_sync.as_ref().is_some_and(|sync| {
                sync.acknowledged().1 == update.base_rev
                    && sync.acknowledged().0 == self.current_text(cx)
            });
        if unchanged {
            // The user approved this transaction over the acknowledged draft.
            // The ordinary snapshot protection still applies to any newer draft.
            self.dirty = false;
        }
        self.apply_snapshot(
            update.rev,
            update.text.clone(),
            update.path.clone(),
            window,
            cx,
        );
        self.dirty |= update.dirty;
    }

    /// Project results carry global UTF-8 byte ranges, independent of displayed
    /// character columns. Defer revealing until the opening snapshot is ready.
    pub fn reveal_project_match(&mut self, start: usize, end: usize, cx: &mut Context<Self>) {
        self.pending = None;
        self.project_reveal = Some(start..end);
        self.apply_project_reveal(cx);
    }

    fn apply_project_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(range) = self.project_reveal.clone() else {
            return;
        };
        let text = self.current_text(cx);
        if self.edit_sync.is_none() || range.end > text.len() {
            return;
        }
        if text.is_char_boundary(range.start) && text.is_char_boundary(range.end) {
            self.editor
                .update(cx, |state, cx| state.set_selected_range(range, cx));
        }
        self.project_reveal = None;
    }

    pub fn apply_snapshot(
        &mut self,
        rev: u64,
        text: String,
        path: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if rev < self.rev { return; }
        if !path.is_empty() {
            self.path = path;
        }
        self.diagnostics.clear();
        let draft = self.current_text(cx);
        if let Some(sync) = self.edit_sync.as_mut() {
            match sync.reconcile_protected_snapshot(rev, text.clone(), &draft, self.dirty) {
                SnapshotReconciliation::Adopted => {
                    self.rev = rev;
                    if draft != text || self.pending.is_some() {
                        let selection = map_selection_through_edits(&draft, &text, self.byte_selection(cx));
                        self.set_editor_text_and_selection(&text, Some(selection), window, cx);
                        self.dirty = false;
                    }
                    self.conflict = false;
                }
                SnapshotReconciliation::KeepDraft => {
                    self.rev = rev;
                    self.conflict = false;
                    self.schedule_edit_flush(cx);
                }
                SnapshotReconciliation::Conflict => {
                    self.rev = rev;
                    self.conflict = true;
                    self.lsp_status = Some("Server and local edits conflict; local draft kept".into());
                }
            }
        } else {
            let (sync, outcome) =
                EditSync::from_initial_snapshot(text.clone(), rev, &draft, self.dirty);
            self.edit_sync = Some(sync);
            self.rev = rev;
            self.conflict = outcome == SnapshotReconciliation::Conflict;
            if self.conflict {
                self.lsp_status =
                    Some("Server text arrived after local editing; local draft kept".into());
            } else {
                self.dirty = false;
                self.set_editor_text_and_selection(&text, None, window, cx);
            }
        }
        // Fetch Fresh's modified state as well as the text supplied by the
        // legacy snapshot. Keep the opening reveal position when text matches.
        if self.range_edits && self.sync_request_id.is_none() {
            let request_id = self.next_edit_request("sync");
            self.sync_request_id = Some(request_id.clone());
            self.ade.send(AdeCmd::SyncBuffer {
                request_id,
                buffer_id: self.buffer_id.clone(),
                view_id: self.view_id.clone(),
            });
        }
        self.flush_pending(cx);
        cx.notify();
    }

    fn set_editor_text_and_selection(
        &mut self,
        text: &str,
        selection: Option<ByteSelection>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.current_text(cx) != text;
        self.inline_markdown_edit = None;
        self.inline_markdown_subscription = None;
        self.editor.update(cx, |state, cx| {
            let scroll = state.scroll_offset();
            if state.value().as_str() != text { state.set_value(text, window, cx); }
            if let Some(selection) = selection {
                state.set_selected_range(selection.anchor..selection.head, cx);
                state.set_scroll_offset(scroll, cx);
            }
        });
        if changed { self.refresh_search(cx); }
        if let Some(jump) = self.pending.take()
            && let Some(line) = jump.line
        {
            let pos = Position::new(
                line.saturating_sub(1),
                jump.column.unwrap_or(1).saturating_sub(1),
            );
            self.editor
                .update(cx, |state, cx| state.set_cursor_position(pos, window, cx));
        }
        self.apply_navigation_offset(window, cx);
        self.apply_project_reveal(cx);
    }

    pub fn apply_edit_result(
        &mut self,
        request_id: &str,
        view_id: &str,
        rev: u64,
        text: String,
        selection: ByteSelection,
        accepted: bool,
        server_dirty: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view_id != view_id {
            return;
        }
        if self.sync_request_id.as_deref() == Some(request_id) {
            self.sync_request_id = None;
            let draft = self.current_text(cx);
            let outcome = if let Some(sync) = self.edit_sync.as_mut() {
                sync.reconcile_protected_snapshot(rev, text.clone(), &draft, self.dirty)
            } else {
                let (sync, outcome) =
                    EditSync::from_initial_snapshot(text.clone(), rev, &draft, self.dirty);
                self.edit_sync = Some(sync);
                outcome
            };
            self.rev = rev;
            match outcome {
                SnapshotReconciliation::Adopted => {
                    if draft != text || self.pending.is_some() {
                        let local_selection =
                            map_selection_through_edits(&draft, &text, self.byte_selection(cx));
                        self.set_editor_text_and_selection(&text, Some(local_selection), window, cx);
                        self.dirty = server_dirty;
                    }
                    self.conflict = false;
                }
                SnapshotReconciliation::KeepDraft => {
                    self.conflict = false;
                    self.schedule_edit_flush(cx);
                }
                SnapshotReconciliation::Conflict => {
                    self.conflict = true;
                    self.lsp_status = Some("Server and local edits conflict; local draft kept".into());
                }
            }
            self.dirty = server_dirty || self.external.is_some() || self.current_text(cx) != text || self.conflict;
            self.flush_pending(cx);
            cx.notify();
            return;
        }
        if self.edit_request_id.as_deref() != Some(request_id) {
            return;
        }
        self.edit_request_id = None;
        self.rev = rev;
        if !accepted {
            if let Some(sync) = self.edit_sync.as_mut() {
                sync.finish(false, rev, text);
            }
            self.conflict = true;
            self.pending_save = None;
            self.pending_format = false;
            self.pending_actions.clear();
            self.action_inflight = false;
            self.action_sent_text = None;
            self.action_sent_selection = None;
            self.edit_sent_selection = None;
            self.lsp_status =
                Some("Server revision changed; local draft kept for conflict resolution".into());
            cx.notify();
            return;
        }

        let action_result = self.action_inflight;
        self.action_inflight = false;
        let draft = self.current_text(cx);
        let action_draft_matches = self.action_sent_text.take().as_deref() == Some(draft.as_str());
        let action_selection_unchanged = self
            .action_sent_selection
            .take()
            .is_some_and(|sent| self.byte_selection(cx) == sent);
        // The GUI already holds the selection produced by a native edit.
        // Restoring the scalar protocol selection here would remove every
        // secondary caret, including rectangular selections, on each ack.
        self.edit_sent_selection = None;
        self.action_sent_selection = None;
        if let Some(sync) = self.edit_sync.as_mut() {
            if action_result {
                if action_draft_matches {
                    sync.reconcile_snapshot(rev, text.clone(), &draft);
                } else {
                    sync.finish(false, rev, text.clone());
                }
            } else {
                sync.finish(true, rev, text.clone());
            }
            self.conflict = sync.conflict().is_some();
        }
        self.rev = rev;
        if action_result && action_draft_matches {
            if draft != text {
                self.dirty = true;
            }
            let selection = if action_selection_unchanged {
                selection
            } else {
                self.byte_selection(cx)
            };
            self.set_editor_text_and_selection(&text, Some(selection), window, cx);
        }

        self.dirty = server_dirty || self.external.is_some() || self.current_text(cx) != text || self.conflict;
        if self.conflict {
            self.lsp_status = Some("Server and local edits conflict; local draft kept".into());
            self.pending_save = None;
            self.pending_format = false;
        } else {
            self.flush_pending(cx);
        }
        cx.notify();
    }

    pub fn acknowledge_legacy(&mut self, request_id: &str, rev: u64, cx: &mut Context<Self>) {
        if self.edit_request_id.as_deref() != Some(request_id) || self.range_edits {
            return;
        }
        self.edit_request_id = None;
        self.rev = rev;
        let text = self
            .legacy_sent_text
            .take()
            .unwrap_or_else(|| self.current_text(cx));
        if let Some(sync) = self.edit_sync.as_mut() {
            sync.finish(true, rev, text);
        }
        self.flush_pending(cx);
    }

    pub fn detach_transport(&mut self) {
        self.cancel_lsp_requests();
        self.signature_help = None;
        self.signature_active = false;
        self.signature_snapshot = None;
        self.external_pending = None;
        self.external_request = None;
        self.external_save_path = None;
        self.transport_connected = false;
        self.edit_request_id = None;
        self.legacy_sent_text = None;
        self.sync_request_id = None;
        self.pending_actions.clear();
        self.action_inflight = false;
        self.pending_save = None;
        self.pending_format = false;
        self.save_request_id = None;
        self.save_sent_text = None;
        self.format_inflight = false;
    }

    pub fn keep_detached_draft(&mut self, cx: &mut Context<Self>) {
        self.cancel_lsp_requests();
        self.external_pending = None;
        self.external_request = None;
        self.external_save_path = None;
        self.transport_connected = false;
        self.sync_paused = true;
        self.closed = true;
        self.edit_request_id = None;
        self.sync_request_id = None;
        self.pending_actions.clear();
        self.pending_save = None;
        self.pending_format = false;
        self.save_request_id = None;
        self.save_sent_text = None;
        self.lsp_status = Some("Untitled draft kept locally; awaiting daemon reattachment".into());
        cx.notify();
    }

    pub fn reconnect(&mut self, ade: AdeHandle, range_edits: bool, paged_reads: bool, cx: &mut Context<Self>) {
        self.cancel_lsp_requests();
        self.external_request = None;
        self.external_pending = None;
        self.external_save_path = None;
        self.ade = ade;
        self.transport_connected = true;
        self.sync_paused = false;
        self.closed = false;
        self.range_edits = range_edits;
        self.edit_request_id = None;
        self.sync_request_id = None;
        self.legacy_sent_text = None;
        self.save_request_id = None;
        self.save_sent_text = None;
        self.format_inflight = false;
        self.page_request = None;
        self.pending_page = None;
        if self.page.is_some() {
            if paged_reads {
                self.request_page_sync(cx);
            } else {
                self.sync_paused = true;
                self.lsp_status = Some("This daemon does not support paged reads; local page draft retained".into());
            }
        } else if range_edits {
            let request_id = self.next_edit_request("sync");
            self.sync_request_id = Some(request_id.clone());
            self.ade.send(AdeCmd::SyncBuffer {
                request_id,
                buffer_id: self.buffer_id.clone(),
                view_id: self.view_id.clone(),
            });
        }
        cx.notify();
    }

    pub fn request_save(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        self.finish_search_review(window, cx);
        self.commit_markdown_inline_edit(window, cx);
        if self.external.is_some() {
            self.external_save_path = Some(path);
            self.lsp_status = Some("Disk differs from this draft. Review, then choose Overwrite disk to save.".into());
            cx.notify();
            return;
        }
        if self.conflict {
            self.lsp_status = Some("Resolve the edit conflict before saving".into());
            cx.notify();
            return;
        }
        self.pending_save = Some(path);
        self.flush_pending(cx);
    }

    pub fn resolve_keep_local(&mut self, cx: &mut Context<Self>) {
        let Some(conflict) = self.edit_sync.as_mut().and_then(EditSync::resolve_conflict) else {
            return;
        };
        self.rev = conflict.rev;
        self.conflict = false;
        self.sync_paused = false;
        self.lsp_status = Some("Keeping local edits; synchronizing against server version".into());
        self.flush_pending(cx);
        cx.notify();
    }

    pub fn resolve_use_server(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selection = self.byte_selection(cx);
        let Some(conflict) = self.edit_sync.as_mut().and_then(EditSync::resolve_conflict) else {
            return;
        };
        self.rev = conflict.rev;
        self.conflict = false;
        self.sync_paused = false;
        self.dirty = true;
        self.inline_markdown_edit = None;
        self.inline_markdown_subscription = None;
        self.set_editor_text_and_selection(&conflict.text, Some(selection), window, cx);
        self.lsp_status = Some("Using server version; save to write changes".into());
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
            let request_pending = self.edit_request_id.is_some()
                || self.sync_request_id.is_some()
                || self.save_request_id.is_some()
                || self.format_inflight || self.external_request.is_some();
            if !request_pending && (rev > self.rev || self.edit_sync.is_none()) {
                let draft = self.current_text(cx);
                if let Some(sync) = self.edit_sync.as_mut() {
                    match sync.reconcile_protected_snapshot(rev, text.clone(), &draft, self.dirty) {
                        SnapshotReconciliation::Adopted => {
                            if draft != text {
                                let selection = map_selection_through_edits(&draft, &text, self.byte_selection(cx));
                        self.set_editor_text_and_selection(&text, Some(selection), window, cx);
                                self.dirty = true;
                            }
                            self.rev = rev;
                            self.conflict = false;
                        }
                        SnapshotReconciliation::KeepDraft => {
                            self.rev = rev;
                            self.schedule_edit_flush(cx);
                        }
                        SnapshotReconciliation::Conflict => {
                            self.rev = rev;
                            self.conflict = true;
                            self.lsp_status =
                                Some("Server and local edits conflict; local draft kept".into());
                        }
                    }
                } else {
                    let (sync, outcome) =
                        EditSync::from_initial_snapshot(text.clone(), rev, &draft, self.dirty);
                    self.edit_sync = Some(sync);
                    self.rev = rev;
                    self.conflict = outcome == SnapshotReconciliation::Conflict;
                    if self.conflict {
                        self.lsp_status =
                            Some("Server text arrived after local editing; local draft kept".into());
                    } else if draft != text {
                        self.set_editor_text_and_selection(&text, None, window, cx);
                        self.dirty = true;
                    }
                }
                self.ade.send(AdeCmd::AcknowledgeBufferState {
                    buffer_id: self.buffer_id.clone(),
                    rev,
                });
            }
        }
        // Never paint offsets against a local draft newer than the daemon snapshot.
        let draft = self.current_text(cx);
        let current = self.edit_sync.as_ref().is_some_and(|sync| { let (text, acknowledged_rev) = sync.acknowledged(); acknowledged_rev == rev && text == draft });
        self.diagnostics = if current && self.page.is_none() { diagnostics } else { Vec::new() };
        let mut decorations = self.diagnostics.iter().map(|d| {
            use gpui_kit::base::input::{Diagnostic, DiagnosticSeverity};
            let start = Position::new(d.start_line, utf16_to_scalar_column(&draft, d.start_line, d.start_character));
            let end = Position::new(d.end_line, utf16_to_scalar_column(&draft, d.end_line, d.end_character));
            let severity = match d.severity.as_str() { "error" => DiagnosticSeverity::Error, "warning" => DiagnosticSeverity::Warning, "hint" => DiagnosticSeverity::Hint, _ => DiagnosticSeverity::Info };
            Diagnostic::new(start..end, d.message.clone()).with_severity(severity).with_source(d.source.clone().unwrap_or_else(|| "LSP".into()))
        }).collect::<Vec<_>>();
        decorations.sort_by_key(|d| d.range.start);
        self.editor.update(cx, |editor, cx| {
            let text = editor.text().clone();
            if let Some(set) = editor.diagnostics_mut() { set.reset(&text); set.extend(decorations); }
            cx.notify();
        });
        if self.lsp_requests && self.page.is_none() && self.edit_sync.is_some()
            && !self.pending_lsp.iter().any(|pending| pending.request.feature == LspRequestFeature::Capabilities) {
            let text = self.current_text(cx);
            let receiver = self.queue_lsp(LspRequestFeature::Capabilities, 0, None, text, cx);
            // Keep the receiver alive until metadata is installed by apply_lsp_result.
            cx.spawn(async move |_, _| { let _ = receiver.recv().await; }).detach();
        }
        if status.is_some() {
            if !self.conflict {
                self.lsp_status = status;
            }
        } else if self
            .lsp_status
            .as_deref()
            .is_some_and(|s| s.starts_with("LSP"))
        {
            self.lsp_status = None;
        }
        cx.notify();
    }

    pub fn apply_formatted(
        &mut self,
        request_id: &str,
        rev: u64,
        text: Option<String>,
        status: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.format_request_id.as_deref() != Some(request_id) {
            return;
        }
        self.format_request_id = None;
        self.format_inflight = false;
        let expected = self.format_pending_text.take();
        let safe = expected
            .as_deref()
            .is_some_and(|expected| expected == self.current_text(cx));
        let prior_selection = self.format_sent_selection.take();
        if let Some(text) = text {
            if safe {
                let selection = expected
                    .as_deref()
                    .zip(prior_selection)
                    .map(|(old, selection)| map_selection_through_edits(old, &text, selection));
                if let Some(sync) = self.edit_sync.as_mut() {
                    sync.finish(true, rev, text.clone());
                } else {
                    self.edit_sync = Some(EditSync::new(text.clone(), rev));
                }
                self.set_editor_text_and_selection(&text, selection, window, cx);
                self.rev = rev;
                self.dirty = true;
                self.conflict = false;
                self.lsp_status = Some("Formatted; save to write changes".into());
            } else {
                let draft = self.current_text(cx);
                let result = self
                    .edit_sync
                    .as_mut()
                    .map(|sync| sync.reconcile_snapshot(rev, text.clone(), &draft));
                self.rev = rev;
                if result == Some(SnapshotReconciliation::Conflict) {
                    self.conflict = true;
                    self.lsp_status =
                        Some("Formatting conflicted with newer edits; local draft kept".into());
                } else {
                    self.conflict = false;
                    self.lsp_status = Some(
                        "Formatting finished after further local edits; those edits were kept".into(),
                    );
                    self.schedule_edit_flush(cx);
                }
            }
        } else {
            if let Some(sync) = self.edit_sync.as_mut() {
                let (acknowledged, _) = sync.acknowledged();
                sync.finish(true, rev, acknowledged.to_owned());
            }
            self.rev = rev;
            self.lsp_status = status.or_else(|| Some("No formatting changes".into()));
        }
        if !self.conflict {
            self.flush_pending(cx);
        }
        cx.notify();
    }

    pub fn handle_request_error(
        &mut self,
        request_id: &str,
        message: &str,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.page_request.as_deref() == Some(request_id) {
            self.page_request = None;
            self.lsp_status = Some(format!("Page read failed: {message}"));
            cx.notify();
            return true;
        }
        if self.external_request.as_ref().is_some_and(|(id, _, _)| id == request_id) {
            self.external_request = None;
            self.external_save_path = None;
            self.lsp_status = Some(format!("Reconciliation failed: {message}"));
            self.check_external();
            cx.notify();
            return true;
        }
        if self.edit_request_id.as_deref() == Some(request_id) {
            self.edit_request_id = None;
            self.legacy_sent_text = None;
            self.action_inflight = false;
            self.action_sent_text = None;
            self.action_sent_selection = None;
            self.edit_sent_selection = None;
            let draft = self.current_text(cx);
            if let Some(sync) = self.edit_sync.as_mut() {
                let (base, rev) = sync.acknowledged();
                let base = base.to_owned();
                sync.reconcile_snapshot(rev, base, &draft);
            }
            self.lsp_status = Some(format!("Edit was not applied: {message}"));
            self.sync_paused = true;
            self.pending_save = None;
            self.pending_format = false;
            self.pending_actions.clear();
            cx.notify();
            return true;
        }
        if self.save_request_id.as_deref() == Some(request_id) {
            self.save_request_id = None;
            self.save_sent_text = None;
            self.lsp_status = Some(format!("Save failed: {message}"));
            self.flush_pending(cx);
            return true;
        }
        if self.format_request_id.as_deref() == Some(request_id) {
            self.format_request_id = None;
            self.format_pending_text = None;
            self.format_sent_selection = None;
            self.format_inflight = false;
            self.lsp_status = Some(format!("Format failed: {message}"));
            self.flush_pending(cx);
            return true;
        }
        if self.sync_request_id.as_deref() == Some(request_id) {
            self.sync_request_id = None;
            self.sync_paused = true;
            self.lsp_status = Some(format!("Editor resync failed: {message}"));
            cx.notify();
            return true;
        }
        false
    }

    pub(crate) fn request_format(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page.is_some() {
            self.lsp_status = Some("Formatting is not available for paged files".into());
            cx.notify();
            return;
        }
        self.commit_markdown_inline_edit(window, cx);
        if self.conflict {
            self.lsp_status = Some("Resolve the edit conflict before formatting".into());
            cx.notify();
            return;
        }
        self.pending_format_range = None;
        self.pending_format = true;
        self.flush_pending(cx);
        cx.notify();
    }

    pub(crate) fn request_format_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.page.is_some() || self.conflict { self.lsp_status = Some("Selection formatting requires a synchronized full buffer".into()); cx.notify(); return; }
        self.commit_markdown_inline_edit(window, cx);
        let selection = self.byte_selection(cx);
        if selection.anchor == selection.head { self.lsp_status = Some("Select text to format".into()); cx.notify(); return; }
        self.pending_format_range = Some(fresh_gui_protocol::ByteRange { start: selection.anchor.min(selection.head), len: selection.anchor.abs_diff(selection.head) });
        self.pending_format = true;
        self.flush_pending(cx);
        cx.notify();
    }

    pub(crate) fn problems(&self) -> &[BufferDiagnostic] { &self.diagnostics }

    pub(crate) fn reveal_problem(&mut self, line: u32, character: u32, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.current_text(cx);
        let col = utf16_to_scalar_column(&text, line, character);
        self.editor.update(cx, |state, cx| { state.set_cursor_position(Position::new(line, col), window, cx); state.focus(window, cx); });
    }

    /// Returns the previous path when the saved path differs, so the workspace
    /// map can be re-keyed.
    pub fn mark_saved(&mut self, path: String, rev: u64, cx: &mut Context<Self>) -> Option<String> {
        let previous = (self.path != path).then(|| self.path.clone());
        self.path = path;
        self.external = None;
        self.external_save_path = None;
        self.unsaved = false;
        self.unsaved_title = None;
        self.recovery_warning = None;
        self.rev = rev;
        self.save_request_id = None;
        let saved_text = self.save_sent_text.take();
        if let (Some(sync), Some(saved)) = (self.edit_sync.as_mut(), saved_text.as_ref()) {
            sync.finish(true, rev, saved.clone());
        }
        self.dirty = saved_text
            .as_deref()
            .is_some_and(|saved| saved != self.current_text(cx));
        cx.notify();
        if self.dirty {
            self.flush_pending(cx);
        }
        previous
    }

    pub fn finish_save(
        &mut self,
        request_id: &str,
        path: String,
        rev: u64,
        outcome: fresh_gui_protocol::SaveOutcome,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<String> {
        if self.save_request_id.as_deref() != Some(request_id) {
            return None;
        }
        if let Some(text) = outcome.text {
            let draft = self.current_text(cx);
            let sent = self.save_sent_text.as_deref();
            if sent == Some(draft.as_str()) {
                let selection = map_selection_through_edits(&draft, &text, self.byte_selection(cx));
                self.set_editor_text_and_selection(&text, Some(selection), window, cx);
                self.save_sent_text = Some(text);
            } else if sent.is_some_and(|sent| sent != text) {
                // Protect a draft typed while a formatter ran. The normal snapshot
                // reconciliation records conflict rather than overwriting it.
                let outcome = self.edit_sync.as_mut().map(|sync| sync.reconcile_snapshot(rev, text.clone(), &draft));
                self.save_sent_text = Some(text);
                if outcome == Some(SnapshotReconciliation::Conflict) {
                    self.conflict = true;
                    self.lsp_status = Some("Save formatting conflicted with newer edits; local draft kept".into());
                }
            }
        }
        let previous = self.mark_saved(path, rev, cx);
        self.dirty |= outcome.dirty;
        if let Some(status) = outcome.status { self.lsp_status = Some(status); }
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
    // Native dock close actions are captured by Workspace. Refuse any direct
    // dock removal that bypasses its asynchronous retention/prompt guard.
    fn closable(&self, cx: &App) -> bool {
        !self.is_dirty() || self.recovery_guaranteed(cx)
    }

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
        self.cancel_lsp_requests();
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
        with_close_items(menu, self.workspace.clone(), panel_id, true, cx)
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
        let markdown = self.page.is_none() && matches!(
            self.path.rsplit('.').next().map(str::to_ascii_lowercase).as_deref(),
            Some("md" | "markdown")
        );
        let mut root = div().key_context("Editor").size_full().flex().flex_col()
            .capture_action::<gpui_kit::component::input::Search>(cx.listener(|this, _, window, cx| {
                this.open_search(false, window, cx); cx.stop_propagation();
            }))
            .capture_action::<gpui_kit::component::input::Replace>(cx.listener(|this, _, window, cx| {
                this.open_search(true, window, cx); cx.stop_propagation();
            }))
            .on_action(cx.listener(Self::on_query_replace))
            .on_action(cx.listener(Self::on_clear_search))
            .on_action(cx.listener(Self::on_next_match))
            .on_action(cx.listener(Self::on_previous_match))
            .capture_key_down(cx.listener(Self::search_key_down))
                    .capture_action::<gpui_kit::component::input::Undo>(cx.listener(|this, _, window, cx| {
                        if this.search_input_focused(window, cx) { return; }
                        if this.search.review.is_some() { cx.stop_propagation(); return; }
                        if this.request_editor_action(EditorAction::Undo, cx) {
                            cx.stop_propagation();
                        }
                    }))
                    .capture_action::<gpui_kit::component::input::Redo>(cx.listener(|this, _, window, cx| {
                        if this.search_input_focused(window, cx) { return; }
                        if this.search.review.is_some() { cx.stop_propagation(); return; }
                        if this.request_editor_action(EditorAction::Redo, cx) {
                            cx.stop_propagation();
                        }
                    }))
            ;
        root = root.capture_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
            let key = event.keystroke.key.as_str();
            let menu_open = this.editor.read(cx).completion_menu_state().open;
            if key == "escape" {
                this.cancel_lsp_requests();
                this.signature_help = None;
                this.signature_active = false;
                this.signature_snapshot = None;
                this.editor.update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
                cx.notify();
            } else if !menu_open || !matches!(key, "up" | "down" | "enter" | "shift") {
                // A positional edit prepared for the previous caret must never be accepted there.
                this.completion_plans.clear();
                this.lsp_request_tracker.cancel(LspRequestFeature::Completion);
                this.editor.update(cx, |editor, cx| editor.dismiss_completion_overlay(cx));
                if matches!(key, "left" | "right" | "home" | "end" | "pageup" | "pagedown") {
                    this.cancel_lsp_requests();
                    this.signature_help = None;
                    this.signature_active = false;
                    this.signature_snapshot = None;
                }
            }
        }));
        if let Some(help) = &self.signature_help {
            root = root.child(h_flex().w_full().px_2().py_1().gap_2().items_center()
                .border_b_1().border_color(cx.theme().border)
                .child(div().flex_1().text_sm().font_family(cx.theme().mono_font_family.clone()).child(help.clone()))
                .child(Button::new("dismiss-signature").ghost().xsmall().label("Dismiss (Esc)")
                    .on_click(cx.listener(|this, _, _, cx| { this.signature_help = None; this.signature_active = false; this.signature_snapshot = None; cx.notify(); }))));
        }
        if self.search.open { root = root.child(self.render_search(cx)); }
        if let Some(page) = &self.page {
            let start = page.start;
            let end = start + self.editor.read(cx).value().len();
            let total = page.total_bytes;
            let busy = self.search.review.is_some() || self.page_request.is_some() || self.sync_request_id.is_some() || !self.transport_connected;
            let previous = start.saturating_sub(PAGE_VIEW_BYTES);
            root = root.child(h_flex().w_full().min_h_9().px_2().gap_2().items_center()
                .border_b_1().border_color(cx.theme().border)
                .child(Button::new("page-previous").ghost().xsmall().label("Previous page").disabled(busy || start == 0)
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate_page(previous, cx))))
                .child(Button::new("page-next").ghost().xsmall().label("Next page").disabled(busy || end >= total)
                    .on_click(cx.listener(move |this, _, _, cx| this.navigate_page(end, cx))))
                .child(div().text_xs().child(format!("Bytes {start}–{end} of {total}")))
                .child(div().w(px(120.)).child(Input::new(&self.byte_offset_input).small()))
                .child(Button::new("page-go").ghost().xsmall().label("Go to byte").disabled(busy)
                    .on_click(cx.listener(move |this, _, _, cx| {
                        match this.byte_offset_input.read(cx).value().parse::<usize>() {
                            Ok(offset) if offset <= total => this.navigate_page(offset, cx),
                            _ => { this.lsp_status = Some(format!("Enter a byte offset from 0 to {total}")); cx.notify(); }
                        }
                    })))
                .when(self.sync_paused && !self.conflict, |bar| bar.child(
                    Button::new("page-retry").ghost().xsmall().label("Retry sync").disabled(busy)
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.sync_paused = false;
                            this.flush_pending(cx);
                            cx.notify();
                        }))))
                .when(busy, |bar| bar.child(div().text_xs().child("Loading…"))));
        }
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
                                    this.cancel_lsp_requests();
                                    this.editor.update(cx, |editor, cx| editor.dismiss_lsp_overlays(cx));
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
                            .context_menu({
                                let editor = self.editor.clone();
                                move |menu, _, cx| {
                                let capabilities = editor.read(cx).context_menu_capabilities();
                                let editable = !capabilities.is_disabled() && !capabilities.is_readonly();
                                use super::actions::*;
                                use gpui_kit::component::input::{Cut, Copy, Paste, SelectAll, Undo, Redo};
                                menu.menu("Go to Definition", Box::new(GoToDefinition))
                                    .menu("Go to Declaration", Box::new(GoToDeclaration))
                                    .menu("Go to Type Definition", Box::new(GoToTypeDefinition))
                                    .menu("Go to Implementation", Box::new(GoToImplementation))
                                    .menu("Find References", Box::new(FindReferences))
                                    .menu("Document Symbols", Box::new(DocumentSymbols))
                                    .menu("Workspace Symbols", Box::new(WorkspaceSymbols))
                                    .separator()
                                    .menu("Navigate Back", Box::new(NavigateBack))
                                    .menu("Navigate Forward", Box::new(NavigateForward))
                                    .separator()
                                    .menu_with_disabled("Cut", !(editable && capabilities.is_copyable()), Box::new(Cut))
                                    .menu_with_disabled("Copy", !capabilities.is_copyable(), Box::new(Copy))
                                    .menu_with_disabled("Paste", !(editable && cx.read_from_clipboard().is_some()), Box::new(Paste))
                                    .menu("Select All", Box::new(SelectAll)).separator()
                                    .menu_with_disabled("Undo", !editable, Box::new(Undo))
                                    .menu_with_disabled("Redo", !editable, Box::new(Redo))
                                }
                            })
                            .disabled(self.search.review.is_some() || self.page_request.is_some() || (self.page.is_some() && self.sync_request_id.is_some()))
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
        if let Some(notice) = &self.external {
            let panel = cx.entity();
            let deleted = notice.disk_text.is_none();
            let label = if deleted { "Deleted on disk; draft retained" }
                else if notice.kept { "Draft differs from disk" }
                else { "File changed on disk; draft retained" };
            root = root.child(
                h_flex().w_full().min_h_9().px_2().gap_2().items_center().border_t_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().text_xs().text_color(cx.theme().danger).child(label))
                    .child(Button::new("external-compare").ghost().xsmall().label("Compare")
                        .on_click({ let panel = panel.clone(); move |_, window, cx| {
                            panel.update(cx, |this, cx| this.compare_external(window, cx));
                        }}))
                    .when(!deleted, |bar| bar.child(Button::new("external-reload").ghost().xsmall().label("Reload")
                        .on_click({ let panel = panel.clone(); move |_, _, cx| {
                            panel.update(cx, |this, cx| this.request_external_resolution(ExternalResolution::Reload, cx));
                        }})))
                    .child(Button::new("external-keep").ghost().xsmall().label("Keep")
                        .on_click({ let panel = panel.clone(); move |_, _, cx| {
                            panel.update(cx, |this, cx| this.request_external_resolution(ExternalResolution::Keep, cx));
                        }}))
                    .when(self.external_save_path.is_some(), |bar| bar.child(Button::new("external-overwrite").ghost().xsmall().label("Overwrite disk")
                        .on_click(move |_, _, cx| {
                            panel.update(cx, |this, cx| this.request_external_resolution(ExternalResolution::Overwrite, cx));
                        })))
            );
        }
        if self.conflict {
            let panel = cx.entity();
            root = root.child(
                h_flex()
                    .w_full()
                    .h_9()
                    .px_2()
                    .gap_2()
                    .items_center()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .child(div().flex_1().text_xs().text_color(cx.theme().danger)
                        .child("Server and local edits conflict"))
                    .child(Button::new("conflict-keep-local").ghost().xsmall().label("Keep my edits")
                        .on_click({
                            let panel = panel.clone();
                            move |_, _, cx| panel.update(cx, |this, cx| this.resolve_keep_local(cx))
                        }))
        .child(Button::new("conflict-use-server").ghost().xsmall().label("Use server text")
                        .on_click(move |_, window, cx| { let _ = panel.update(cx, |this, cx| this.resolve_use_server(window, cx)); }))
            );
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

fn map_selection_through_edits(old: &str, new: &str, selection: ByteSelection) -> ByteSelection {
    let Some(edit) = contiguous_diff(old, new).into_iter().next() else {
        return selection;
    };
    let map = |offset: usize| {
        if offset <= edit.start {
            offset
        } else if offset >= edit.end {
            offset - (edit.end - edit.start) + edit.text.len()
        } else {
            edit.start + edit.text.len()
        }
    };
    ByteSelection {
        anchor: map(selection.anchor),
        head: map(selection.head),
    }
}

#[cfg(test)]
mod lsp_position_tests {
    use super::{ByteSelection, map_selection_through_edits, utf16_to_scalar_column};

    #[test]
    fn diagnostic_columns_after_non_bmp_characters() {
        assert_eq!(utf16_to_scalar_column("first\na😀b\n", 1, 3), 2);
        assert_eq!(utf16_to_scalar_column("first\na😀b\n", 1, 4), 3);
    }

    #[test]
    fn formatting_maps_forward_and_reverse_byte_selections_across_emoji() {
        let old = "a😀bc";
        let new = "a🙂Xbc";
        assert_eq!(
            map_selection_through_edits(old, new, ByteSelection { anchor: 5, head: 6 }),
            ByteSelection { anchor: 6, head: 7 },
        );
        assert_eq!(
            map_selection_through_edits(old, new, ByteSelection { anchor: 6, head: 1 }),
            ByteSelection { anchor: 7, head: 1 },
        );
    }
}

#[cfg(test)]
mod project_update_tests {
    use super::*;
    use crate::gui::{
        connect::parse_connect_target,
        workspace::Workspace,
    };
    use core::prelude::v1::test;
    use gpui::TestAppContext;

    fn test_workspace(window: &mut Window, cx: &mut Context<Workspace>) -> Workspace {
        Workspace::new_for_test(parse_connect_target("ws://", None), window, cx)
    }

    #[gpui::test]
    fn accepted_project_update_adopts_matching_acknowledged_dirty_draft(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (workspace, test_cx) = cx.add_window_view(test_workspace);
        let (ade, _) = AdeHandle::test_channel();
        let panel = test_cx.update(|window, cx| {
            cx.new(|cx| {
                EditorPanel::new(
                    "buffer".into(),
                    "test.txt".into(),
                    None,
                    None,
                    None,
                    true,
                    None,
                    ade,
                    workspace.downgrade(),
                    TabStripMetrics::default(),
                    window,
                    cx,
                )
            })
        });
        panel.update_in(test_cx, |panel, window, cx| {
            let original = "猫 needle";
            panel.set_editor_text_and_selection(original, None, window, cx);
            panel.edit_sync = Some(EditSync::new(original.into(), 4));
            panel.rev = 4;
            panel.dirty = true;

            panel.apply_project_update(
                &fresh_gui_protocol::ProjectBufferUpdate {
                    buffer_id: "buffer".into(),
                    base_rev: 4,
                    rev: 5,
                    text: "猫 replacement".into(),
                    path: "test.txt".into(),
                    dirty: true,
                },
                window,
                cx,
            );

            assert_eq!(panel.current_text(cx), "猫 replacement");
            assert_eq!(panel.rev, 5);
            assert!(panel.is_dirty());
            assert!(!panel.conflict);
        });
    }

    #[gpui::test]
    fn project_update_keeps_newer_visible_edits_and_marks_conflict(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (workspace, test_cx) = cx.add_window_view(test_workspace);
        let (ade, _) = AdeHandle::test_channel();
        let panel = test_cx.update(|window, cx| {
            cx.new(|cx| {
                EditorPanel::new(
                    "buffer".into(),
                    "test.txt".into(),
                    None,
                    None,
                    None,
                    true,
                    None,
                    ade,
                    workspace.downgrade(),
                    TabStripMetrics::default(),
                    window,
                    cx,
                )
            })
        });
        panel.update_in(test_cx, |panel, window, cx| {
            panel.set_editor_text_and_selection("猫 newer local", None, window, cx);
            panel.edit_sync = Some(EditSync::new("猫 original".into(), 9));
            panel.rev = 9;
            panel.dirty = true;

            panel.apply_project_update(
                &fresh_gui_protocol::ProjectBufferUpdate {
                    buffer_id: "buffer".into(),
                    base_rev: 9,
                    rev: 10,
                    text: "猫 accepted replacement".into(),
                    path: "test.txt".into(),
                    dirty: true,
                },
                window,
                cx,
            );

            assert_eq!(panel.current_text(cx), "猫 newer local");
            assert!(panel.conflict);
            assert!(panel.is_dirty());
        });
    }

    #[gpui::test]
    fn project_match_reveal_uses_utf8_byte_offsets(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (workspace, test_cx) = cx.add_window_view(test_workspace);
        let (ade, _) = AdeHandle::test_channel();
        let panel = test_cx.update(|window, cx| {
            cx.new(|cx| {
                EditorPanel::new(
                    "buffer".into(),
                    "test.txt".into(),
                    None,
                    None,
                    None,
                    true,
                    None,
                    ade,
                    workspace.downgrade(),
                    TabStripMetrics::default(),
                    window,
                    cx,
                )
            })
        });
        panel.update_in(test_cx, |panel, window, cx| {
            let text = "猫 needle";
            let start = text.find("needle").unwrap();
            let end = start + "needle".len();
            panel.set_editor_text_and_selection(text, None, window, cx);
            panel.edit_sync = Some(EditSync::new(text.into(), 1));
            panel.reveal_project_match(start, end, cx);
            assert_eq!(panel.editor.read(cx).selected_range(), start..end);
            assert_eq!(&text[start..end], "needle");
        });
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
    if workspace.read_with(cx, |workspace, cx| workspace.tab_has_dirty_draft(panel_id, cx)).unwrap_or(false) {
        let discard_workspace = workspace.clone();
        menu = menu.item(PopupMenuItem::new("Discard Draft and Close").on_click(move |_, window, cx| {
            discard_workspace.update(cx, |workspace, cx| workspace.discard_panel_id(panel_id, window, cx)).ok();
        }));
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

#[cfg(test)]
#[path = "diagnostic_tests.rs"]
mod diagnostic_tests;

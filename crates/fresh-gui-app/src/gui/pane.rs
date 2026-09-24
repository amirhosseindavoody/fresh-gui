//! Dock panels for terminals and editors.
//!
//! Splits, tab reorder, and merging tabs back into one group are gpui-component's
//! dock (`DockArea` / `TabGroup`). These panels are the surfaces that dock hosts.
//! Tab titles are per workspace. `SessionTabTitle::workspace_id` is the workspace
//! that owns the panel.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Instant;

use alacritty_terminal::vte::ansi::CursorShape;

use gpui_kit::base::ElementExt as _;
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelControl, PanelEvent, PanelId};
use gpui_kit::component::input::{Editor, EditorState, InputEvent, Position};
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
use super::paths::display_path;
use super::rail::path_basename;
use super::tab_chrome::{TabCloseScope, TabStripMetrics};
use super::terminal::{
    TermMouseButton, TermMouseKind, TermMouseMods, TermScreen, TermSpan, keystroke_to_bytes,
    paste_payload, readable_light_foreground,
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
    /// Monospace cell, scaled with content and UI zoom. 8×18 at 14px.
    cell_w: f32,
    cell_h: f32,
    font_px: f32,
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
            ade,
            workspace,
            metrics,
            plus_shift: Rc::new(Cell::new(0.0)),
            pending_grid: Rc::new(Cell::new(None)),
            grid_origin: Rc::new(Cell::new(None)),
            selecting: false,
            pressed: None,
            last_mouse_cell: None,
            cell_w: TERM_CELL_W,
            cell_h: TERM_CELL_H,
            font_px: 14.0,
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
        if down && self.host_selects(button, modifiers) {
            self.pressed = None;
            self.pointer_select(position, true, cx);
            return;
        }
        if !down {
            self.release_mouse(button, position, modifiers, cx);
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
        cx: &mut Context<Self>,
    ) {
        if self.selecting && button == TermMouseButton::Left {
            self.finish_select(cx);
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
        if self.selecting {
            self.pointer_select(position, false, cx);
            return;
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

    fn finish_select(&mut self, cx: &mut Context<Self>) {
        if !self.selecting {
            return;
        }
        self.selecting = false;
        if self.screen.selection_is_empty() {
            self.screen.clear_selection();
        }
        cx.notify();
    }

    fn copy_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(text) = self.screen.selection_text() else {
            return;
        };
        if let Err(error) = clipboard::write_text(window, cx, &text) {
            tracing::warn!(%error, "terminal copy failed");
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
        cx.defer(move |cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.note_terminal_active(&pty, active, cx);
                })
                .ok();
        });
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

    fn title_suffix(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<impl IntoElement> {
        Some(new_terminal_button(
            format!("new-term-{}", self.pty_id),
            self.metrics.clone(),
            self.plus_shift.clone(),
            self.workspace.clone(),
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
        if !self.pending_clipboard.is_empty() {
            for text in std::mem::take(&mut self.pending_clipboard) {
                if let Err(error) = clipboard::write_text(window, cx, &text) {
                    tracing::warn!(%error, "terminal clipboard copy failed");
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
                        window.on_mouse_event(move |event: &MouseUpEvent, phase, _, app| {
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
                                        cx,
                                    );
                                })
                                .ok();
                        });
                    })
                    .children(rows.into_iter().map(|row| {
                        h_flex().h(px(cell_h)).items_center().children(
                            row.spans
                                .into_iter()
                                .map(|span| {
                                    term_span_el(span, cell_w, fg_default, accent, light_theme)
                                }),
                        )
                    }))
                    .when_some(cursor, |grid, (row, col, shape)| {
                        grid.child(terminal_cursor_el(
                            row, col, shape, cell_w, cell_h, fg_default,
                        ))
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

fn is_ui_zoom_in_chord(key: &str, key_char: Option<&str>, modifiers: &Modifiers) -> bool {
    modifiers.control
        && modifiers.shift
        && !modifiers.alt
        && (key == "=" || key == "+" || key_char == Some("+"))
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
) -> gpui::Div {
    let text = if span.text.is_empty() {
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
    let bg = span.bg;
    div()
        .w(px(span.cells as f32 * cell_w))
        .flex_shrink_0()
        .whitespace_nowrap()
        .when(bold, |el| el.font_semibold())
        .when(!span.selected, |el| match fg {
            Some(fg) => el.text_color(term_rgb(fg)),
            None => el.text_color(fg_default),
        })
        .when(!span.selected, |el| match bg {
            Some(bg) => el.bg(term_rgb(bg)),
            None => el,
        })
        .when(span.selected, |el| {
            el.bg(accent.opacity(0.45)).text_color(fg_default)
        })
        .child(text)
}

fn terminal_cursor_el(
    row: usize,
    col: usize,
    shape: CursorShape,
    cell_w: f32,
    cell_h: f32,
    color: Hsla,
) -> gpui::Div {
    let (left, top, width, height) = match shape {
        CursorShape::Beam => (col as f32 * cell_w, row as f32 * cell_h, 2., cell_h),
        CursorShape::Underline => (
            col as f32 * cell_w,
            (row as f32 + 1.) * cell_h - 2.,
            cell_w,
            2.,
        ),
        _ => (col as f32 * cell_w, row as f32 * cell_h, cell_w, cell_h),
    };
    div()
        .absolute()
        .left(px(left))
        .top(px(top))
        .w(px(width))
        .h(px(height))
        .when(shape == CursorShape::Block, |el| el.bg(color.opacity(0.4)))
        .when(shape == CursorShape::HollowBlock || shape == CursorShape::Block, |el| {
            el.border_1().border_color(color)
        })
        .when(shape == CursorShape::Beam || shape == CursorShape::Underline, |el| el.bg(color))
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
    rev: u64,
    pending: Option<EditorPending>,
    editor: Entity<EditorState>,
    ade: AdeHandle,
    workspace: WeakEntity<Workspace>,
    metrics: TabStripMetrics,
    plus_shift: Rc<Cell<f32>>,
    font_px: f32,
    closed: bool,
    markdown_preview: bool,
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
            unsaved,
            unsaved_title,
            dirty: false,
            rev: 0,
            pending: Some(EditorPending { line, column }),
            editor,
            ade,
            workspace,
            metrics,
            plus_shift: Rc::new(Cell::new(0.0)),
            font_px: 14.0,
            closed: false,
            markdown_preview: false,
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

    pub fn is_unsaved(&self) -> bool {
        self.unsaved
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

    fn title_suffix(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<impl IntoElement> {
        Some(new_terminal_button(
            format!("new-term-ed-{}", self.buffer_id),
            self.metrics.clone(),
            self.plus_shift.clone(),
            self.workspace.clone(),
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
                    .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Markdown · click preview text to edit"))
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
                    ),
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
            ));
        } else {
            root = root.child(
                Editor::new(&self.editor)
                    .bordered(false)
                    .p_0()
                    .flex_1()
                    .text_size(px(self.font_px))
                    .font_family(cx.theme().mono_font_family.clone()),
            );
        }
        root
    }
}

/// A lightweight native Markdown presentation for the first WYSIWYG pass.
/// Source remains authoritative and editable in Source mode; Preview reflects
/// live changes and gives block structure without a second document model.
fn render_markdown_preview(
    source: &str,
    block_bg: Hsla,
    muted_fg: Hsla,
    panel: Entity<EditorPanel>,
    inline_edit: Option<(usize, Entity<EditorState>)>,
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
        } else if let Some((level, text)) = markdown_heading(trimmed) {
            let font = match level {
                1 => px(28.),
                2 => px(24.),
                3 => px(20.),
                _ => px(17.),
            };
            div().font_semibold().text_size(font).child(text.to_string())
        } else if let Some(text) = trimmed.strip_prefix("> ") {
            h_flex()
                .gap_2()
                .child(div().w(px(3.)).h_full().bg(block_bg))
                .child(div().italic().text_color(muted_fg).child(text.to_string()))
        } else if let Some(text) = markdown_list_item(trimmed) {
            h_flex().gap_2().pl_4().child("•").child(text.to_string())
        } else if trimmed.starts_with("---") || trimmed.starts_with("***") {
            div().w_full().h(px(1.)).my_2().bg(block_bg)
        } else {
            div().text_size(px(15.)).child(trimmed.to_string())
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
        markdown_edit_parts, markdown_heading, markdown_list_item, replace_markdown_line,
    };

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
                    .tooltip("New Tab or New File")
                    .dropdown_menu(move |menu, _, _| {
                        let new_tab = workspace.clone();
                        let new_file = workspace.clone();
                        menu.item(PopupMenuItem::new("New Tab").on_click(move |_, _, cx| {
                            new_tab
                                .update(cx, |workspace, cx| {
                                    workspace.new_terminal(cx);
                                    cx.notify();
                                })
                                .ok();
                        }))
                        .item(PopupMenuItem::new("New File").on_click(move |_, _, cx| {
                            new_file
                                .update(cx, |workspace, cx| {
                                    workspace.new_file(cx);
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
    if let Some(lang) = reported.filter(|s| !s.is_empty()) {
        return Some(lang.to_string());
    }
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    let name = match ext.as_str() {
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

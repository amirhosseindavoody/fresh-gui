//! Whole-file split diff and the binary-file placeholder.
//!
//! A changed file opens here instead of being decoded into the text editor.
//! One unpinned diff is the preview; a double-click pins it like any other tab.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::base::ElementExt as _;
use gpui_kit::assets::IconName;
use gpui_kit::component::dock::{BasePanel, Panel as DockPanel, PanelEvent, PanelId};
use gpui_kit::component::menu::ContextMenuExt as _;
use gpui_kit::component::input::{Editor, EditorState, InputEvent, TextDecoration, TextDecorationCollection};
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, button::Button, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::pane::{new_terminal_button, note_tab_edge, tab_close_button, with_close_items};
use super::clipboard;
use super::paths::display_path;
use super::rail::path_basename;
use super::tab_chrome::TabStripMetrics;
use super::workspace::Workspace;

const MAX_DIFF_LINES: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Same,
    Added,
    Removed,
    Changed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SplitRow {
    pub left: Option<String>,
    pub right: Option<String>,
    pub kind: RowKind,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Equal(usize, usize),
    Delete(usize),
    Insert(usize),
}

fn diff_ops(old: &[&str], new: &[&str]) -> Vec<Op> {
    let n = old.len();
    let m = new.len();
    let cols = m + 1;
    let mut dp = vec![0u32; (n + 1) * cols];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i * cols + j] = if old[i] == new[j] {
                dp[(i + 1) * cols + (j + 1)] + 1
            } else {
                dp[(i + 1) * cols + j].max(dp[i * cols + (j + 1)])
            };
        }
    }
    let mut ops = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if old[i] == new[j] {
            ops.push(Op::Equal(i, j));
            i += 1;
            j += 1;
        } else if dp[(i + 1) * cols + j] >= dp[i * cols + (j + 1)] {
            ops.push(Op::Delete(i));
            i += 1;
        } else {
            ops.push(Op::Insert(j));
            j += 1;
        }
    }
    while i < n {
        ops.push(Op::Delete(i));
        i += 1;
    }
    while j < m {
        ops.push(Op::Insert(j));
        j += 1;
    }
    ops
}

/// Whole-file split diff: old on the left, working tree on the right.
pub fn split_diff_lines(old: &str, new: &str) -> Vec<SplitRow> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    if old_lines.len() > MAX_DIFF_LINES || new_lines.len() > MAX_DIFF_LINES {
        return vec![SplitRow {
            left: Some(format!(
                "File is too large for a split diff ({} / {} lines).",
                old_lines.len(),
                new_lines.len()
            )),
            right: None,
            kind: RowKind::Same,
        }];
    }
    let ops = diff_ops(&old_lines, &new_lines);
    let mut rows = Vec::new();
    let mut index = 0;
    while index < ops.len() {
        match ops[index] {
            Op::Equal(left, right) => {
                rows.push(SplitRow {
                    left: Some(old_lines[left].to_string()),
                    right: Some(new_lines[right].to_string()),
                    kind: RowKind::Same,
                });
                index += 1;
            }
            Op::Delete(_) | Op::Insert(_) => {
                let start = index;
                while index < ops.len() && !matches!(ops[index], Op::Equal(_, _)) {
                    index += 1;
                }
                let dels: Vec<usize> = ops[start..index]
                    .iter()
                    .filter_map(|op| match op {
                        Op::Delete(i) => Some(*i),
                        _ => None,
                    })
                    .collect();
                let ins: Vec<usize> = ops[start..index]
                    .iter()
                    .filter_map(|op| match op {
                        Op::Insert(i) => Some(*i),
                        _ => None,
                    })
                    .collect();
                let pairs = dels.len().max(ins.len());
                for n in 0..pairs {
                    let left = dels.get(n).map(|i| old_lines[*i].to_string());
                    let right = ins.get(n).map(|i| new_lines[*i].to_string());
                    let kind = match (&left, &right) {
                        (Some(_), Some(_)) => RowKind::Changed,
                        (Some(_), None) => RowKind::Removed,
                        (None, Some(_)) => RowKind::Added,
                        (None, None) => RowKind::Same,
                    };
                    rows.push(SplitRow { left, right, kind });
                }
            }
        }
    }
    rows
}

pub fn click_count(event: &ClickEvent) -> usize {
    match event {
        ClickEvent::Mouse(ev) => ev.up.click_count.max(ev.down.click_count),
        ClickEvent::Touch(ev) => ev.tap_count,
        ClickEvent::Keyboard(_) => 1,
    }
}

/// Absolute explorer path → repo-relative path, when `path` is inside `root`.
#[cfg(test)]
pub fn git_relative(root: &str, path: &str) -> Option<String> {
    let root = display_path(root);
    let path = display_path(path);
    let root = root.trim_end_matches(['/', '\\']).to_string();
    if root.is_empty() {
        return None;
    }
    let (path_cmp, root_cmp) = if cfg!(windows) {
        (path.to_ascii_lowercase(), root.to_ascii_lowercase())
    } else {
        (path.clone(), root.clone())
    };
    if path_cmp == root_cmp {
        return None;
    }
    for sep in ['/', '\\'] {
        let prefix = format!("{root_cmp}{sep}");
        if let Some(rest) = path_cmp.strip_prefix(&prefix) {
            let rel = &path[path.len() - rest.len()..];
            if rel.is_empty()
                || rel
                    .split(['/', '\\'])
                    .any(|seg| seg.is_empty() || seg == "..")
            {
                return None;
            }
            return Some(rel.replace('\\', "/"));
        }
    }
    if path_cmp.starts_with('/') || path_cmp.chars().nth(1) == Some(':') {
        return None;
    }
    let rel = path.replace('\\', "/");
    if rel.split('/').any(|seg| seg.is_empty() || seg == "..") {
        None
    } else {
        Some(rel)
    }
}

pub fn join_repo(root: &str, rel: &str) -> String {
    let root = display_path(root);
    let root = root.trim_end_matches(['/', '\\']);
    let rel = rel.trim_start_matches(['/', '\\']);
    if root.is_empty() {
        return rel.to_string();
    }
    let sep = if root.contains('\\') && !root.contains('/') {
        '\\'
    } else {
        '/'
    };
    format!("{root}{sep}{rel}")
}

pub struct DiffPanel {
    /// Repo-relative path. Also the map key.
    rel: String,
    title_path: String,
    pinned: bool,
    rows: Vec<SplitRow>,
    /// Immutable comparison side used to reclassify the editable side after
    /// every editor change.
    original_text: String,
    editor: Entity<EditorState>,
    right_diff_decorations: TextDecorationCollection,
    left_scroll: ScrollHandle,
    buffer_id: Option<String>,
    rev: u64,
    dirty: bool,
    ready: bool,
    word_wrap: bool,
    binary: bool,
    note: String,
    focus: FocusHandle,
    workspace: WeakEntity<Workspace>,
    metrics: TabStripMetrics,
    plus_shift: Rc<Cell<f32>>,
    closed: bool,
    _editor_subscription: Subscription,
    _editor_scroll_subscription: Subscription,
    last_editor_scroll: Option<(Point<Pixels>, usize)>,
}

impl DiffPanel {
    pub fn new(
        rel: String,
        title_path: String,
        pinned: bool,
        workspace: WeakEntity<Workspace>,
        metrics: TabStripMetrics,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| EditorState::new(window, cx).line_number(true));
        let right_diff_decorations = editor.update(cx, |state, cx| {
            state.create_decorations_collection(Vec::new(), cx)
        });
        let initial_editor_scroll = editor.read(cx).scroll_offset();
        let editor_subscription = cx.subscribe(&editor, |this, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::Change) {
                this.dirty = true;
                if !this.binary && this.ready {
                    let edited = this.editor.read(cx).value().to_string();
                    this.rows = split_diff_lines(&this.original_text, &edited);
                    this.right_diff_decorations.set(
                        build_right_diff_decorations(&edited, &this.rows, cx),
                        cx,
                    );
                }
                cx.notify();
            }
        });
        // EditorState stops wheel propagation when it consumes a scroll, so a
        // parent wheel listener cannot observe right-side scrolling. Observe
        // editor notifications instead; scrolling updates its visible range.
        let editor_scroll_subscription = cx.observe(&editor, |this, editor, cx| {
            let (visible_line, offset, line_height) = {
                let editor = editor.read(cx);
                (
                    editor.visible_row_range().map(|range| range.start).unwrap_or(0),
                    editor.scroll_offset(),
                    editor.line_height(),
                )
            };
            // The laid-out visible range can lag during wheel handling. Without
            // wrapping, the current offset gives the precise first line.
            let line = if !this.word_wrap {
                line_height
                    .map(|height| ((-offset.y / height).floor().max(0.0)) as usize)
                    .unwrap_or(visible_line)
            } else {
                visible_line
            };
            if this.last_editor_scroll == Some((offset, line)) {
                return;
            }
            this.last_editor_scroll = Some((offset, line));
            let horizontal = offset.x;
            if let Some(row) = this
                .rows
                .iter()
                .enumerate()
                .filter_map(|(row, split)| split.right.as_ref().map(|_| row))
                .nth(line)
            {
                this.left_scroll.scroll_to_top_of_item(row);
                if !this.word_wrap {
                    let mut offset = this.left_scroll.offset();
                    offset.x = horizontal;
                    this.left_scroll.set_offset(offset);
                }
                cx.notify();
            }
        });
        Self {
            rel,
            title_path,
            pinned,
            rows: Vec::new(),
            original_text: String::new(),
            editor,
            right_diff_decorations,
            left_scroll: ScrollHandle::new(),
            buffer_id: None,
            rev: 0,
            dirty: false,
            ready: false,
            word_wrap: true,
            binary: false,
            note: "Loading diff…".into(),
            focus: cx.focus_handle(),
            workspace,
            metrics,
            plus_shift: Rc::new(Cell::new(0.0)),
            closed: false,
            _editor_subscription: editor_subscription,
            _editor_scroll_subscription: editor_scroll_subscription,
            last_editor_scroll: Some((initial_editor_scroll, 0)),
        }
    }

    pub fn pin(&mut self, cx: &mut Context<Self>) {
        self.pinned = true;
        cx.notify();
    }

    pub fn release(&mut self) {
        self.closed = true;
    }

    pub fn show_sides(
        &mut self,
        old_text: String,
        new_text: String,
        binary: bool,
        truncated: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.binary = binary;
        if binary {
            self.rows.clear();
            self.note = "Binary file — diff is not shown.".into();
        } else {
            self.original_text = old_text.clone();
            self.rows = split_diff_lines(&old_text, &new_text);
            self.note = if truncated {
                "Diff truncated to the first 256KB of each side.".into()
            } else if self.rows.is_empty() {
                "No differences against HEAD.".into()
            } else {
                String::new()
            };
        }
        cx.notify();
    }

    pub fn set_buffer(&mut self, buffer_id: String) { self.buffer_id = Some(buffer_id); }
    pub fn set_word_wrap(&mut self, enabled: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.word_wrap == enabled { return; }
        self.word_wrap = enabled;
        self.editor.update(cx, |state, cx| state.set_soft_wrap(enabled, window, cx));
        cx.notify();
    }
    pub fn toggle_word_wrap(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let enabled = !self.word_wrap;
        self.set_word_wrap(enabled, window, cx);
        enabled
    }
    pub fn buffer_id(&self) -> Option<&str> { self.buffer_id.as_deref() }
    pub fn apply_snapshot(&mut self, buffer_id: &str, rev: u64, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.buffer_id.as_deref() != Some(buffer_id) { return; }
        self.rev = rev;
        if !self.dirty {
            self.editor.update(cx, |state, cx| state.set_value(text, window, cx));
            self.dirty = false;
        }
        self.ready = true;
        if !self.binary {
            let text = self.editor.read(cx).value().to_string();
            self.rows = split_diff_lines(&self.original_text, &text);
            self.right_diff_decorations
                .set(build_right_diff_decorations(&text, &self.rows, cx), cx);
        }
        cx.notify();
    }
    pub fn save_data(&self, cx: &App) -> Option<(String, u64, String)> {
        if !self.ready || !self.dirty { return None; }
        Some((self.buffer_id.clone()?, self.rev, self.editor.read(cx).value().to_string()))
    }
    pub fn set_rev(&mut self, rev: u64) { self.rev = rev; }
    pub fn mark_saved(&mut self, rev: u64, cx: &mut Context<Self>) { self.rev = rev; self.dirty = false; cx.notify(); }

    fn label(&self) -> String {
        let shown = display_path(&self.title_path);
        let name = path_basename(&shown).unwrap_or(self.rel.as_str());
        if self.pinned {
            name.to_string()
        } else {
            format!("{name} (preview)")
        }
    }
}

impl EventEmitter<PanelEvent> for DiffPanel {}

impl Focusable for DiffPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl BasePanel for DiffPanel {
    fn panel_name(&self) -> &'static str {
        "Diff"
    }

    fn zoomable(&self, _: &App) -> bool {
        false
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rel = self.rel.clone();
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.note_diff_active(&rel, active, cx);
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
        let rel = self.rel.clone();
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            workspace
                .update(cx, |workspace, cx| workspace.forget_diff(&rel, cx))
                .ok();
        });
    }
}

impl DockPanel for DiffPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rel = self.rel.clone();
        let workspace = self.workspace.clone();
        let pin_workspace = workspace.clone();
        let panel_id = PanelId::from(cx.entity().entity_id());
        let metrics = self.metrics.clone();
        h_flex()
            .id(format!("diff-title-{}", self.rel))
            .gap_1()
            .items_center()
            .min_w_0()
            .on_prepaint(move |bounds, _, _| note_tab_edge(&metrics, bounds))
            .on_click(move |event, _, cx| {
                if click_count(event) >= 2 {
                    pin_workspace
                        .update(cx, |workspace, cx| workspace.pin_diff(&rel, cx))
                        .ok();
                }
            })
            .child(Icon::new(IconName::FileDiff).small())
            .child(div().text_ellipsis().child(self.label()))
            .child(tab_close_button(
                format!("close-diff-{}", self.rel),
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
            format!("new-term-diff-{}", self.rel),
            self.metrics.clone(),
            self.plus_shift.clone(),
            self.workspace.clone(),
            panel_id,
        ))
    }

    fn dropdown_menu(
        &mut self,
        menu: gpui_kit::component::menu::PopupMenu,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::component::menu::PopupMenu {
        let panel_id = PanelId::from(cx.entity().entity_id());
        with_close_items(menu, self.workspace.clone(), panel_id, false, cx)
    }

    fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for DiffPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = self.rows.clone();
        let note = self.note.clone();
        let left_scroll = self.left_scroll.clone();
        let editor_for_left_scroll = self.editor.clone();
        let word_wrap = self.word_wrap;
        // Deletions occupy rows on the left but have no corresponding editor
        // line. Keep a mapping between the editor's logical rows and the
        // aligned diff rows instead of copying raw pixel offsets.
        let mut left_to_right = Vec::with_capacity(rows.len());
        let mut right_line = 0usize;
        for row in &rows {
            if row.right.is_some() {
                right_line += 1;
            }
            left_to_right.push(right_line.saturating_sub(1));
        }
        let left_to_right_for_scroll = left_to_right.clone();
        v_flex()
            .id(format!("diff-pane-{}", self.rel))
            .role(Role::Group)
            .aria_label("Git diff")
            .key_context("Editor")
            .size_full()
            .track_focus(&self.focus)
            .capture_action::<gpui_kit::component::input::Copy>(cx.listener(|this, _, window, cx| {
                if !this.editor.read(cx).selected_value().is_empty() {
                    clipboard::notify_copied(window, cx);
                }
            }))
            .bg(cx.theme().background)
            .font_family(cx.theme().mono_font_family.clone())
            .text_xs()
            .when(!note.is_empty(), |this| {
                this.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_color(cx.theme().muted_foreground)
                        .child(note),
                )
            })
            .child(
                div()
                    .id(format!("diff-scroll-{}", self.rel))
                    .flex_1()
                    .min_h_0()
                    .child(h_flex().size_full().items_start()
                        .child(v_flex().id(format!("diff-left-{}", self.rel)).w(relative(0.5)).h_full().min_w_0().overflow_y_scroll().overflow_x_scroll().track_scroll(&left_scroll)
                            .on_scroll_wheel(move |_, _, cx| {
                                let left_scroll = left_scroll.clone();
                                let editor = editor_for_left_scroll.clone();
                                let line_map = left_to_right_for_scroll.clone();
                                cx.defer(move |cx| {
                                    let row = left_scroll.top_item();
                                    let line = line_map.get(row).copied().unwrap_or(0);
                                    let x = left_scroll.offset().x;
                                    editor.update(cx, |state, cx| {
                                        let mut offset = state.scroll_offset();
                                        offset.x = x;
                                        if let Some(line_height) = state.line_height() {
                                            offset.y = -(line_height * line as f32);
                                            state.set_scroll_offset(offset, cx);
                                        }
                                    });
                                });
                            })
                            .children(rows.iter().cloned().map(|row| diff_cell(row.left.unwrap_or_default(), row_left_bg(row.kind, cx), Some(cx.theme().border), word_wrap))))
                        .child(if self.binary || !self.ready {
                            div().w(relative(0.5)).min_w_0()
                                .when(!self.binary, |panel| panel.p_2().text_color(cx.theme().muted_foreground).child("Loading working tree…"))
                                .into_any_element()
                        } else {
                            div().w(relative(0.5)).h_full().min_w_0()
                                .child(Editor::new(&self.editor).bordered(false).p_0().size_full().text_size(px(12.)).font_family(cx.theme().mono_font_family.clone()))
                                .into_any_element()
                        }))
            )
    }
}

fn row_left_bg(kind: RowKind, cx: &App) -> Option<Hsla> {
    match kind {
        RowKind::Removed => Some(cx.theme().danger.opacity(0.28)),
        RowKind::Changed => Some(cx.theme().danger.opacity(0.22)),
        _ => None,
    }
}

fn build_right_diff_decorations(text: &str, rows: &[SplitRow], cx: &App) -> Vec<TextDecoration> {
    let mut decorations = Vec::new();
    let mut offset = 0usize;
    for row in rows {
        let Some(line) = row.right.as_deref() else {
            continue;
        };
        if offset > text.len() || !text.is_char_boundary(offset) {
            break;
        }
        let raw_end = text[offset..]
            .find('\n')
            .map(|relative_end| offset + relative_end)
            .unwrap_or(text.len());
        let raw_line = text[offset..raw_end].strip_suffix('\r').unwrap_or(&text[offset..raw_end]);
        // The editor is the source of offsets; rows are only used to classify
        // each corresponding line. Keeping CRLF bytes in the range prevents
        // later decorations from drifting on Windows files.
        let end = if raw_line == line {
            raw_end
        } else {
            offset.saturating_add(line.len()).min(text.len())
        };
        let end_with_newline = if text.as_bytes().get(raw_end) == Some(&b'\n') {
            raw_end + 1
        } else {
            end
        };
        let color = match row.kind {
            RowKind::Added => Some(cx.theme().success.opacity(0.28)),
            RowKind::Changed => Some(cx.theme().success.opacity(0.22)),
            RowKind::Same | RowKind::Removed => None,
        };
        if let Some(background_color) = color {
            if offset < end_with_newline {
                decorations.push(TextDecoration::new(
                    offset..end_with_newline,
                    HighlightStyle {
                        background_color: Some(background_color),
                        ..Default::default()
                    },
                ));
            }
        }
        offset = end_with_newline;
    }
    decorations
}

fn diff_cell(text: String, bg: Option<Hsla>, rule: Option<Hsla>, word_wrap: bool) -> impl IntoElement {
    div()
        .w_full()
        .min_w_0()
        .px_2()
        .when_some(bg, |cell, color| cell.bg(color))
        .when_some(rule, |cell, color| cell.border_r_1().border_color(color))
        .when(!word_wrap, |cell| cell.whitespace_nowrap())
        .child(if text.is_empty() {
            " ".to_string()
        } else {
            text
        })
}

/// Placeholder for a file the editor must not decode.
pub struct BinaryPanel {
    path: String,
    focus: FocusHandle,
    workspace: WeakEntity<Workspace>,
    metrics: TabStripMetrics,
    plus_shift: Rc<Cell<f32>>,
    closed: bool,
}

impl BinaryPanel {
    pub fn new(
        path: String,
        workspace: WeakEntity<Workspace>,
        metrics: TabStripMetrics,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            path,
            focus: cx.focus_handle(),
            workspace,
            metrics,
            plus_shift: Rc::new(Cell::new(0.0)),
            closed: false,
        }
    }

    pub fn release(&mut self) {
        self.closed = true;
    }

    fn label(&self) -> String {
        let shown = display_path(&self.path);
        path_basename(&shown).unwrap_or("binary").to_string()
    }
}

impl EventEmitter<PanelEvent> for BinaryPanel {}

impl Focusable for BinaryPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl BasePanel for BinaryPanel {
    fn panel_name(&self) -> &'static str {
        "Binary"
    }

    fn zoomable(&self, _: &App) -> bool {
        false
    }

    fn set_active(&mut self, active: bool, window: &mut Window, cx: &mut Context<Self>) {
        let path = self.path.clone();
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            workspace
                .update(cx, |workspace, cx| {
                    workspace.note_binary_active(&path, active, cx)
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
        let path = self.path.clone();
        let workspace = self.workspace.clone();
        cx.defer(move |cx| {
            workspace
                .update(cx, |workspace, cx| workspace.forget_binary(&path, cx))
                .ok();
        });
    }
}

impl DockPanel for BinaryPanel {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace = self.workspace.clone();
        let panel_id = PanelId::from(cx.entity().entity_id());
        let metrics = self.metrics.clone();
        h_flex()
            .id(format!("binary-title-{}", self.path))
            .gap_1()
            .items_center()
            .min_w_0()
            .on_prepaint(move |bounds, _, _| note_tab_edge(&metrics, bounds))
            .child(Icon::new(IconName::File).small())
            .child(div().text_ellipsis().child(self.label()))
            .child(tab_close_button(
                format!("close-binary-{}", self.path),
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
            format!("new-term-binary-{}", self.path),
            self.metrics.clone(),
            self.plus_shift.clone(),
            self.workspace.clone(),
            panel_id,
        ))
    }

    fn dropdown_menu(
        &mut self,
        menu: gpui_kit::component::menu::PopupMenu,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui_kit::component::menu::PopupMenu {
        let panel_id = PanelId::from(cx.entity().entity_id());
        with_close_items(menu, self.workspace.clone(), panel_id, false, cx)
    }

    fn zoom_control(&self, _: &App) -> Option<gpui_kit::component::dock::PanelControl> {
        None
    }

    fn inner_padding(&self, _: &App) -> bool {
        false
    }
}

impl Render for BinaryPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let path = display_path(&self.path);
        let open_path = self.path.clone();
        let workspace = self.workspace.clone();
        v_flex()
            .id(format!("binary-pane-{}", self.path))
            .role(Role::Group)
            .aria_label("Binary file")
            .size_full()
            .track_focus(&self.focus)
            .items_center()
            .justify_center()
            .gap_2()
            .bg(cx.theme().background)
            .child(div().text_lg().font_semibold().child("Binary file"))
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().muted_foreground)
                    .child(path),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Not opened in the editor."),
            )
            .child(
                Button::new(format!("open-ext-{}", self.path))
                    .small()
                    .label("Open externally")
                    .on_click(move |_, _, cx| {
                        let path = open_path.clone();
                        let workspace = workspace.clone();
                        workspace
                            .update(cx, |workspace, cx| workspace.open_external(path, cx))
                            .ok();
                        cx.stop_propagation();
                    }),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::{RowKind, SplitRow, git_relative, split_diff_lines};

    #[test]
    fn split_pairs_a_change() {
        let rows = split_diff_lines("a\nb\nc\n", "a\nB\nc\n");
        assert_eq!(
            rows,
            vec![
                SplitRow {
                    left: Some("a".into()),
                    right: Some("a".into()),
                    kind: RowKind::Same,
                },
                SplitRow {
                    left: Some("b".into()),
                    right: Some("B".into()),
                    kind: RowKind::Changed,
                },
                SplitRow {
                    left: Some("c".into()),
                    right: Some("c".into()),
                    kind: RowKind::Same,
                },
            ]
        );
    }

    #[test]
    fn split_shows_insert_and_delete() {
        let rows = split_diff_lines("a\n", "a\nb\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].kind, RowKind::Added);
        assert_eq!(rows[1].right.as_deref(), Some("b"));
        let rows = split_diff_lines("a\nb\n", "a\n");
        assert_eq!(rows[1].kind, RowKind::Removed);
        assert_eq!(rows[1].left.as_deref(), Some("b"));
    }

    #[test]
    fn split_reclassifies_rows_against_original_after_edit() {
        let rows = split_diff_lines("one\ntwo\n", "one\nthree\nfour\n");
        assert_eq!(rows[1].kind, RowKind::Changed);
        assert_eq!(rows[1].left.as_deref(), Some("two"));
        assert_eq!(rows[1].right.as_deref(), Some("three"));
        assert_eq!(rows[2].kind, RowKind::Added);
        assert_eq!(rows[2].right.as_deref(), Some("four"));
    }

    #[test]
    fn relative_path_strips_the_repo_root() {
        assert_eq!(
            git_relative("/work/proj", "/work/proj/src/main.rs").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(git_relative("/work/proj", "/other/file.rs"), None);
        assert_eq!(
            git_relative("/work/proj", "src/a.rs").as_deref(),
            Some("src/a.rs")
        );
    }
}

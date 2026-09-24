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
use gpui_kit::component::{
    ActiveTheme as _, Icon, Sizable as _, StyledExt as _, button::Button, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::pane::{new_terminal_button, note_tab_edge, tab_close_button, with_close_items};
use super::paths::display_path;
use super::rail::path_basename;
use super::tab_chrome::TabStripMetrics;
use super::workspace::Workspace;

const MAX_DIFF_LINES: usize = 2000;
const MAX_RENDER_ROWS: usize = 800;

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
    note: String,
    focus: FocusHandle,
    workspace: WeakEntity<Workspace>,
    metrics: TabStripMetrics,
    plus_shift: Rc<Cell<f32>>,
    closed: bool,
}

impl DiffPanel {
    pub fn new(
        rel: String,
        title_path: String,
        pinned: bool,
        workspace: WeakEntity<Workspace>,
        metrics: TabStripMetrics,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            rel,
            title_path,
            pinned,
            rows: Vec::new(),
            note: "Loading diff…".into(),
            focus: cx.focus_handle(),
            workspace,
            metrics,
            plus_shift: Rc::new(Cell::new(0.0)),
            closed: false,
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
        cx: &mut Context<Self>,
    ) {
        if binary {
            self.rows.clear();
            self.note = "Binary file — diff is not shown.".into();
        } else {
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

    fn title_suffix(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<impl IntoElement> {
        Some(new_terminal_button(
            format!("new-term-diff-{}", self.rel),
            self.metrics.clone(),
            self.plus_shift.clone(),
            self.workspace.clone(),
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
        let rows = self
            .rows
            .iter()
            .take(MAX_RENDER_ROWS)
            .cloned()
            .collect::<Vec<_>>();
        let hidden = self.rows.len().saturating_sub(rows.len());
        let note = self.note.clone();
        v_flex()
            .id(format!("diff-pane-{}", self.rel))
            .role(Role::Group)
            .aria_label("Git diff")
            .size_full()
            .track_focus(&self.focus)
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
                    .overflow_y_scroll()
                    .children(rows.into_iter().map(|row| split_row(row, cx)))
                    .when(hidden > 0, |this| {
                        this.child(
                            div()
                                .px_2()
                                .py_1()
                                .text_color(cx.theme().muted_foreground)
                                .child(format!("{hidden} more lines not shown")),
                        )
                    }),
            )
    }
}

fn split_row(row: SplitRow, cx: &App) -> impl IntoElement {
    let (left_bg, right_bg) = match row.kind {
        RowKind::Same => (None, None),
        RowKind::Removed => (Some(cx.theme().danger.opacity(0.28)), None),
        RowKind::Added => (None, Some(cx.theme().success.opacity(0.28))),
        RowKind::Changed => (
            Some(cx.theme().danger.opacity(0.22)),
            Some(cx.theme().success.opacity(0.22)),
        ),
    };
    let rule = cx.theme().border;
    h_flex()
        .w_full()
        .items_start()
        .child(diff_cell(row.left.unwrap_or_default(), left_bg, Some(rule)))
        .child(diff_cell(row.right.unwrap_or_default(), right_bg, None))
}

fn diff_cell(text: String, bg: Option<Hsla>, rule: Option<Hsla>) -> impl IntoElement {
    div()
        .w(relative(0.5))
        .min_w_0()
        .px_2()
        .when_some(bg, |cell, color| cell.bg(color))
        .when_some(rule, |cell, color| cell.border_r_1().border_color(color))
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

    fn title_suffix(&mut self, _: &mut Window, _: &mut Context<Self>) -> Option<impl IntoElement> {
        Some(new_terminal_button(
            format!("new-term-binary-{}", self.path),
            self.metrics.clone(),
            self.plus_shift.clone(),
            self.workspace.clone(),
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

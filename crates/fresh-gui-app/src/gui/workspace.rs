//! Zed/VS Code-like ADE workspace: activity bar, explorer, docked tabs, status, palette.
//!
//! Editor and terminal surfaces are dock panels. Dragging a tab to a pane edge
//! splits; dropping it on a tab merges; dropping it in the strip reorders.
//! gpui-component refuses to drag the last remaining tab, so a split needs two
//! tabs. Terminal titles are herdr-style numbers inside the focused workspace.
//! A left spaces rail lists daemon workspaces (name and project root);
//! switching swaps this dock for that workspace's session without closing
//! its PTYs.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use fresh_gui_protocol::{
    CAP_GIT, CAP_WORKSPACE, CAP_WORKSPACE_SET_ROOT, FsEntry, FsKind, GitFile, Hello, PtyInfo,
    LayoutNode, WorkspaceInfo, WorkspaceLayoutExtra, WorkspaceTab, WorkspaceTabKind,
};
use gpui_kit::base::Placement;
use gpui_kit::component::dock::{BasePanelView, DockArea, DockEvent, DockLayout, DockPlacement, InsertTarget, PaneNode, PaneRef, PanelId, panel_handle};
use gpui_kit::component::menu::{AppMenuBar, ContextMenuExt as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme, Disableable as _, Icon, IconName, Root, Selectable, Sizable, StyledExt, TitleBar,
    button::{Button, ButtonVariants as _},
    command::{Command, CommandGroup, CommandItem, CommandState},
    h_flex,
    input::{Input, InputEvent, InputState},
    list::ListItem,
    status_bar::StatusBar,
    tree::{TreeEvent, TreeState, tree},
    v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::actions::{
    ClearExplorerInput, CloseAllEditors, CloseAllOtherTabs, CloseAllOtherTerminals,
    CloseAllTerminals, CloseTab, CloseWorkspace, CopyExplorer, DeleteExplorer, Disconnect, FilterExplorer,
    AskCopilot, FormatDocument, GoToFile, NewFile, NewTerminal, NewWorkspace, NextTab, OpenDefaultSettings, OpenSettings, PasteExplorer,
    PrevTab, QuitClient, Reconnect, RenameWorkspace, ResetContentZoom, ResetUiZoom, SaveBuffer,
    SplitTerminal, StopServer, TerminalCopyOrInterrupt, TogglePinTab, ToggleCommandPalette, ToggleSidebar, ZoomInContent, ZoomInUi, ZoomOutContent,
    ZoomOutUi,
};
use super::ade::AttachedWorkspace;
use super::ade::{AdeCmd, AdeEvent, AdeHandle};
use super::chrome;
use super::connect::{ConnectTarget, parse_goto_spec};
use super::diff_view::{self, BinaryPanel, DiffPanel};
use super::dock_a11y::install_workspace_dock;
use super::explorer::{
    absolute_paths_text, apply_selection, build_explorer_tree, copyable_sources, drag_paths,
    entry_kinds, gesture_from_modifiers, is_placeholder, is_untitled_editor_key,
    movable_sources, parent_dir, pick_goto_target, prune_expanded, real_ids, rebase_listing,
    record_tree_toggle, save_target_path, tree_row_indent_px, untitled_editor_key,
    unused_file_name,
};
use super::file_icons::explorer_glyph;
use super::pane::{EditorPanel, TerminalPanel};
use super::paths::{
    daemon_uses_unix_paths, display_path, strip_verbatim_prefixes, workspace_root_for_daemon,
};
use super::rail::{
    WORKSPACE_RAIL_W, WORKSPACE_ROW_H, choose_shell_cwd, empty_workspace_name_hint,
    explorer_header_label, path_basename, show_workspace_rail, user_home, workspace_rail_hint,
    workspace_root_label,
};
use super::restore::{RestoreStep, restore_plan};
use super::tab_chrome::{TabCloseScope, TabStripMetrics, panels_for_close_scope};

/// Limit a single parser/paint update without delaying the first PTY byte.
const PTY_BATCH_BYTES: usize = 128 * 1024;
const PTY_BATCH_EVENTS: usize = 32;

fn coalesce_pty_event(
    event: AdeEvent,
    rx: &async_channel::Receiver<AdeEvent>,
    pending: &mut Option<AdeEvent>,
) -> AdeEvent {
    let AdeEvent::PtyData { id, mut bytes } = event else {
        return event;
    };
    for _ in 1..PTY_BATCH_EVENTS {
        let Ok(next) = rx.try_recv() else { break };
        match next {
            AdeEvent::PtyData { id: next_id, bytes: next_bytes }
                if next_id == id && bytes.len() + next_bytes.len() <= PTY_BATCH_BYTES =>
            {
                bytes.extend(next_bytes);
            }
            other => {
                *pending = Some(other);
                break;
            }
        }
    }
    AdeEvent::PtyData { id, bytes }
}

#[cfg(test)]
mod pty_batch_tests {
    use super::{AdeEvent, PTY_BATCH_BYTES, coalesce_pty_event};

    #[test]
    fn batches_adjacent_bytes_but_keeps_event_barriers() {
        let (tx, rx) = async_channel::unbounded();
        tx.try_send(AdeEvent::PtyData { id: "a".into(), bytes: b"b".to_vec() }).unwrap();
        tx.try_send(AdeEvent::PtyData { id: "other".into(), bytes: b"c".to_vec() }).unwrap();
        tx.try_send(AdeEvent::PtyData { id: "a".into(), bytes: b"d".to_vec() }).unwrap();
        let mut pending = None;
        let first = coalesce_pty_event(
            AdeEvent::PtyData { id: "a".into(), bytes: b"a".to_vec() },
            &rx, &mut pending,
        );
        assert!(matches!(first, AdeEvent::PtyData { id, bytes } if id == "a" && bytes == b"ab"));
        assert!(matches!(pending.take(), Some(AdeEvent::PtyData { id, bytes }) if id == "other" && bytes == b"c"));
        assert!(matches!(rx.try_recv(), Ok(AdeEvent::PtyData { id, bytes }) if id == "a" && bytes == b"d"));
    }

    #[test]
    fn batch_byte_cap_keeps_remainder_for_next_turn() {
        let (tx, rx) = async_channel::unbounded();
        tx.try_send(AdeEvent::PtyData { id: "a".into(), bytes: vec![2; PTY_BATCH_BYTES] }).unwrap();
        let mut pending = None;
        let first = coalesce_pty_event(
            AdeEvent::PtyData { id: "a".into(), bytes: vec![1] },
            &rx, &mut pending,
        );
        assert!(matches!(first, AdeEvent::PtyData { bytes, .. } if bytes == vec![1]));
        assert!(matches!(pending, Some(AdeEvent::PtyData { bytes, .. }) if bytes.len() == PTY_BATCH_BYTES));
    }

    #[test]
    fn ready_eight_kib_chunks_need_one_screen_update_per_128_kib() {
        let (tx, rx) = async_channel::unbounded();
        for _ in 1..16 {
            tx.try_send(AdeEvent::PtyData { id: "a".into(), bytes: vec![b'x'; 8192] }).unwrap();
        }
        let mut pending = None;
        let batch = coalesce_pty_event(
            AdeEvent::PtyData { id: "a".into(), bytes: vec![b'x'; 8192] },
            &rx, &mut pending,
        );
        assert!(matches!(batch, AdeEvent::PtyData { bytes, .. } if bytes.len() == PTY_BATCH_BYTES));
        assert!(pending.is_none());
        assert!(rx.is_empty());
    }
}

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

fn zoom_percent(zoom: f32) -> i32 {
    (zoom * 100.0).round() as i32
}

/// `{request_id}: {path}` or `{request_id}: binary file: {path}`.
fn split_request_message(message: &str) -> Option<(&str, &str)> {
    let (request_id, rest) = message.split_once(": ")?;
    let rest = rest.strip_prefix("binary file: ").unwrap_or(rest).trim();
    if request_id.is_empty() || rest.is_empty() {
        None
    } else {
        Some((request_id, rest))
    }
}

fn git_lookup_key(path: &str) -> String {
    let shown = display_path(path);
    if cfg!(windows) {
        shown.to_ascii_lowercase()
    } else {
        shown
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum GitTreeRow {
    Dir { path: String, depth: usize, name: String },
    File { file: GitFile, depth: usize, name: String },
}

struct GitTreeNode {
    dirs: BTreeMap<String, GitTreeNode>,
    files: BTreeMap<String, GitFile>,
}

impl GitTreeNode {
    fn new() -> Self {
        Self { dirs: BTreeMap::new(), files: BTreeMap::new() }
    }
}

fn git_path_parts(path: &str) -> Vec<&str> {
    path.split(['/', '\\']).filter(|seg| !seg.is_empty()).collect()
}

/// Directory paths that contain at least one changed file, using `/`.
fn git_dir_paths(files: &[GitFile]) -> HashSet<String> {
    let mut dirs = HashSet::new();
    for file in files {
        let parts = git_path_parts(&file.path);
        if parts.len() <= 1 {
            continue;
        }
        let mut acc = String::new();
        for seg in &parts[..parts.len() - 1] {
            if !acc.is_empty() {
                acc.push('/');
            }
            acc.push_str(seg);
            dirs.insert(acc.clone());
        }
    }
    dirs
}

/// Changed files grouped by directory. Directories absent from `collapsed` are open.
fn git_change_rows(files: &[GitFile], collapsed: &HashSet<String>) -> Vec<GitTreeRow> {
    let mut root = GitTreeNode::new();
    for file in files {
        let parts = git_path_parts(&file.path);
        if parts.is_empty() {
            continue;
        }
        let mut node = &mut root;
        for seg in &parts[..parts.len() - 1] {
            node = node.dirs.entry((*seg).to_string()).or_insert_with(GitTreeNode::new);
        }
        let name = parts[parts.len() - 1].to_string();
        node.files.insert(name, file.clone());
    }
    let mut rows = Vec::new();
    walk_git_tree(&root, "", 0, collapsed, &mut rows);
    rows
}

fn walk_git_tree(
    node: &GitTreeNode,
    prefix: &str,
    depth: usize,
    collapsed: &HashSet<String>,
    rows: &mut Vec<GitTreeRow>,
) {
    for (name, child) in &node.dirs {
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };
        rows.push(GitTreeRow::Dir { path: path.clone(), depth, name: name.clone() });
        if !collapsed.contains(&path) {
            walk_git_tree(child, &path, depth + 1, collapsed, rows);
        }
    }
    for (name, file) in &node.files {
        rows.push(GitTreeRow::File { file: file.clone(), depth, name: name.clone() });
    }
}

fn git_dir_row(
    ix: usize,
    path: String,
    name: String,
    depth: usize,
    open: bool,
    view: Entity<Workspace>,
    zoom: f32,
) -> impl IntoElement {
    let chevron = if open { IconName::ChevronDown } else { IconName::ChevronRight };
    h_flex()
        .id(format!("git-dir-{ix}"))
        .w_full()
        .h(px(TREE_ROW_H * zoom))
        .pl(px(tree_row_indent_px(depth) * zoom))
        .pr_1()
        .gap_1()
        .items_center()
        .cursor_pointer()
        .on_click({
            let view = view.clone();
            move |_, _, cx| {
                let dir = path.clone();
                view.update(cx, |this, cx| {
                    if !this.git_collapsed.remove(&dir) {
                        this.git_collapsed.insert(dir);
                    }
                    cx.notify();
                });
            }
        })
        .child(div().w(px(16. * zoom)).flex_shrink_0().flex().justify_center().child(Icon::new(chevron).xsmall()))
        .child(div().flex_1().min_w_0().text_sm().text_ellipsis().child(name))
}

fn git_file_row(
    ix: usize,
    file: GitFile,
    name: String,
    depth: usize,
    busy: bool,
    view: Entity<Workspace>,
    zoom: f32,
) -> impl IntoElement {
    let mut chars = file.xy.chars();
    let index = chars.next().unwrap_or(' ');
    let work = chars.next().unwrap_or(' ');
    let unstaged = work != ' ' || index == '?';
    let staged = index != ' ' && index != '?';
    let xy = if file.xy.is_empty() { "  ".to_string() } else { file.xy.clone() };
    let open_rel = file.path.clone();
    let stage_rel = file.path.clone();
    let unstage_rel = file.path.clone();
    let revert_rel = file.path;

    h_flex()
        .id(format!("git-file-{ix}"))
        .w_full()
        .h(px(TREE_ROW_H * zoom))
        .pl(px(tree_row_indent_px(depth) * zoom))
        .pr_1()
        .gap_1()
        .items_center()
        .cursor_pointer()
        .on_click({
            let view = view.clone();
            move |event, window, cx| {
                let pin = diff_view::click_count(event) >= 2;
                let rel = open_rel.clone();
                view.update(cx, |this, cx| this.open_diff(rel, pin, window, cx));
            }
        })
        .child(div().w(px(20. * zoom)).flex_shrink_0().text_xs().child(xy))
        .child(div().flex_1().min_w_0().text_sm().text_ellipsis().child(name))
        .when(unstaged, |row| {
            let view = view.clone();
            row.child(
                Button::new(format!("git-stage-{ix}"))
                    .ghost()
                    .xsmall()
                    .label("Stage")
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        let rel = stage_rel.clone();
                        view.update(cx, |this, cx| this.git_stage(vec![rel], true, cx));
                        cx.stop_propagation();
                    }),
            )
        })
        .when(staged, |row| {
            let view = view.clone();
            row.child(
                Button::new(format!("git-unstage-{ix}"))
                    .ghost()
                    .xsmall()
                    .label("Unstage")
                    .disabled(busy)
                    .on_click(move |_, _, cx| {
                        let rel = unstage_rel.clone();
                        view.update(cx, |this, cx| this.git_stage(vec![rel], false, cx));
                        cx.stop_propagation();
                    }),
            )
        })
        .child(
            Button::new(format!("git-revert-{ix}"))
                .ghost()
                .xsmall()
                .label("Revert")
                .disabled(busy)
                .on_click(move |_, _, cx| {
                    let rel = revert_rel.clone();
                    view.update(cx, |this, cx| this.git_restore(vec![rel], cx));
                    cx.stop_propagation();
                }),
        )
}

#[cfg(test)]
mod git_tree_tests {
    use super::{git_change_rows, git_dir_paths, GitTreeRow};
    use fresh_gui_protocol::GitFile;
    use std::collections::HashSet;

    fn file(path: &str) -> GitFile {
        GitFile { path: path.into(), xy: " M".into() }
    }

    #[test]
    fn groups_files_under_directories_and_hides_collapsed_children() {
        let files = vec![
            file("src/gui/workspace.rs"),
            file("src/main.rs"),
            file("README.md"),
        ];
        let rows = git_change_rows(&files, &HashSet::new());
        let labels: Vec<String> = rows
            .iter()
            .map(|row| match row {
                GitTreeRow::Dir { path, depth, .. } => format!("d{depth}:{path}"),
                GitTreeRow::File { file, depth, name } => format!("f{depth}:{name}:{}", file.path),
            })
            .collect();
        assert_eq!(
            labels,
            vec![
                "d0:src",
                "d1:src/gui",
                "f2:workspace.rs:src/gui/workspace.rs",
                "f1:main.rs:src/main.rs",
                "f0:README.md:README.md",
            ]
        );
        let mut collapsed = HashSet::new();
        collapsed.insert("src".into());
        let collapsed_rows = git_change_rows(&files, &collapsed);
        assert!(collapsed_rows.iter().all(|row| !matches!(
            row,
            GitTreeRow::File { file, .. } if file.path.starts_with("src/")
        )));
        assert!(git_dir_paths(&files).contains("src/gui"));
    }
}

fn display_paths(paths: &[String]) -> Vec<String> {
    paths.iter().map(|path| display_path(path)).collect()
}

fn tab_key(tab: &WorkspaceTab) -> Option<String> {
    match tab.kind {
        WorkspaceTabKind::Terminal => tab.pty_id.as_ref().map(|id| format!("pty:{id}")),
        WorkspaceTabKind::Editor => tab.path.as_ref().map(|path| format!("file:{path}")),
    }
}

fn capture_center(node: &PaneNode, indices: &HashMap<PanelId, u32>) -> Option<LayoutNode> {
    match node.kind() {
        PaneRef::Tabs { panels, active_ix } => {
            let tabs: Vec<u32> = panels.iter().filter_map(|id| indices.get(id).copied()).collect();
            (!tabs.is_empty()).then_some(LayoutNode::Tabs { tabs, active: active_ix as u32 })
        }
        PaneRef::Split { axis, children, sizes } => {
            let mut kept = Vec::new();
            let mut kept_sizes = Vec::new();
            for (child, size) in children.iter().zip(sizes.iter()) {
                if let Some(snapshot) = capture_center(child, indices) {
                    kept.push(snapshot);
                    kept_sizes.push(size.map(f32::from));
                }
            }
            (!kept.is_empty()).then_some(LayoutNode::Split {
                axis: if axis == Axis::Horizontal { "horizontal" } else { "vertical" }.into(),
                children: kept,
                sizes: kept_sizes,
            })
        }
    }
}

fn restore_center(node: &LayoutNode, panels: &[Arc<dyn BasePanelView>], cx: &App) -> Option<DockLayout> {
    match node {
        LayoutNode::Tabs { tabs, active } => {
            if tabs.is_empty() { return None; }
            let mut layout = DockLayout::tabs();
            for ix in tabs {
                layout = layout.panel_view(panels.get(*ix as usize)?.clone(), cx);
            }
            Some(layout.active_index((*active as usize).min(tabs.len() - 1)))
        }
        LayoutNode::Split { axis, children, sizes } => {
            if children.is_empty() { return None; }
            let mut layout = match axis.as_str() {
                "horizontal" => DockLayout::h_split(),
                "vertical" => DockLayout::v_split(),
                _ => return None,
            };
            for (ix, child) in children.iter().enumerate() {
                let size = sizes.get(ix).and_then(|size| *size).filter(|size| size.is_finite() && *size >= 0.0 && *size <= 10000.0).map(px);
                layout = layout.child(restore_center(child, panels, cx)?, size);
            }
            Some(layout)
        }
    }
}

fn complete_center(node: &LayoutNode, count: usize) -> bool {
    fn collect(node: &LayoutNode, found: &mut Vec<u32>) {
        match node {
            LayoutNode::Tabs { tabs, .. } => found.extend(tabs),
            LayoutNode::Split { children, .. } => for child in children { collect(child, found); },
        }
    }
    let mut found = Vec::new();
    collect(node, &mut found);
    found.sort_unstable();
    found == (0..count as u32).collect::<Vec<_>>()
}

#[cfg(test)]
mod layout_tests {
    use super::complete_center;
    use fresh_gui_protocol::LayoutNode;

    #[test]
    fn restored_split_requires_each_tab_once() {
        let valid = LayoutNode::Split {
            axis: "horizontal".into(),
            children: vec![LayoutNode::Tabs { tabs: vec![0], active: 0 }, LayoutNode::Tabs { tabs: vec![1], active: 0 }],
            sizes: vec![Some(240.0), None],
        };
        assert!(complete_center(&valid, 2));
        assert!(!complete_center(&valid, 3));
        assert!(!complete_center(&LayoutNode::Tabs { tabs: vec![0, 0], active: 0 }, 2));
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Activity {
    Explorer,
    Git,
}

enum ConnectionState {
    Connecting,
    Online,
    Offline { reason: String },
}

enum ActiveSurface {
    Terminal(String),
    Editor(String),
    Diff(String),
    Binary(String),
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
    /// Panel to split beside when the next terminal opens.
    pending_split: Option<PanelId>,
    /// Tab group (any panel in it) that should receive the next + New Terminal/File.
    pending_tab_group: Option<PanelId>,
    /// Editor opens issued by this view. Unsolicited `editor_opened` does not
    /// add a tab.
    pending_editors: HashMap<String, bool>,
    restoring: bool,
    /// Workspace whose name is being edited in the rail. Any row, not only the
    /// active one. `None` when the inline field is closed.
    renaming_id: Option<String>,
    relocating_id: Option<String>,
    /// Row under the pointer, so Rename / Close stay off the resting layout.
    rail_hover: Option<String>,
    create_open: bool,
    capabilities: Vec<String>,
    config_path: Option<String>,
    defaults_path: Option<String>,
    /// Editor and terminal text scale. Does not resize the rails.
    content_zoom: f32,
    /// Rem-based chrome text. Also multiplies panel text.
    ui_zoom: f32,
    editor_font_base: f32,
    terminal_font_base: f32,
    status: SharedString,
    sidebar_collapsed: bool,
    workspace_rail_width: f32,
    explorer_width: f32,
    resizing_rail: bool,
    resizing_explorer: bool,
    resize_last_x: Option<f32>,
    activity: Activity,
    dock: Entity<DockArea>,
    tab_metrics: TabStripMetrics,
    terminals: HashMap<String, Entity<TerminalPanel>>,
    editors: HashMap<String, Entity<EditorPanel>>,
    diffs: HashMap<String, Entity<DiffPanel>>,
    binaries: HashMap<String, Entity<BinaryPanel>>,
    /// Unpinned diff tab. The next preview replaces it.
    diff_preview: Option<String>,
    /// Last terminal or editor panel, so a diff tab does not rewrite the saved active index.
    last_saved_panel: Option<PanelId>,
    pinned_tabs: HashSet<String>,
    restore_extra: Option<WorkspaceLayoutExtra>,
    restore_tabs: Vec<WorkspaceTab>,
    restore_scroll_index: Option<usize>,
    next_terminal_number: u32,
    active: Option<ActiveSurface>,
    git_cap: bool,
    git_repo: bool,
    git_root: String,
    git_branch: String,
    git_upstream: Option<String>,
    git_ahead: u32,
    git_behind: u32,
    git_files: Vec<GitFile>,
    /// Directory paths (relative, `/`-separated) the user has collapsed.
    git_collapsed: HashSet<String>,
    git_detail: Option<String>,
    git_busy: bool,
    git_status_req: Option<String>,
    /// Display path and repo-relative path → repo-relative path.
    git_paths: HashMap<String, String>,
    pending_diffs: HashMap<String, String>,
    commit_input: Entity<InputState>,
    menu_bar: Entity<AppMenuBar>,
    last_cwd: Option<String>,
    /// Directory whose repository is shown in the Git panel.
    git_context_dir: String,
    explorer: Entity<TreeState>,
    explorer_root: String,
    explorer_cache: HashMap<String, Vec<FsEntry>>,
    /// Directories the user has open. A cache hit is not an expand: rebuilding
    /// from listings used to reopen every listed folder and close one whose
    /// `fs_list` was still in flight.
    expanded_dirs: HashSet<String>,
    /// Path → kind for `explorer_cache`, rebuilt with the tree. Rendering runs
    /// on every PTY chunk, so rows read this instead of walking the cache.
    explorer_kinds: Rc<HashMap<String, FsKind>>,
    /// `fs_list` request id → the path the explorer asked for.
    pending_lists: HashMap<String, String>,
    /// Titles for terminal tabs whose PTY did not survive a daemon restart,
    /// in tab order. Each `pty_opened` for a respawn takes the next one.
    respawn_titles: VecDeque<String>,
    /// Saved active tab when it is a live terminal, selected once the
    /// restore's editors have opened (they would otherwise take focus).
    restore_focus: Option<String>,
    explorer_focus: FocusHandle,
    filter_open: bool,
    filter_input: Entity<InputState>,
    renaming_path: Option<String>,
    file_rename_input: Entity<InputState>,
    save_open: bool,
    save_path_input: Entity<InputState>,
    pending_renames: HashMap<String, String>,
    pending_creates: HashSet<String>,
    /// Selected absolute paths. The last entry is the primary row.
    selection: Vec<String>,
    anchor: Option<String>,
    /// In-app file clipboard for explorer paste (`fs_copy`). Absolute paths.
    file_clipboard: Option<Vec<String>>,
    pending_fs: HashMap<String, PendingFs>,
    command_state: Entity<CommandState>,
    copilot_input: Entity<InputState>,
    copilot_open: bool,
    copilot_busy: bool,
    copilot_result: Option<String>,
    palette_open: bool,
    goto_open: bool,
    goto_input: Entity<InputState>,
    create_name: Entity<InputState>,
    create_root: Entity<InputState>,
    ws_rename_input: Entity<InputState>,
    ws_root_input: Entity<InputState>,
    rename_pty: Option<String>,
    rename_input: Entity<InputState>,
    _subscriptions: Vec<Subscription>,
    _recv_task: Task<()>,
}

impl Workspace {
    pub fn new(target: ConnectTarget, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let (ade, evt_rx) = super::ade::spawn(target.clone());
        let (dock, _) = install_workspace_dock(window, cx);
        let explorer = cx.new(|cx| TreeState::new(cx));
        let command_state = cx.new(|cx| CommandState::new(window, cx));
        let copilot_input = cx.new(|cx| InputState::new(window, cx).placeholder("Ask Copilot about this project…"));
        let goto_input = cx.new(|cx| InputState::new(window, cx).placeholder("path[:line[:col]]"));
        let create_name = cx.new(|cx| InputState::new(window, cx).placeholder("Folder name"));
        let create_root = cx.new(|cx| InputState::new(window, cx).placeholder("/absolute/path"));
        let ws_rename_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Workspace name"));
        let ws_root_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Absolute path on daemon"));
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Terminal name"));
        let filter_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Filter files and folders"));
        let file_rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("New name"));
        let save_path_input = cx.new(|cx| InputState::new(window, cx).placeholder("/path/to/file"));
        let commit_input = cx.new(|cx| InputState::new(window, cx).placeholder("Commit message"));
        let menu_bar = AppMenuBar::new(cx);
        let tree_sub = cx.subscribe(&explorer, |this, _, ev: &TreeEvent, cx| {
            // The tree already toggled on mouse-down. Record that and list a
            // directory we have not seen. Do not rebuild here: this callback
            // runs inside the tree update, and `set_items` would panic.
            match ev {
                TreeEvent::Expanded(id) => {
                    let path = id.to_string();
                    record_tree_toggle(&mut this.expanded_dirs, &path, true);
                    if !is_placeholder(&path) && !this.explorer_cache.contains_key(&path) {
                        this.list_dir(&path);
                    }
                }
                TreeEvent::Collapsed(id) => {
                    record_tree_toggle(&mut this.expanded_dirs, id.as_ref(), false);
                }
            }
            this.publish_layout(cx);
        });
        // Reorder, split, and merge change the tab order without changing the
        // active tab. The dock emits this from inside its own update, and
        // `capture_layout` reads the dock, so publish after it returns.
        let dock_sub = cx.subscribe(&dock, |this, _, ev: &DockEvent, cx| {
            if matches!(ev, DockEvent::LayoutChanged) && !this.restoring {
                let this = cx.entity();
                cx.defer(move |cx| this.update(cx, |this, cx| this.publish_layout(cx)));
            }
        });
        let rename_sub = cx.subscribe(&rename_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) && this.rename_pty.is_some() {
                this.confirm_rename(cx);
            }
        });
        let filter_sub = cx.subscribe(&filter_input, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                let workspace = cx.entity();
                cx.defer(move |cx| {
                    workspace.update(cx, |this, cx| {
                        this.rebuild_tree(cx);
                        cx.notify();
                    });
                });
            }
        });
        let file_rename_sub = cx.subscribe(&file_rename_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) && this.renaming_path.is_some() {
                this.confirm_file_rename(cx);
            }
        });
        let save_path_sub = cx.subscribe(&save_path_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) && this.save_open {
                this.confirm_save(cx);
            }
        });
        let ws_rename_sub = cx.subscribe(&ws_rename_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) && this.renaming_id.is_some() {
                this.confirm_workspace_rename(cx);
            }
        });
        let ws_root_sub = cx.subscribe(&ws_root_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) && this.relocating_id.is_some() {
                this.confirm_workspace_root(cx);
                cx.notify();
            }
        });
        let create_name_sub = cx.subscribe(&create_name, |this, _, ev: &InputEvent, cx| {
            if !this.create_open {
                return;
            }
            match ev {
                InputEvent::PressEnter { .. } => {
                    this.confirm_create(cx);
                    cx.notify();
                }
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });
        let create_root_sub = cx.subscribe(&create_root, |this, _, ev: &InputEvent, cx| {
            if !this.create_open {
                return;
            }
            match ev {
                InputEvent::PressEnter { .. } => {
                    this.confirm_create(cx);
                    cx.notify();
                }
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });
        let goto_sub = cx.subscribe(&goto_input, |this, _, ev: &InputEvent, cx| {
            if !this.goto_open {
                return;
            }
            match ev {
                InputEvent::PressEnter { .. } => this.confirm_goto(cx),
                InputEvent::Change => cx.notify(),
                _ => {}
            }
        });
        let commit_sub = cx.subscribe(&commit_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.git_commit(cx);
            }
        });

        let recv_task = cx.spawn_in(window, async move |this, cx| {
            let mut pending = None;
            let mut handled = 0;
            loop {
                let ev = match pending.take() {
                    Some(ev) => ev,
                    None => match evt_rx.recv().await { Ok(ev) => ev, Err(_) => break },
                };
                let ev = coalesce_pty_event(ev, &evt_rx, &mut pending);
                if cx
                    .update(|window, app| {
                        this.update(app, |this, cx| this.handle_event(ev, window, cx))
                    })
                    .is_err()
                {
                    break;
                }
                handled += 1;
                if handled == 8 {
                    handled = 0;
                    if pending.is_some() || !evt_rx.is_empty() {
                        // Let a paint/input turn run during sustained PTY output.
                        cx.background_executor().timer(Duration::from_millis(1)).await;
                    }
                }
            }
        });

        let closing = cx.weak_entity();
        // The Windows close button asks this hook while the HWND is still
        // valid. Layout is published here, before GPUI destroys the window.
        // `window not found` and the invalid-handle errors that follow are
        // GPUI calling ShowWindow / DestroyWindow after that HWND is gone.
        // This host does not keep a window handle of its own.
        window.on_window_should_close(cx, move |_, cx| {
            if let Some(this) = closing.upgrade() {
                this.update(cx, |this, cx| this.save_before_exit(cx));
            }
            true
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
            pending_split: None,
            pending_tab_group: None,
            pending_editors: HashMap::new(),
            restoring: false,
            renaming_id: None,
            relocating_id: None,
            rail_hover: None,
            create_open: false,
            capabilities: Vec::new(),
            config_path: None,
            defaults_path: None,
            content_zoom: 1.0,
            ui_zoom: 1.0,
            editor_font_base: 14.0,
            terminal_font_base: 14.0,
            status: "Connecting…".into(),
            sidebar_collapsed: false,
            workspace_rail_width: WORKSPACE_RAIL_W,
            explorer_width: 260.,
            resizing_rail: false,
            resizing_explorer: false,
            resize_last_x: None,
            activity: Activity::Explorer,
            dock,
            tab_metrics: TabStripMetrics::default(),
            terminals: HashMap::new(),
            editors: HashMap::new(),
            diffs: HashMap::new(),
            binaries: HashMap::new(),
            diff_preview: None,
            last_saved_panel: None,
            pinned_tabs: HashSet::new(),
            restore_extra: None,
            restore_tabs: Vec::new(),
            restore_scroll_index: None,
            next_terminal_number: 1,
            active: None,
            git_cap: false,
            git_repo: false,
            git_root: String::new(),
            git_branch: String::new(),
            git_upstream: None,
            git_ahead: 0,
            git_behind: 0,
            git_files: Vec::new(),
            git_collapsed: HashSet::new(),
            git_detail: None,
            git_busy: false,
            git_status_req: None,
            git_paths: HashMap::new(),
            pending_diffs: HashMap::new(),
            commit_input,
            menu_bar,
            last_cwd: None,
            git_context_dir: String::new(),
            explorer,
            explorer_root: String::new(),
            explorer_cache: HashMap::new(),
            expanded_dirs: HashSet::new(),
            explorer_kinds: Rc::default(),
            pending_lists: HashMap::new(),
            respawn_titles: VecDeque::new(),
            restore_focus: None,
            explorer_focus: cx.focus_handle(),
            filter_open: false,
            filter_input,
            renaming_path: None,
            file_rename_input,
            save_open: false,
            save_path_input,
            pending_renames: HashMap::new(),
            pending_creates: HashSet::new(),
            selection: Vec::new(),
            anchor: None,
            file_clipboard: None,
            pending_fs: HashMap::new(),
            command_state,
            copilot_input,
            copilot_open: false,
            copilot_busy: false,
            copilot_result: None,
            palette_open: false,
            goto_open: false,
            goto_input,
            create_name,
            create_root,
            ws_rename_input,
            ws_root_input,
            rename_pty: None,
            rename_input,
            _subscriptions: vec![
                tree_sub,
                dock_sub,
                rename_sub,
                filter_sub,
                file_rename_sub,
                save_path_sub,
                goto_sub,
                ws_rename_sub,
                ws_root_sub,
                create_name_sub,
                create_root_sub,
                commit_sub,
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
                self.apply_hello(&hello, window, cx);
                self.workspace_cap = hello.capabilities.iter().any(|cap| cap == CAP_WORKSPACE);
                self.workspaces = workspaces;
                self.session_id = Some(session_id);
                self.connection = ConnectionState::Online;
                self.status = "Online".into();
                self.pty_opens_pending = 0;
                self.pending_editors.clear();
                if let Some(attached) = attached {
                    self.restore_workspace(*attached, window, cx);
                } else {
                    self.active_workspace_id = None;
                    self.pty_opens_pending = 1;
                    self.ade.send(AdeCmd::OpenPty {
                        cols: 80,
                        rows: 24,
                        cwd: self.shell_cwd(cx),
                    });
                    self.list_dir("");
                    self.refresh_git();
                }
            }
            AdeEvent::ConfigUpdated { shortkeys } => {
                super::actions::apply_shortkeys(cx, &shortkeys);
                self.status = "Keyboard shortcuts updated".into();
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
            AdeEvent::WorkspaceRootSet { workspace } => {
                let active = self.active_workspace_id.as_deref() == Some(workspace.id.as_str());
                self.status = format!(
                    "Workspace location changed to {}",
                    display_path(&workspace.root)
                )
                .into();
                if active {
                    self.explorer_root = workspace.root.clone();
                    self.git_context_dir.clear();
                    self.explorer_cache.clear();
                    self.restore_scroll_index = None;
                    self.pending_lists.clear();
                    self.expanded_dirs.clear();
                    self.selection.clear();
                    self.anchor = None;
                    self.rebuild_tree(cx);
                    self.list_dir(&workspace.root);
                    self.clear_git_view();
                    self.close_open_diffs(window, cx);
                    self.refresh_git();
                }
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
                self.restore_workspace(*attached, window, cx);
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
                    if let Some(title) = self.respawn_titles.pop_front() {
                        self.attach_terminal(id, title, window, cx);
                    } else {
                        self.add_terminal_tab(id.clone(), window, cx);
                        if let Some(beside) = self.pending_split.take()
                            && let Some(panel) = self.terminals.get(&id)
                        {
                            let new_id = PanelId::from(panel.entity_id());
                            let node = self.dock.read(cx).layout(DockPlacement::Center).and_then(|tree| tree.find_panel_node(beside));
                            if let Some(node) = node {
                                self.dock.update(cx, |dock, cx| {
                                    dock.split_at(node, new_id, Placement::Right, window, cx);
                                });
                            }
                        }
                    }
                    self.publish_layout(cx);
                    if self.pty_opens_pending == 0 && !self.restoring {
                        self.apply_restored_center(window, cx);
                    }
                }
            }
            AdeEvent::PtyData { id, bytes } => {
                // The terminal panel notifies itself. Redrawing the whole
                // workspace (rail, explorer, dock, status) per chunk is waste.
                self.on_pty_data(&id, &bytes, window, cx);
                return;
            }
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
            AdeEvent::FsListed {
                request_id,
                path,
                entries,
            } => {
                let requested = self.pending_lists.remove(&request_id).unwrap_or_default();
                let (path, entries) = rebase_listing(&requested, &path, entries);
                if self.explorer_root.is_empty() {
                    self.explorer_root = path.clone();
                }
                self.explorer_cache.insert(path, entries);
                self.rebuild_tree(cx);
                if self.pending_lists.is_empty() { self.restore_scroll_index = None; }
            }
            AdeEvent::FsMoved {
                request_id,
                entries,
            } => {
                self.finish_fs(&request_id, entries, true, cx);
            }
            AdeEvent::FsCreated { request_id, entry } => {
                if self.pending_creates.remove(&request_id) {
                    let path = entry.path.clone();
                    if let Some(parent) = parent_dir(&path) {
                        self.relist(&parent);
                    }
                    self.selection = vec![path.clone()];
                    self.anchor = Some(path.clone());
                    self.status = format!("Created {}", entry.name).into();
                    self.open_path(path.clone(), false);
                    self.begin_file_rename(path, window, cx);
                }
            }
            AdeEvent::FsRenamed { request_id, entry } => {
                if let Some(old_path) = self.pending_renames.remove(&request_id) {
                    self.renaming_path = None;
                    self.filter_open = false;
                    self.filter_input
                        .update(cx, |state, cx| state.set_value("", window, cx));
                    self.selection = vec![entry.path.clone()];
                    self.anchor = Some(entry.path.clone());
                    if let Some(parent) = parent_dir(&old_path) {
                        self.relist(&parent);
                    }
                    self.status = format!("Renamed to {}", entry.name).into();
                    self.refresh_git();
                    window.focus(&self.explorer_focus, cx);
                }
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
                    self.finish_restore_if_idle(window, cx);
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
            AdeEvent::BufferLspState { buffer_id, rev, text, diagnostics, status } => {
                if let Some(panel) = self.editor_by_buffer(&buffer_id, cx) {
                    panel.update(cx, |panel, cx| {
                        panel.set_lsp_state(rev, text, diagnostics, status, window, cx);
                    });
                }
            }
            AdeEvent::BufferFormatted { buffer_id, rev, text, status } => {
                if let Some(panel) = self.editor_by_buffer(&buffer_id, cx) {
                    panel.update(cx, |panel, cx| {
                        panel.apply_formatted(rev, text, status, window, cx);
                    });
                }
            }
            AdeEvent::Error { code, message } => {
                self.pending_fs.clear();
                if code == "buffer_format_failed"
                    && let Some((request_id, detail)) = split_request_message(&message)
                    && let Some(buffer_id) = request_id.strip_prefix("fmt-")
                        .and_then(|value| value.rsplit_once('-').map(|(id, _)| id))
                    && let Some(panel) = self.editor_by_buffer(buffer_id, cx)
                {
                    panel.update(cx, |panel, cx| panel.set_format_error(detail.to_string(), cx));
                }
                if code == "fs_create_failed" {
                    self.pending_creates.clear();
                }
                if code == "fs_rename_failed" {
                    if let Some((request_id, _)) = split_request_message(&message) {
                        self.pending_renames.remove(request_id);
                    }
                }
                if code == "pty_open_failed" {
                    self.pty_opens_pending = self.pty_opens_pending.saturating_sub(1);
                    self.respawn_titles.pop_front();
                }
                if code == "binary_file" {
                    if let Some((request_id, path)) = split_request_message(&message) {
                        let activate = self.pending_editors.remove(request_id).unwrap_or(true);
                        if !path.is_empty() {
                            self.open_binary(path.to_string(), activate, window, cx);
                        }
                        self.finish_restore_if_idle(window, cx);
                    }
                    self.status = "Binary file — not opened in the editor".into();
                } else if code == "git_failed" {
                    self.git_busy = false;
                    self.status = format!("Git: {message}").into();
                } else {
                    // A folder saved as open may be gone now; its failed listing
                    // is not a reason to abandon reopening the workspace's tabs.
                    let side = code.starts_with("fs_list")
                        || code.starts_with("fs_stat")
                        || code == "fs_open_failed";
                    if self.restoring && !side {
                        self.restoring = false;
                        self.pending_editors.clear();
                    }
                    self.status = if code == "fs_rename_failed" {
                        let detail = split_request_message(&message)
                            .map(|(_, detail)| detail)
                            .unwrap_or(&message);
                        format!("Rename failed: {detail}").into()
                    } else {
                        format!("{code}: {message}").into()
                    };
                }
            }
            AdeEvent::GitStatus {
                request_id,
                repo,
                root,
                branch,
                upstream,
                ahead,
                behind,
                files,
                detail,
            } => {
                if self.git_status_req.as_deref() == Some(request_id.as_str()) {
                    self.git_status_req = None;
                    self.apply_git_status(
                        repo, root, branch, upstream, ahead, behind, files, detail,
                    );
                }
            }
            AdeEvent::GitDiff {
                request_id,
                path,
                old_text,
                new_text,
                binary,
                truncated,
            } => {
                self.pending_diffs.remove(&request_id);
                if let Some(panel) = self.diffs.get(&path).cloned() {
                    panel.update(cx, |panel, cx| {
                        panel.show_sides(old_text, new_text, binary, truncated, cx);
                    });
                }
            }
            AdeEvent::GitOp {
                request_id: _,
                ok,
                output,
            } => {
                self.git_busy = false;
                let line = output
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap_or(if ok {
                        "Git command finished"
                    } else {
                        "Git command failed"
                    });
                let line = line.trim();
                self.status = if line.chars().count() > 180 {
                    format!("{}…", line.chars().take(180).collect::<String>()).into()
                } else {
                    line.to_string().into()
                };
                if ok {
                    self.refresh_git();
                }
            }
            AdeEvent::FsOpened { message, .. } => {
                self.status = message.into();
            }
        }
        cx.notify();
    }

    fn apply_hello(&mut self, hello: &Hello, window: &mut Window, cx: &mut Context<Self>) {
        if !hello.shortkeys.is_empty() {
            super::actions::apply_shortkeys(cx, &hello.shortkeys);
        }
        self.capabilities = hello.capabilities.clone();
        self.config_path = hello.config_path.clone();
        self.defaults_path = hello.defaults_path.clone();
        self.git_cap = hello.capabilities.iter().any(|cap| cap == CAP_GIT);
        if let Some(ui) = &hello.ui {
            self.editor_font_base = (ui.editor_font_size as f32).clamp(8.0, 64.0);
            self.terminal_font_base = (ui.terminal_font_size as f32).clamp(8.0, 64.0);
            chrome::apply_configured_theme(&ui.theme, Some(window), cx);
        }
        self.apply_zoom(window, cx);
    }

    fn apply_zoom(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.set_rem_size(px(chrome::ui_rem(self.ui_zoom)));
        chrome::scale_ui_fonts(self.ui_zoom, cx);
        let editor_px =
            chrome::content_font_px(self.editor_font_base, self.content_zoom, self.ui_zoom);
        let term_px =
            chrome::content_font_px(self.terminal_font_base, self.content_zoom, self.ui_zoom);
        let (cell_w, cell_h) = chrome::terminal_cell(term_px);
        for panel in self.terminals.values() {
            panel.update(cx, |panel, cx| {
                panel.set_metrics(cell_w, cell_h, term_px, cx);
            });
        }
        for panel in self.editors.values() {
            panel.update(cx, |panel, cx| panel.set_font_px(editor_px, cx));
        }
        window.refresh();
    }

    fn ui_px(&self, base: f32) -> Pixels {
        px(base * self.ui_zoom)
    }

    fn add_terminal_tab(&mut self, pty_id: String, window: &mut Window, cx: &mut Context<Self>) {
        let number = self.next_terminal_number;
        self.next_terminal_number = self.next_terminal_number.saturating_add(1);
        let workspace = cx.weak_entity();
        let ade = self.ade.clone();
        let metrics = self.tab_metrics.clone();
        let panel =
            cx.new(|cx| TerminalPanel::new(pty_id.clone(), number, ade, workspace, metrics, cx));
        let workspace_id = self.active_workspace_id.clone();
        panel.update(cx, |panel, _| panel.bind_workspace(workspace_id));
        let panel_id = PanelId::from(panel.entity_id());
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
        self.place_in_pending_tab_group(panel_id, window, cx);
        self.apply_zoom(window, cx);
    }

    fn on_pty_data(
        &mut self,
        pty_id: &str,
        bytes: &[u8],
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(panel) = self.terminals.get(pty_id).cloned() else {
            return;
        };
        let previous_cwd = panel.read(cx).cwd();
        let cwd = panel.update(cx, |panel, cx| panel.push_bytes(bytes, window, cx));
        if let Some(cwd) = cwd {
            self.last_cwd = Some(cwd.clone());
            if previous_cwd.as_deref() != Some(cwd.as_str())
                && matches!(&self.active, Some(ActiveSurface::Terminal(id)) if id == pty_id)
            {
                self.follow_directory(&cwd, cx);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
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
        let unsaved = path.is_empty();
        let path = if unsaved {
            untitled_editor_key(&buffer_id)
        } else {
            path
        };
        let unsaved_title = unsaved.then(|| self.untitled_title(cx));
        let workspace = cx.weak_entity();
        let ade = self.ade.clone();
        let panel = cx.new(|cx| {
            EditorPanel::new(
                buffer_id,
                path.clone(),
                language,
                line,
                column,
                unsaved,
                unsaved_title,
                ade,
                workspace,
                self.tab_metrics.clone(),
                window,
                cx,
            )
        });
        let panel_id = PanelId::from(panel.entity_id());
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
        self.place_in_pending_tab_group(panel_id, window, cx);
        self.apply_zoom(window, cx);
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
                self.active = Some(ActiveSurface::Editor(path.clone()));
            }
            if let Some(parent) = parent_dir(&path) {
                self.relist(&parent);
                self.selection = vec![path.clone()];
                self.anchor = Some(path.clone());
            }
        }
        self.status = "Saved".into();
    }

    fn untitled_title(&self, cx: &App) -> String {
        let n = self
            .editors
            .values()
            .filter(|panel| panel.read(cx).is_unsaved())
            .count();
        if n == 0 {
            "Untitled".to_string()
        } else {
            format!("Untitled {}", n + 1)
        }
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
            Some(ActiveSurface::Diff(rel)) => self
                .diffs
                .get(rel)
                .map(|panel| PanelId::from(panel.entity_id())),
            Some(ActiveSurface::Binary(path)) => self
                .binaries
                .get(path)
                .map(|panel| PanelId::from(panel.entity_id())),
            None => None,
        }
    }

    fn is_saved_tab(&self, id: PanelId) -> bool {
        self.terminals
            .values()
            .any(|panel| PanelId::from(panel.entity_id()) == id)
            || self.editors.iter().any(|(path, panel)| {
                Some(path) != self.defaults_path.as_ref()
                    && !is_untitled_editor_key(path)
                    && PanelId::from(panel.entity_id()) == id
            })
    }

    pub(crate) fn note_terminal_active(
        &mut self,
        pty_id: &str,
        active: bool,
        cx: &mut Context<Self>,
    ) {
        if active {
            self.active = Some(ActiveSurface::Terminal(pty_id.to_string()));
            if let Some(cwd) = self.terminals.get(pty_id).and_then(|panel| panel.read(cx).cwd()) {
                self.follow_directory(&cwd, cx);
            }
            if let Some(id) = self.active_panel_id() {
                self.last_saved_panel = Some(id);
            }
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
            if self.defaults_path.as_deref() != Some(path)
                && let Some(dir) = parent_dir(path)
            {
                self.follow_directory(&dir, cx);
            }
            if let Some(id) = self.active_panel_id() {
                self.last_saved_panel = Some(id);
            }
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
        if self.defaults_path.as_deref() == Some(path) {
            self.ade.send(AdeCmd::DeletePaths {
                request_id: next_id("defaults-delete"),
                paths: vec![path.to_owned()],
            });
        }
        if matches!(&self.active, Some(ActiveSurface::Editor(current)) if current == path) {
            self.active = None;
        }
        if !self.restoring {
            self.publish_layout(cx);
        }
        cx.notify();
    }

    fn rebuild_tree(&mut self, cx: &mut Context<Self>) {
        prune_expanded(&mut self.expanded_dirs, &self.explorer_cache);
        self.explorer_kinds = Rc::new(entry_kinds(&self.explorer_cache));
        let root = self.explorer_root.clone();
        if root.is_empty() {
            return;
        }
        let filter = if self.filter_open {
            self.filter_input.read(cx).value().to_string()
        } else {
            String::new()
        };
        let items = build_explorer_tree(&root, &self.explorer_cache, &self.expanded_dirs, &filter);
        self.explorer.update(cx, |state, cx| {
            state.set_items(items, cx);
            if let Some(ix) = self.restore_scroll_index {
                state.scroll_to_item(ix, ScrollStrategy::Top);
            }
        });
        self.sync_tree_highlight(None, cx);
    }

    
    fn set_pending_tab_group(&mut self, source: PanelId) {
        self.pending_tab_group = Some(source);
    }

    /// Open a terminal and place it in the tab group that owns `source`.
    pub(crate) fn new_terminal_in_group(&mut self, source: PanelId, cx: &App) {
        self.set_pending_tab_group(source);
        self.new_terminal(cx);
    }

    /// Open a new file and place it in the tab group that owns `source`.
    pub(crate) fn new_file_in_group(&mut self, source: PanelId, cx: &mut Context<Self>) {
        self.set_pending_tab_group(source);
        self.new_file(cx);
    }

    fn place_in_pending_tab_group(
        &mut self,
        panel_id: PanelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(source) = self.pending_tab_group.take() else {
            return;
        };
        let node = self
            .dock
            .read(cx)
            .layout(DockPlacement::Center)
            .and_then(|tree| tree.find_panel_node(source));
        let Some(node) = node else {
            return;
        };
        self.dock.update(cx, |dock, cx| {
            dock.move_panel(
                panel_id,
                InsertTarget::Tabs {
                    node,
                    ix: None,
                    activate: true,
                },
                window,
                cx,
            );
        });
    }

pub(crate) fn new_terminal(&mut self, cx: &App) {
        if !matches!(self.connection, ConnectionState::Online) {
            self.status = "Not connected".into();
            return;
        }
        let cwd = self.shell_cwd(cx);
        self.pty_opens_pending = self.pty_opens_pending.saturating_add(1);
        self.ade.send(AdeCmd::OpenPty {
            cols: 80,
            rows: 24,
            cwd,
        });
    }

    /// Open an empty unsaved buffer. Ctrl+S or File → Save writes it later.
    pub(crate) fn new_file(&mut self, cx: &mut Context<Self>) {
        if !matches!(self.connection, ConnectionState::Online) {
            self.status = "Not connected".into();
            cx.notify();
            return;
        }
        let request_id = next_id("new");
        self.pending_editors.insert(request_id.clone(), true);
        self.ade.send(AdeCmd::NewBuffer { request_id });
        self.status = "New file".into();
        cx.notify();
    }

    fn create_parent(&self) -> Option<String> {
        let root = self.explorer_root.clone();
        if root.is_empty() {
            return None;
        }
        let Some(path) = self.selection.last() else {
            return Some(root);
        };
        if self.is_dir(path) {
            Some(path.clone())
        } else {
            parent_dir(path).or(Some(root))
        }
    }

    /// OSC 7 cwd of the focused shell, otherwise the workspace or session root.
    ///
    /// A missing cwd used to leave the PTY in the daemon's process directory,
    /// which for an SSH session is `$HOME` even when the remote root is set.
    fn shell_cwd(&self, cx: &App) -> Option<String> {
        let from_active = match &self.active {
            Some(ActiveSurface::Terminal(id)) => self
                .terminals
                .get(id)
                .and_then(|panel| panel.read(cx).cwd()),
            Some(ActiveSurface::Editor(path)) => parent_dir(path),
            _ => None,
        };
        let selected_dir = self.selection.last().and_then(|path| {
            if self.is_dir(path) {
                Some(path.clone())
            } else {
                parent_dir(path)
            }
        });
        let explicit = selected_dir.or(from_active).or(self.last_cwd.clone());
        choose_shell_cwd(explicit.as_deref(), self.workspace_root().as_deref())
    }

    /// Focused workspace directory, else the explorer folder, else the root
    /// this window was opened with (saved remote root or `--root`).
    fn workspace_root(&self) -> Option<String> {
        let from_workspace = self.active_workspace_id.as_ref().and_then(|id| {
            self.workspaces
                .iter()
                .find(|workspace| &workspace.id == id)
                .map(|workspace| workspace.root.clone())
        });
        let explorer = if self.explorer_root.trim().is_empty() {
            None
        } else {
            Some(self.explorer_root.clone())
        };
        choose_shell_cwd(
            from_workspace.as_deref(),
            explorer
                .as_deref()
                .or(self.target.preferred_root.as_deref()),
        )
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
        let diffs: Vec<_> = self.diffs.values().cloned().collect();
        let binaries: Vec<_> = self.binaries.values().cloned().collect();
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
        for panel in diffs {
            panel.update(cx, |panel, _| panel.release());
            self.dock
                .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
        }
        for panel in binaries {
            panel.update(cx, |panel, _| panel.release());
            self.dock
                .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
        }
        self.terminals.clear();
        self.editors.clear();
        self.diffs.clear();
        self.binaries.clear();
        self.diff_preview = None;
        self.pending_diffs.clear();
        self.active = None;
        self.last_saved_panel = None;
        self.next_terminal_number = 1;
        // A shell cwd from the workspace we just left must not follow the next one.
        self.last_cwd = None;
        self.git_context_dir.clear();
        self.clear_git_view();
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
            } else if let Some((path, _)) = self.editors.iter().find(|(path, panel)| {
                Some(*path) != self.defaults_path.as_ref()
                    && !is_untitled_editor_key(path)
                    && PanelId::from(panel.entity_id()) == id
            }) {
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
            if Some(path) == self.defaults_path.as_ref() || is_untitled_editor_key(path) {
                continue;
            }
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
        let active_id = match &self.active {
            Some(ActiveSurface::Diff(_)) | Some(ActiveSurface::Binary(_)) => self.last_saved_panel,
            _ => self.active_panel_id(),
        };
        let active_tab = active_id
            .and_then(|id| {
                self.panel_order(cx)
                    .iter()
                    .filter(|item| self.is_saved_tab(**item))
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
        let extra = self.capture_extra(&tabs, cx);
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
            explorer_expanded: self.expanded_list(),
            extra,
        });
    }

    fn capture_extra(&self, tabs: &[WorkspaceTab], cx: &App) -> WorkspaceLayoutExtra {
        let ids: HashMap<PanelId, u32> = tabs.iter().enumerate().filter_map(|(ix, tab)| {
            let panel_id = match tab.kind {
                WorkspaceTabKind::Terminal => self.terminals.get(tab.pty_id.as_ref()?)?.entity_id(),
                WorkspaceTabKind::Editor => self.editors.get(tab.path.as_ref()?)?.entity_id(),
            };
            Some((PanelId::from(panel_id), ix as u32))
        }).collect();
        let center = self.dock.read(cx).layout(DockPlacement::Center)
            .and_then(|tree| capture_center(tree.root(), &ids));
        let mut pinned: Vec<_> = self.pinned_tabs.iter().cloned().collect();
        pinned.sort();
        WorkspaceLayoutExtra {
            explorer_scroll: self.restore_scroll_index.map(|ix| ix as u32).unwrap_or_else(||
                self.explorer.read(cx).scroll_handle().0.borrow().base_handle.logical_scroll_top().0 as u32),
            sidebar_collapsed: self.sidebar_collapsed,
            pinned,
            center,
        }
    }

    fn expanded_list(&self) -> Vec<String> {
        let mut dirs: Vec<String> = self.expanded_dirs.iter().cloned().collect();
        dirs.sort();
        dirs
    }

    /// Send the current layout and wait (briefly) for it to reach the daemon.
    /// Window close can end the process before the ADE thread runs.
    fn save_before_exit(&mut self, cx: &App) {
        if !matches!(self.connection, ConnectionState::Online) {
            return;
        }
        self.publish_layout(cx);
        self.ade
            .flush_blocking(std::time::Duration::from_millis(500));
    }

    fn finish_restore_if_idle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.restoring && self.pending_editors.is_empty() {
            self.finish_restore(window, cx);
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
        let extra = self.capture_extra(&tabs, cx);
        let explorer_expanded = self.expanded_list();
        self.restoring = true;
        self.pty_opens_pending = 0;
        self.pending_editors.clear();
        self.renaming_id = None;
        self.relocating_id = None;
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
            explorer_expanded,
            extra,
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
            explorer_expanded,
            extra,
        } = attached;
        self.sidebar_collapsed = extra.sidebar_collapsed;
        self.restore_scroll_index = Some(extra.explorer_scroll as usize);
        self.pinned_tabs = extra.pinned.iter().cloned().collect();
        self.restore_extra = Some(extra);
        self.restore_tabs = tabs.clone();
        self.restoring = true;
        self.pty_opens_pending = 0;
        self.pending_editors.clear();
        self.respawn_titles.clear();
        self.restore_focus = None;
        self.release_dock(window, cx);
        self.session_id = Some(info.session_id.clone());
        self.active_workspace_id = Some(info.id.clone());
        self.explorer_cache.clear();
        self.filter_open = false;
        self.renaming_path = None;
        self.pending_renames.clear();
        self.pending_creates.clear();
        self.pending_lists.clear();
        self.expanded_dirs = explorer_expanded.into_iter().collect();
        self.selection.clear();
        self.anchor = None;
        self.explorer_root = info.root.clone();
        self.git_context_dir.clear();
        let root = info.root.clone();
        self.upsert_workspace(info);
        self.rebuild_tree(cx);
        self.list_dir(&root);
        // Folders open last time need their listings, or they rebuild as
        // open rows with only a placeholder child.
        let mut reopen: Vec<String> = self.expanded_dirs.iter().cloned().collect();
        reopen.sort();
        for dir in reopen {
            self.list_dir(&dir);
        }

        let live: Vec<String> = ptys.into_iter().map(|PtyInfo { id, .. }| id).collect();
        let plan = restore_plan(tabs, active_tab, &live);
        let mut editor_jobs = Vec::new();
        for step in plan.steps {
            match step {
                RestoreStep::Attach { pty_id, title } => {
                    self.attach_terminal(pty_id, title, window, cx);
                }
                RestoreStep::Respawn { title } => {
                    self.respawn_titles.push_back(title);
                }
                RestoreStep::Editor { path, activate } => editor_jobs.push((path, activate)),
            }
        }
        for id in plan.orphans {
            let n = self.next_terminal_number;
            self.attach_terminal(id, n.to_string(), window, cx);
        }
        for _ in 0..self.respawn_titles.len() {
            self.pty_opens_pending = self.pty_opens_pending.saturating_add(1);
            self.ade.send(AdeCmd::OpenPty {
                cols: 80,
                rows: 24,
                cwd: Some(root.clone()),
            });
        }

        let expect_editors = !editor_jobs.is_empty();
        if self.terminals.is_empty() && !expect_editors && self.respawn_titles.is_empty() {
            self.restoring = false;
            self.new_terminal(cx);
        }
        self.restore_focus = plan.focus_pty;
        for (path, activate) in editor_jobs {
            self.open_editor(path, false, activate);
        }
        if !expect_editors {
            self.finish_restore(window, cx);
        }
        self.refresh_git();
    }

    /// Select the saved active terminal (editors reopened by the restore may
    /// have taken the selection) and publish the rebuilt tab list.
    fn finish_restore(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.restoring = false;
        self.apply_restored_center(window, cx);
        if let Some(pty_id) = self.restore_focus.take()
            && let Some(panel) = self.terminals.get(&pty_id).cloned()
        {
            self.select_entity(&panel, window, cx);
        }
        if !self.panes_empty() {
            self.publish_layout(cx);
        }
    }

    fn apply_restored_center(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(center) = self.restore_extra.as_ref().and_then(|extra| extra.center.as_ref()).cloned() else { return; };
        // Never displace an orphan or a still-opening tab while rebuilding the split.
        if self.panel_order(cx).len() != self.restore_tabs.len() { return; }
        let mut panels: Vec<Arc<dyn BasePanelView>> = Vec::new();
        for tab in &self.restore_tabs {
            match tab.kind {
                WorkspaceTabKind::Terminal => {
                    let panel = tab.pty_id.as_ref().and_then(|id| self.terminals.get(id))
                        .or_else(|| self.terminals.values().find(|panel| panel.read(cx).label() == tab.title));
                    let Some(panel) = panel else { return; };
                    panels.push(panel_handle(panel.clone()));
                }
                WorkspaceTabKind::Editor => {
                    let Some(panel) = tab.path.as_ref().and_then(|path| self.editors.get(path)) else { return; };
                    panels.push(panel_handle(panel.clone()));
                }
            }
        }
        if panels.iter().map(|panel| panel.panel_id(cx)).collect::<HashSet<_>>().len() != panels.len() { return; }
        if !complete_center(&center, panels.len()) { return; }
        let Some(layout) = restore_center(&center, &panels, cx) else { return; };
        self.dock.update(cx, |dock, cx| dock.set_center(layout, window, cx));
        self.restore_extra = None;
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
        let metrics = self.tab_metrics.clone();
        let panel =
            cx.new(|cx| TerminalPanel::new(pty_id.clone(), number, ade, workspace, metrics, cx));
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
        self.apply_zoom(window, cx);
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
            self.status = "This daemon has no workspace list. Upgrade and restart the remote fresh-gui to add projects.".into();
            return;
        }
        self.create_open = true;
        self.palette_open = false;
        self.goto_open = false;
        self.rename_pty = None;
        self.renaming_id = None;
        self.relocating_id = None;
        let root = self.default_new_workspace_root();
        self.create_name.update(cx, |state, cx| {
            state.set_value("", window, cx);
            state.focus(window, cx);
        });
        self.create_root
            .update(cx, |state, cx| state.set_value(root, window, cx));
    }

    /// Folder the create panel starts in: the open workspace, or the explorer
    /// root when that workspace has no root of its own. Empty only when neither
    /// exists yet (the daemon default).
    fn default_new_workspace_root(&self) -> String {
        let from_workspace = self
            .workspaces
            .iter()
            .find(|workspace| Some(&workspace.id) == self.active_workspace_id.as_ref())
            .map(|workspace| display_path(&workspace.root))
            .filter(|root| !root.is_empty());
        if let Some(root) = from_workspace {
            return root;
        }
        let explorer = display_path(&self.explorer_root);
        if explorer.is_empty() {
            String::new()
        } else {
            explorer
        }
    }

    fn confirm_create(&mut self, cx: &mut Context<Self>) {
        let name = self.create_name.read(cx).value().to_string();
        let raw_root = self.create_root.read(cx).value().to_string();
        let unix = daemon_uses_unix_paths(
            self.config_path.as_deref(),
            &self
                .workspaces
                .iter()
                .map(|ws| ws.root.as_str())
                .collect::<Vec<_>>(),
        );
        let root = match workspace_root_for_daemon(&raw_root, unix) {
            Ok(root) => root,
            Err(message) => {
                self.status = message.into();
                return;
            }
        };
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
        self.relocating_id = None;
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

    fn begin_workspace_root_for(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if !self
            .capabilities
            .iter()
            .any(|cap| cap == CAP_WORKSPACE_SET_ROOT)
        {
            self.status = "Upgrade the remote fresh-gui to change workspace location".into();
            return;
        }
        let Some(root) = self
            .workspaces
            .iter()
            .find(|ws| ws.id == id)
            .map(|ws| display_path(&ws.root))
        else {
            return;
        };
        self.relocating_id = Some(id.to_string());
        self.renaming_id = None;
        self.create_open = false;
        self.ws_root_input.update(cx, |state, cx| {
            state.set_value(root, window, cx);
            state.focus(window, cx);
        });
    }

    fn confirm_workspace_root(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.relocating_id.clone() else {
            return;
        };
        let root = self.ws_root_input.read(cx).value().to_string();
        let unix = daemon_uses_unix_paths(
            self.config_path.as_deref(),
            &self
                .workspaces
                .iter()
                .map(|ws| ws.root.as_str())
                .collect::<Vec<_>>(),
        );
        let root = match workspace_root_for_daemon(&root, unix) {
            Ok(root) if !root.is_empty() => root,
            Ok(_) => {
                self.status = "Workspace location is empty".into();
                return;
            }
            Err(message) => {
                self.status = message.into();
                return;
            }
        };
        self.relocating_id = None;
        self.ade.send(AdeCmd::SetWorkspaceRoot { id, root });
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
        let Some(id) = self.active_panel_id() else {
            return;
        };
        self.close_panel_id(id, window, cx);
    }

    fn panel_key(&self, id: PanelId) -> Option<String> {
        if let Some((pty, _)) = self.terminals.iter().find(|(_, panel)| PanelId::from(panel.entity_id()) == id) {
            return Some(format!("pty:{pty}"));
        }
        self.editors.iter().find(|(_, panel)| PanelId::from(panel.entity_id()) == id)
            .map(|(path, _)| format!("file:{path}"))
    }

    pub(crate) fn tab_pin_state(&self, id: PanelId) -> Option<bool> {
        self.panel_key(id).map(|key| self.pinned_tabs.contains(&key))
    }

    pub(crate) fn toggle_pin(&mut self, id: PanelId, cx: &mut Context<Self>) {
        let Some(key) = self.panel_key(id) else { return; };
        if !self.pinned_tabs.remove(&key) { self.pinned_tabs.insert(key); }
        self.publish_layout(cx);
        cx.notify();
    }

    pub(crate) fn tab_close_availability(&self, panel: PanelId, cx: &App) -> (bool, bool) {
        let order = self.panel_order(cx);
        let ix = order.iter().position(|id| *id == panel);
        let others = order.len() > 1 && ix.is_some();
        let right = ix.is_some_and(|index| index + 1 < order.len());
        (others, right)
    }

    pub(crate) fn close_panel_id(
        &mut self,
        id: PanelId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_dock_ids(&[id], window, cx);
    }

    pub(crate) fn close_panel_scope(
        &mut self,
        id: PanelId,
        scope: TabCloseScope,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let order = self.panel_order(cx);
        let ids = panels_for_close_scope(&order, &id, scope);
        self.remove_dock_ids(&ids, window, cx);
    }

    pub(crate) fn close_all_editors(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<_> = self.editors.values().map(|panel| PanelId::from(panel.entity_id()))
            .chain(self.diffs.values().map(|panel| PanelId::from(panel.entity_id())))
            .chain(self.binaries.values().map(|panel| PanelId::from(panel.entity_id())))
            .collect();
        self.remove_dock_ids(&ids, window, cx);
    }

    pub(crate) fn close_all_terminals(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<_> = self.terminals.values().map(|panel| PanelId::from(panel.entity_id())).collect();
        self.remove_dock_ids(&ids, window, cx);
    }

    pub(crate) fn close_all_other_terminals(&mut self, keep: Option<PanelId>, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<_> = self.terminals.values().map(|panel| PanelId::from(panel.entity_id()))
            .filter(|id| Some(*id) != keep).collect();
        self.remove_dock_ids(&ids, window, cx);
    }

    pub(crate) fn close_all_other_tabs(&mut self, keep: Option<PanelId>, window: &mut Window, cx: &mut Context<Self>) {
        let ids: Vec<_> = self.panel_order(cx).into_iter().filter(|id| Some(*id) != keep).collect();
        self.remove_dock_ids(&ids, window, cx);
    }

    fn remove_dock_ids(&mut self, ids: &[PanelId], window: &mut Window, cx: &mut Context<Self>) {
        let mut terminals = Vec::new();
        let mut editors = Vec::new();
        let mut diffs = Vec::new();
        let mut binaries = Vec::new();
        for id in ids {
            if let Some(panel) = self
                .terminals
                .values()
                .find(|panel| PanelId::from(panel.entity_id()) == *id)
                .cloned()
            {
                terminals.push(panel);
            } else if let Some(panel) = self
                .editors
                .values()
                .find(|panel| PanelId::from(panel.entity_id()) == *id)
                .cloned()
            {
                editors.push(panel);
            } else if let Some(panel) = self
                .diffs
                .values()
                .find(|panel| PanelId::from(panel.entity_id()) == *id)
                .cloned()
            {
                diffs.push(panel);
            } else if let Some(panel) = self
                .binaries
                .values()
                .find(|panel| PanelId::from(panel.entity_id()) == *id)
                .cloned()
            {
                binaries.push(panel);
            }
        }
        for panel in terminals {
            self.dock
                .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
        }
        for panel in editors {
            self.dock
                .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
        }
        for panel in diffs {
            self.dock
                .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
        }
        for panel in binaries {
            self.dock
                .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
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

    fn save_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ActiveSurface::Editor(path)) = &self.active else {
            return;
        };
        let Some(panel) = self.editors.get(path).cloned() else {
            return;
        };
        panel.update(cx, |panel, cx| panel.commit_markdown_inline_edit(window, cx));
        let (buffer_id, rev, text, dirty, unsaved) = {
            let panel = panel.read(cx);
            (
                panel.buffer_id().to_string(),
                panel.rev(),
                panel.current_text(cx),
                panel.is_dirty(),
                panel.is_unsaved(),
            )
        };
        if unsaved {
            self.open_save_dialog(window, cx);
            return;
        }
        if !dirty {
            self.status = "No changes".into();
            cx.notify();
            return;
        }
        self.write_buffer(&buffer_id, rev, &text, dirty, String::new());
        self.status = "Saving…".into();
        cx.notify();
    }

    fn open_save_dialog(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let parent = self.create_parent().unwrap_or_default();
        let existing = self
            .explorer_cache
            .get(&parent)
            .map(|entries| {
                entries
                    .iter()
                    .map(|entry| entry.name.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let suggestion = save_target_path(&parent, &unused_file_name(&existing));
        self.save_open = true;
        self.palette_open = false;
        self.goto_open = false;
        self.rename_pty = None;
        self.renaming_id = None;
        self.save_path_input.update(cx, |state, cx| {
            state.set_value(suggestion, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn confirm_save(&mut self, cx: &mut Context<Self>) {
        let raw = self.save_path_input.read(cx).value().to_string();
        let raw = raw.trim().to_string();
        if raw.is_empty() {
            self.status = "Path cannot be empty".into();
            cx.notify();
            return;
        }
        let unix = daemon_uses_unix_paths(
            self.config_path.as_deref(),
            &self
                .workspaces
                .iter()
                .map(|workspace| workspace.root.as_str())
                .collect::<Vec<_>>(),
        );
        let normalized = match workspace_root_for_daemon(&raw, unix) {
            Ok(path) => path,
            Err(message) => {
                self.status = message.into();
                cx.notify();
                return;
            }
        };
        let parent = self.create_parent().unwrap_or_default();
        let path = save_target_path(&parent, &normalized);
        let Some(ActiveSurface::Editor(key)) = &self.active else {
            self.save_open = false;
            cx.notify();
            return;
        };
        let Some(panel) = self.editors.get(key).cloned() else {
            self.save_open = false;
            cx.notify();
            return;
        };
        let (buffer_id, rev, text, dirty) = {
            let panel = panel.read(cx);
            (
                panel.buffer_id().to_string(),
                panel.rev(),
                panel.current_text(cx),
                panel.is_dirty(),
            )
        };
        self.save_open = false;
        self.write_buffer(&buffer_id, rev, &text, dirty, path);
        self.status = "Saving…".into();
        cx.notify();
    }

    fn write_buffer(&mut self, buffer_id: &str, rev: u64, text: &str, dirty: bool, path: String) {
        let base_rev = if dirty {
            self.ade.send(AdeCmd::EditBuffer {
                request_id: next_id("ed"),
                buffer_id: buffer_id.to_string(),
                base_rev: rev,
                text: text.to_string(),
            });
            rev + 1
        } else {
            rev
        };
        self.ade.send(AdeCmd::SaveBuffer {
            request_id: next_id("sv"),
            buffer_id: buffer_id.to_string(),
            base_rev,
            path,
        });
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

    fn open_default_settings(&mut self) {
        if let Some(path) = self.defaults_path.clone() {
            self.open_path(path, false);
        } else {
            self.status = "Backend did not send defaults_path".into();
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
        if matches!(self.connection, ConnectionState::Online) {
            self.publish_layout(cx);
        }
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
        self.pending_split = None;
        self.pending_tab_group = None;
        self.pending_editors.clear();
        self.restoring = false;
        self.renaming_id = None;
        self.relocating_id = None;
        self.rail_hover = None;
        self.create_open = false;
        self.explorer_cache.clear();
        self.filter_open = false;
        self.renaming_path = None;
        self.pending_renames.clear();
        self.pending_creates.clear();
        self.expanded_dirs.clear();
        self.explorer_kinds = Rc::default();
        self.pending_lists.clear();
        self.respawn_titles.clear();
        self.restore_focus = None;
        self.explorer_root.clear();
        self.git_context_dir.clear();
        self.selection.clear();
        self.anchor = None;
        self.git_status_req = None;
        self.git_busy = false;
        self._recv_task = cx.spawn_in(window, async move |this, cx| {
            let mut pending = None;
            let mut handled = 0;
            loop {
                let ev = match pending.take() {
                    Some(ev) => ev,
                    None => match evt_rx.recv().await { Ok(ev) => ev, Err(_) => break },
                };
                let ev = coalesce_pty_event(ev, &evt_rx, &mut pending);
                if cx
                    .update(|window, app| {
                        this.update(app, |this, cx| this.handle_event(ev, window, cx))
                    })
                    .is_err()
                {
                    break;
                }
                handled += 1;
                if handled == 8 {
                    handled = 0;
                    if pending.is_some() || !evt_rx.is_empty() {
                        cx.background_executor().timer(Duration::from_millis(1)).await;
                    }
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

    fn clear_git_view(&mut self) {
        self.git_repo = false;
        self.git_root.clear();
        self.git_branch.clear();
        self.git_upstream = None;
        self.git_ahead = 0;
        self.git_behind = 0;
        self.git_files.clear();
        self.git_collapsed.clear();
        self.git_detail = None;
        self.git_paths.clear();
    }

    fn workspace_id_or_empty(&self) -> String {
        self.active_workspace_id.clone().unwrap_or_default()
    }

    fn refresh_git(&mut self) {
        if !self.git_cap || !matches!(self.connection, ConnectionState::Online) {
            return;
        }
        let request_id = next_id("git");
        self.git_status_req = Some(request_id.clone());
        self.ade.send(AdeCmd::GitStatus {
            request_id,
            workspace_id: self.workspace_id_or_empty(),
            directory: self.git_context_dir.clone(),
        });
    }

    /// Change explorer and Git context only when the focused directory changes.
    /// Repeated OSC 7 reports during TUI frames then cost no FS or Git request.
    fn follow_directory(&mut self, directory: &str, cx: &mut Context<Self>) {
        let directory = if directory == "/"
            || directory.ends_with(":/")
            || directory.ends_with(":\\")
        {
            directory
        } else {
            directory.trim_end_matches(['/', '\\'])
        };
        let directory = if directory.is_empty() { "/" } else { directory };
        if !std::path::Path::new(directory).is_absolute() {
            return;
        }
        let mut changed = false;
        if self.explorer_root != directory {
            changed = true;
            self.explorer_root = directory.to_string();
            self.explorer_cache.clear();
            self.pending_lists.clear();
            self.expanded_dirs.clear();
            self.ade.send(AdeCmd::AuthorizeDir {
                request_id: next_id("auth"),
                path: directory.to_string(),
            });
            self.list_dir(directory);
            self.rebuild_tree(cx);
        }
        if self.git_context_dir != directory {
            changed = true;
            self.git_context_dir = directory.to_string();
            self.refresh_git();
        }
        if changed {
            cx.notify();
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn apply_git_status(
        &mut self,
        repo: bool,
        root: String,
        branch: String,
        upstream: Option<String>,
        ahead: u32,
        behind: u32,
        files: Vec<GitFile>,
        detail: Option<String>,
    ) {
        self.git_repo = repo;
        self.git_root = root;
        self.git_branch = branch;
        self.git_upstream = upstream;
        self.git_ahead = ahead;
        self.git_behind = behind;
        self.git_detail = detail;
        self.git_paths.clear();
        let root = if self.git_root.is_empty() {
            self.explorer_root.clone()
        } else {
            self.git_root.clone()
        };
        for file in &files {
            self.git_paths
                .insert(git_lookup_key(&file.path), file.path.clone());
            let abs = diff_view::join_repo(&root, &file.path);
            self.git_paths
                .insert(git_lookup_key(&abs), file.path.clone());
            self.git_paths
                .insert(git_lookup_key(&display_path(&abs)), file.path.clone());
        }
        self.git_files = files;
        let present = git_dir_paths(&self.git_files);
        self.git_collapsed.retain(|dir| present.contains(dir));
    }

    fn git_rel_for(&self, path: &str) -> Option<String> {
        if let Some(rel) = self.git_paths.get(&git_lookup_key(path)) {
            return Some(rel.clone());
        }
        let root = if self.git_root.is_empty() {
            self.explorer_root.as_str()
        } else {
            self.git_root.as_str()
        };
        let rel = diff_view::git_relative(root, path)?;
        self.git_paths.get(&git_lookup_key(&rel)).cloned()
    }

    fn apply_tree_click(
        &mut self,
        path: &str,
        is_folder: bool,
        gesture: super::explorer::SelectGesture,
        click_count: usize,
        cx: &mut Context<Self>,
    ) -> Option<(String, bool)> {
        if is_placeholder(path) {
            return None;
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
        if !is_folder
            && gesture == super::explorer::SelectGesture::Replace
            && let Some(dir) = parent_dir(path)
        {
            self.follow_directory(&dir, cx);
        }
        let highlight =
            (gesture == super::explorer::SelectGesture::Range).then(|| path.to_string());
        self.sync_tree_highlight(highlight, cx);
        if is_folder {
            if !self.explorer_cache.contains_key(path) {
                self.list_dir(path);
            }
        } else if gesture == super::explorer::SelectGesture::Replace {
            if let Some(rel) = self.git_rel_for(path) {
                return Some((rel, click_count >= 2));
            }
            self.open_path(path.to_string(), true);
        }
        None
    }

    fn open_diff(&mut self, rel: String, pin: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.diffs.get(&rel).cloned() {
            if pin {
                panel.update(cx, |panel, cx| panel.pin(cx));
                if self.diff_preview.as_deref() == Some(rel.as_str()) {
                    self.diff_preview = None;
                }
            }
            self.select_entity(&panel, window, cx);
            self.request_git_diff(&rel);
            return;
        }
        if !pin
            && let Some(prev) = self.diff_preview.clone()
            && prev != rel
        {
            self.close_diff(&prev, window, cx);
        }
        let root = if self.git_root.is_empty() {
            self.explorer_root.clone()
        } else {
            self.git_root.clone()
        };
        let title = diff_view::join_repo(&root, &rel);
        let workspace = cx.weak_entity();
        let metrics = self.tab_metrics.clone();
        let panel = cx.new(|cx| DiffPanel::new(rel.clone(), title, pin, workspace, metrics, cx));
        self.dock.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            );
        });
        self.diffs.insert(rel.clone(), panel.clone());
        if !pin {
            self.diff_preview = Some(rel.clone());
        }
        self.select_entity(&panel, window, cx);
        self.request_git_diff(&rel);
    }

    fn close_open_diffs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.pending_diffs.clear();
        let rels: Vec<String> = self.diffs.keys().cloned().collect();
        for rel in rels {
            self.close_diff(&rel, window, cx);
        }
    }

    fn close_diff(&mut self, rel: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(panel) = self.diffs.get(rel).cloned() else {
            return;
        };
        panel.update(cx, |panel, _| panel.release());
        self.diffs.remove(rel);
        if self.diff_preview.as_deref() == Some(rel) {
            self.diff_preview = None;
        }
        self.dock
            .update(cx, |dock, cx| dock.remove_panel(panel, window, cx));
    }

    fn request_git_diff(&mut self, rel: &str) {
        if !self.git_cap {
            return;
        }
        let request_id = next_id("git");
        self.pending_diffs
            .insert(request_id.clone(), rel.to_string());
        self.ade.send(AdeCmd::GitDiff {
            request_id,
            workspace_id: self.workspace_id_or_empty(),
            directory: self.git_context_dir.clone(),
            path: rel.to_string(),
        });
    }

    pub(crate) fn note_diff_active(&mut self, rel: &str, active: bool, cx: &mut Context<Self>) {
        if active {
            self.active = Some(ActiveSurface::Diff(rel.to_string()));
        } else if matches!(&self.active, Some(ActiveSurface::Diff(current)) if current == rel) {
            self.active = None;
        }
        cx.notify();
    }

    pub(crate) fn forget_diff(&mut self, rel: &str, cx: &mut Context<Self>) {
        self.diffs.remove(rel);
        if self.diff_preview.as_deref() == Some(rel) {
            self.diff_preview = None;
        }
        if matches!(&self.active, Some(ActiveSurface::Diff(current)) if current == rel) {
            self.active = None;
        }
        if !self.restoring {
            let this = cx.entity();
            cx.defer(move |cx| {
                this.update(cx, |this, cx| this.publish_layout(cx));
            });
        }
        cx.notify();
    }

    pub(crate) fn pin_diff(&mut self, rel: &str, cx: &mut Context<Self>) {
        if let Some(panel) = self.diffs.get(rel).cloned() {
            panel.update(cx, |panel, cx| panel.pin(cx));
        }
        if self.diff_preview.as_deref() == Some(rel) {
            self.diff_preview = None;
        }
        cx.notify();
    }

    fn open_binary(
        &mut self,
        path: String,
        activate: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(panel) = self.binaries.get(&path).cloned() {
            if activate {
                self.select_entity(&panel, window, cx);
            }
            return;
        }
        let workspace = cx.weak_entity();
        let metrics = self.tab_metrics.clone();
        let panel = cx.new(|cx| BinaryPanel::new(path.clone(), workspace, metrics, cx));
        self.dock.update(cx, |dock, cx| {
            dock.add_panel_view(
                panel_handle(panel.clone()),
                DockPlacement::Center,
                None,
                window,
                cx,
            );
        });
        self.binaries.insert(path, panel.clone());
        if activate {
            self.select_entity(&panel, window, cx);
        }
    }

    pub(crate) fn note_binary_active(&mut self, path: &str, active: bool, cx: &mut Context<Self>) {
        if active {
            self.active = Some(ActiveSurface::Binary(path.to_string()));
        } else if matches!(&self.active, Some(ActiveSurface::Binary(current)) if current == path) {
            self.active = None;
        }
        cx.notify();
    }

    pub(crate) fn forget_binary(&mut self, path: &str, cx: &mut Context<Self>) {
        self.binaries.remove(path);
        if matches!(&self.active, Some(ActiveSurface::Binary(current)) if current == path) {
            self.active = None;
        }
        cx.notify();
    }

    pub(crate) fn open_external(&mut self, path: String, cx: &mut Context<Self>) {
        self.ade.send(AdeCmd::OpenExternal {
            request_id: next_id("ext"),
            path,
        });
        self.status = "Opening externally…".into();
        cx.notify();
    }

    fn git_restore(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        if paths.is_empty() || !self.git_cap || self.git_busy {
            return;
        }
        self.git_busy = true;
        self.status = "Reverting…".into();
        self.ade.send(AdeCmd::GitRestore {
            request_id: next_id("git"),
            workspace_id: self.workspace_id_or_empty(),
            directory: self.git_context_dir.clone(),
            paths,
        });
        cx.notify();
    }

    fn git_stage(&mut self, paths: Vec<String>, stage: bool, cx: &mut Context<Self>) {
        if paths.is_empty() || !self.git_cap || self.git_busy {
            return;
        }
        self.git_busy = true;
        self.status = if stage {
            "Staging…".into()
        } else {
            "Unstaging…".into()
        };
        self.ade.send(AdeCmd::GitStage {
            request_id: next_id("git"),
            workspace_id: self.workspace_id_or_empty(),
            directory: self.git_context_dir.clone(),
            paths,
            stage,
        });
        cx.notify();
    }

    fn git_commit(&mut self, cx: &mut Context<Self>) {
        if !self.git_cap || self.git_busy || !self.git_repo {
            return;
        }
        let message = self.commit_input.read(cx).value().to_string();
        if message.trim().is_empty() {
            self.status = "Commit message is empty".into();
            cx.notify();
            return;
        }
        self.git_busy = true;
        self.status = "Committing…".into();
        self.ade.send(AdeCmd::GitCommit {
            request_id: next_id("git"),
            workspace_id: self.workspace_id_or_empty(),
            directory: self.git_context_dir.clone(),
            message,
        });
        cx.notify();
    }

    fn git_pull(&mut self, cx: &mut Context<Self>) {
        self.git_simple(true, cx);
    }

    fn git_push(&mut self, cx: &mut Context<Self>) {
        self.git_simple(false, cx);
    }

    fn git_simple(&mut self, pull: bool, cx: &mut Context<Self>) {
        if !self.git_cap || self.git_busy || !self.git_repo {
            return;
        }
        self.git_busy = true;
        self.status = if pull {
            "Pulling…".into()
        } else {
            "Pushing…".into()
        };
        let request_id = next_id("git");
        let workspace_id = self.workspace_id_or_empty();
        if pull {
            self.ade.send(AdeCmd::GitPull {
                request_id,
                workspace_id,
                directory: self.git_context_dir.clone(),
            });
        } else {
            self.ade.send(AdeCmd::GitPush {
                request_id,
                workspace_id,
                directory: self.git_context_dir.clone(),
            });
        }
        cx.notify();
    }

    fn ensure_context_selection(&mut self, path: &str, cx: &mut Context<Self>) {
        if is_placeholder(path) || self.selection.iter().any(|item| item == path) {
            return;
        }
        self.selection = vec![path.to_string()];
        self.anchor = Some(path.to_string());
        self.sync_tree_highlight(None, cx);
    }

    fn begin_file_rename(&mut self, path: String, window: &mut Window, cx: &mut Context<Self>) {
        let name = path_basename(&display_path(&path))
            .unwrap_or("")
            .to_string();
        self.renaming_path = Some(path);
        self.file_rename_input.update(cx, |state, cx| {
            state.set_value(name, window, cx);
            state.focus(window, cx);
        });
        cx.notify();
    }

    fn confirm_file_rename(&mut self, cx: &mut Context<Self>) {
        let Some(path) = self.renaming_path.clone() else {
            return;
        };
        let name = self.file_rename_input.read(cx).value().to_string();
        if name.is_empty()
            || name == "."
            || name == ".."
            || name.chars().any(|ch| matches!(ch, '/' | '\\' | '\0'))
        {
            self.status = "Invalid name: enter one non-empty file or folder name".into();
            cx.notify();
            return;
        }
        if self
            .pending_renames
            .values()
            .any(|pending| pending == &path)
        {
            return;
        }
        let request_id = next_id("rename");
        self.pending_renames
            .insert(request_id.clone(), path.clone());
        self.ade.send(AdeCmd::RenamePath {
            request_id,
            path,
            name,
        });
        self.status = "Renaming…".into();
        cx.notify();
    }

    fn on_filter_explorer(
        &mut self,
        _: &FilterExplorer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.filter_open = true;
        self.filter_input
            .update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    fn on_clear_explorer_input(
        &mut self,
        _: &ClearExplorerInput,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.renaming_path.take().is_some() {
            window.focus(&self.explorer_focus, cx);
        } else if self.filter_open {
            self.filter_open = false;
            self.filter_input
                .update(cx, |state, cx| state.set_value("", window, cx));
            self.rebuild_tree(cx);
            window.focus(&self.explorer_focus, cx);
        }
        cx.notify();
    }

    fn copy_path_text(&mut self, paths: &[String], window: &Window, cx: &mut Context<Self>) {
        if paths.is_empty() {
            self.status = "No selection".into();
            cx.notify();
            return;
        }
        let text = absolute_paths_text(&display_paths(paths));
        self.status = match super::clipboard::write_text(window, cx, &text) {
            Ok(()) if paths.len() == 1 => "Copied path".into(),
            Ok(()) => format!("Copied {} paths", paths.len()).into(),
            Err(error) => error.into(),
        };
        cx.notify();
    }

    fn arm_file_copy(&mut self, paths: Vec<String>, cx: &mut Context<Self>) {
        if paths.is_empty() {
            self.status = "No selection".into();
            cx.notify();
            return;
        }
        let text = absolute_paths_text(&display_paths(&paths));
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

    fn delete_explorer_selection(&mut self, cx: &mut Context<Self>) {
        let paths = self.selected_or_primary();
        if paths.is_empty() {
            self.status = "No selection to delete".into();
            cx.notify();
            return;
        }
        self.ade.send(AdeCmd::DeletePaths {
            request_id: next_id("delete"),
            paths: paths.clone(),
        });
        self.status = format!("Deleting {} item(s)…", paths.len()).into();
        cx.notify();
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
        self.pending_lists.retain(|_, path| path != dir);
        self.list_dir(dir);
    }

    /// `fs_list` for `path`, remembered so the reply is keyed by the path the
    /// tree asked for (see [`rebase_listing`]). A listing already in flight
    /// for the same path is not sent twice.
    fn list_dir(&mut self, path: &str) {
        if self.pending_lists.values().any(|pending| pending == path) {
            return;
        }
        let request_id = next_id("ex");
        self.pending_lists
            .insert(request_id.clone(), path.to_string());
        self.ade.send(AdeCmd::ListDir {
            request_id,
            path: path.to_string(),
        });
    }

    fn on_new_terminal(&mut self, _: &NewTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.new_terminal(cx);
        cx.notify();
    }

    fn on_close_tab(&mut self, _: &CloseTab, window: &mut Window, cx: &mut Context<Self>) {
        self.close_active_tab(window, cx);
    }

    fn on_close_all_editors(&mut self, _: &CloseAllEditors, window: &mut Window, cx: &mut Context<Self>) {
        self.close_all_editors(window, cx);
    }

    fn on_close_all_terminals(&mut self, _: &CloseAllTerminals, window: &mut Window, cx: &mut Context<Self>) {
        self.close_all_terminals(window, cx);
    }

    fn on_close_all_other_terminals(&mut self, _: &CloseAllOtherTerminals, window: &mut Window, cx: &mut Context<Self>) {
        let keep = self.active_panel_id().filter(|id| self.terminals.values().any(|p| PanelId::from(p.entity_id()) == *id));
        self.close_all_other_terminals(keep, window, cx);
    }

    fn on_close_all_other_tabs(&mut self, _: &CloseAllOtherTabs, window: &mut Window, cx: &mut Context<Self>) {
        self.close_all_other_tabs(self.active_panel_id(), window, cx);
    }

    fn on_save(&mut self, _: &SaveBuffer, window: &mut Window, cx: &mut Context<Self>) {
        self.save_active(window, cx);
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_collapsed = !self.sidebar_collapsed;
        self.publish_layout(cx);
        cx.notify();
    }

    fn on_zoom_in_content(
        &mut self,
        _: &ZoomInContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_content_zoom(1, window, cx);
    }

    fn on_zoom_out_content(
        &mut self,
        _: &ZoomOutContent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step_content_zoom(-1, window, cx);
    }

    fn on_reset_content_zoom(
        &mut self,
        _: &ResetContentZoom,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.content_zoom = 1.0;
        self.apply_zoom(window, cx);
        self.status = "Panel zoom 100%".into();
        cx.notify();
    }

    fn on_zoom_in_ui(&mut self, _: &ZoomInUi, window: &mut Window, cx: &mut Context<Self>) {
        self.step_ui_zoom(1, window, cx);
    }

    fn on_zoom_out_ui(&mut self, _: &ZoomOutUi, window: &mut Window, cx: &mut Context<Self>) {
        self.step_ui_zoom(-1, window, cx);
    }

    fn on_reset_ui_zoom(&mut self, _: &ResetUiZoom, window: &mut Window, cx: &mut Context<Self>) {
        self.ui_zoom = 1.0;
        self.apply_zoom(window, cx);
        self.status = "UI zoom 100%".into();
        cx.notify();
    }

    fn step_content_zoom(&mut self, steps: i32, window: &mut Window, cx: &mut Context<Self>) {
        self.content_zoom = chrome::step_zoom(
            self.content_zoom,
            steps,
            chrome::CONTENT_ZOOM_MIN,
            chrome::CONTENT_ZOOM_MAX,
        );
        self.apply_zoom(window, cx);
        self.status = format!("Panel zoom {}%", zoom_percent(self.content_zoom)).into();
        cx.notify();
    }

    fn step_ui_zoom(&mut self, steps: i32, window: &mut Window, cx: &mut Context<Self>) {
        self.ui_zoom = chrome::step_zoom(
            self.ui_zoom,
            steps,
            chrome::UI_ZOOM_MIN,
            chrome::UI_ZOOM_MAX,
        );
        self.apply_zoom(window, cx);
        self.status = format!("UI zoom {}%", zoom_percent(self.ui_zoom)).into();
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
        let matches = self.goto_matches(&query);
        let target = pick_goto_target(&query, &matches);
        self.goto_open = false;
        if !target.is_empty() {
            self.open_path(target, false);
        }
        cx.notify();
    }

    fn goto_matches(&self, query: &str) -> Vec<String> {
        let query = query.trim().to_lowercase();
        let mut paths = Vec::new();
        for entries in self.explorer_cache.values() {
            for entry in entries {
                if !matches!(entry.kind, FsKind::File | FsKind::Symlink) {
                    continue;
                }
                let path = entry.path.to_lowercase();
                let name = entry.name.to_lowercase();
                if query.is_empty() || path.contains(&query) || name.contains(&query) {
                    paths.push(entry.path.clone());
                }
            }
        }
        paths.sort();
        paths.truncate(12);
        paths
    }

    pub(crate) fn open_path_link(
        &mut self,
        line: String,
        column: u32,
        cwd: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if !matches!(self.connection, ConnectionState::Online) {
            self.status = "Not connected".into();
            cx.notify();
            return;
        }
        let cwd = cwd.filter(|path| !path.is_empty()).or_else(|| {
            self.last_cwd
                .clone()
                .or_else(|| self.workspace_root())
        });
        let request_id = next_id("link");
        self.pending_editors.insert(request_id.clone(), true);
        self.ade.send(AdeCmd::OpenLink {
            request_id,
            line_text: line,
            column,
            cwd,
        });
    }

    fn split_terminal_vertical(&mut self, cx: &mut Context<Self>) {
        let Some(ActiveSurface::Terminal(id)) = &self.active else {
            self.status = "No terminal to split".into();
            cx.notify();
            return;
        };
        let Some(panel) = self.terminals.get(id) else {
            return;
        };
        self.pending_split = Some(PanelId::from(panel.entity_id()));
        self.new_terminal(cx);
        self.status = "Splitting terminal…".into();
        cx.notify();
    }

    fn format_active(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ActiveSurface::Editor(path)) = &self.active else {
            self.status = "No document to format".into();
            cx.notify();
            return;
        };
        let Some(panel) = self.editors.get(path).cloned() else {
            return;
        };
        panel.update(cx, |panel, cx| panel.commit_markdown_inline_edit(window, cx));
        let (buffer_id, rev, text, dirty) = {
            let panel = panel.read(cx);
            (
                panel.buffer_id().to_string(),
                panel.rev(),
                panel.current_text(cx),
                panel.is_dirty(),
            )
        };
        let base_rev = if dirty {
            self.ade.send(AdeCmd::EditBuffer {
                request_id: next_id("ed"),
                buffer_id: buffer_id.clone(),
                base_rev: rev,
                text,
            });
            rev + 1
        } else {
            rev
        };
        self.ade.send(AdeCmd::FormatBuffer {
            request_id: next_id("fmt"),
            buffer_id,
            base_rev,
        });
        self.status = "Formatting…".into();
        cx.notify();
    }

    fn on_settings(&mut self, _: &OpenSettings, _: &mut Window, cx: &mut Context<Self>) {
        self.open_settings();
        cx.notify();
    }

    fn on_default_settings(
        &mut self,
        _: &OpenDefaultSettings,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_default_settings();
        cx.notify();
    }

    fn on_reconnect(&mut self, _: &Reconnect, window: &mut Window, cx: &mut Context<Self>) {
        self.reconnect(window, cx);
        cx.notify();
    }

    fn on_disconnect(&mut self, _: &Disconnect, _: &mut Window, cx: &mut Context<Self>) {
        // Queued ahead of Disconnect, so the ADE worker sends it first.
        if matches!(self.connection, ConnectionState::Online) {
            self.publish_layout(cx);
        }
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

    fn on_ask_copilot(&mut self, _: &AskCopilot, window: &mut Window, cx: &mut Context<Self>) {
        self.palette_open = false;
        self.copilot_open = true;
        self.copilot_result = super::copilot::find_cli().is_none().then(|| {
            "GitHub Copilot CLI was not found on PATH. Install it with `npm install -g @github/copilot`, then authenticate with `copilot`.".to_owned()
        });
        self.copilot_input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        cx.notify();
    }

    fn submit_copilot(&mut self, cx: &mut Context<Self>) {
        if self.copilot_busy {
            return;
        }
        let prompt = self.copilot_input.read(cx).value().trim().to_owned();
        if prompt.is_empty() {
            self.copilot_result = Some("Enter a prompt for Copilot.".into());
            cx.notify();
            return;
        }
        let Some(cli) = super::copilot::find_cli() else {
            self.copilot_result = Some("GitHub Copilot CLI was not found on PATH. Install it with `npm install -g @github/copilot`, then authenticate with `copilot`.".into());
            cx.notify();
            return;
        };
        let cwd = (!self.explorer_root.is_empty())
            .then(|| PathBuf::from(&self.explorer_root))
            .filter(|path| path.is_dir());
        self.copilot_busy = true;
        self.copilot_result = Some("Asking Copilot…".into());
        cx.notify();
        cx.spawn(async move |workspace, cx| {
            let result = cx.background_executor().spawn(async move {
                super::copilot::ask(cli, prompt, cwd)
            }).await;
            let _ = workspace.update(cx, |this, cx| {
                this.copilot_busy = false;
                this.copilot_result = Some(result.unwrap_or_else(|error| error));
                cx.notify();
            });
        }).detach();
    }


    fn on_terminal_copy_or_interrupt(
        &mut self,
        _: &TerminalCopyOrInterrupt,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActiveSurface::Terminal(id)) = &self.active
            && let Some(panel) = self.terminals.get(id)
        {
            panel.update(cx, |panel, cx| panel.copy_or_interrupt(window, cx));
        }
    }

    fn on_new_file(&mut self, _: &NewFile, _: &mut Window, cx: &mut Context<Self>) {
        self.new_file(cx);
    }

    fn on_split_terminal(&mut self, _: &SplitTerminal, _: &mut Window, cx: &mut Context<Self>) {
        self.split_terminal_vertical(cx);
    }

    fn on_format_document(&mut self, _: &FormatDocument, window: &mut Window, cx: &mut Context<Self>) {
        self.format_active(window, cx);
    }

    fn on_toggle_pin_tab(&mut self, _: &TogglePinTab, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(id) = self.active_panel_id() { self.toggle_pin(id, cx); }
    }

    fn on_delete_explorer(&mut self, _: &DeleteExplorer, _: &mut Window, cx: &mut Context<Self>) {
        self.delete_explorer_selection(cx);
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

    fn on_stop_server(&mut self, _: &StopServer, window: &mut Window, cx: &mut Context<Self>) {
        if matches!(self.connection, ConnectionState::Online) {
            self.save_before_exit(cx);
        }
        let remote = self
            .target
            .label
            .as_ref()
            .is_some_and(|label| !label.is_empty());
        self.ade.send(AdeCmd::Disconnect);
        if remote {
            self.connection = ConnectionState::Offline {
                reason: "Disconnected".into(),
            };
            self.status = "Disconnected. The remote daemon keeps running; stop it with fresh-gui close on that machine.".into();
            cx.notify();
            return;
        }
        self.connection = ConnectionState::Offline {
            reason: "Stopping daemon".into(),
        };
        self.status = "Stopping the local daemon…".into();
        cx.notify();
        let (tx, rx) = async_channel::bounded(1);
        std::thread::spawn(move || {
            let _ = tx.send_blocking(crate::launch::close_local_daemon());
        });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(result) = rx.recv().await else {
                return;
            };
            let _ = cx.update(|_window, app| {
                let _ = this.update(app, |this, cx| {
                    this.connection = ConnectionState::Offline {
                        reason: "Daemon stopped".into(),
                    };
                    this.status = match result {
                        Ok(text) => text.into(),
                        Err(err) => format!("{err:#}").into(),
                    };
                    cx.notify();
                });
            });
        })
        .detach();
    }

    fn on_quit_client(&mut self, _: &QuitClient, window: &mut Window, cx: &mut Context<Self>) {
        self.save_before_exit(cx);
        window.remove_window();
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

    fn workspace_rail_visible(&self) -> bool {
        show_workspace_rail(
            self.workspace_cap,
            matches!(self.connection, ConnectionState::Online),
            self.workspace_root().as_deref(),
        )
    }

    /// One row for a daemon that never advertised `workspace` (an older remote
    /// binary the probe left running). Create/switch still need the capability.
    fn render_session_rail(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let root = self.workspace_root().unwrap_or_default();
        let shown = display_path(&root);
        let name = path_basename(&shown).unwrap_or("Session").to_string();
        let home = user_home();
        let root_label = workspace_root_label(&root, home.as_deref());
        let muted = cx.theme().muted_foreground;

        v_flex()
            .id("workspace-rail")
            .relative()
            .w(self.ui_px(self.workspace_rail_width))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .h(self.ui_px(SIDEBAR_HEADER_H))
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
                h_flex()
                    .id("session-root-row")
                    .w_full()
                    .min_h(self.ui_px(WORKSPACE_ROW_H))
                    .items_stretch()
                    .bg(cx.theme().accent.opacity(0.20))
                    .child(
                        div()
                            .w(px(3.))
                            .min_h(self.ui_px(WORKSPACE_ROW_H))
                            .flex_shrink_0()
                            .bg(cx.theme().accent),
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
                            .child(
                                div()
                                    .text_sm()
                                    .text_ellipsis()
                                    .child(name),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_ellipsis()
                                    .text_color(muted)
                                    .child(root_label),
                            ),
                    ),
            )
            .child(
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
                    .child(
                        "This daemon has no workspace list. Upgrade and restart it to add projects.",
                    ),
            )
            .child(self.side_panel_drag_handle("resize-session-rail", true, cx))

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
            .w(self.ui_px(self.workspace_rail_width))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .h(self.ui_px(SIDEBAR_HEADER_H))
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
            .child(self.side_panel_drag_handle("resize-workspace-rail", true, cx))

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
        let relocating = self.relocating_id.as_deref() == Some(workspace.id.as_str());
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
            .min_h(self.ui_px(WORKSPACE_ROW_H))
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
                    .min_h(self.ui_px(WORKSPACE_ROW_H))
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
                    .child(if relocating {
                        self.render_workspace_root_editor(&id, cx)
                            .into_any_element()
                    } else {
                        div()
                            .w_full()
                            .min_w_0()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .child(root_label)
                            .into_any_element()
                    }),
            )
            .context_menu(move |menu, _, _| {
                let rename_id = menu_id.clone();
                let close_id = menu_id.clone();
                let relocate_id = menu_id.clone();
                let rename_view = view.clone();
                let close_view = view.clone();
                let relocate_view = view.clone();
                menu.item(PopupMenuItem::new("Rename").on_click(move |_, window, cx| {
                    rename_view.update(cx, |this, cx| {
                        this.begin_workspace_rename_for(&rename_id, window, cx);
                        cx.notify();
                    });
                }))
                .item(
                    PopupMenuItem::new("Change location…").on_click(move |_, window, cx| {
                        relocate_view.update(cx, |this, cx| {
                            this.begin_workspace_root_for(&relocate_id, window, cx);
                            cx.notify();
                        });
                    }),
                )
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

    fn render_workspace_root_editor(&self, id: &str, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .w_full()
            .min_w_0()
            .gap_1()
            .items_center()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key.eq_ignore_ascii_case("escape") {
                    this.relocating_id = None;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(Input::new(&self.ws_root_input)),
            )
            .child(
                Button::new(format!("ws-root-ok-{id}"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Check)
                    .tooltip("Change location")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.confirm_workspace_root(cx);
                        cx.notify();
                    })),
            )
            .child(
                Button::new(format!("ws-root-cancel-{id}"))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Cancel")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.relocating_id = None;
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
                    .top(self.ui_px(TITLE_BAR_H + SIDEBAR_HEADER_H + 4.))
                    .w(self.ui_px(320.))
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
                            "Absolute path on the daemon. Starts as the current workspace folder. Empty uses the daemon project root.",
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
            .w(self.ui_px(ACTIVITY_RAIL_W))
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
                        this.publish_layout(cx);
                        cx.notify();
                    })),
            )
            .when(self.git_cap, |rail| {
                rail.child(
                    Button::new("act-git")
                        .ghost()
                        .small()
                        .icon(gpui_kit::assets::IconName::FolderGit)
                        .tooltip("Source Control")
                        .selected(self.activity == Activity::Git && !self.sidebar_collapsed)
                        .on_click(cx.listener(|this, _, _, cx| {
                            if this.activity == Activity::Git {
                                this.sidebar_collapsed = !this.sidebar_collapsed;
                            } else {
                                this.activity = Activity::Git;
                                this.sidebar_collapsed = false;
                                this.refresh_git();
                            }
                            this.publish_layout(cx);
                            cx.notify();
                        })),
                )
            })
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




    fn on_side_panel_drag_move(
        &mut self,
        event: &MouseMoveEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !(self.resizing_rail || self.resizing_explorer) {
            return;
        }
        if !event.dragging() {
            self.resizing_rail = false;
            self.resizing_explorer = false;
            self.resize_last_x = None;
            return;
        }
        let x = f32::from(event.position.x);
        let Some(last) = self.resize_last_x else {
            self.resize_last_x = Some(x);
            return;
        };
        let delta = x - last;
        self.resize_last_x = Some(x);
        if self.resizing_rail {
            self.workspace_rail_width = Self::clamp_rail_width(self.workspace_rail_width + delta);
        } else if self.resizing_explorer {
            self.explorer_width = Self::clamp_explorer_width(self.explorer_width + delta);
        }
        cx.notify();
    }

    fn side_panel_drag_handle(
        &self,
        id: &'static str,
        rail: bool,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        div()
            .id(id)
            .absolute()
            .top_0()
            .right_0()
            .w(px(3.))
            .h_full()
            .cursor(CursorStyle::ResizeColumn)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &MouseDownEvent, _, cx| {
                    this.resizing_rail = rail;
                    this.resizing_explorer = !rail;
                    this.resize_last_x = Some(f32::from(event.position.x));
                    cx.notify();
                }),
            )
    }

    fn clamp_rail_width(width: f32) -> f32 {
        width.clamp(160., 360.)
    }

    fn clamp_explorer_width(width: f32) -> f32 {
        width.clamp(180., 480.)
    }

    fn refresh_explorer(&mut self, cx: &mut Context<Self>) {
        let root = self.explorer_root.clone();
        if root.is_empty() {
            self.status = "No explorer root".into();
            cx.notify();
            return;
        }
        let expanded: Vec<String> = self.expanded_dirs.iter().cloned().collect();
        self.relist(&root);
        for dir in expanded {
            self.relist(&dir);
        }
        // Reload open editors that map to real paths.
        for (path, panel) in self.editors.clone() {
            if path.is_empty() || is_untitled_editor_key(&path) {
                continue;
            }
            let request_id = next_id("reload");
            self.pending_editors.insert(request_id.clone(), false);
            self.ade.send(AdeCmd::OpenEditor {
                request_id,
                path: path.clone(),
                preview: false,
                line: None,
                column: None,
            });
            let _ = panel;
        }
        self.status = "Explorer refreshed".into();
        cx.notify();
    }

    fn render_explorer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let root_label = explorer_header_label(&self.explorer_root);
        let selected: HashSet<String> = self.selection.iter().cloned().collect();

        let kinds = self.explorer_kinds.clone();
        let dark = cx.theme().is_dark();
        let renaming_path = self.renaming_path.clone();
        let file_rename_input = self.file_rename_input.clone();
        let tree_row_height = self.ui_px(TREE_ROW_H);

        v_flex()
            .id("explorer-pane")
            .relative()
            .on_scroll_wheel(cx.listener(|_, _, _, cx| {
                let workspace = cx.entity();
                cx.defer(move |cx| { workspace.update(cx, |this, cx| this.publish_layout(cx)); });
            }))
            .role(Role::Group)
            .aria_label("Explorer")
            .key_context("Explorer")
            .track_focus(&self.explorer_focus)
            .w(self.ui_px(self.explorer_width))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .h(self.ui_px(SIDEBAR_HEADER_H))
                    .px_2()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().font_semibold().child(root_label))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("refresh-explorer")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::RefreshCw)
                                    .tooltip("Refresh")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh_explorer(cx);
                                    })),
                            )
                            .child(
                                Button::new("collapse-sidebar")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::PanelLeftClose)
                                    .tooltip("Hide Explorer (Ctrl+B)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sidebar_collapsed = true;
                                        this.publish_layout(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when(self.filter_open, |this| {
                this.child(
                    h_flex()
                        .w_full()
                        .px_2()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .child(Input::new(&self.filter_input)),
                        )
                        .child(
                            Button::new("clear-explorer-filter")
                                .ghost()
                                .xsmall()
                                .icon(IconName::Close)
                                .tooltip("Clear filter")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.filter_open = false;
                                    this.filter_input
                                        .update(cx, |state, cx| state.set_value("", window, cx));
                                    this.rebuild_tree(cx);
                                    window.focus(&this.explorer_focus, cx);
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child({
                let row_view = view.clone();
                let menu_view = view;
                tree(&self.explorer, move |ix, entry, _selected, _window, cx| {
                    let item = entry.item();
                    let path = item.id.to_string();
                    let label = item.label.clone();
                    let file_name = label.to_string();
                    let placeholder = is_placeholder(&path);
                    let kind = kinds.get(&path).copied();
                    // `FsKind`, not `entry.is_folder()`: a listed empty directory
                    // has no children, so the tree would otherwise open it as a file.
                    let is_dir = kind == Some(FsKind::Dir);
                    let can_expand = entry.is_folder();
                    let in_selection = selected.contains(&path);
                    let view = row_view.clone();
                    let drag_view = view.clone();
                    let drop_view = view.clone();
                    let grabbed = path.clone();
                    let selected_now: Vec<String> = selected.iter().cloned().collect();
                    ListItem::new(ix)
                        .w_full()
                        .h(tree_row_height)
                        .text_sm()
                        .rounded(cx.theme().radius)
                        .py_0()
                        .px_1()
                        .pl(px(tree_row_indent_px(entry.depth())))
                        .child(
                            h_flex()
                                .w_full()
                                .gap_1()
                                .items_center()
                                .when(in_selection, |this| {
                                    this.bg(cx.theme().accent.opacity(0.28))
                                })
                                .child(div().w(px(16.)).flex_shrink_0().flex().justify_center().when(
                                    can_expand,
                                    |this| {
                                        let chevron = if entry.is_expanded() {
                                            IconName::ChevronDown
                                        } else {
                                            IconName::ChevronRight
                                        };
                                        this.child(Icon::new(chevron).xsmall())
                                    },
                                ))
                                .when(!placeholder, |this| {
                                    let glyph = explorer_glyph(
                                        &file_name,
                                        kind.unwrap_or(FsKind::File),
                                        entry.is_expanded(),
                                    );
                                    this.child(
                                        div().flex_shrink_0().child(
                                            Icon::new(glyph.icon)
                                                .small()
                                                .text_color(glyph.color(dark)),
                                        ),
                                    )
                                })
                                .when(renaming_path.as_deref() == Some(path.as_str()), |this| {
                                    let view = view.clone();
                                    let cancel_view = view.clone();
                                    this.child(
                                        h_flex()
                                            .flex_1()
                                            .min_w_0()
                                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                                cx.stop_propagation()
                                            })
                                            .on_mouse_up(MouseButton::Left, |_, _, cx| {
                                                cx.stop_propagation()
                                            })
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .child(Input::new(&file_rename_input).small()),
                                            )
                                            .child(
                                                Button::new(format!("file-rename-ok-{ix}"))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Check)
                                                    .tooltip("Rename")
                                                    .on_click(move |_, _, cx| {
                                                        view.update(cx, |this, cx| {
                                                            this.confirm_file_rename(cx)
                                                        });
                                                        cx.stop_propagation();
                                                    }),
                                            )
                                            .child(
                                                Button::new(format!("file-rename-cancel-{ix}"))
                                                    .ghost()
                                                    .xsmall()
                                                    .icon(IconName::Close)
                                                    .tooltip("Cancel")
                                                    .on_click(move |_, window, cx| {
                                                        cancel_view.update(cx, |this, cx| {
                                                            this.renaming_path = None;
                                                            window.focus(&this.explorer_focus, cx);
                                                            cx.notify();
                                                        });
                                                        cx.stop_propagation();
                                                    }),
                                            ),
                                    )
                                })
                                .when(renaming_path.as_deref() != Some(path.as_str()), |this| {
                                    this.child(label)
                                }),
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
                        .when(is_dir && !placeholder, |this| {
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
                                let count = super::diff_view::click_count(event);
                                let (focus, diff) = drag_view.update(cx, |this, cx| {
                                    let diff =
                                        this.apply_tree_click(&path, is_dir, gesture, count, cx);
                                    (this.explorer_focus.clone(), diff)
                                });
                                if let Some((rel, pin)) = diff {
                                    drag_view.update(cx, |this, cx| {
                                        this.open_diff(rel, pin, window, cx);
                                    });
                                }
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
                        let is_file = !entry.is_folder();
                        let copy_paths = paths.clone();
                        let delete_paths = paths.clone();
                        let file_paths = paths;
                        let open_path = path.clone();
                        menu.when(is_file, |menu| {
                            let view = view.clone();
                            menu.item(PopupMenuItem::new("Open Editor").on_click(
                                move |_, _, cx| {
                                    let path = open_path.clone();
                                    view.update(cx, |this, cx| {
                                        this.open_path(path, false);
                                        cx.notify();
                                    });
                                },
                            ))
                        })
                        .item(PopupMenuItem::new("Copy Path").on_click({
                            let view = view.clone();
                            move |_, window, cx| {
                                view.update(cx, |this, cx| {
                                    this.copy_path_text(&copy_paths, window, cx)
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("Rename").on_click({
                            let view = view.clone();
                            let path = path.clone();
                            move |_, window, cx| {
                                view.update(cx, |this, cx| {
                                    this.begin_file_rename(path.clone(), window, cx);
                                });
                            }
                        }))
                        .item(PopupMenuItem::new("Delete").on_click({
                            let view = view.clone();
                            move |_, _, cx| {
                                view.update(cx, |this, cx| {
                                    this.selection = delete_paths.clone();
                                    this.delete_explorer_selection(cx);
                                });
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
            .child(self.side_panel_drag_handle("resize-explorer", false, cx))

    }

    fn render_git(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let zoom = self.ui_zoom;
        let branch = if !self.git_repo {
            "Not a Git repository".to_string()
        } else if self.git_branch.is_empty() {
            "HEAD".to_string()
        } else {
            let mut label = self.git_branch.clone();
            if self.git_ahead > 0 {
                label.push_str(&format!(" ↑{}", self.git_ahead));
            }
            if self.git_behind > 0 {
                label.push_str(&format!(" ↓{}", self.git_behind));
            }
            label
        };
        let rows = git_change_rows(&self.git_files, &self.git_collapsed);
        let busy = self.git_busy;
        let repo = self.git_repo;
        let root_label = if !self.git_root.is_empty() {
            Some(display_path(&self.git_root))
        } else if !self.explorer_root.trim().is_empty() {
            Some(display_path(&self.explorer_root))
        } else {
            None
        };

        v_flex()
            .id("git-pane")
            .role(Role::Group)
            .aria_label("Source Control")
            .w(self.ui_px(self.explorer_width))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().sidebar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .w_full()
                    .h(self.ui_px(SIDEBAR_HEADER_H))
                    .px_2()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().font_semibold().child("Source Control"))
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("git-refresh")
                                    .ghost()
                                    .xsmall()
                                    .label("Refresh")
                                    .disabled(busy)
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.refresh_git();
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("collapse-git")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::PanelLeftClose)
                                    .tooltip("Hide Source Control (Ctrl+B)")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.sidebar_collapsed = true;
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .child(
                v_flex()
                    .px_2()
                    .py_1()
                    .gap_1()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(branch),
                    )
                    .when_some(root_label, |column, root| {
                        column.child(
                            div()
                                .w_full()
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(root),
                        )
                    })
                    .when_some(self.git_detail.clone(), |column, detail| {
                        column.child(
                            div()
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(detail),
                        )
                    })
                    .child(Input::new(&self.commit_input).small())
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("git-commit")
                                    .xsmall()
                                    .primary()
                                    .label("Commit")
                                    .disabled(busy || !repo)
                                    .on_click(cx.listener(|this, _, _, cx| this.git_commit(cx))),
                            )
                            .child(
                                Button::new("git-pull")
                                    .xsmall()
                                    .label("Pull")
                                    .disabled(busy || !repo)
                                    .on_click(cx.listener(|this, _, _, cx| this.git_pull(cx))),
                            )
                            .child(
                                Button::new("git-push")
                                    .xsmall()
                                    .label("Push")
                                    .disabled(busy || !repo)
                                    .on_click(cx.listener(|this, _, _, cx| this.git_push(cx))),
                            ),
                    ),
            )
            .child(
                div()
                    .id("git-file-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows.into_iter().enumerate().map(|(ix, row)| {
                        let view = view.clone();
                        match row {
                            GitTreeRow::Dir { path, depth, name } => {
                                let open = !self.git_collapsed.contains(&path);
                                git_dir_row(ix, path, name, depth, open, view, zoom).into_any_element()
                            }
                            GitTreeRow::File { file, depth, name } => {
                                git_file_row(ix, file, name, depth, busy, view, zoom).into_any_element()
                            }
                        }
                    })),
            )
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
            ("Ask Copilot…", Box::new(AskCopilot) as Box<dyn Action>),
            ("New Terminal", Box::new(NewTerminal) as Box<dyn Action>),
            ("New File", Box::new(NewFile)),
            ("Split Terminal Vertically", Box::new(SplitTerminal)),
            ("Format Document", Box::new(FormatDocument)),
            ("New Workspace", Box::new(NewWorkspace)),
            ("Rename Workspace", Box::new(RenameWorkspace)),
            ("Close Workspace", Box::new(CloseWorkspace)),
            ("Close Tab", Box::new(CloseTab)),
            ("Pin or Unpin Tab", Box::new(TogglePinTab)),
            ("Close All Editors", Box::new(CloseAllEditors)),
            ("Close All Terminals", Box::new(CloseAllTerminals)),
            ("Close All Other Terminals", Box::new(CloseAllOtherTerminals)),
            ("Close All Other Tabs", Box::new(CloseAllOtherTabs)),
            ("Save", Box::new(SaveBuffer)),
            ("Toggle Sidebar", Box::new(ToggleSidebar)),
            ("Go to File…", Box::new(GoToFile)),
            ("Open Settings", Box::new(OpenSettings)),
            ("Open Default Settings", Box::new(OpenDefaultSettings)),
            ("Zoom In Panel", Box::new(ZoomInContent)),
            ("Zoom Out Panel", Box::new(ZoomOutContent)),
            ("Reset Panel Zoom", Box::new(ResetContentZoom)),
            ("Zoom In UI", Box::new(ZoomInUi)),
            ("Zoom Out UI", Box::new(ZoomOutUi)),
            ("Reset UI Zoom", Box::new(ResetUiZoom)),
            ("Reconnect", Box::new(Reconnect)),
            ("Disconnect", Box::new(Disconnect)),
            ("Stop Server", Box::new(StopServer)),
            ("Quit Client", Box::new(QuitClient)),
        ];
        Command::new(&self.command_state)
            .placeholder("Type a command…")
            .bordered(true)
            .w(self.ui_px(520.))
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

    fn render_pinned_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let (tabs, _) = self.capture_layout(cx);
        let mut bar = h_flex().id("pinned-tabs").w_full().h(self.ui_px(27.)).px_2().gap_1()
            .items_center().border_b_1().border_color(cx.theme().border);
        for tab in tabs {
            let Some(key) = tab_key(&tab) else { continue; };
            if !self.pinned_tabs.contains(&key) { continue; }
            let id = match tab.kind {
                WorkspaceTabKind::Terminal => tab.pty_id.as_ref().and_then(|id| self.terminals.get(id)).map(|panel| PanelId::from(panel.entity_id())),
                WorkspaceTabKind::Editor => tab.path.as_ref().and_then(|path| self.editors.get(path)).map(|panel| PanelId::from(panel.entity_id())),
            };
            let Some(id) = id else { continue; };
            let view = cx.entity();
            bar = bar.child(Button::new(format!("pinned-{key}")).ghost().xsmall()
                .label(format!("⌑ {}", tab.title)).on_click(move |_, window, cx| {
                    view.update(cx, |this, cx| this.dock.update(cx, |dock, cx| dock.select_panel(id, window, cx)));
                }));
        }
        bar
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
                    .w(self.ui_px(480.))
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_sm().font_bold().child("Go to File"))
                    .child(Input::new(&self.goto_input))
                    .children(self.goto_matches(&self.goto_input.read(cx).value().to_string()).into_iter().map(|path| {
                        let open = cx.entity();
                        let label = display_path(&path);
                        Button::new(SharedString::from(format!("goto-{path}")))
                            .ghost()
                            .label(label)
                            .on_click(move |_, _, cx| {
                                let path = path.clone();
                                open.update(cx, |this, cx| {
                                    this.goto_open = false;
                                    this.open_path(path, false);
                                    cx.notify();
                                });
                            })
                    }))
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

    fn render_copilot(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let submit = cx.entity();
        let close = cx.entity();
        div()
            .id("copilot-overlay")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt(px(72.))
            .bg(cx.theme().background.opacity(0.45))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                this.copilot_open = false;
                cx.notify();
            }))
            .child(
                v_flex()
                    .id("copilot-dialog")
                    .w(self.ui_px(620.))
                    .max_h(self.ui_px(540.))
                    .gap_3()
                    .p_4()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_lg().child("Ask GitHub Copilot"))
                    .child(Input::new(&self.copilot_input).w_full())
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("copilot-cancel").label("Close").on_click(move |_, _, cx| {
                                close.update(cx, |this, cx| { this.copilot_open = false; cx.notify(); });
                            }))
                            .child(Button::new("copilot-ask").label(if self.copilot_busy { "Working…" } else { "Ask" }).disabled(self.copilot_busy).on_click(move |_, _, cx| {
                                submit.update(cx, |this, cx| this.submit_copilot(cx));
                            })),
                    )
                    .when_some(self.copilot_result.as_ref(), |this, result| {
                        this.child(
                            div()
                                .id("copilot-result")
                                .max_h(self.ui_px(350.))
                                .overflow_y_scroll()
                                .p_2()
                                .bg(cx.theme().muted.opacity(0.2))
                                .text_sm()
                                .child(result.clone()),
                        )
                    }),
            )
    }

    fn render_save(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("save-overlay")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt(px(80.))
            .bg(cx.theme().background.opacity(0.45))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.save_open = false;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .w(self.ui_px(480.))
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(div().text_sm().font_bold().child("Save File"))
                    .child(Input::new(&self.save_path_input))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("save-cancel")
                                    .ghost()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.save_open = false;
                                        cx.notify();
                                    })),
                            )
                            .child(Button::new("save-ok").primary().label("Save").on_click(
                                cx.listener(|this, _, _, cx| {
                                    this.confirm_save(cx);
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
                    .w(self.ui_px(420.))
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
        self.terminals.is_empty()
            && self.editors.is_empty()
            && self.diffs.is_empty()
            && self.binaries.is_empty()
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.tab_metrics.begin_frame();
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
        let dock = self.dock.clone();

        div()
            .id("workspace")
            .on_mouse_move(cx.listener(Self::on_side_panel_drag_move))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resizing_rail || this.resizing_explorer {
                        this.resizing_rail = false;
                        this.resizing_explorer = false;
                        this.resize_last_x = None;
                        this.publish_layout(cx);
                        cx.notify();
                    }
                }),
            )
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(Self::on_new_terminal))
            .on_action(cx.listener(Self::on_ask_copilot))
            .on_action(cx.listener(Self::on_close_tab))
            .on_action(cx.listener(Self::on_close_all_editors))
            .on_action(cx.listener(Self::on_close_all_terminals))
            .on_action(cx.listener(Self::on_close_all_other_terminals))
            .on_action(cx.listener(Self::on_close_all_other_tabs))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_zoom_in_content))
            .on_action(cx.listener(Self::on_zoom_out_content))
            .on_action(cx.listener(Self::on_reset_content_zoom))
            .on_action(cx.listener(Self::on_zoom_in_ui))
            .on_action(cx.listener(Self::on_zoom_out_ui))
            .on_action(cx.listener(Self::on_reset_ui_zoom))
            .on_action(cx.listener(Self::on_toggle_palette))
            .on_action(cx.listener(Self::on_goto_file))
            .on_action(cx.listener(Self::on_settings))
            .on_action(cx.listener(Self::on_default_settings))
            .on_action(cx.listener(Self::on_reconnect))
            .on_action(cx.listener(Self::on_disconnect))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_copy_explorer))
            .on_action(cx.listener(Self::on_terminal_copy_or_interrupt))
            .on_action(cx.listener(Self::on_toggle_pin_tab))
            .on_action(cx.listener(Self::on_paste_explorer))
            .on_action(cx.listener(Self::on_delete_explorer))
            .on_action(cx.listener(Self::on_filter_explorer))
            .on_action(cx.listener(Self::on_clear_explorer_input))
            .on_action(cx.listener(Self::on_new_file))
            .on_action(cx.listener(Self::on_split_terminal))
            .on_action(cx.listener(Self::on_format_document))
            .on_action(cx.listener(Self::on_new_workspace))
            .on_action(cx.listener(Self::on_rename_workspace))
            .on_action(cx.listener(Self::on_close_workspace))
            .on_action(cx.listener(Self::on_stop_server))
            .on_action(cx.listener(Self::on_quit_client))
            .child(
                TitleBar::new()
                    .h(self.ui_px(TITLE_BAR_H))
                    // Linux draws its own close button, which removes the
                    // window without the platform should-close hook.
                    .on_close_window(cx.listener(|this, _, window, cx| {
                        this.save_before_exit(cx);
                        window.remove_window();
                    }))
                    .child(
                        h_flex()
                            .flex_1()
                            .h_full()
                            .gap_2()
                            .items_center()
                            .min_w_0()
                            .child(
                                div()
                                    .id("app-menu-host")
                                    .h_full()
                                    .w(self.ui_px(72.))
                                    .flex_shrink_0()
                                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                        cx.stop_propagation();
                                    })
                                    .child(self.menu_bar.clone()),
                            )
                            .child(div().text_sm().font_semibold().child("fresh-gui"))
                            .child(
                                div()
                                    .text_xs()
                                    .text_ellipsis()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(self.target.chrome_label()),
                            ),
                    ),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .when(self.workspace_rail_visible(), |this| {
                        this.child(if self.workspace_cap {
                            self.render_workspace_rail(cx).into_any_element()
                        } else {
                            self.render_session_rail(cx).into_any_element()
                        })
                    })
                    .child(self.render_activity_bar(cx))
                    .when(!self.sidebar_collapsed, |this| {
                        this.child(match self.activity {
                            Activity::Explorer => self.render_explorer(cx).into_any_element(),
                            Activity::Git => self.render_git(cx).into_any_element(),
                        })
                    })
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .when(!self.pinned_tabs.is_empty(), |this| this.child(self.render_pinned_bar(cx)))
                            .child(div().flex_1().min_h_0().relative().child(dock)
                                .when(self.panes_empty(), |this| this.child(self.render_empty(cx)))),
                    ),
            )
            .child(
                StatusBar::new()
                    .h(self.ui_px(STATUS_BAR_H))
                    .py_0()
                    .px_2()
                    .gap_1()
                    .left(strip_verbatim_prefixes(self.status.as_ref()))
                    .child(self.connection_label())
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
            .when(self.copilot_open, |this| this.child(self.render_copilot(cx)))
            .when(self.create_open, |this| {
                this.child(self.render_create_workspace(cx))
            })
            .when(self.rename_pty.is_some(), |this| {
                this.child(self.render_rename(cx))
            })
            .when(self.save_open, |this| this.child(self.render_save(cx)))
            .children(dialog_layer)
            .children(notification_layer)
    }
}

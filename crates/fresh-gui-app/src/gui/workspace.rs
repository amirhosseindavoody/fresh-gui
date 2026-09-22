//! Zed/VS Code-like ADE workspace: activity bar, explorer, docked tabs, status, palette.
//!
//! Editor and terminal surfaces are dock panels. Dragging a tab to a pane edge
//! splits; dropping it on a tab merges; dropping it in the strip reorders.
//! gpui-component refuses to drag the last remaining tab, so a split needs two
//! tabs. Terminal titles are herdr-style numbers inside the focused workspace.
//! A left spaces rail lists daemon workspaces (name and project root);
//! switching swaps this dock for that workspace's session without closing
//! its PTYs.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use fresh_gui_protocol::{
    CAP_GIT, CAP_WORKSPACE, FsEntry, FsKind, GitFile, Hello, PtyInfo, WorkspaceInfo, WorkspaceTab,
    WorkspaceTabKind,
};
use gpui_kit::component::dock::{DockArea, DockEvent, DockPlacement, PanelId, panel_handle};
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
    CloseTab, CloseWorkspace, CopyExplorer, Disconnect, GoToFile, NewTerminal, NewWorkspace,
    NextTab, OpenSettings, PasteExplorer, PrevTab, QuitClient, Reconnect, RenameWorkspace,
    SaveBuffer, StopServer, ToggleCommandPalette, ToggleSidebar,
};
use super::ade::AttachedWorkspace;
use super::ade::{AdeCmd, AdeEvent, AdeHandle};
use super::connect::{ConnectTarget, parse_goto_spec};
use super::diff_view::{self, BinaryPanel, DiffPanel};
use super::dock_a11y::install_workspace_dock;
use super::explorer::{
    absolute_paths_text, apply_selection, build_explorer_tree, copyable_sources, drag_paths,
    entry_kinds, gesture_from_modifiers, is_placeholder, movable_sources, parent_dir,
    prune_expanded, real_ids, rebase_listing, record_tree_toggle,
};
use super::file_icons::explorer_glyph;
use super::pane::{EditorPanel, TerminalPanel};
use super::paths::{display_path, strip_verbatim_prefixes};
use super::rail::{
    WORKSPACE_RAIL_W, WORKSPACE_ROW_H, empty_workspace_name_hint, explorer_header_label, user_home,
    workspace_rail_hint, workspace_root_label,
};
use super::restore::{RestoreStep, restore_plan};
use super::tab_chrome::{TabCloseScope, TabStripMetrics, panels_for_close_scope};

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

fn git_file_row(
    ix: usize,
    file: GitFile,
    busy: bool,
    view: Entity<Workspace>,
    _cx: &App,
) -> impl IntoElement {
    let mut chars = file.xy.chars();
    let index = chars.next().unwrap_or(' ');
    let work = chars.next().unwrap_or(' ');
    let unstaged = work != ' ' || index == '?';
    let staged = index != ' ' && index != '?';
    let name = file
        .path
        .rsplit(['/', '\\'])
        .find(|seg| !seg.is_empty())
        .unwrap_or(file.path.as_str())
        .to_string();
    let xy = if file.xy.is_empty() {
        "  ".to_string()
    } else {
        file.xy.clone()
    };
    let open_rel = file.path.clone();
    let stage_rel = file.path.clone();
    let unstage_rel = file.path;

    h_flex()
        .id(format!("git-file-{ix}"))
        .w_full()
        .h(px(TREE_ROW_H))
        .px_1()
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
        .child(div().w(px(20.)).flex_shrink_0().text_xs().child(xy))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .text_sm()
                .text_ellipsis()
                .child(name),
        )
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
}

fn display_paths(paths: &[String]) -> Vec<String> {
    paths.iter().map(|path| display_path(path)).collect()
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
    tab_metrics: TabStripMetrics,
    terminals: HashMap<String, Entity<TerminalPanel>>,
    editors: HashMap<String, Entity<EditorPanel>>,
    diffs: HashMap<String, Entity<DiffPanel>>,
    binaries: HashMap<String, Entity<BinaryPanel>>,
    /// Unpinned diff tab. The next preview replaces it.
    diff_preview: Option<String>,
    /// Last terminal or editor panel, so a diff tab does not rewrite the saved active index.
    last_saved_panel: Option<PanelId>,
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
    git_detail: Option<String>,
    git_busy: bool,
    git_status_req: Option<String>,
    /// Display path and repo-relative path → repo-relative path.
    git_paths: HashMap<String, String>,
    pending_diffs: HashMap<String, String>,
    commit_input: Entity<InputState>,
    menu_bar: Entity<AppMenuBar>,
    last_cwd: Option<String>,
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
        let (dock, _) = install_workspace_dock(window, cx);
        let explorer = cx.new(|cx| TreeState::new(cx));
        let command_state = cx.new(|cx| CommandState::new(window, cx));
        let goto_input = cx.new(|cx| InputState::new(window, cx).placeholder("path[:line[:col]]"));
        let create_name = cx.new(|cx| InputState::new(window, cx).placeholder("Folder name"));
        let create_root = cx.new(|cx| InputState::new(window, cx).placeholder("/absolute/path"));
        let ws_rename_input =
            cx.new(|cx| InputState::new(window, cx).placeholder("Workspace name"));
        let rename_input = cx.new(|cx| InputState::new(window, cx).placeholder("Terminal name"));
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
        let commit_sub = cx.subscribe(&commit_input, |this, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.git_commit(cx);
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
            tab_metrics: TabStripMetrics::default(),
            terminals: HashMap::new(),
            editors: HashMap::new(),
            diffs: HashMap::new(),
            binaries: HashMap::new(),
            diff_preview: None,
            last_saved_panel: None,
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
            git_detail: None,
            git_busy: false,
            git_status_req: None,
            git_paths: HashMap::new(),
            pending_diffs: HashMap::new(),
            commit_input,
            menu_bar,
            last_cwd: None,
            explorer,
            explorer_root: String::new(),
            explorer_cache: HashMap::new(),
            expanded_dirs: HashSet::new(),
            explorer_kinds: Rc::default(),
            pending_lists: HashMap::new(),
            respawn_titles: VecDeque::new(),
            restore_focus: None,
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
                dock_sub,
                rename_sub,
                ws_rename_sub,
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
                self.apply_hello(&hello);
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
                        cwd: None,
                    });
                    self.list_dir("");
                    self.refresh_git();
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
                        self.add_terminal_tab(id, window, cx);
                    }
                    self.publish_layout(cx);
                }
            }
            AdeEvent::PtyData { id, bytes } => {
                // The terminal panel notifies itself. Redrawing the whole
                // workspace (rail, explorer, dock, status) per chunk is waste.
                self.on_pty_data(&id, &bytes, cx);
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
            AdeEvent::Error { code, message } => {
                self.pending_fs.clear();
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
                    self.status = format!("{code}: {message}").into();
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

    fn apply_hello(&mut self, hello: &Hello) {
        self.capabilities = hello.capabilities.clone();
        self.config_path = hello.config_path.clone();
        self.git_cap = hello.capabilities.iter().any(|cap| cap == CAP_GIT);
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
                self.tab_metrics.clone(),
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
            || self
                .editors
                .values()
                .any(|panel| PanelId::from(panel.entity_id()) == id)
    }

    pub(crate) fn note_terminal_active(
        &mut self,
        pty_id: &str,
        active: bool,
        cx: &mut Context<Self>,
    ) {
        if active {
            self.active = Some(ActiveSurface::Terminal(pty_id.to_string()));
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
        let items = build_explorer_tree(&root, &self.explorer_cache, &self.expanded_dirs);
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
        });
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
        let explorer_expanded = self.expanded_list();
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
            explorer_expanded,
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
        } = attached;
        self.restoring = true;
        self.pty_opens_pending = 0;
        self.pending_editors.clear();
        self.respawn_titles.clear();
        self.restore_focus = None;
        self.release_dock(window, cx);
        self.session_id = Some(info.session_id.clone());
        self.active_workspace_id = Some(info.id.clone());
        self.explorer_cache.clear();
        self.pending_lists.clear();
        self.expanded_dirs = explorer_expanded.into_iter().collect();
        self.selection.clear();
        self.anchor = None;
        self.explorer_root = info.root.clone();
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
        if let Some(pty_id) = self.restore_focus.take()
            && let Some(panel) = self.terminals.get(&pty_id).cloned()
        {
            self.select_entity(&panel, window, cx);
        }
        if !self.panes_empty() {
            self.publish_layout(cx);
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
        let Some(id) = self.active_panel_id() else {
            return;
        };
        self.close_panel_id(id, window, cx);
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
        self.pending_editors.clear();
        self.restoring = false;
        self.renaming_id = None;
        self.rail_hover = None;
        self.create_open = false;
        self.explorer_cache.clear();
        self.expanded_dirs.clear();
        self.explorer_kinds = Rc::default();
        self.pending_lists.clear();
        self.respawn_titles.clear();
        self.restore_focus = None;
        self.explorer_root.clear();
        self.selection.clear();
        self.anchor = None;
        self.git_status_req = None;
        self.git_busy = false;
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

    fn clear_git_view(&mut self) {
        self.git_repo = false;
        self.git_root.clear();
        self.git_branch.clear();
        self.git_upstream = None;
        self.git_ahead = 0;
        self.git_behind = 0;
        self.git_files.clear();
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
        });
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
        let panel = cx.new(|cx| DiffPanel::new(rel.clone(), title, pin, workspace, cx));
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
        let panel = cx.new(|cx| BinaryPanel::new(path.clone(), workspace, cx));
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
            });
        } else {
            self.ade.send(AdeCmd::GitPush {
                request_id,
                workspace_id,
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

    fn copy_path_text(&mut self, paths: &[String], cx: &mut Context<Self>) {
        if paths.is_empty() {
            self.status = "No selection".into();
            cx.notify();
            return;
        }
        cx.write_to_clipboard(ClipboardItem::new_string(absolute_paths_text(
            &display_paths(paths),
        )));
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

    fn render_explorer(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
        let root_label = explorer_header_label(&self.explorer_root);
        let selected: HashSet<String> = self.selection.iter().cloned().collect();

        let kinds = self.explorer_kinds.clone();
        let dark = cx.theme().is_dark();

        v_flex()
            .id("explorer-pane")
            .role(Role::Group)
            .aria_label("Explorer")
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
                                .child(div().w(px(14.)).flex().justify_center().when(
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
                                        Icon::new(glyph.icon).small().text_color(glyph.color(dark)),
                                    )
                                })
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

    fn render_git(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let view = cx.entity();
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
        let files = self.git_files.clone();
        let busy = self.git_busy;
        let repo = self.git_repo;

        v_flex()
            .id("git-pane")
            .role(Role::Group)
            .aria_label("Source Control")
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
                    .children(
                        files
                            .into_iter()
                            .enumerate()
                            .map(|(ix, file)| git_file_row(ix, file, busy, view.clone(), cx)),
                    ),
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
            ("Stop Server", Box::new(StopServer)),
            ("Quit Client", Box::new(QuitClient)),
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
            .on_action(cx.listener(Self::on_stop_server))
            .on_action(cx.listener(Self::on_quit_client))
            .child(
                TitleBar::new()
                    .h(px(TITLE_BAR_H))
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
                                    .w(px(72.))
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
                    .when(self.workspace_cap, |this| {
                        this.child(self.render_workspace_rail(cx))
                    })
                    .child(self.render_activity_bar(cx))
                    .when(!self.sidebar_collapsed, |this| {
                        this.child(match self.activity {
                            Activity::Explorer => self.render_explorer(cx).into_any_element(),
                            Activity::Git => self.render_git(cx).into_any_element(),
                        })
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
                    .left(strip_verbatim_prefixes(self.status.as_ref()))
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

//! Explorer selection, path rules, and the file-tree model.
//!
//! The gpui tree keeps a single selected row and toggles a folder on
//! mouse-down. Multi-select, drag sources, and which directories stay open
//! live here so a later `fs_list` rebuild does not reopen every cached
//! directory or close one whose listing has not returned yet.

use std::collections::{HashMap, HashSet};

use fresh_gui_protocol::{FsEntry, FsKind};
use gpui_kit::component::tree::TreeItem;

/// Why a click changed the selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectGesture {
    /// Plain click. Replaces the selection.
    Replace,
    /// Ctrl/Cmd-click. Toggles the row.
    Toggle,
    /// Shift-click. Selects the visible range from the anchor.
    Range,
}

pub fn gesture_from_modifiers(shift: bool, toggle: bool) -> SelectGesture {
    if shift {
        SelectGesture::Range
    } else if toggle {
        SelectGesture::Toggle
    } else {
        SelectGesture::Replace
    }
}

/// Lazy directory placeholders use `{path}/.` and are not real entries.
pub fn is_placeholder(id: &str) -> bool {
    id.ends_with("/.")
}

/// Parent directory of an absolute path. `/` has none.
pub fn parent_dir(path: &str) -> Option<String> {
    let path = path.trim_end_matches(['/', '\\']);
    if path.is_empty() || path == "/" {
        return None;
    }
    match path.rfind(['/', '\\']) {
        Some(0) => Some(path[..1].to_string()),
        Some(2) if path.as_bytes().get(1) == Some(&b':') => Some(path[..3].to_string()),
        Some(ix) => Some(path[..ix].to_string()),
        None => None,
    }
}

/// `path` is `ancestor` or a child of it. `/tmp` does not contain `/tmp2`.
pub fn is_same_or_descendant(path: &str, ancestor: &str) -> bool {
    let ancestor = ancestor.trim_end_matches('/');
    path == ancestor || path.starts_with(&format!("{ancestor}/"))
}

/// New selection and the anchor to keep for the next shift-click.
///
/// The returned vec's last item is the primary row (the one the tree
/// highlight should follow), except for a range, which stays in visible order.
/// Callers highlight `clicked` after a range.
pub fn apply_selection(
    selected: &[String],
    anchor: Option<&str>,
    visible: &[String],
    clicked: &str,
    gesture: SelectGesture,
) -> (Vec<String>, Option<String>) {
    match gesture {
        SelectGesture::Replace => (vec![clicked.to_string()], Some(clicked.to_string())),
        SelectGesture::Toggle => {
            let mut next: Vec<String> = selected
                .iter()
                .filter(|path| path.as_str() != clicked)
                .cloned()
                .collect();
            if next.len() == selected.len() {
                next.push(clicked.to_string());
            }
            let anchor = next.last().cloned();
            (next, anchor)
        }
        SelectGesture::Range => {
            let Some(anchor) = anchor else {
                return (vec![clicked.to_string()], Some(clicked.to_string()));
            };
            let Some(start) = visible.iter().position(|id| id == anchor) else {
                return (vec![clicked.to_string()], Some(clicked.to_string()));
            };
            let Some(end) = visible.iter().position(|id| id == clicked) else {
                return (selected.to_vec(), Some(anchor.to_string()));
            };
            let (lo, hi) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            (visible[lo..=hi].to_vec(), Some(anchor.to_string()))
        }
    }
}

/// Drag every selected path when the grabbed row is part of the selection.
pub fn drag_paths(selected: &[String], clicked: &str) -> Vec<String> {
    if selected.iter().any(|path| path == clicked) {
        selected.to_vec()
    } else {
        vec![clicked.to_string()]
    }
}

/// Sources that can move into `destination`. Drops no-ops and moves into self.
pub fn movable_sources(sources: &[String], destination: &str) -> Vec<String> {
    sources
        .iter()
        .filter(|src| {
            !is_same_or_descendant(destination, src)
                && parent_dir(src).as_deref() != Some(destination)
        })
        .cloned()
        .collect()
}

/// Sources that can be copied into `destination`. Same-folder copies stay;
/// the daemon gives those a unique name.
pub fn copyable_sources(sources: &[String], destination: &str) -> Vec<String> {
    sources
        .iter()
        .filter(|src| !is_same_or_descendant(destination, src))
        .cloned()
        .collect()
}

/// Newline-separated absolute paths. Copy Path uses this text.
pub fn absolute_paths_text(paths: &[String]) -> String {
    paths.join("\n")
}

/// A row the explorer can expand: a directory, or a symlink the daemon says
/// resolves to one inside the sandbox.
pub fn is_dir_entry(entry: &FsEntry) -> bool {
    entry.kind == FsKind::Dir
        || (entry.kind == FsKind::Symlink && entry.target_kind == Some(FsKind::Dir))
}

/// Path → kind for every cached entry, with folder symlinks reported as
/// `Dir`. Placeholders are absent.
pub fn entry_kinds(cache: &HashMap<String, Vec<FsEntry>>) -> HashMap<String, FsKind> {
    cache
        .values()
        .flatten()
        .map(|entry| {
            let kind = if is_dir_entry(entry) {
                FsKind::Dir
            } else {
                entry.kind
            };
            (entry.path.clone(), kind)
        })
        .collect()
}

/// The daemon answers `fs_list` with the canonical directory, so listing a
/// folder symlink returns the target's path and children. Key the listing by
/// the path the tree asked for and re-parent the children under it, or the
/// row never finds its children and a visible target duplicates tree ids.
pub fn rebase_listing(
    requested: &str,
    returned: &str,
    entries: Vec<FsEntry>,
) -> (String, Vec<FsEntry>) {
    if requested.is_empty() || requested == returned {
        return (returned.to_string(), entries);
    }
    let sep = if requested.contains('\\') && !requested.contains('/') {
        '\\'
    } else {
        '/'
    };
    let base = requested.trim_end_matches(sep);
    let entries = entries
        .into_iter()
        .map(|mut entry| {
            entry.path = format!("{base}{sep}{}", entry.name);
            entry
        })
        .collect();
    (requested.to_string(), entries)
}

/// Record the folder the tree just toggled. A listing refresh reads this set
/// instead of “every directory we have listed”. Placeholders are not folders.
pub fn record_tree_toggle(expanded: &mut HashSet<String>, path: &str, open: bool) {
    if is_placeholder(path) {
        return;
    }
    if open {
        expanded.insert(path.to_string());
    } else {
        expanded.remove(path);
    }
}

/// Drop open folders that their (listed) parent no longer contains, so a
/// deleted or renamed directory does not stay in the saved set forever.
/// Folders whose parent has not been listed yet are kept.
pub fn prune_expanded(expanded: &mut HashSet<String>, cache: &HashMap<String, Vec<FsEntry>>) {
    expanded.retain(|dir| {
        let Some(parent) = parent_dir(dir) else {
            return true;
        };
        match cache.get(&parent) {
            Some(entries) => entries.iter().any(|entry| &entry.path == dir),
            None => true,
        }
    });
}

/// Explorer rows. `filter` matches only direct children of `root`.
/// A directory is expanded only when `expanded` says so.
/// Listing it (so `cache` has its children) does not open it. A directory
/// that has not been listed yet keeps a `{path}/.` child so the tree treats
/// the row as a folder and the click can toggle it.
pub fn build_explorer_tree(
    root: &str,
    cache: &HashMap<String, Vec<FsEntry>>,
    expanded: &HashSet<String>,
    filter: &str,
) -> Vec<TreeItem> {
    build_tree_layer(root, cache, expanded, Some(filter))
}

fn build_tree_layer(
    root: &str,
    cache: &HashMap<String, Vec<FsEntry>>,
    expanded: &HashSet<String>,
    filter: Option<&str>,
) -> Vec<TreeItem> {
    let Some(entries) = cache.get(root) else {
        return Vec::new();
    };
    let needle = filter.unwrap_or_default().to_lowercase();
    entries
        .iter()
        .filter(|entry| needle.is_empty() || entry.name.to_lowercase().contains(&needle))
        .map(|entry| {
            if is_dir_entry(entry) {
                let listed = cache.contains_key(&entry.path);
                let children = if listed {
                    build_tree_layer(&entry.path, cache, expanded, None)
                } else {
                    vec![TreeItem::new(format!("{}/.", entry.path), "…")]
                };
                TreeItem::new(entry.path.clone(), entry.name.clone())
                    .children(children)
                    .expanded(expanded.contains(&entry.path))
            } else {
                TreeItem::new(entry.path.clone(), entry.name.clone())
            }
        })
        .collect()
}

/// Chevron column width. A child row indents by one full column so its icon
/// lines up under the parent’s label, not under the parent row itself.
pub const TREE_GUTTER_PX: f32 = 16.;

/// Left padding for an explorer row at `depth` (0 is a root child).
pub fn tree_row_indent_px(depth: usize) -> f32 {
    TREE_GUTTER_PX * depth as f32 + 4.
}

/// Editor-map key for a buffer that has not been saved yet.
pub const UNTITLED_PREFIX: &str = "untitled:";

pub fn untitled_editor_key(buffer_id: &str) -> String {
    format!("{UNTITLED_PREFIX}{buffer_id}")
}

pub fn is_untitled_editor_key(path: &str) -> bool {
    path.starts_with(UNTITLED_PREFIX)
}

/// Path Ctrl+P should open. A unique or name-only match wins; a typed path
/// that already contains a separator is opened as written.
pub fn pick_goto_target(query: &str, matches: &[impl AsRef<str>]) -> String {
    let query = query.trim();
    if query.is_empty() {
        return String::new();
    }
    if let Some(exact) = matches.iter().find(|path| {
        let path = path.as_ref();
        path == query || path.ends_with(&format!("/{query}")) || path.ends_with(&format!("\\{query}"))
    }) {
        return exact.as_ref().to_string();
    }
    let typed_path = query.contains('/') || query.contains('\\') || query.contains(':');
    if !typed_path
        && let Some(first) = matches.first()
    {
        return first.as_ref().to_string();
    }
    query.to_string()
}

/// Absolute save path. A bare name or relative path is placed under `parent`.
pub fn save_target_path(parent: &str, input: &str) -> String {
    let input = input.trim();
    if input.is_empty() {
        return String::new();
    }
    let bytes = input.as_bytes();
    let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if input.starts_with('/') || input.starts_with('\\') || drive {
        return input.to_string();
    }
    let parent = parent.trim_end_matches(['/', '\\']);
    if parent.is_empty() {
        return input.to_string();
    }
    let sep = if parent.contains('\\') && !parent.contains('/') {
        '\\'
    } else {
        '/'
    };
    format!("{parent}{sep}{input}")
}

/// Name for a new file in `existing` names. `untitled`, then `untitled-2`, …
pub fn unused_file_name(existing: &[impl AsRef<str>]) -> String {
    let taken: HashSet<&str> = existing.iter().map(|name| name.as_ref()).collect();
    if !taken.contains("untitled") {
        return "untitled".to_string();
    }
    let mut n = 2u32;
    loop {
        let name = format!("untitled-{n}");
        if !taken.contains(name.as_str()) {
            return name;
        }
        n += 1;
    }
}

/// Flat visible ids, skipping lazy placeholders.
pub fn real_ids<'a>(ids: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    ids.into_iter()
        .filter(|id| !is_placeholder(id))
        .map(|id| id.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_windows_file_has_a_directory() {
        assert_eq!(
            parent_dir("C:\\work\\project\\file.rs"),
            Some("C:\\work\\project".into())
        );
        assert_eq!(parent_dir("C:\\file.rs"), Some("C:\\".into()));
    }

    #[test]
    fn plain_click_replaces_selection() {
        let (next, anchor) = apply_selection(
            &["/a".into(), "/b".into()],
            Some("/a"),
            &["/a".into(), "/b".into(), "/c".into()],
            "/c",
            SelectGesture::Replace,
        );
        assert_eq!(next, vec!["/c".to_string()]);
        assert_eq!(anchor.as_deref(), Some("/c"));
    }

    #[test]
    fn ctrl_click_toggles() {
        let visible = ["/a".into(), "/b".into()];
        let (next, _) = apply_selection(
            &["/a".into()],
            Some("/a"),
            &visible,
            "/b",
            SelectGesture::Toggle,
        );
        assert_eq!(next, vec!["/a".to_string(), "/b".to_string()]);
        let (next, anchor) =
            apply_selection(&next, Some("/b"), &visible, "/a", SelectGesture::Toggle);
        assert_eq!(next, vec!["/b".to_string()]);
        assert_eq!(anchor.as_deref(), Some("/b"));
    }

    #[test]
    fn shift_click_selects_inclusive_range_and_keeps_anchor() {
        let visible = vec!["/a".into(), "/b".into(), "/c".into(), "/d".into()];
        let (next, anchor) = apply_selection(
            &["/b".into()],
            Some("/b"),
            &visible,
            "/d",
            SelectGesture::Range,
        );
        assert_eq!(
            next,
            vec!["/b".to_string(), "/c".to_string(), "/d".to_string()]
        );
        assert_eq!(anchor.as_deref(), Some("/b"));
        let (next, _) = apply_selection(&next, Some("/b"), &visible, "/a", SelectGesture::Range);
        assert_eq!(next, vec!["/a".to_string(), "/b".to_string()]);
    }

    #[test]
    fn drag_uses_the_whole_selection_only_when_the_row_is_in_it() {
        let selected = vec!["/a".into(), "/c".into()];
        assert_eq!(drag_paths(&selected, "/c"), selected);
        assert_eq!(drag_paths(&selected, "/b"), vec!["/b".to_string()]);
    }

    #[test]
    fn move_skips_self_descendants_and_same_folder() {
        assert_eq!(
            movable_sources(
                &[
                    "/proj/a".into(),
                    "/proj/dir".into(),
                    "/proj/dir/child".into()
                ],
                "/proj/dir",
            ),
            vec!["/proj/a".to_string()]
        );
        assert_eq!(
            movable_sources(&["/proj/a".into(), "/proj/b".into()], "/proj/other"),
            vec!["/proj/a".to_string(), "/proj/b".to_string()]
        );
        assert!(movable_sources(&["/proj/dir".into()], "/proj/dir/nested").is_empty());
        assert_eq!(
            movable_sources(&["/tmp".into()], "/tmp2"),
            vec!["/tmp".to_string()]
        );
        assert!(movable_sources(&["/tmp/file".into()], "/tmp").is_empty());
    }

    #[test]
    fn copy_allows_same_folder_but_not_into_self() {
        assert_eq!(
            copyable_sources(&["/proj/a".into()], "/proj"),
            vec!["/proj/a".to_string()]
        );
        assert!(copyable_sources(&["/proj/dir".into()], "/proj/dir/nested").is_empty());
    }

    #[test]
    fn copy_path_text_is_absolute_and_newline_separated() {
        assert_eq!(
            absolute_paths_text(&["/proj/a".into(), "/proj/b".into()]),
            "/proj/a\n/proj/b"
        );
    }

    #[test]
    fn terminal_titles_are_plain_numbers() {
        use crate::gui::pane::{SessionTabTitle, terminal_title};

        assert_eq!(terminal_title(1), "1");
        assert_eq!(terminal_title(2), "2");
        assert_eq!(terminal_title(12), "12");
        let title = SessionTabTitle::numbered(3);
        assert_eq!(title.label, "3");
        assert!(!title.custom);
        assert!(title.workspace_id.is_none());
    }

    #[test]
    fn cached_directory_stays_collapsed_until_toggled() {
        let (root, cache) = sample_tree();
        let items = build_explorer_tree(&root, &cache, &HashSet::new(), "");
        let src = find(&items, "/proj/src").unwrap();
        assert!(!src.is_expanded());
        assert!(src.is_folder());
        assert_eq!(src.children[0].id.as_ref(), "/proj/src/main.rs");
        assert!(!find(&items, "/proj/docs").unwrap().is_expanded());
    }

    #[test]
    fn only_the_toggled_directory_expands() {
        let (root, cache) = sample_tree();
        let mut expanded = HashSet::new();
        record_tree_toggle(&mut expanded, "/proj/src", true);
        record_tree_toggle(&mut expanded, "/proj/docs", true);
        record_tree_toggle(&mut expanded, "/proj/docs", false);
        record_tree_toggle(&mut expanded, "/proj/src/.", true);
        let items = build_explorer_tree(&root, &cache, &expanded, "");
        assert!(find(&items, "/proj/src").unwrap().is_expanded());
        assert!(!find(&items, "/proj/docs").unwrap().is_expanded());
        assert!(!expanded.iter().any(|path| is_placeholder(path)));
    }

    #[test]
    fn filter_only_matches_direct_children_and_keeps_nested_rows() {
        let (root, cache) = sample_tree();
        let expanded = HashSet::from(["/proj/src".to_string()]);
        let items = build_explorer_tree(&root, &cache, &expanded, "SRC");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id.as_ref(), "/proj/src");
        assert!(find(&items, "/proj/src/main.rs").is_some());
        assert!(build_explorer_tree(&root, &cache, &expanded, "main").is_empty());
        assert_eq!(
            build_explorer_tree(&root, &cache, &expanded, "").len(),
            cache[&root].len()
        );
    }

    #[test]
    fn unlisted_directory_is_still_a_folder() {
        let root = "/proj";
        let mut cache = HashMap::new();
        cache.insert(
            root.into(),
            vec![FsEntry {
                name: "src".into(),
                path: "/proj/src".into(),
                kind: FsKind::Dir,
                size: None,
                target_kind: None,
            }],
        );
        let expanded = HashSet::from(["/proj/src".to_string()]);
        let items = build_explorer_tree(root, &cache, &expanded, "");
        let src = find(&items, "/proj/src").unwrap();
        assert!(src.is_expanded());
        assert!(src.is_folder());
        assert!(is_placeholder(src.children[0].id.as_ref()));
    }

    #[test]
    fn listed_empty_directory_is_not_a_tree_folder() {
        let root = "/proj";
        let mut cache = HashMap::new();
        cache.insert(
            root.into(),
            vec![FsEntry {
                name: "empty".into(),
                path: "/proj/empty".into(),
                kind: FsKind::Dir,
                size: None,
                target_kind: None,
            }],
        );
        cache.insert("/proj/empty".into(), Vec::new());
        let items = build_explorer_tree(root, &cache, &HashSet::new(), "");
        let empty = find(&items, "/proj/empty").unwrap();
        assert!(!empty.is_folder());
        assert!(empty.children.is_empty());
        assert_eq!(entry_kinds(&cache).get("/proj/empty"), Some(&FsKind::Dir));
    }

    #[test]
    fn folder_symlinks_expand_and_other_links_do_not() {
        let link = |name: &str, target: Option<FsKind>| FsEntry {
            name: name.into(),
            path: format!("/proj/{name}"),
            kind: FsKind::Symlink,
            size: None,
            target_kind: target,
        };
        let mut cache = HashMap::new();
        cache.insert(
            "/proj".to_string(),
            vec![
                link("dirlink", Some(FsKind::Dir)),
                link("filelink", Some(FsKind::File)),
                link("escape", None),
            ],
        );
        let items = build_explorer_tree("/proj", &cache, &HashSet::new(), "");
        assert!(find(&items, "/proj/dirlink").unwrap().is_folder());
        assert!(!find(&items, "/proj/filelink").unwrap().is_folder());
        assert!(!find(&items, "/proj/escape").unwrap().is_folder());
        let kinds = entry_kinds(&cache);
        assert_eq!(kinds["/proj/dirlink"], FsKind::Dir);
        assert_eq!(kinds["/proj/escape"], FsKind::Symlink);
    }

    #[test]
    fn deleted_folders_leave_the_open_set() {
        let (_, cache) = sample_tree();
        let mut expanded: HashSet<String> =
            ["/proj/src", "/proj/gone", "/elsewhere/unlisted/child"]
                .into_iter()
                .map(String::from)
                .collect();
        prune_expanded(&mut expanded, &cache);
        assert!(expanded.contains("/proj/src"));
        assert!(!expanded.contains("/proj/gone"));
        assert!(expanded.contains("/elsewhere/unlisted/child"));
    }

    #[test]
    fn listing_through_a_symlink_stays_under_the_link() {
        let child = FsEntry {
            name: "x.rs".into(),
            path: "/real/target/x.rs".into(),
            kind: FsKind::File,
            size: Some(1),
            target_kind: None,
        };
        let (key, entries) = rebase_listing("/proj/dirlink", "/real/target", vec![child.clone()]);
        assert_eq!(key, "/proj/dirlink");
        assert_eq!(entries[0].path, "/proj/dirlink/x.rs");

        let (key, entries) = rebase_listing(r"C:\proj\link", r"C:\real", vec![child.clone()]);
        assert_eq!(key, r"C:\proj\link");
        assert_eq!(entries[0].path, r"C:\proj\link\x.rs");

        let (key, entries) = rebase_listing("/real/target", "/real/target", vec![child.clone()]);
        assert_eq!(key, "/real/target");
        assert_eq!(entries[0].path, child.path);
        let (key, _) = rebase_listing("", "/root", vec![]);
        assert_eq!(key, "/root");
    }

    #[test]
    fn nested_rows_indent_by_a_full_gutter() {
        assert_eq!(tree_row_indent_px(0), 4.);
        assert_eq!(tree_row_indent_px(1) - tree_row_indent_px(0), TREE_GUTTER_PX);
        assert!(tree_row_indent_px(1) > TREE_GUTTER_PX);
    }

    #[test]
    fn new_file_names_skip_names_already_present() {
        assert_eq!(unused_file_name(&["readme"]), "untitled");
        assert_eq!(
            unused_file_name(&["untitled", "untitled-2"]),
            "untitled-3"
        );
        assert_eq!(untitled_editor_key("7"), "untitled:7");
        assert!(is_untitled_editor_key("untitled:7"));
        assert!(!is_untitled_editor_key("/tmp/untitled"));
        assert_eq!(
            save_target_path("/work/proj", "notes.rs"),
            "/work/proj/notes.rs"
        );
        assert_eq!(
            save_target_path("/work/proj", "/tmp/other.rs"),
            "/tmp/other.rs"
        );
        assert_eq!(save_target_path(r"C:\work", "a.txt"), r"C:\work\a.txt");
        assert_eq!(
            pick_goto_target("lib.rs", &["/proj/src/lib.rs", "/proj/src/main.rs"]),
            "/proj/src/lib.rs"
        );
        assert_eq!(
            pick_goto_target("src/main.rs:12", &["/proj/src/main.rs"]),
            "src/main.rs:12"
        );
    }

    #[test]
    fn placeholder_rows_are_not_real_ids() {
        assert!(is_placeholder("/proj/src/."));
        assert_eq!(
            real_ids(["/proj/src", "/proj/src/.", "/proj/src/main.rs"].into_iter()),
            vec!["/proj/src".to_string(), "/proj/src/main.rs".to_string()]
        );
    }

    fn sample_tree() -> (String, HashMap<String, Vec<FsEntry>>) {
        let root = "/proj".to_string();
        let mut cache = HashMap::new();
        cache.insert(
            root.clone(),
            vec![
                FsEntry {
                    name: "src".into(),
                    path: "/proj/src".into(),
                    kind: FsKind::Dir,
                    size: None,
                    target_kind: None,
                },
                FsEntry {
                    name: "docs".into(),
                    path: "/proj/docs".into(),
                    kind: FsKind::Dir,
                    size: None,
                    target_kind: None,
                },
                FsEntry {
                    name: "README.md".into(),
                    path: "/proj/README.md".into(),
                    kind: FsKind::File,
                    size: Some(12),
                    target_kind: None,
                },
            ],
        );
        cache.insert(
            "/proj/src".into(),
            vec![FsEntry {
                name: "main.rs".into(),
                path: "/proj/src/main.rs".into(),
                kind: FsKind::File,
                size: Some(4),
                target_kind: None,
            }],
        );
        cache.insert(
            "/proj/docs".into(),
            vec![FsEntry {
                name: "guide.md".into(),
                path: "/proj/docs/guide.md".into(),
                kind: FsKind::File,
                size: Some(8),
                target_kind: None,
            }],
        );
        (root, cache)
    }

    fn find<'a>(items: &'a [TreeItem], id: &str) -> Option<&'a TreeItem> {
        for item in items {
            if item.id.as_ref() == id {
                return Some(item);
            }
            if let Some(found) = find(&item.children, id) {
                return Some(found);
            }
        }
        None
    }
}

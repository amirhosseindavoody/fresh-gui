//! Explorer selection and path rules.
//!
//! The gpui tree keeps a single selected row. Multi-select, drag sources, and
//! move/copy filtering live here so the workspace can apply them on click
//! without a second tree widget.

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
    let path = path.trim_end_matches('/');
    if path.is_empty() || path == "/" {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/".to_string()),
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
    fn placeholder_rows_are_not_real_ids() {
        assert!(is_placeholder("/proj/src/."));
        assert_eq!(
            real_ids(
                [
                    "/proj/src".into(),
                    "/proj/src/.".into(),
                    "/proj/src/main.rs"
                ]
                .into_iter()
            ),
            vec!["/proj/src".to_string(), "/proj/src/main.rs".to_string()]
        );
    }
}

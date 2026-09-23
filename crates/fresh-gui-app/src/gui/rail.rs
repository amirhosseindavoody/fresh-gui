//! Presentation helpers for the GPUI workspace rail.
//!
//! The rail is a spaces list: a section title, a two-line project row, and a
//! short hint when the daemon has nothing to switch between. Path shortening
//! lives here so the row layout does not depend on a window.

use super::paths::display_path;

/// Wider than the first 168px rail so a name and a root fit on two lines
/// without loosening the rest of the chrome. Kept inside a dense Zed / VS Code
/// sidebar (about 220–240px).
pub const WORKSPACE_RAIL_W: f32 = 232.;

/// Two text lines plus vertical padding. Explorer rows stay 22px; this is the
/// spaces row, which has a secondary line.
pub const WORKSPACE_ROW_H: f32 = 40.;

/// `text_xs` budget inside [`WORKSPACE_RAIL_W`] after the accent and padding.
pub const WORKSPACE_ROOT_LABEL_MAX: usize = 34;

/// Secondary line for one workspace row.
///
/// Home is folded to `~`. A long path keeps both ends and ellipsizes the
/// middle. An empty root is the daemon default and is labeled as such.
pub fn workspace_root_label(root: &str, home: Option<&str>) -> String {
    let root = display_path(root);
    if root.is_empty() {
        return "Default root".to_string();
    }
    let root = if root == "/" || root == "\\" {
        root
    } else {
        root.trim_end_matches(['/', '\\']).to_string()
    };
    if root.is_empty() {
        return "Default root".to_string();
    }
    let display = fold_home(&root, home);
    truncate_middle(&display, WORKSPACE_ROOT_LABEL_MAX)
}

/// The left rail is the workspace manager. A daemon that advertises
/// `workspace` gets the full list. An older daemon still shows one row for
/// the session root so a remote window is not a blank activity bar.
pub fn show_workspace_rail(
    workspace_cap: bool,
    online: bool,
    preferred_root: Option<&str>,
) -> bool {
    if workspace_cap {
        return true;
    }
    online && preferred_root.is_some_and(|root| !root.trim().is_empty())
}

/// Where a new shell starts. An explicit cwd (OSC 7 after the user `cd`s)
/// wins. Otherwise the workspace or session root.
pub fn choose_shell_cwd(explicit: Option<&str>, workspace_root: Option<&str>) -> Option<String> {
    fn nonempty(value: Option<&str>) -> Option<String> {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }
    nonempty(explicit).or_else(|| nonempty(workspace_root))
}

/// Footer copy when the list would otherwise look like unused chrome.
pub fn workspace_rail_hint(count: usize) -> Option<&'static str> {
    match count {
        0 => Some("No projects yet. + creates one."),
        1 => Some("One project open. + adds another."),
        _ => None,
    }
}

/// Shown under the name field while it is blank. The daemon already turns an
/// empty name into the root basename; this only previews that.
pub fn empty_workspace_name_hint(root: &str) -> String {
    let root = display_path(root);
    if root.is_empty() {
        return "Leave empty to use the daemon project folder.".to_string();
    }
    match path_basename(&root) {
        Some(base) => format!("Leave empty to use \"{base}\"."),
        None => "Leave empty to use the folder name.".to_string(),
    }
}

pub fn path_basename(path: &str) -> Option<&str> {
    path.rsplit(['/', '\\']).find(|seg| !seg.is_empty())
}

pub fn user_home() -> Option<String> {
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key) {
            let value = value.trim().trim_end_matches(['/', '\\']).to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    None
}

/// Explorer header: the open folder's name, never the `\\?\` form.
pub fn explorer_header_label(root: &str) -> String {
    let shown = display_path(root);
    if shown.is_empty() {
        return "Explorer".to_string();
    }
    path_basename(&shown).unwrap_or("Explorer").to_string()
}

fn fold_home(path: &str, home: Option<&str>) -> String {
    let Some(home) = home.map(display_path).filter(|home| !home.is_empty()) else {
        return path.to_string();
    };
    let home = home.trim_end_matches(['/', '\\']);
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    match path.strip_prefix(home) {
        Some(rest) if rest.starts_with('/') || rest.starts_with('\\') => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn truncate_middle(text: &str, max_chars: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if max_chars == 0 || chars.len() <= max_chars {
        return text.to_string();
    }
    if max_chars == 1 {
        return "…".to_string();
    }
    let keep = max_chars - 1;
    let head = keep / 2;
    let tail = keep - head;
    let start: String = chars.iter().take(head).collect();
    let end: String = chars.iter().skip(chars.len() - tail).collect();
    format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rail_width_stays_in_the_dense_sidebar_range() {
        assert!((220.0..=240.0).contains(&WORKSPACE_RAIL_W));
        const { assert!(WORKSPACE_ROW_H >= 36.0) };
        const { assert!(WORKSPACE_ROOT_LABEL_MAX >= 24) };
    }

    #[test]
    fn empty_root_reads_as_the_daemon_default() {
        assert_eq!(workspace_root_label("", None), "Default root");
        assert_eq!(
            workspace_root_label("   ", Some("/home/me")),
            "Default root"
        );
    }

    #[test]
    fn verbatim_windows_paths_display_as_normal_paths() {
        assert_eq!(
            workspace_root_label(r"\\?\C:\Users\jondoe", None),
            r"C:\Users\jondoe"
        );
        assert_eq!(
            workspace_root_label(r"\\?\C:\Users\me\proj", Some(r"C:\Users\me")),
            r"~\proj"
        );
        assert_eq!(
            workspace_root_label(r"\\?\C:\Users\me\proj", Some(r"\\?\C:\Users\me")),
            r"~\proj"
        );
        assert_eq!(explorer_header_label(r"\\?\C:\Users\jondoe"), "jondoe");
        assert_eq!(explorer_header_label(""), "Explorer");
        assert_eq!(
            empty_workspace_name_hint(r"\\?\C:\work\demo"),
            "Leave empty to use \"demo\"."
        );
    }

    #[test]
    fn folds_home_and_leaves_sibling_prefixes_alone() {
        assert_eq!(
            workspace_root_label("/home/me/fresh-gui", Some("/home/me")),
            "~/fresh-gui"
        );
        assert_eq!(workspace_root_label("/home/me/", Some("/home/me")), "~");
        assert_eq!(
            workspace_root_label("/home/me2/fresh-gui", Some("/home/me")),
            "/home/me2/fresh-gui"
        );
        assert_eq!(
            workspace_root_label(r"C:\Users\me\proj", Some(r"C:\Users\me")),
            r"~\proj"
        );
    }

    #[test]
    fn long_paths_keep_both_ends() {
        let path = format!("/home/me/{}", "a".repeat(40));
        let label = workspace_root_label(&path, Some("/home/me"));
        assert_eq!(label.chars().count(), WORKSPACE_ROOT_LABEL_MAX);
        assert!(label.starts_with("~/"));
        assert!(label.contains('…'));
        assert!(label.ends_with('a'));
    }

    #[test]
    fn hint_only_when_there_is_nothing_to_switch() {
        assert_eq!(
            workspace_rail_hint(0),
            Some("No projects yet. + creates one.")
        );
        assert_eq!(
            workspace_rail_hint(1),
            Some("One project open. + adds another.")
        );
        assert_eq!(workspace_rail_hint(2), None);
        assert_eq!(workspace_rail_hint(6), None);
    }

    #[test]
    fn rail_shows_for_a_session_root_without_the_workspace_capability() {
        assert!(show_workspace_rail(true, false, None));
        assert!(!show_workspace_rail(false, true, None));
        assert!(!show_workspace_rail(false, false, Some("/work/demo")));
        assert!(show_workspace_rail(false, true, Some("/work/demo")));
        assert!(!show_workspace_rail(false, true, Some("  ")));
    }

    #[test]
    fn shell_cwd_uses_the_workspace_when_the_user_has_not_cd() {
        assert_eq!(
            choose_shell_cwd(Some("/tmp/elsewhere"), Some("/work/demo")),
            Some("/tmp/elsewhere".into())
        );
        assert_eq!(
            choose_shell_cwd(None, Some("/work/demo")),
            Some("/work/demo".into())
        );
        assert_eq!(
            choose_shell_cwd(Some("  "), Some("/work/demo")),
            Some("/work/demo".into())
        );
        assert_eq!(choose_shell_cwd(None, Some(" ")), None);
    }

    #[test]
    fn empty_name_previews_the_daemon_basename() {
        assert_eq!(
            empty_workspace_name_hint("/work/fresh-gui"),
            "Leave empty to use \"fresh-gui\"."
        );
        assert_eq!(
            empty_workspace_name_hint(r"C:\work\demo\"),
            "Leave empty to use \"demo\"."
        );
        assert_eq!(
            empty_workspace_name_hint("  "),
            "Leave empty to use the daemon project folder."
        );
    }
}

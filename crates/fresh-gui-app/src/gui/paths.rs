//! User-visible paths.
//!
//! Windows `canonicalize` stores the extended-length form (`\\?\C:\...`,
//! `\\?\UNC\server\share`). Win32 calls may keep that prefix. Anything a
//! person reads — explorer, tab titles, the workspace rail, status text —
//! should be a normal path.

/// `\\?\` and `//?/` prefixes that introduce a normal drive or UNC path.
const VERBATIM_UNC: &[(&str, &str)] = &[
    (r"\\?\UNC\", r"\\"),
    (r"\\?\UNC/", r"\\"),
    (r"//?/UNC/", r"\\"),
    (r"//?/UNC\", r"\\"),
];

const VERBATIM_DISK: &[&str] = &[r"\\?\", r"//?/"];

/// A single path for display. Drive and UNC verbatim prefixes are removed.
/// Device paths (`\\?\Volume{...}`) and ordinary paths are left alone.
pub fn display_path(path: &str) -> String {
    let path = path.trim();
    match strip_one_verbatim(path) {
        Some(stripped) => stripped,
        None => path.to_string(),
    }
}

/// Replace verbatim prefixes wherever they appear, including inside a status
/// line that quotes a path.
pub fn strip_verbatim_prefixes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((consumed, replacement)) = match_verbatim(rest) {
            out.push_str(replacement);
            rest = &rest[consumed..];
            continue;
        }
        let ch = rest.chars().next().expect("rest is non-empty");
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }
    out
}

fn strip_one_verbatim(path: &str) -> Option<String> {
    let (consumed, replacement) = match_verbatim(path)?;
    Some(format!("{replacement}{}", &path[consumed..]))
}

/// If `text` begins with a verbatim prefix, how many bytes to drop and what
/// to write instead. `None` when the prefix is absent or not a drive/UNC path.
fn match_verbatim(text: &str) -> Option<(usize, &'static str)> {
    for (prefix, replacement) in VERBATIM_UNC {
        if text.starts_with(prefix) {
            return Some((prefix.len(), replacement));
        }
    }
    for prefix in VERBATIM_DISK {
        let Some(rest) = text.strip_prefix(prefix) else {
            continue;
        };
        let mut chars = rest.chars();
        let (Some(letter), Some(':')) = (chars.next(), chars.next()) else {
            continue;
        };
        if letter.is_ascii_alphabetic() {
            return Some((prefix.len(), ""));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_verbatim_disk_and_unc_prefixes() {
        assert_eq!(display_path(r"\\?\C:\Users\jondoe"), r"C:\Users\jondoe");
        assert_eq!(display_path(r"\\?\c:\work"), r"c:\work");
        assert_eq!(display_path(r"//?/D:/proj"), "D:/proj");
        assert_eq!(
            display_path(r"\\?\UNC\server\share\dir"),
            r"\\server\share\dir"
        );
        assert_eq!(display_path(r"//?/UNC/server/share"), r"\\server/share");
    }

    #[test]
    fn leaves_ordinary_and_device_paths_alone() {
        assert_eq!(display_path(r"C:\Users\jondoe"), r"C:\Users\jondoe");
        assert_eq!(display_path("/home/me/proj"), "/home/me/proj");
        assert_eq!(display_path(""), "");
        assert_eq!(
            display_path(r"\\?\Volume{abc}\work"),
            r"\\?\Volume{abc}\work"
        );
        assert_eq!(display_path(r"\\server\share"), r"\\server\share");
    }

    #[test]
    fn strips_prefixes_embedded_in_status_text() {
        assert_eq!(
            strip_verbatim_prefixes(r"pty_open_failed: \\?\C:\Users\jondoe"),
            r"pty_open_failed: C:\Users\jondoe"
        );
        assert_eq!(
            strip_verbatim_prefixes(r"root \\?\UNC\host\share is missing"),
            r"root \\host\share is missing"
        );
        assert_eq!(strip_verbatim_prefixes("Online"), "Online");
    }
}

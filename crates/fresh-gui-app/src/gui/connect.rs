//! Parse host connect strings (ws URL, printed Local access URL, token).

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectTarget {
    pub ws_url: String,
    pub token: Option<String>,
    /// Shown in the title bar (SSH destination). The WebSocket URL stays local.
    pub label: Option<String>,
    /// Absolute project directory to focus or create after connect.
    /// Set when `fresh-gui /path` attaches to a daemon that is already running.
    pub preferred_root: Option<String>,
}

/// Turn a CLI `--backend` plus optional `--token` into a WebSocket URL.
///
/// Accepts `ws://…/ws`, `http://127.0.0.1:7420/?token=…` (the daemon banner
/// URL), or `host:port`. Query `token` fills in when `--token` is omitted.
pub fn parse_connect_target(backend: &str, cli_token: Option<String>) -> ConnectTarget {
    let trimmed = backend.trim();
    let (base, query_token) = split_query_token(trimmed);
    let token = cli_token.or(query_token);
    let ws_url = to_ws_url(&base);
    ConnectTarget {
        ws_url,
        token,
        label: None,
        preferred_root: None,
    }
}

impl ConnectTarget {
    /// Title-bar text. SSH sessions show the destination next to the tunnel URL.
    pub fn chrome_label(&self) -> String {
        match &self.label {
            Some(label) if !label.is_empty() => format!("{label}  {}", self.ws_url),
            _ => self.ws_url.clone(),
        }
    }
}

/// Split `path[:line[:col]]` the way the command-palette Go to File prompt does.
pub fn parse_goto_spec(query: &str) -> (String, Option<u32>, Option<u32>) {
    let query = query.trim();
    if query.is_empty() {
        return (String::new(), None, None);
    }
    let mut segs = Vec::new();
    let mut end = query.len();
    while segs.len() < 2 {
        match query[..end].rfind(':') {
            Some(i) => {
                let part = &query[i + 1..end];
                if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                    break;
                }
                segs.push(part.parse::<u32>().ok());
                end = i;
            }
            None => break,
        }
    }
    let path = query[..end].to_string();
    segs.reverse();
    let line = segs.first().copied().flatten();
    let column = segs.get(1).copied().flatten();
    (path, line, column)
}

fn split_query_token(input: &str) -> (String, Option<String>) {
    let Some((base, query)) = input.split_once('?') else {
        return (input.to_string(), None);
    };
    let mut token = None;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("token=") {
            let value = value.split('#').next().unwrap_or(value);
            if !value.is_empty() {
                token = Some(percent_decode(value));
            }
        }
    }
    (base.to_string(), token)
}

fn to_ws_url(base: &str) -> String {
    let base = base.trim_end_matches('/');
    if base.starts_with("ws://") || base.starts_with("wss://") {
        if base.ends_with("/ws") {
            base.to_string()
        } else {
            format!("{base}/ws")
        }
    } else if let Some(rest) = base.strip_prefix("http://") {
        ws_from_http("ws://", rest)
    } else if let Some(rest) = base.strip_prefix("https://") {
        ws_from_http("wss://", rest)
    } else if base.contains("://") {
        base.to_string()
    } else {
        format!("ws://{base}/ws")
    }
}

fn ws_from_http(scheme: &str, rest: &str) -> String {
    let host = rest.trim_end_matches('/');
    if host.ends_with("/ws") {
        format!("{scheme}{host}")
    } else {
        format!("{scheme}{host}/ws")
    }
}

/// What the host should do with the workspace list before attaching.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootAction {
    /// Switch to an existing workspace (creates nothing).
    Switch { id: String },
    /// Create a workspace, then switch to it. `None` root uses the daemon FS root.
    Create { root: Option<String> },
}

/// Pick the workspace to show. A preferred root (from `fresh-gui /path` while
/// a session is already up) switches to a matching workspace or creates one.
pub fn plan_workspace_boot(
    workspaces: &[fresh_gui_protocol::WorkspaceInfo],
    focused_id: Option<&str>,
    preferred_root: Option<&str>,
) -> BootAction {
    let preferred = preferred_root
        .map(str::trim)
        .filter(|root| !root.is_empty());
    if let Some(root) = preferred {
        if let Some(found) = workspaces
            .iter()
            .find(|workspace| roots_equal(&workspace.root, root))
        {
            return BootAction::Switch {
                id: found.id.clone(),
            };
        }
        return BootAction::Create {
            root: Some(root.to_string()),
        };
    }
    if workspaces.is_empty() {
        return BootAction::Create { root: None };
    }
    let fallback = &workspaces[0].id;
    let id = focused_id
        .filter(|id| workspaces.iter().any(|workspace| workspace.id == *id))
        .unwrap_or(fallback);
    BootAction::Switch { id: id.to_string() }
}

fn roots_equal(left: &str, right: &str) -> bool {
    let unix = left.starts_with('/') || right.starts_with('/');
    normalize_root(left, unix) == normalize_root(right, unix)
}

fn normalize_root(path: &str, unix: bool) -> String {
    let trimmed = path.trim();
    let stripped = trimmed.trim_end_matches(['/', '\\']);
    let stripped = if stripped.is_empty() {
        trimmed
    } else {
        stripped
    };
    if unix {
        stripped.to_string()
    } else {
        stripped.replace('\\', "/").to_ascii_lowercase()
    }
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(v as char);
            i += 3;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printed_http_url_becomes_ws_and_token() {
        let t = parse_connect_target("http://127.0.0.1:7420/?token=abc%2Fdef", None);
        assert_eq!(t.ws_url, "ws://127.0.0.1:7420/ws");
        assert_eq!(t.token.as_deref(), Some("abc/def"));
    }

    #[test]
    fn explicit_token_wins_over_query() {
        let t = parse_connect_target("ws://127.0.0.1:7420/ws?token=fromurl", Some("cli".into()));
        assert_eq!(t.ws_url, "ws://127.0.0.1:7420/ws");
        assert_eq!(t.token.as_deref(), Some("cli"));
    }

    #[test]
    fn host_port_defaults_to_ws_path() {
        let t = parse_connect_target("127.0.0.1:7420", None);
        assert_eq!(t.ws_url, "ws://127.0.0.1:7420/ws");
        assert!(t.preferred_root.is_none());
    }

    #[test]
    fn preferred_root_switches_or_creates() {
        use fresh_gui_protocol::WorkspaceInfo;
        let workspaces = vec![WorkspaceInfo {
            id: "a".into(),
            name: "alpha".into(),
            root: "/work/alpha".into(),
            session_id: "s".into(),
            pty_count: 0,
            tab_count: 0,
        }];
        assert_eq!(
            plan_workspace_boot(&workspaces, Some("missing"), None),
            BootAction::Switch { id: "a".into() }
        );
        assert_eq!(
            plan_workspace_boot(&workspaces, Some("a"), Some("/work/alpha/")),
            BootAction::Switch { id: "a".into() }
        );
        assert_eq!(
            plan_workspace_boot(&workspaces, Some("a"), Some("/work/beta")),
            BootAction::Create {
                root: Some("/work/beta".into())
            }
        );
        assert_eq!(
            plan_workspace_boot(&[], None, None),
            BootAction::Create { root: None }
        );
    }

    #[test]
    fn unix_roots_keep_case_even_on_windows_client() {
        assert!(!roots_equal("/Work/Project", "/work/project"));
        assert!(roots_equal(r"C:\Work\Project", "c:/work/project/"));
    }

    #[test]
    fn host_port_has_no_token() {
        let t = parse_connect_target("127.0.0.1:7420", None);
        assert!(t.token.is_none());
    }

    #[test]
    fn goto_spec_splits_line_and_column() {
        assert_eq!(
            parse_goto_spec("/tmp/a.rs:12:4"),
            ("/tmp/a.rs".into(), Some(12), Some(4))
        );
        assert_eq!(
            parse_goto_spec("src/main.rs:9"),
            ("src/main.rs".into(), Some(9), None)
        );
        assert_eq!(parse_goto_spec("README"), ("README".into(), None, None));
    }
}

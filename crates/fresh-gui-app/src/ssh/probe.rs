//! Remote install/session probe and small parsers shared with bootstrap.

use anyhow::{Context, Result, bail};

/// POSIX `sh` script. Prints one `FRESH_GUI_PROBE` line on stdout.
///
/// Looks for `~/.local/bin/fresh-gui` first (the path we install), then
/// `PATH`. A live session is `session.json` whose pid still answers `kill -0`,
/// matching the daemon's runtime dir (`$XDG_RUNTIME_DIR/fresh-gui` or
/// `/tmp/fresh-gui-$UID`).
pub const PROBE_SCRIPT: &str = r#"set -eu
bin=""
if [ -x "$HOME/.local/bin/fresh-gui" ]; then
  bin="$HOME/.local/bin/fresh-gui"
elif command -v fresh-gui >/dev/null 2>&1; then
  bin=$(command -v fresh-gui)
fi
if [ -n "${XDG_RUNTIME_DIR:-}" ]; then
  meta="${XDG_RUNTIME_DIR}/fresh-gui/session.json"
else
  meta="/tmp/fresh-gui-$(id -u)/session.json"
fi
installed=0
running=0
port=""
token=""
if [ -n "$bin" ]; then
  installed=1
fi
if [ -f "$meta" ]; then
  pid=$(sed -n 's/.*"pid"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$meta" | head -n 1)
  if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
    running=1
    token=$(sed -n 's/.*"token"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$meta" | head -n 1)
    bound=$(sed -n 's/.*"bound"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$meta" | head -n 1)
    case "$bound" in
      *:*) port=${bound##*:} ;;
    esac
  fi
fi
echo "FRESH_GUI_PROBE v=1 installed=${installed} running=${running} port=${port} token=${token} binary=${bin}"
"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probe {
    pub installed: bool,
    pub running: bool,
    pub port: Option<u16>,
    pub token: Option<String>,
    pub binary: Option<String>,
}

pub fn parse_probe(text: &str) -> Result<Probe> {
    let line = text
        .lines()
        .rev()
        .find(|line| line.starts_with("FRESH_GUI_PROBE "))
        .context("remote probe did not print a FRESH_GUI_PROBE line")?;
    let Some(binary_at) = line.find(" binary=") else {
        bail!("probe line is missing binary=");
    };
    let binary = {
        let raw = &line[binary_at + " binary=".len()..];
        if raw.is_empty() {
            None
        } else {
            Some(raw.to_string())
        }
    };
    let mut installed = false;
    let mut running = false;
    let mut port = None;
    let mut token = None;
    for part in line[..binary_at].split_whitespace() {
        if let Some(v) = part.strip_prefix("installed=") {
            installed = v == "1";
        } else if let Some(v) = part.strip_prefix("running=") {
            running = v == "1";
        } else if let Some(v) = part.strip_prefix("port=").filter(|v| !v.is_empty()) {
            port = Some(
                v.parse::<u16>()
                    .with_context(|| format!("probe port {v}"))?,
            );
        } else if let Some(v) = part.strip_prefix("token=").filter(|v| !v.is_empty()) {
            token = Some(v.to_string());
        }
    }
    Ok(Probe {
        installed,
        running,
        port,
        token,
        binary,
    })
}

/// `user@host` or an OpenSSH `Host` alias. No shell metacharacters — the
/// value is passed as one argv to `ssh` / `scp`.
pub fn validate_destination(dest: &str) -> Result<()> {
    let dest = dest.trim();
    if dest.is_empty() {
        bail!("SSH destination is empty");
    }
    if dest.starts_with('-') || dest.chars().any(|c| c.is_whitespace() || c == '\0') {
        bail!("SSH destination {dest:?} is not a user@host or Host alias");
    }
    if has_unbracketed_colon(dest) {
        bail!("SSH destination must not include a path (no ':' outside brackets)");
    }
    if let Some((user, host)) = dest.split_once('@') {
        if user.is_empty() || host.is_empty() || host.contains('@') {
            bail!("SSH destination must be user@host or an OpenSSH Host alias");
        }
        if !is_user(user) || !is_host(host) {
            bail!("SSH destination {dest:?} has unsupported characters");
        }
    } else if !is_host_alias(dest) {
        bail!("SSH destination {dest:?} is not a user@host or Host alias");
    }
    Ok(())
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
    {
        bail!("target name must be letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

pub fn shell_single_quote(s: &str) -> String {
    let mut out = String::from("'");
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// Hide ADE tokens if a daemon banner is shown after a failed start.
pub fn redact_secrets(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(idx) = find_token_key(rest) {
        out.push_str(&rest[..idx]);
        let after_key = if rest[idx..].starts_with("?token=") {
            out.push_str("?token=");
            idx + "?token=".len()
        } else {
            out.push_str("token=");
            idx + "token=".len()
        };
        out.push_str("<redacted>");
        let value = &rest[after_key..];
        let skip = value
            .find(|c: char| c.is_whitespace() || c == '&' || c == '"' || c == '\'')
            .unwrap_or(value.len());
        rest = &value[skip..];
    }
    out.push_str(rest);
    out
}

fn find_token_key(text: &str) -> Option<usize> {
    let mut search = text;
    let mut base = 0;
    while !search.is_empty() {
        if let Some(rel) = search.find("token=") {
            let abs = base + rel;
            let query = rel >= 1 && search.as_bytes()[rel - 1] == b'?';
            let start = if query { abs - 1 } else { abs };
            let boundary_ok = if query {
                true
            } else {
                rel == 0
                    || (!search.as_bytes()[rel - 1].is_ascii_alphanumeric()
                        && search.as_bytes()[rel - 1] != b'_')
            };
            if boundary_ok {
                return Some(start);
            }
            let next = rel + "token=".len();
            base += next;
            search = &search[next..];
            continue;
        }
        break;
    }
    None
}

pub fn linux_gnu_asset_url(release_json: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(release_json).context("parse GitHub release JSON")?;
    let assets = value
        .get("assets")
        .and_then(|v| v.as_array())
        .context("GitHub release JSON has no assets array")?;
    for asset in assets {
        let name = asset.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.contains("x86_64-unknown-linux-gnu") && name.ends_with(".tar.gz") {
            return asset
                .get("browser_download_url")
                .and_then(|v| v.as_str())
                .filter(|u| !u.is_empty())
                .map(str::to_string)
                .context("linux-gnu asset is missing browser_download_url");
        }
    }
    bail!("latest release has no x86_64-unknown-linux-gnu .tar.gz asset");
}

fn has_unbracketed_colon(s: &str) -> bool {
    let mut depth = 0i32;
    for c in s.chars() {
        match c {
            '[' => depth += 1,
            ']' => depth -= 1,
            ':' if depth == 0 => return true,
            _ => {}
        }
    }
    false
}

fn is_user(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'))
}

fn is_host(s: &str) -> bool {
    if let Some(inner) = s.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
    }
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_host_alias(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::process::{Command, Stdio};

    #[test]
    fn parses_probe_line() {
        let probe = parse_probe(
            "motd\nFRESH_GUI_PROBE v=1 installed=1 running=1 port=7420 token=abc binary=/home/u/.local/bin/fresh-gui\n",
        )
        .unwrap();
        assert!(probe.installed && probe.running);
        assert_eq!(probe.port, Some(7420));
        assert_eq!(probe.token.as_deref(), Some("abc"));
        assert_eq!(
            probe.binary.as_deref(),
            Some("/home/u/.local/bin/fresh-gui")
        );
    }

    #[test]
    fn parses_missing_install_and_binary_with_spaces() {
        let probe =
            parse_probe("FRESH_GUI_PROBE v=1 installed=0 running=0 port= token= binary=").unwrap();
        assert!(!probe.installed);
        assert!(probe.port.is_none() && probe.token.is_none() && probe.binary.is_none());

        let spaced = parse_probe(
            "FRESH_GUI_PROBE v=1 installed=1 running=0 port= token= binary=/home/my user/bin/fresh-gui",
        )
        .unwrap();
        assert_eq!(
            spaced.binary.as_deref(),
            Some("/home/my user/bin/fresh-gui")
        );
    }

    #[test]
    fn destination_accepts_user_host_alias_and_ipv6() {
        validate_destination("ada@lab.example").unwrap();
        validate_destination("my-server").unwrap();
        validate_destination("ada@[::1]").unwrap();
        assert!(validate_destination("").is_err());
        assert!(validate_destination("-oProxyCommand=evil").is_err());
        assert!(validate_destination("ada@lab:22").is_err());
        assert!(validate_destination("ada@lab:/tmp").is_err());
        assert!(validate_destination("has space").is_err());
    }

    #[test]
    fn quotes_and_redacts() {
        assert_eq!(shell_single_quote("a'b"), "'a'\\''b'");
        let banner = "open http://127.0.0.1:7420/?token=secret&x=1 token=secret\n";
        let red = redact_secrets(banner);
        assert!(!red.contains("secret"));
        assert!(red.contains("<redacted>"));
    }

    #[test]
    fn picks_linux_gnu_tarball_not_checksum() {
        let json = r#"{
            "assets": [
                {"name": "fresh-gui-1-x86_64-unknown-linux-gnu.tar.gz.sha256", "browser_download_url": "https://example.test/sum"},
                {"name": "fresh-gui-1-x86_64-unknown-linux-gnu.tar.gz", "browser_download_url": "https://example.test/daemon.tar.gz"},
                {"name": "fresh-gui-1-x86_64-pc-windows-msvc.zip", "browser_download_url": "https://example.test/win.zip"}
            ]
        }"#;
        assert_eq!(
            linux_gnu_asset_url(json).unwrap(),
            "https://example.test/daemon.tar.gz"
        );
    }

    #[test]
    fn probe_script_passes_sh_syntax_check() {
        let mut child = Command::new("sh")
            .arg("-n")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("sh");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(PROBE_SCRIPT.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "sh -n failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

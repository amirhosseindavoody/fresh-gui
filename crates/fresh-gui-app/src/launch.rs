//! Local daemon lookup and the `fresh-gui` / `fresh-gui /path` / `user@host` dispatch.
//!
//! The GPUI binary is the user-facing command. The headless ADE process stays a
//! separate executable (`fresh-gui-daemon` next to this binary, or `fresh-gui`
//! beside `fresh-gui-app` in a Cargo target dir) so remote SCP can keep shipping
//! a headless `bin/fresh-gui`.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::ssh::validate_destination;

/// One saved SSH target, enough to tell a name from a destination.
pub struct KnownRemote<'a> {
    pub name: &'a str,
    pub destination: &'a str,
}

/// Where `fresh-gui [TARGET]` should go.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchTarget {
    /// Ensure the local daemon and open the window. `root` is the project directory.
    Local { root: Option<PathBuf> },
    /// `remote connect` for a target already in `remotes.json`.
    SavedRemote {
        name: String,
        root_override: Option<String>,
    },
    /// SSH destination that is not saved. Not written to `remotes.json`.
    AdHocRemote {
        destination: String,
        root: Option<String>,
    },
}

/// Classify a positional target. An existing path wins over a saved remote
/// of the same name. `user@host` is SSH. A bare word that is neither a path
/// nor a saved remote is an error (it is not treated as an OpenSSH host alias).
pub fn classify_launch(
    target: Option<&str>,
    root_flag: Option<&str>,
    saved: &[KnownRemote<'_>],
) -> Result<LaunchTarget> {
    let target = target.map(str::trim).filter(|value| !value.is_empty());
    let root_flag = root_flag.map(str::trim).filter(|value| !value.is_empty());
    match target {
        None => Ok(LaunchTarget::Local {
            root: root_flag.map(PathBuf::from),
        }),
        Some(raw) if raw.contains('@') => classify_ssh(raw, root_flag, saved),
        Some(raw) if Path::new(raw).exists() => {
            if root_flag.is_some() {
                bail!("pass the project directory either as the argument or as --root");
            }
            Ok(LaunchTarget::Local {
                root: Some(project_root(Path::new(raw))),
            })
        }
        Some(raw) if saved.iter().any(|remote| remote.name == raw) => {
            Ok(LaunchTarget::SavedRemote {
                name: raw.to_string(),
                root_override: root_flag.map(str::to_string),
            })
        }
        Some(raw) if saved.iter().any(|remote| remote.destination == raw) => {
            let name = saved
                .iter()
                .find(|remote| remote.destination == raw)
                .map(|remote| remote.name.to_string())
                .unwrap_or_else(|| raw.to_string());
            Ok(LaunchTarget::SavedRemote {
                name,
                root_override: root_flag.map(str::to_string),
            })
        }
        Some(raw) if has_path_separator(raw) || looks_like_relative(raw) => {
            bail!("project path '{raw}' does not exist")
        }
        Some(raw) => bail!(
            "'{raw}' is not a project path or a saved remote.\n\
             Open a local directory: fresh-gui /path/to/project\n\
             Connect over SSH: fresh-gui user@host\n\
             Or save one: fresh-gui remote add <name> user@host"
        ),
    }
}

fn classify_ssh(
    raw: &str,
    root_flag: Option<&str>,
    saved: &[KnownRemote<'_>],
) -> Result<LaunchTarget> {
    validate_destination(raw)?;
    if let Some(remote) = saved.iter().find(|remote| remote.destination == raw) {
        return Ok(LaunchTarget::SavedRemote {
            name: remote.name.to_string(),
            root_override: root_flag.map(str::to_string),
        });
    }
    if let Some(remote) = saved.iter().find(|remote| remote.name == raw) {
        return Ok(LaunchTarget::SavedRemote {
            name: remote.name.to_string(),
            root_override: root_flag.map(str::to_string),
        });
    }
    Ok(LaunchTarget::AdHocRemote {
        destination: raw.to_string(),
        root: root_flag.map(str::to_string),
    })
}

fn has_path_separator(value: &str) -> bool {
    value.contains('/') || value.contains('\\')
}

fn looks_like_relative(value: &str) -> bool {
    value == "." || value == ".." || value.starts_with('.')
}

/// Directory the daemon should use. A file argument uses its parent.
pub fn project_root(path: &Path) -> PathBuf {
    if path.is_dir() {
        return path.to_path_buf();
    }
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// Name stored on an unsaved `user@host` target. `SshTarget::validate` rejects `@`.
pub fn ad_hoc_remote_name(destination: &str) -> String {
    let mut name: String = destination
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '-'
            }
        })
        .collect();
    if name.is_empty() {
        name = "remote".into();
    }
    name
}

/// Session fields the desktop command needs. Extra daemon JSON keys are ignored.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DaemonSession {
    pub ws_url: String,
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub pid: u32,
    #[serde(default)]
    pub root: String,
}

/// Result of attaching to the per-user daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnsuredSession {
    pub ws_url: String,
    pub token: Option<String>,
    pub pid: u32,
    pub started: bool,
    /// Set when a session was already running and the user named a project.
    pub preferred_root: Option<String>,
}

pub fn parse_session_json(stdout: &str) -> Result<DaemonSession> {
    let text = stdout.trim();
    if text.is_empty() {
        bail!("daemon did not return session JSON");
    }
    serde_json::from_str(text).context("daemon session JSON")
}

/// Args for a quiet start-or-attach. Token stays in the session file, not argv.
pub fn daemon_start_args(root: Option<&Path>) -> Vec<String> {
    let mut args = vec!["--json".to_string()];
    if let Some(root) = root {
        args.push("--root".into());
        args.push(root.display().to_string());
    }
    args
}

/// Locate the headless daemon for this host binary.
///
/// Order: `FRESH_GUI_DAEMON`, `fresh-gui-daemon` beside the executable, then
/// (only when this binary is `fresh-gui-app`) a sibling `fresh-gui` from
/// `cargo build`, then `fresh-gui-daemon` on `PATH`.
pub fn locate_daemon(exe: &Path, path_env: Option<&OsStr>) -> Option<PathBuf> {
    if let Some(path) = sibling(exe, daemon_file_name()) {
        return Some(path);
    }
    if is_dev_host(exe)
        && let Some(path) = sibling(exe, dev_daemon_file_name())
    {
        return Some(path);
    }
    let path_env = path_env?;
    search_path(path_env, daemon_file_name(), exe)
}

pub fn find_daemon() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os("FRESH_GUI_DAEMON") {
        let path = PathBuf::from(explicit);
        if is_usable_bin(&path) && !same_file(&path, &std::env::current_exe().unwrap_or_default()) {
            return Ok(path);
        }
        bail!(
            "FRESH_GUI_DAEMON={} is not a usable daemon binary",
            path.display()
        );
    }
    let exe = std::env::current_exe().context("current_exe")?;
    locate_daemon(&exe, std::env::var_os("PATH").as_deref()).with_context(|| {
        format!(
            "could not find the headless daemon ({}) next to {} or on PATH.\n\
             Install with the fresh-gui install script (client and daemon), or from a checkout run `cargo build -p fresh-gui`.\n\
             Connect remotely without a local daemon:\n\
             fresh-gui user@host\n\
             fresh-gui remote add <name> user@host\n\
             fresh-gui remote connect <name>",
            daemon_file_name(),
            exe.display()
        )
    })
}

/// Start the daemon when this user has no live session. Reuse one when it does.
pub fn ensure_local_session(root: Option<&Path>) -> Result<EnsuredSession> {
    let prepared = match root {
        Some(path) => Some(prepare_project(path)?),
        None => None,
    };
    let bin = find_daemon()?;
    if let Some(existing) = query_status(&bin)? {
        return Ok(EnsuredSession {
            ws_url: existing.ws_url,
            token: existing.token,
            pid: existing.pid,
            started: false,
            preferred_root: prepared.map(|path| path.display().to_string()),
        });
    }
    let started = run_json(
        &bin,
        &daemon_start_args(prepared.as_deref()),
        "start the fresh-gui daemon",
    )?;
    Ok(EnsuredSession {
        ws_url: started.ws_url,
        token: started.token,
        pid: started.pid,
        started: true,
        preferred_root: None,
    })
}

fn prepare_project(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .context("current directory")?
            .join(path)
    };
    if !absolute.exists() {
        bail!("project path {} does not exist", absolute.display());
    }
    let root = project_root(&absolute);
    match root.canonicalize() {
        Ok(canon) => Ok(canon),
        Err(_) => Ok(root),
    }
}

fn daemon_command(bin: &Path) -> Command {
    let mut cmd = Command::new(bin);
    // If this path is actually the desktop binary, it exits instead of
    // spawning itself again (a copied `fresh-gui` over `fresh-gui-daemon`).
    cmd.env("FRESH_GUI_DAEMON_CHILD", "1");
    cmd.env("FRESH_GUI_QUIET", "1");
    cmd
}

fn query_status(bin: &Path) -> Result<Option<DaemonSession>> {
    let output = daemon_command(bin)
        .args(["status", "--json"])
        .output()
        .with_context(|| format!("run {} status --json", bin.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(parse_session_json(&text)?))
}

fn run_json(bin: &Path, args: &[String], what: &str) -> Result<DaemonSession> {
    let output = daemon_command(bin)
        .args(args)
        .output()
        .with_context(|| format!("{what} ({})", bin.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stderr = stderr.trim();
        if stderr.is_empty() {
            bail!("{what} failed (exit {})", output.status);
        }
        bail!("{what} failed (exit {}): {stderr}", output.status);
    }
    let text = String::from_utf8_lossy(&output.stdout);
    parse_session_json(&text).with_context(|| what.to_string())
}

fn daemon_file_name() -> &'static str {
    if cfg!(windows) {
        "fresh-gui-daemon.exe"
    } else {
        "fresh-gui-daemon"
    }
}

fn dev_daemon_file_name() -> &'static str {
    if cfg!(windows) {
        "fresh-gui.exe"
    } else {
        "fresh-gui"
    }
}

fn is_dev_host(exe: &Path) -> bool {
    matches!(
        exe.file_name().and_then(|name| name.to_str()),
        Some("fresh-gui-app" | "fresh-gui-app.exe")
    )
}

fn sibling(exe: &Path, name: &str) -> Option<PathBuf> {
    let path = exe.parent()?.join(name);
    if is_usable_bin(&path) && !same_file(&path, exe) {
        Some(path)
    } else {
        None
    }
}

fn search_path(path_env: &OsStr, name: &str, exe: &Path) -> Option<PathBuf> {
    std::env::split_paths(path_env).find_map(|dir| {
        let path = dir.join(name);
        if is_usable_bin(&path) && !same_file(&path, exe) {
            Some(path)
        } else {
            None
        }
    })
}

fn is_usable_bin(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return path
            .metadata()
            .map(|meta| meta.permissions().mode() & 0o111 != 0)
            .unwrap_or(false);
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn same_file(left: &Path, right: &Path) -> bool {
    if left.as_os_str().is_empty() || right.as_os_str().is_empty() {
        return false;
    }
    if let (Ok(left_meta), Ok(right_meta)) = (fs::metadata(left), fs::metadata(right)) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if left_meta.dev() == right_meta.dev() && left_meta.ino() == right_meta.ino() {
                return true;
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (left_meta, right_meta);
        }
        if let (Ok(left_canon), Ok(right_canon)) = (fs::canonicalize(left), fs::canonicalize(right))
        {
            return left_canon == right_canon;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch_bin(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(path, perms).unwrap();
        }
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-launch-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_args_is_local() {
        let launch = classify_launch(None, None, &[]).unwrap();
        assert_eq!(launch, LaunchTarget::Local { root: None });
    }

    #[test]
    fn root_flag_without_target_is_local() {
        let launch = classify_launch(None, Some("/work/app"), &[]).unwrap();
        assert_eq!(
            launch,
            LaunchTarget::Local {
                root: Some(PathBuf::from("/work/app"))
            }
        );
    }

    #[test]
    fn user_at_host_is_adhoc_until_saved() {
        let launch = classify_launch(Some("ada@lab"), Some("/work"), &[]).unwrap();
        assert_eq!(
            launch,
            LaunchTarget::AdHocRemote {
                destination: "ada@lab".into(),
                root: Some("/work".into())
            }
        );
        let saved = [KnownRemote {
            name: "lab",
            destination: "ada@lab",
        }];
        let launch = classify_launch(Some("ada@lab"), None, &saved).unwrap();
        assert_eq!(
            launch,
            LaunchTarget::SavedRemote {
                name: "lab".into(),
                root_override: None
            }
        );
    }

    #[test]
    fn saved_name_and_existing_directory() {
        let dir = temp_dir("saved-name");
        let project = dir.join("lab");
        fs::create_dir_all(&project).unwrap();
        let saved = [KnownRemote {
            name: "lab-remote-f4aa",
            destination: "ada@lab",
        }];
        let launch = classify_launch(Some("lab-remote-f4aa"), None, &saved).unwrap();
        assert_eq!(
            launch,
            LaunchTarget::SavedRemote {
                name: "lab-remote-f4aa".into(),
                root_override: None
            }
        );
        let launch = classify_launch(Some(project.to_str().unwrap()), None, &saved).unwrap();
        assert_eq!(
            launch,
            LaunchTarget::Local {
                root: Some(project)
            }
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_word_is_not_an_ssh_alias() {
        let err = classify_launch(Some("not-a-real-project-f4aa"), None, &[]).unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("not a project path"));
        assert!(text.contains("user@host"));
    }

    #[test]
    fn file_argument_uses_parent_directory() {
        let dir = temp_dir("file-root");
        let file = dir.join("main.rs");
        fs::write(&file, b"fn main() {}\n").unwrap();
        assert_eq!(project_root(&file), dir);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn ad_hoc_name_strips_at() {
        assert_eq!(ad_hoc_remote_name("ada@lab"), "ada-lab");
    }

    #[test]
    fn session_json_ignores_unknown_fields() {
        let session = parse_session_json(
            r#"{"pid":7,"ws_url":"ws://127.0.0.1:7420/ws","token":"sekret","root":"/work","extra":1}"#,
        )
        .unwrap();
        assert_eq!(session.pid, 7);
        assert_eq!(session.token.as_deref(), Some("sekret"));
        assert_eq!(session.ws_url, "ws://127.0.0.1:7420/ws");
    }

    #[test]
    fn start_args_omit_token() {
        assert_eq!(daemon_start_args(None), vec!["--json".to_string()]);
        let args = daemon_start_args(Some(Path::new("/work")));
        assert_eq!(args, vec!["--json", "--root", "/work"]);
    }

    #[test]
    fn installed_layout_prefers_daemon_sibling() {
        let dir = temp_dir("installed");
        let gui = dir.join(if cfg!(windows) {
            "fresh-gui.exe"
        } else {
            "fresh-gui"
        });
        let daemon = dir.join(daemon_file_name());
        touch_bin(&gui);
        touch_bin(&daemon);
        let found = locate_daemon(&gui, None).unwrap();
        assert_eq!(found, daemon);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn dev_layout_uses_sibling_daemon_binary() {
        let dir = temp_dir("dev");
        let gui = dir.join(if cfg!(windows) {
            "fresh-gui-app.exe"
        } else {
            "fresh-gui-app"
        });
        let daemon = dir.join(dev_daemon_file_name());
        touch_bin(&gui);
        touch_bin(&daemon);
        let found = locate_daemon(&gui, None).unwrap();
        assert_eq!(found, daemon);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn primary_binary_does_not_launch_itself() {
        let dir = temp_dir("self");
        let gui = dir.join(if cfg!(windows) {
            "fresh-gui.exe"
        } else {
            "fresh-gui"
        });
        touch_bin(&gui);
        assert!(locate_daemon(&gui, None).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn path_search_finds_daemon_name_only() {
        let dir = temp_dir("path");
        let elsewhere = temp_dir("path-exe");
        let gui = elsewhere.join("fresh-gui");
        let daemon = dir.join(daemon_file_name());
        touch_bin(&gui);
        touch_bin(&daemon);
        let found = locate_daemon(&gui, Some(dir.as_os_str())).unwrap();
        assert_eq!(found, daemon);
        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&elsewhere);
    }
}

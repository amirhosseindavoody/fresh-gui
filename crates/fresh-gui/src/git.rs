//! Git status, diff, and the basic write commands for one workspace root.
//!
//! v1 shells out to `git` in that directory. Paths the client sends are
//! relative to the repo and cannot climb out of it. Prompts are disabled so
//! a missing credential fails instead of hanging the daemon.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use fresh_gui_protocol::GitFile;

const MAX_SIDE: usize = 256 * 1024;
const OP_TIMEOUT: Duration = Duration::from_secs(45);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status {
    pub repo: bool,
    pub root: String,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<GitFile>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffSides {
    pub old_text: String,
    pub new_text: String,
    pub binary: bool,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    pub ok: bool,
    pub output: String,
}

pub fn status(workspace: &Path) -> Result<Status> {
    let root = match repo_root(workspace) {
        Ok(root) => root,
        Err(err) => {
            return Ok(Status {
                repo: false,
                root: workspace.display().to_string(),
                branch: String::new(),
                upstream: None,
                ahead: 0,
                behind: 0,
                files: Vec::new(),
                detail: Some(err.to_string()),
            });
        }
    };
    let output = git(
        &root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "-b",
            "--untracked-files=all",
        ],
        OP_TIMEOUT,
    )?;
    if !output.status.success() {
        bail!("{}", stderr_text(&output));
    }
    let mut parsed = parse_porcelain_z(&output.stdout);
    parsed.repo = true;
    parsed.root = root.display().to_string();
    Ok(parsed)
}

pub fn diff(workspace: &Path, rel: &str) -> Result<DiffSides> {
    let rel = safe_rel(rel)?;
    let root = repo_root(workspace)?;
    let old = git_show(&root, rel);
    let new_path = root.join(rel);
    let new = read_side(&new_path);
    Ok(combine_sides(old, new))
}

pub fn stage(workspace: &Path, paths: &[String], stage: bool) -> Result<Op> {
    let root = repo_root(workspace)?;
    let rels: Vec<&str> = paths
        .iter()
        .map(|path| safe_rel(path))
        .collect::<Result<_>>()?;
    if rels.is_empty() {
        bail!("no paths");
    }
    let mut args = if stage {
        vec!["add", "--"]
    } else {
        vec!["restore", "--staged", "--"]
    };
    args.extend(rels);
    let output = git(&root, &args, OP_TIMEOUT)?;
    Ok(op_from(output))
}

pub fn commit(workspace: &Path, message: &str) -> Result<Op> {
    let message = message.trim();
    if message.is_empty() {
        bail!("commit message is empty");
    }
    if message.len() > 16 * 1024 {
        bail!("commit message is too long");
    }
    let root = repo_root(workspace)?;
    let output = git(&root, &["commit", "-m", message], OP_TIMEOUT)?;
    Ok(op_from(output))
}

pub fn pull(workspace: &Path) -> Result<Op> {
    let root = repo_root(workspace)?;
    let output = git(&root, &["pull", "--no-edit"], OP_TIMEOUT)?;
    Ok(op_from(output))
}

pub fn push(workspace: &Path) -> Result<Op> {
    let root = repo_root(workspace)?;
    let output = git(&root, &["push"], OP_TIMEOUT)?;
    Ok(op_from(output))
}

fn repo_root(workspace: &Path) -> Result<PathBuf> {
    if !workspace.is_dir() {
        bail!("{} is not a directory", workspace.display());
    }
    let output = git(workspace, &["rev-parse", "--show-toplevel"], OP_TIMEOUT)
        .context("git is not available")?;
    if !output.status.success() {
        bail!("not a git repository");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let line = text.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        bail!("not a git repository");
    }
    Ok(PathBuf::from(line))
}

fn git_show(root: &Path, rel: &str) -> Side {
    let spec = format!("HEAD:{rel}");
    match git(root, &["show", &spec], OP_TIMEOUT) {
        Ok(output) if output.status.success() => side_from_bytes(&output.stdout),
        _ => Side::Missing,
    }
}

fn read_side(path: &Path) -> Side {
    if !path.exists() {
        return Side::Missing;
    }
    match std::fs::read(path) {
        Ok(bytes) => side_from_bytes(&bytes),
        Err(_) => Side::Missing,
    }
}

enum Side {
    Missing,
    Binary,
    Truncated(String),
    Text(String),
}

fn side_from_bytes(bytes: &[u8]) -> Side {
    let sample = &bytes[..bytes.len().min(8192)];
    if crate::binary::looks_binary(sample) {
        return Side::Binary;
    }
    if bytes.len() > MAX_SIDE {
        let end = floor_char_boundary(bytes, MAX_SIDE);
        return Side::Truncated(String::from_utf8_lossy(&bytes[..end]).into_owned());
    }
    Side::Text(String::from_utf8_lossy(bytes).into_owned())
}

fn combine_sides(old: Side, new: Side) -> DiffSides {
    let binary = matches!(old, Side::Binary) || matches!(new, Side::Binary);
    if binary {
        return DiffSides {
            old_text: String::new(),
            new_text: String::new(),
            binary: true,
            truncated: false,
        };
    }
    let truncated = matches!(old, Side::Truncated(_)) || matches!(new, Side::Truncated(_));
    DiffSides {
        old_text: side_text(old),
        new_text: side_text(new),
        binary: false,
        truncated,
    }
}

fn side_text(side: Side) -> String {
    match side {
        Side::Missing | Side::Binary => String::new(),
        Side::Truncated(text) | Side::Text(text) => text,
    }
}

fn op_from(output: Output) -> Op {
    let mut text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    if text.is_empty() {
        text = if output.status.success() {
            "ok".into()
        } else {
            format!("git failed ({})", output.status)
        };
    }
    Op {
        ok: output.status.success(),
        output: text,
    }
}

fn stderr_text(output: &Output) -> String {
    let err = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if err.is_empty() {
        format!("git failed ({})", output.status)
    } else {
        err
    }
}

/// Relative repo path: no absolute form, no empty segments, no `..`.
pub fn safe_rel(path: &str) -> Result<&str> {
    let path = path.trim();
    if path.is_empty() || path.starts_with('/') || path.starts_with('\\') {
        bail!("path must be relative to the repository");
    }
    if path.contains('\0') {
        bail!("path contains NUL");
    }
    if path
        .split(['/', '\\'])
        .any(|seg| seg.is_empty() || seg == "..")
    {
        bail!("path escapes the repository");
    }
    Ok(path)
}

pub fn parse_porcelain_z(bytes: &[u8]) -> Status {
    let mut status = Status {
        repo: true,
        root: String::new(),
        branch: String::new(),
        upstream: None,
        ahead: 0,
        behind: 0,
        files: Vec::new(),
        detail: None,
    };
    let mut records = bytes.split(|byte| *byte == 0).filter(|rec| !rec.is_empty());
    let Some(first) = records.next() else {
        return status;
    };
    let rest = if let Some(header) = std::str::from_utf8(first)
        .ok()
        .filter(|text| text.starts_with("## "))
    {
        apply_branch(&mut status, header);
        None
    } else {
        Some(first)
    };
    let mut pending: Option<&[u8]> = rest;
    while let Some(rec) = pending.take().or_else(|| records.next()) {
        if rec.len() < 3 {
            continue;
        }
        let xy = String::from_utf8_lossy(&rec[..2]).into_owned();
        let path = String::from_utf8_lossy(&rec[3..]).into_owned();
        // With -z, Git emits the destination first and the source as a
        // separate NUL-terminated record. The destination is the file to
        // show and the path accepted by git diff/stage.
        if xy.contains(['R', 'C']) {
            records.next();
        }
        if path.is_empty() {
            continue;
        }
        status.files.push(GitFile { path, xy });
    }
    status
}

fn apply_branch(status: &mut Status, header: &str) {
    let rest = header.trim_start_matches("## ").trim();
    let (name, tracking) = rest
        .split_once(" [")
        .map(|(name, bracket)| (name, Some(bracket.trim_end_matches(']'))))
        .unwrap_or((rest, None));
    let (branch, upstream) = name
        .split_once("...")
        .map(|(branch, upstream)| (branch, Some(upstream)))
        .unwrap_or((name, None));
    status.branch = branch.trim().to_string();
    status.upstream = upstream
        .map(str::trim)
        .filter(|upstream| !upstream.is_empty())
        .map(str::to_string);
    if let Some(tracking) = tracking {
        for part in tracking.split(',') {
            let part = part.trim();
            if let Some(n) = part.strip_prefix("ahead ") {
                status.ahead = n.trim().parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                status.behind = n.trim().parse().unwrap_or(0);
            }
        }
    }
}

fn git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<Output> {
    use std::io::Read;

    let mut child = Command::new("git")
        .arg("-C")
        .arg(git_cwd(cwd))
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "never")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn git {}", args.join(" ")))?;
    // Drain the pipes while waiting so a large diff cannot fill the OS buffer
    // and stall `git` before the timeout fires.
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let stdout_task = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = stdout.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let stderr_task = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(pipe) = stderr.as_mut() {
            let _ = pipe.read_to_end(&mut buf);
        }
        buf
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_task.join();
            let _ = stderr_task.join();
            bail!("git {} timed out", args.join(" "));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let stdout = stdout_task.join().unwrap_or_default();
    let stderr = stderr_task.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

fn git_cwd(cwd: &Path) -> PathBuf {
    #[cfg(windows)]
    if let Some(path) = cwd.to_str().and_then(git_windows_path) {
        return PathBuf::from(path);
    }
    cwd.to_path_buf()
}

/// Rust's Windows `canonicalize` returns verbatim paths. Git for Windows does
/// not accept those as a `-C` directory, so pass the equivalent Win32 path.
#[cfg(any(test, windows))]
fn git_windows_path(path: &str) -> Option<String> {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{rest}"));
    }
    let rest = path.strip_prefix(r"\\?\")?;
    let bytes = rest.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && bytes[2] == b'\\'
    {
        return Some(rest.to_string());
    }
    None
}

fn floor_char_boundary(bytes: &[u8], mut index: usize) -> usize {
    if index >= bytes.len() {
        return bytes.len();
    }
    while index > 0 && (bytes[index] & 0b1100_0000) == 0b1000_0000 {
        index -= 1;
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_parses_branch_and_renames() {
        // In porcelain -z output, the destination precedes the source.
        let raw = b"## main...origin/main [ahead 1, behind 2]\0 M src/a.rs\0A  src/b.rs\0?? new file.txt\0R  new.rs\0old.rs\0";
        let status = parse_porcelain_z(raw);
        assert_eq!(status.branch, "main");
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!(status.ahead, 1);
        assert_eq!(status.behind, 2);
        assert_eq!(status.files.len(), 4);
        assert_eq!(status.files[0].xy, " M");
        assert_eq!(status.files[0].path, "src/a.rs");
        assert_eq!(status.files[2].path, "new file.txt");
        assert_eq!(status.files[3].path, "new.rs");
        assert_eq!(status.files[3].xy, "R ");
    }

    #[test]
    fn porcelain_keeps_literal_paths_and_skips_rename_sources() {
        let raw = concat!(
            "## feature\0 M dir/back\\slash ü.txt\0",
            " R moved name.txt\0old name.txt\0",
            "C  copied.txt\0original.txt\0?? last.txt\0",
        );
        let status = parse_porcelain_z(raw.as_bytes());
        assert_eq!(status.branch, "feature");
        assert_eq!(status.files.len(), 4);
        assert_eq!(status.files[0].path, "dir/back\\slash ü.txt");
        assert_eq!(status.files[1].path, "moved name.txt");
        assert_eq!(status.files[2].path, "copied.txt");
        assert_eq!(status.files[3].path, "last.txt");
    }

    #[test]
    fn porcelain_empty_repository_has_no_files() {
        let status = parse_porcelain_z(b"## No commits yet on main\0");
        assert!(status.files.is_empty());
        assert!(status.repo);
    }

    #[test]
    fn windows_git_paths_drop_only_verbatim_prefixes() {
        assert_eq!(
            git_windows_path(r"\\?\C:\Users\Ada\repo"),
            Some(r"C:\Users\Ada\repo".into())
        );
        assert_eq!(
            git_windows_path(r"\\?\UNC\server\share\repo"),
            Some(r"\\server\share\repo".into())
        );
        assert_eq!(git_windows_path(r"C:\Users\Ada\repo"), None);
        assert_eq!(git_windows_path(r"\\?\Volume{abc}\repo"), None);
    }

    #[test]
    fn safe_rel_rejects_escape() {
        assert!(safe_rel("src/a.rs").is_ok());
        assert!(safe_rel("../secret").is_err());
        assert!(safe_rel("/etc/passwd").is_err());
        assert!(safe_rel("a/../../b").is_err());
    }

    #[test]
    fn live_status_roundtrip_when_git_exists() {
        let git = Command::new("git").arg("--version").output();
        if git.is_err() || !git.unwrap().status.success() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("fresh-gui-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outside = status(&dir).unwrap();
        assert!(!outside.repo, "{outside:?}");
        assert!(outside.files.is_empty());
        assert_eq!(outside.detail.as_deref(), Some("not a git repository"));
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["init", "-q"])
                .status()
                .unwrap()
                .success()
        );
        let empty = status(&dir).unwrap();
        assert!(empty.repo, "{empty:?}");
        assert!(empty.files.is_empty(), "{empty:?}");
        std::fs::write(dir.join("note.txt"), "hello\n").unwrap();
        let dirty = status(&dir).unwrap();
        assert!(dirty.repo, "{dirty:?}");
        assert!(
            dirty.files.iter().any(|file| file.path == "note.txt" && file.xy == "??"),
            "{dirty:?}"
        );
        assert!(
            Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["add", "--", "note.txt"])
                .status()
                .unwrap()
                .success()
        );
        let staged = status(&dir).unwrap();
        assert!(
            staged.files.iter().any(|file| file.path == "note.txt" && file.xy == "A "),
            "{staged:?}"
        );
        std::fs::create_dir(dir.join("subdir")).unwrap();
        let nested = status(&dir.join("subdir")).unwrap();
        assert!(nested.repo, "{nested:?}");
        assert_eq!(nested.files, staged.files);
        std::fs::write(dir.join("note.txt"), "hello again\n").unwrap();
        let both = status(&dir).unwrap();
        assert!(
            both.files.iter().any(|file| file.path == "note.txt" && file.xy == "AM"),
            "{both:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

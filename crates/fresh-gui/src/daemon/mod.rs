//! Per-user background session: lock file, meta, log path, detach, close.
//!
//! Layout (Linux / Unix):
//! - Runtime (lock + meta): `$XDG_RUNTIME_DIR/fresh-gui/` or `/tmp/fresh-gui-$UID/`
//! - Log: `$XDG_STATE_HOME/fresh-gui/fresh-gui.log` or `~/.local/state/fresh-gui/…`
//!
//! Layout (Windows):
//! - Runtime + log under `%LOCALAPPDATA%\fresh-gui\` (fallback: `%TEMP%\fresh-gui\`)
//!
//! Only one background session per user (exclusive lock on the lock file).
//! Detach/spawn follows Fresh’s daemon pattern (`vendor/fresh` …/server/daemon).

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

pub use platform::{is_process_running, spawn_daemon, SessionLock};

const META_NAME: &str = "session.json";
const LOCK_NAME: &str = "session.lock";
const LOG_NAME: &str = "fresh-gui.log";
const READY_TIMEOUT: Duration = Duration::from_secs(15);

/// On-disk session descriptor written by the daemon child after bind.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionMeta {
    pub pid: u32,
    pub version: String,
    pub bound: String,
    pub http_url: String,
    pub ws_url: String,
    /// Full local URL including `?token=` when auth is required.
    pub local_url: Option<String>,
    pub token: Option<String>,
    pub require_auth: bool,
    pub root: String,
    pub log_path: String,
    pub started_at_unix: u64,
}

#[derive(Debug, Clone)]
pub struct SessionPaths {
    #[allow(dead_code)]
    pub runtime_dir: PathBuf,
    #[allow(dead_code)]
    pub state_dir: PathBuf,
    pub lock_path: PathBuf,
    pub meta_path: PathBuf,
    pub log_path: PathBuf,
}

impl SessionPaths {
    pub fn resolve() -> Result<Self> {
        let runtime_dir = platform::runtime_dir()?;
        let state_dir = platform::state_dir()?;
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("create runtime dir {}", runtime_dir.display()))?;
        fs::create_dir_all(&state_dir)
            .with_context(|| format!("create state dir {}", state_dir.display()))?;
        platform::chmod_dir_private(&runtime_dir);
        platform::chmod_dir_private(&state_dir);
        Ok(Self {
            lock_path: runtime_dir.join(LOCK_NAME),
            meta_path: runtime_dir.join(META_NAME),
            log_path: state_dir.join(LOG_NAME),
            runtime_dir,
            state_dir,
        })
    }
}

pub fn read_meta(paths: &SessionPaths) -> Result<Option<SessionMeta>> {
    if !paths.meta_path.is_file() {
        return Ok(None);
    }
    let text = fs::read_to_string(&paths.meta_path)
        .with_context(|| format!("read {}", paths.meta_path.display()))?;
    let meta: SessionMeta = serde_json::from_str(&text)
        .with_context(|| format!("parse {}", paths.meta_path.display()))?;
    Ok(Some(meta))
}

pub fn write_meta(paths: &SessionPaths, meta: &SessionMeta) -> Result<()> {
    let text = serde_json::to_string_pretty(meta).context("serialize session meta")?;
    let tmp = paths.meta_path.with_extension("json.tmp");
    {
        let mut f = platform::open_private_write(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        f.write_all(text.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &paths.meta_path)
        .with_context(|| format!("rename {}", paths.meta_path.display()))?;
    platform::chmod_file_private(&paths.meta_path);
    Ok(())
}

pub fn remove_session_files(paths: &SessionPaths) {
    let _ = fs::remove_file(&paths.meta_path);
    // Leave the lock file; flock is released when the daemon exits.
}

/// Live session if meta exists and the recorded pid is still running.
pub fn live_session(paths: &SessionPaths) -> Result<Option<SessionMeta>> {
    match read_meta(paths)? {
        Some(meta) if is_process_running(meta.pid) => Ok(Some(meta)),
        Some(_) => {
            // Stale meta from a crashed daemon.
            remove_session_files(paths);
            Ok(None)
        }
        None => Ok(None),
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Open the session log for append (private). Used by the daemon child.
pub fn open_log_file(paths: &SessionPaths) -> Result<File> {
    let file = platform::open_private_append(&paths.log_path)
        .with_context(|| format!("open log {}", paths.log_path.display()))?;
    platform::chmod_file_private(&paths.log_path);
    Ok(file)
}

/// Wait until meta appears with a live pid (and optional bound address).
pub fn wait_until_ready(paths: &SessionPaths) -> Result<SessionMeta> {
    let deadline = Instant::now() + READY_TIMEOUT;
    let mut last_err = None;
    while Instant::now() < deadline {
        match live_session(paths) {
            Ok(Some(meta)) => {
                // Quick health check once the process is up.
                if healthz_ok(&meta.bound) {
                    return Ok(meta);
                }
                last_err = Some(format!(
                    "daemon pid {} is up but /healthz on {} is not ready yet",
                    meta.pid, meta.bound
                ));
            }
            Ok(None) => {
                last_err = Some("waiting for daemon to write session meta".into());
            }
            Err(err) => last_err = Some(format!("{err:#}")),
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!(
        "fresh-gui daemon did not become ready within {}s ({})",
        READY_TIMEOUT.as_secs(),
        last_err.unwrap_or_else(|| "unknown".into())
    );
}

fn healthz_ok(bound: &str) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let Ok(mut stream) = TcpStream::connect(bound) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_millis(500)));
    let _ = stream.set_write_timeout(Some(Duration::from_millis(500)));
    let req = format!("GET /healthz HTTP/1.0\r\nHost: {bound}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = Vec::new();
    let _ = stream.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf);
    text.contains("200") && text.contains("ok")
}

/// Print operator-facing session info (URL, token, log, pid).
pub fn print_session_info(meta: &SessionMeta) {
    println!();
    println!("  fresh-gui session");
    println!("  pid:  {}", meta.pid);
    println!("  UI:   {}", meta.http_url);
    println!("  WS:   {}", meta.ws_url);
    println!("  root: {}", meta.root);
    println!("  log:  {}", meta.log_path);
    if let Some(local) = &meta.local_url {
        let port = meta
            .bound
            .rsplit_once(':')
            .map(|(_, p)| p.to_owned())
            .unwrap_or_else(|| "7420".into());
        let user = std::env::var("USER")
            .or_else(|_| std::env::var("USERNAME"))
            .or_else(|_| std::env::var("LOGNAME"))
            .unwrap_or_else(|_| "user".into());
        println!();
        println!("  Local access (this machine):");
        println!("    {local}");
        println!();
        println!(
            "  From another machine (e.g. your laptop) — SSH tunnel, nothing exposed to the network:"
        );
        println!("    ssh -L {port}:127.0.0.1:{port} {user}@your-server");
        println!("    then open: {local}");
    } else if !meta.require_auth {
        println!();
        println!("  auth: disabled (--allow-no-auth)");
    }
    println!();
    println!("  Stop with: fresh-gui close");
    println!();
}

/// Stop the background session (graceful signal/terminate, then cleanup).
pub fn close_session(paths: &SessionPaths) -> Result<()> {
    let Some(meta) = read_meta(paths)? else {
        println!("No fresh-gui session is running.");
        return Ok(());
    };

    if !is_process_running(meta.pid) {
        remove_session_files(paths);
        println!(
            "Cleared stale session metadata (process {} was not running).",
            meta.pid
        );
        return Ok(());
    }

    println!("Stopping fresh-gui session (pid {})…", meta.pid);
    platform::request_stop(meta.pid)?;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if !is_process_running(meta.pid) {
            remove_session_files(paths);
            println!("Stopped. Log: {}", meta.log_path);
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }

    // Escalate.
    platform::force_kill(meta.pid);
    thread::sleep(Duration::from_millis(100));
    remove_session_files(paths);
    println!("Force-killed session {}. Log: {}", meta.pid, meta.log_path);
    Ok(())
}

/// Reconstruct CLI args for the daemon child from parsed serve options.
pub fn serve_args_for_child(
    listen: &str,
    strict_listen: bool,
    token: Option<&str>,
    allow_no_auth: bool,
    root: Option<&Path>,
    no_editor: bool,
    ui_dir: Option<&Path>,
    no_ui: bool,
    public_host: Option<&str>,
    config: Option<&Path>,
) -> Vec<String> {
    let mut args = Vec::new();
    args.push("--listen".into());
    args.push(listen.to_owned());
    if strict_listen {
        args.push("--strict-listen".into());
    }
    if let Some(t) = token.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--token".into());
        args.push(t.to_owned());
    }
    if allow_no_auth {
        args.push("--allow-no-auth".into());
    }
    if let Some(r) = root {
        args.push("--root".into());
        args.push(r.display().to_string());
    }
    if no_editor {
        args.push("--no-editor".into());
    }
    if let Some(d) = ui_dir {
        args.push("--ui-dir".into());
        args.push(d.display().to_string());
    }
    if no_ui {
        args.push("--no-ui".into());
    }
    if let Some(h) = public_host.map(str::trim).filter(|s| !s.is_empty()) {
        args.push("--public-host".into());
        args.push(h.to_owned());
    }
    if let Some(c) = config {
        args.push("--config".into());
        args.push(c.display().to_string());
    }
    args
}

/// Shared helpers used by platform modules when opening private files.
pub(crate) fn open_options_private_write() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    platform::apply_private_mode(&mut opts);
    opts
}

pub(crate) fn open_options_private_append() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.create(true).append(true);
    platform::apply_private_mode(&mut opts);
    opts
}

pub(crate) fn open_options_lock() -> OpenOptions {
    let mut opts = OpenOptions::new();
    opts.read(true).write(true).create(true);
    platform::apply_private_mode(&mut opts);
    opts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_process_is_running() {
        assert!(is_process_running(std::process::id()));
        assert!(!is_process_running(0));
    }

    #[test]
    fn meta_roundtrip() {
        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-daemon-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        let paths = SessionPaths {
            runtime_dir: dir.clone(),
            state_dir: dir.clone(),
            lock_path: dir.join(LOCK_NAME),
            meta_path: dir.join(META_NAME),
            log_path: dir.join(LOG_NAME),
        };
        let meta = SessionMeta {
            pid: 42,
            version: "test".into(),
            bound: "127.0.0.1:7420".into(),
            http_url: "http://127.0.0.1:7420/".into(),
            ws_url: "ws://127.0.0.1:7420/ws".into(),
            local_url: Some("http://127.0.0.1:7420/?token=abc".into()),
            token: Some("abc".into()),
            require_auth: true,
            root: "/tmp".into(),
            log_path: paths.log_path.display().to_string(),
            started_at_unix: 1,
        };
        write_meta(&paths, &meta).unwrap();
        let loaded = read_meta(&paths).unwrap().unwrap();
        assert_eq!(loaded, meta);
        let _ = fs::remove_dir_all(&dir);
    }
}

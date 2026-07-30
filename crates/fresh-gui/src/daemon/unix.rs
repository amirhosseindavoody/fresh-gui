//! Unix session lock, detach spawn, and process control (Fresh-aligned).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{now_unix, open_log_file, open_options_lock, open_options_private_append, open_options_private_write, SessionPaths};

pub fn runtime_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_RUNTIME_DIR") {
        let xdg = xdg.trim();
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("fresh-gui"));
        }
    }
    let uid = unsafe { libc::getuid() };
    Ok(PathBuf::from(format!("/tmp/fresh-gui-{uid}")))
}

pub fn state_dir() -> Result<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        let xdg = xdg.trim();
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("fresh-gui"));
        }
    }
    let home = std::env::var_os("HOME").context("HOME is unset")?;
    Ok(PathBuf::from(home).join(".local/state/fresh-gui"))
}

pub fn chmod_dir_private(path: &Path) {
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}

pub fn chmod_file_private(path: &Path) {
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

pub fn apply_private_mode(opts: &mut OpenOptions) {
    opts.mode(0o600);
}

pub fn open_private_write(path: &Path) -> io::Result<File> {
    open_options_private_write().open(path)
}

pub fn open_private_append(path: &Path) -> io::Result<File> {
    open_options_private_append().open(path)
}

/// Exclusive lock held for the lifetime of the daemon process.
pub struct SessionLock {
    _file: File,
}

impl SessionLock {
    pub fn try_acquire(paths: &SessionPaths) -> Result<Self> {
        let file = open_options_lock()
            .open(&paths.lock_path)
            .with_context(|| format!("open lock {}", paths.lock_path.display()))?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::WouldBlock
                || err.raw_os_error() == Some(libc::EWOULDBLOCK)
                || err.raw_os_error() == Some(libc::EAGAIN)
            {
                bail!(
                    "another fresh-gui session is already running for this user \
                     (lock {}). Run `fresh-gui` for status or `fresh-gui close` to stop it.",
                    paths.lock_path.display()
                );
            }
            return Err(err).context("flock session.lock");
        }
        Ok(Self { _file: file })
    }
}

/// Spawn `fresh-gui --daemon-serve …` detached (own session, stdio → log file).
///
/// Mirrors Fresh `server::daemon::unix::spawn_server_detached`: `setsid()` so
/// closing the launching terminal does not SIGHUP the daemon.
pub fn spawn_daemon(serve_args: &[String], paths: &SessionPaths) -> Result<u32> {
    let exe = std::env::current_exe().context("current_exe")?;
    {
        let mut file = open_log_file(paths)?;
        writeln!(
            file,
            "\n======== fresh-gui spawn ts={} ========",
            now_unix()
        )?;
    }

    let log = open_options_private_append()
        .open(&paths.log_path)
        .with_context(|| format!("open log for child {}", paths.log_path.display()))?;
    let log_err = log.try_clone().context("clone log fd for stderr")?;

    let mut cmd = Command::new(&exe);
    cmd.arg("--daemon-serve");
    for a in serve_args {
        cmd.arg(a);
    }
    // Propagate cwd so --root defaults stay meaningful.
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(cwd);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    // Own session / process group so closing the launching terminal does not
    // SIGHUP the daemon (same approach as Fresh’s unix daemon spawn).
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }

    let child = cmd.spawn().context("spawn fresh-gui --daemon-serve")?;
    Ok(child.id())
}

pub fn is_process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    Path::new(&format!("/proc/{pid}")).exists()
}

pub fn request_stop(pid: u32) -> Result<()> {
    let rc = unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        // Already gone.
        if err.raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        return Err(err).context("SIGTERM daemon");
    }
    Ok(())
}

pub fn force_kill(pid: u32) {
    let _ = unsafe { libc::kill(pid as i32, libc::SIGKILL) };
}

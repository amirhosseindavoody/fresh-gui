//! Windows session lock, detach spawn, and process control.
//!
//! Detach spawn follows Fresh `server::daemon::windows` (`DETACHED_PROCESS |
//! CREATE_NEW_PROCESS_GROUP`). Process liveness uses `OpenProcess` +
//! `GetExitCodeProcess` like Fresh.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, STILL_ACTIVE};
use windows_sys::Win32::Storage::FileSystem::{
    LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY,
};
use windows_sys::Win32::System::IO::OVERLAPPED;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, TerminateProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE,
};

use super::{
    now_unix, open_log_file, open_options_lock, open_options_private_append,
    open_options_private_write, SessionPaths,
};

const DETACHED_PROCESS: u32 = 0x00000008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

pub fn runtime_dir() -> Result<PathBuf> {
    if let Ok(local) = std::env::var("LOCALAPPDATA") {
        let local = local.trim();
        if !local.is_empty() {
            return Ok(PathBuf::from(local).join("fresh-gui"));
        }
    }
    Ok(std::env::temp_dir().join("fresh-gui"))
}

pub fn state_dir() -> Result<PathBuf> {
    // Keep lock/meta/log together under LocalAppData for a simple Windows layout.
    runtime_dir()
}

pub fn chmod_dir_private(_path: &Path) {}
pub fn chmod_file_private(_path: &Path) {}

pub fn apply_private_mode(_opts: &mut OpenOptions) {
    // Windows ACLs are left at the process default; directories live under the
    // user profile (LocalAppData).
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
        let handle = file.as_raw_handle() as HANDLE;
        if handle.is_null() || handle == INVALID_HANDLE_VALUE {
            bail!("invalid lock file handle");
        }
        let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
        let ok = unsafe {
            LockFileEx(
                handle,
                LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
                0,
                !0,
                !0,
                &mut overlapped,
            )
        };
        if ok == 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(33) /* ERROR_LOCK_VIOLATION */
                || err.kind() == io::ErrorKind::WouldBlock
            {
                bail!(
                    "another fresh-gui session is already running for this user \
                     (lock {}). Run `fresh-gui` for status or `fresh-gui close` to stop it.",
                    paths.lock_path.display()
                );
            }
            return Err(err).context("LockFileEx session.lock");
        }
        Ok(Self { _file: file })
    }
}

/// Spawn `fresh-gui --daemon-serve …` detached (Fresh Windows daemon pattern).
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
    let log_err = log.try_clone().context("clone log handle for stderr")?;

    let mut cmd = Command::new(&exe);
    cmd.arg("--daemon-serve");
    for a in serve_args {
        cmd.arg(a);
    }
    if let Ok(cwd) = std::env::current_dir() {
        cmd.current_dir(cwd);
    }
    cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    let child = cmd.spawn().context("spawn fresh-gui --daemon-serve")?;
    Ok(child.id())
}

pub fn is_process_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return false;
        }
        let mut exit_code: u32 = 0;
        let result = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        result != 0 && exit_code == STILL_ACTIVE as u32
    }
}

pub fn request_stop(pid: u32) -> Result<()> {
    // No graceful SIGTERM equivalent for a console-less detached process;
    // TerminateProcess is the supported stop path (same as Fresh Windows kill).
    force_kill_inner(pid).context("TerminateProcess daemon")
}

pub fn force_kill(pid: u32) {
    let _ = force_kill_inner(pid);
}

fn force_kill_inner(pid: u32) -> io::Result<()> {
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            let err = io::Error::last_os_error();
            // Already gone.
            if err.raw_os_error() == Some(87) /* ERROR_INVALID_PARAMETER */
                || err.raw_os_error() == Some(5)
            {
                return Ok(());
            }
            return Err(err);
        }
        let ok = TerminateProcess(handle, 1);
        CloseHandle(handle);
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

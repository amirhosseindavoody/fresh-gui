//! Hand a sandboxed path to the operating system's file opener.
//!
//! The editor refuses binary snapshots. This is how the host offers "Open
//! externally" without decoding those bytes.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};

pub fn open_with_os(path: &Path) -> Result<()> {
    let display = path.display().to_string();
    let mut command = opener(path);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
        .spawn()
        .with_context(|| format!("open {display} externally"))?;
    Ok(())
}

#[cfg(windows)]
fn opener(path: &Path) -> Command {
    use std::os::windows::process::CommandExt;
    // `start` is a cmd builtin. The empty title keeps a quoted path from being
    // read as the window title. CREATE_NO_WINDOW avoids a flash console.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut command = Command::new("cmd.exe");
    command
        .args(["/C", "start", "", &path.display().to_string()])
        .creation_flags(CREATE_NO_WINDOW);
    command
}

#[cfg(target_os = "macos")]
fn opener(path: &Path) -> Command {
    let mut command = Command::new("open");
    command.arg(path);
    command
}

#[cfg(all(unix, not(target_os = "macos")))]
fn opener(path: &Path) -> Command {
    let mut command = Command::new("xdg-open");
    command.arg(path);
    command
}

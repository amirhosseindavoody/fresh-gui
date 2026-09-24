//! Small adapter for GitHub Copilot CLI's non-interactive prompt mode.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Resolve `copilot` from PATH without shelling out, including Windows PATHEXT.
pub fn find_cli() -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    #[cfg(windows)]
    let extensions: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
        .split(';')
        .map(str::to_owned)
        .collect();
    #[cfg(not(windows))]
    let extensions = vec![String::new()];

    for directory in std::env::split_paths(&path) {
        for extension in &extensions {
            let candidate = directory.join(if extension.is_empty() {
                "copilot".to_owned()
            } else {
                format!("copilot{extension}")
            });
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

pub fn ask(cli: PathBuf, prompt: String, cwd: Option<PathBuf>) -> Result<String, String> {
    let mut command = Command::new(cli);
    command.arg("-p").arg(prompt);
    if let Some(cwd) = cwd {
        command.current_dir(cwd);
    }
    let Output { status, stdout, stderr } = command
        .output()
        .map_err(|error| format!("Could not start Copilot CLI: {error}"))?;
    let stdout = String::from_utf8_lossy(&stdout).trim().to_owned();
    if status.success() {
        Ok(if stdout.is_empty() {
            "Copilot returned an empty response.".into()
        } else {
            stdout
        })
    } else {
        let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
        Err(if stderr.is_empty() {
            format!("Copilot CLI exited with {status}.")
        } else {
            format!("Copilot CLI failed: {stderr}")
        })
    }
}

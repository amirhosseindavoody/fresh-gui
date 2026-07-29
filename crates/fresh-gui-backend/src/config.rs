//! JSON config for `fresh-gui-backend`.
//!
//! Mirrors Fresh editor’s user-facing shape for the terminal shell
//! (`terminal.shell.{command,args}`) so the same mental model applies, but
//! lives under `fresh-gui`’s own config directory (not Fresh’s).
//!
//! Default path (Linux): `$XDG_CONFIG_HOME/fresh-gui/config.json`
//! (falls back to `~/.config/fresh-gui/config.json`).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Filename under the config directory (same as Fresh).
pub const FILENAME: &str = "config.json";

/// Top-level config file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Config {
    #[serde(default)]
    pub terminal: TerminalConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            terminal: TerminalConfig::default(),
        }
    }
}

/// Terminal / PTY settings (Fresh-compatible nesting).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalConfig {
    /// Default shell for new PTYs when the client does not pass `shell`.
    ///
    /// When unset, [`Config::resolve_shell`] uses [`DEFAULT_SHELL_COMMAND`].
    #[serde(default)]
    pub shell: Option<TerminalShellConfig>,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            // Explicit default so a written config.json documents the choice.
            shell: Some(TerminalShellConfig {
                command: DEFAULT_SHELL_COMMAND.to_owned(),
                args: Vec::new(),
            }),
        }
    }
}

/// Explicit shell command + args (same fields as Fresh’s `TerminalShellConfig`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TerminalShellConfig {
    /// Executable to launch (e.g. `zsh`, `/bin/zsh`). Resolved via `$PATH`
    /// when not absolute.
    pub command: String,

    /// Arguments passed to the shell. When empty, the backend applies its
    /// interactive / OSC 7 setup for known shells (`zsh`, `bash`, …).
    #[serde(default)]
    pub args: Vec<String>,
}

/// Default shell when config omits `terminal.shell` or leaves it empty.
pub const DEFAULT_SHELL_COMMAND: &str = "zsh";

impl Config {
    /// Load from `explicit` if set, else the default user config path.
    /// Missing / empty files yield [`Config::default`] (zsh).
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => p.to_path_buf(),
            None => default_config_path(),
        };
        Self::load_from_path(&path)
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.is_file() {
            info!(
                path = %path.display(),
                "no config file — using defaults (shell={DEFAULT_SHELL_COMMAND})"
            );
            return Ok(Self::default());
        }

        let text = fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Self::default());
        }

        // Strip // line comments and /* */ blocks so Fresh-style JSONC works.
        let json = strip_jsonc(trimmed);
        let mut cfg: Config = serde_json::from_str(&json)
            .with_context(|| format!("parse config {}", path.display()))?;
        cfg.normalize();
        info!(
            path = %path.display(),
            shell = %cfg.resolve_shell().0,
            "loaded config"
        );
        Ok(cfg)
    }

    fn normalize(&mut self) {
        if let Some(shell) = &mut self.terminal.shell {
            shell.command = shell.command.trim().to_owned();
            if shell.command.is_empty() {
                warn!("terminal.shell.command is empty — falling back to {DEFAULT_SHELL_COMMAND}");
                self.terminal.shell = None;
            }
        }
    }

    /// `(command, args)` for a new PTY when the client did not override `shell`.
    pub fn resolve_shell(&self) -> (String, Vec<String>) {
        match &self.terminal.shell {
            Some(s) if !s.command.is_empty() => (s.command.clone(), s.args.clone()),
            _ => (DEFAULT_SHELL_COMMAND.to_owned(), Vec::new()),
        }
    }
}

/// `$XDG_CONFIG_HOME/fresh-gui/config.json` or `~/.config/fresh-gui/config.json`.
pub fn default_config_path() -> PathBuf {
    default_config_dir().join(FILENAME)
}

pub fn default_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        let xdg = xdg.trim();
        if !xdg.is_empty() {
            return PathBuf::from(xdg).join("fresh-gui");
        }
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("fresh-gui")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Minimal JSONC stripper (line `//` and block `/* */`); strings are left intact.
fn strip_jsonc(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_string {
            out.push(c);
            if escape {
                escape = false;
            } else if c == '\\' {
                escape = true;
            } else if c == '"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == '/' && i + 1 < bytes.len() {
            let next = bytes[i + 1] as char;
            if next == '/' {
                i += 2;
                while i < bytes.len() && bytes[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            if next == '*' {
                i += 2;
                while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                    i += 1;
                }
                i = (i + 2).min(bytes.len());
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_resolves_to_zsh() {
        let (cmd, args) = Config::default().resolve_shell();
        assert_eq!(cmd, "zsh");
        assert!(args.is_empty());
    }

    #[test]
    fn missing_file_uses_defaults() {
        let cfg = Config::load_from_path(Path::new("/no/such/fresh-gui-config.json")).unwrap();
        assert_eq!(cfg.resolve_shell().0, "zsh");
    }

    #[test]
    fn loads_fresh_shaped_shell() {
        let dir = tempfile_dir();
        let path = dir.join("config.json");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{
              // Fresh-compatible shell override
              "terminal": {{
                "shell": {{ "command": "/bin/bash", "args": ["-l"] }}
              }}
            }}"#
        )
        .unwrap();

        let cfg = Config::load_from_path(&path).unwrap();
        let (cmd, args) = cfg.resolve_shell();
        assert_eq!(cmd, "/bin/bash");
        assert_eq!(args, vec!["-l"]);
    }

    #[test]
    fn empty_command_falls_back_to_zsh() {
        let dir = tempfile_dir();
        let path = dir.join("config.json");
        fs::write(&path, r#"{"terminal":{"shell":{"command":"  "}}}"#).unwrap();
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.resolve_shell().0, "zsh");
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-config-test-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

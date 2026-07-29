//! JSON config for `fresh-gui-backend` (+ host UI prefs).
//!
//! Mirrors Fresh editor’s user-facing shape for the terminal shell
//! (`terminal.shell.{command,args}`) and stores host chrome under `ui.*`.
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

/// Default shell when config omits `terminal.shell` or leaves it empty.
pub const DEFAULT_SHELL_COMMAND: &str = "zsh";

/// Documented starter file (JSONC) written on first settings open.
pub const DEFAULT_CONFIG_TEMPLATE: &str = r#"{
  // Host UI chrome — applied on connect and when this file is saved.
  "ui": {
    // system | light | dark
    "theme": "system",
    "terminalFontSize": 14,
    "editorFontSize": 14,
    "webgl": true
  },
  // Default PTY shell when the client does not pass `shell`.
  // Empty args keep interactive / OSC 7 setup for known shells.
  "terminal": {
    "shell": {
      "command": "zsh",
      "args": []
    }
  }
}
"#;

/// Top-level config file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ui: UiConfig::default(),
            terminal: TerminalConfig::default(),
        }
    }
}

/// Host UI settings (shared with the browser / Tauri chrome).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiConfig {
    /// `system` | `light` | `dark`
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_font_size", rename = "terminalFontSize")]
    pub terminal_font_size: u32,
    #[serde(default = "default_font_size", rename = "editorFontSize")]
    pub editor_font_size: u32,
    #[serde(default = "default_true")]
    pub webgl: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            terminal_font_size: default_font_size(),
            editor_font_size: default_font_size(),
            webgl: true,
        }
    }
}

fn default_theme() -> String {
    "system".to_owned()
}

fn default_font_size() -> u32 {
    14
}

fn default_true() -> bool {
    true
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

impl Config {
    /// Resolve the path that should be used (CLI override or default location).
    pub fn resolve_path(explicit: Option<&Path>) -> PathBuf {
        match explicit {
            Some(p) => p.to_path_buf(),
            None => default_config_path(),
        }
    }

    /// Load from `explicit` if set, else the default user config path.
    /// Missing / empty files yield [`Config::default`] (zsh + system theme).
    pub fn load(explicit: Option<&Path>) -> Result<(Self, PathBuf)> {
        let path = Self::resolve_path(explicit);
        let cfg = Self::load_from_path(&path)?;
        Ok((cfg, path))
    }

    pub fn load_from_path(path: &Path) -> Result<Self> {
        if !path.is_file() {
            info!(
                path = %path.display(),
                "no config file — using defaults (shell={DEFAULT_SHELL_COMMAND}, theme=system)"
            );
            return Ok(Self::default());
        }

        let text = fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        Self::parse(&text)
            .with_context(|| format!("parse config {}", path.display()))
            .map(|cfg| {
                info!(
                    path = %path.display(),
                    shell = %cfg.resolve_shell().0,
                    theme = %cfg.ui.theme,
                    "loaded config"
                );
                cfg
            })
    }

    pub fn parse(text: &str) -> Result<Self> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Self::default());
        }
        // Strip // line comments and /* */ blocks so Fresh-style JSONC works.
        let json = strip_jsonc(trimmed);
        let mut cfg: Config = serde_json::from_str(&json).context("parse config json")?;
        cfg.normalize();
        Ok(cfg)
    }

    /// Create the config file (and parent dir) with the documented template if missing.
    pub fn ensure_file(path: &Path) -> Result<()> {
        if path.is_file() {
            return Ok(());
        }
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("create config dir {}", parent.display()))?;
        }
        fs::write(path, DEFAULT_CONFIG_TEMPLATE)
            .with_context(|| format!("write default config {}", path.display()))?;
        info!(path = %path.display(), "wrote default config.json");
        Ok(())
    }

    fn normalize(&mut self) {
        let theme = self.ui.theme.trim().to_ascii_lowercase();
        self.ui.theme = match theme.as_str() {
            "light" | "dark" | "system" => theme,
            "" => {
                warn!("ui.theme is empty — falling back to system");
                "system".to_owned()
            }
            other => {
                warn!("ui.theme `{other}` is unknown — falling back to system");
                "system".to_owned()
            }
        };
        self.ui.terminal_font_size = self.ui.terminal_font_size.clamp(10, 28);
        self.ui.editor_font_size = self.ui.editor_font_size.clamp(10, 28);

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

    /// True when `path` refers to this config file (after canonicalize when possible).
    pub fn path_matches(config_path: &Path, candidate: &str) -> bool {
        let cand = Path::new(candidate);
        if config_path == cand {
            return true;
        }
        match (config_path.canonicalize(), cand.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => config_path.to_string_lossy() == candidate,
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
    fn default_resolves_to_zsh_and_system_theme() {
        let cfg = Config::default();
        let (cmd, args) = cfg.resolve_shell();
        assert_eq!(cmd, "zsh");
        assert!(args.is_empty());
        assert_eq!(cfg.ui.theme, "system");
    }

    #[test]
    fn missing_file_uses_defaults() {
        let cfg = Config::load_from_path(Path::new("/no/such/fresh-gui-config.json")).unwrap();
        assert_eq!(cfg.resolve_shell().0, "zsh");
        assert_eq!(cfg.ui.theme, "system");
    }

    #[test]
    fn loads_fresh_shaped_shell_and_ui() {
        let dir = tempfile_dir();
        let path = dir.join("config.json");
        let mut f = fs::File::create(&path).unwrap();
        writeln!(
            f,
            r#"{{
              // Fresh-compatible shell override
              "ui": {{ "theme": "light", "terminalFontSize": 16 }},
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
        assert_eq!(cfg.ui.theme, "light");
        assert_eq!(cfg.ui.terminal_font_size, 16);
    }

    #[test]
    fn empty_command_falls_back_to_zsh() {
        let dir = tempfile_dir();
        let path = dir.join("config.json");
        fs::write(&path, r#"{"terminal":{"shell":{"command":"  "}}}"#).unwrap();
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.resolve_shell().0, "zsh");
    }

    #[test]
    fn ensure_file_writes_template() {
        let dir = tempfile_dir();
        let path = dir.join("nested").join("config.json");
        Config::ensure_file(&path).unwrap();
        assert!(path.is_file());
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.resolve_shell().0, "zsh");
        assert_eq!(cfg.ui.theme, "system");
    }

    #[test]
    fn default_template_parses() {
        let cfg = Config::parse(DEFAULT_CONFIG_TEMPLATE).unwrap();
        assert_eq!(cfg.ui.theme, "system");
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

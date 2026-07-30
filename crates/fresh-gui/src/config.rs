//! JSON config for `fresh-gui` (+ host UI prefs).
//!
//! Mirrors Fresh editor’s user-facing shape for the terminal shell
//! (`terminal.shell.{command,args}`) and stores host chrome under `ui.*`.
//! Color packs under `ui.palette` reuse Fresh theme names where applicable
//! (`nord`, `dracula`, …) mapped onto host CSS tokens.
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

/// Sentinel path the host sends to open the embedded default-settings catalog.
pub const DEFAULT_SETTINGS_OPEN_PATH: &str = "fresh-gui://defaults/config.json";

/// Documented starter / catalog file (JSONC), embedded at build via `include_str!`
/// (same pattern as Fresh built-in keymaps). Written on first settings open and
/// shown as a temp file for “Open Default Settings”.
pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../defaults/config.json");

const KNOWN_PALETTES: &[&str] = &[
    "primer",
    "nord",
    "dracula",
    "solarized-dark",
    "high-contrast",
    "nostalgia",
    "dark",
    "light",
];

/// Top-level config file.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    /// Host chrome keyboard shortcuts. Empty → embedded defaults at runtime.
    #[serde(default)]
    pub shortkeys: Vec<Shortkey>,
}

impl Default for Config {
    fn default() -> Self {
        // Prefer the embedded catalog so defaults stay in sync with the file.
        Self::parse(DEFAULT_CONFIG_TEMPLATE).unwrap_or_else(|_| Self {
            ui: UiConfig::default(),
            terminal: TerminalConfig::default(),
            shortkeys: Vec::new(),
        })
    }
}

/// One host shortcut binding (issue #61 / Fresh-aligned action + when).
///
/// `shortkey` is a chord string (`Mod+T`, `Ctrl+Shift+Tab`, `Alt+Z`). `Mod` is
/// Cmd on macOS and Ctrl elsewhere in the host. `when` is a Fresh-style context
/// (`global`, `terminal`, `editor`, `fileExplorer`); omit / empty = `global`.
///
/// Selection-aware terminal copy vs interrupt is **not** modeled as a when-clause
/// (same as Fresh): the terminal clipboard action body checks selection.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Shortkey {
    /// Action id handled by the host (`tab.new`, `settings.open`, …).
    pub action: String,
    /// Key chord, e.g. `Mod+Shift+P`.
    pub shortkey: String,
    /// Fresh-style context clause. Default `global`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub when: Option<String>,
}

/// Host UI settings (shared with the browser UI chrome).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UiConfig {
    /// `system` | `light` | `dark`
    #[serde(default = "default_theme")]
    pub theme: String,
    /// Color pack id (`primer` or a Fresh theme name).
    #[serde(default = "default_palette")]
    pub palette: String,
    #[serde(default = "default_font_size", rename = "terminalFontSize")]
    pub terminal_font_size: u32,
    #[serde(default = "default_font_size", rename = "editorFontSize")]
    pub editor_font_size: u32,
    /// UI chrome font weight (100–900).
    #[serde(default = "default_font_weight", rename = "fontWeight")]
    pub font_weight: u32,
    /// Terminal + editor monospace weight (100–900).
    #[serde(default = "default_font_weight", rename = "monoFontWeight")]
    pub mono_font_weight: u32,
    /// Optional UI `font-family` CSS value; empty → IBM Plex Sans.
    #[serde(default, rename = "fontFamily")]
    pub font_family: String,
    /// Optional mono `font-family` CSS value; empty → IBM Plex Mono.
    #[serde(default, rename = "monoFontFamily")]
    pub mono_font_family: String,
    #[serde(default = "default_true")]
    pub webgl: bool,
    /// Show names starting with `.` in the explorer (except `.git`, see [`Self::show_git_dirs`]).
    #[serde(default, rename = "showDotfiles")]
    pub show_dotfiles: bool,
    /// Show `.git` directories in the explorer. Independent of [`Self::show_dotfiles`]; default off.
    #[serde(default, rename = "showGitDirs")]
    pub show_git_dirs: bool,
    /// VS Code–style editor document map (minimap). Default off.
    #[serde(default, rename = "editorMinimap")]
    pub editor_minimap: bool,
    /// Soft-wrap long lines in the host editor (Fresh `editor.line_wrap`). Default on.
    #[serde(default = "default_true", rename = "editorLineWrap")]
    pub editor_line_wrap: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            palette: default_palette(),
            terminal_font_size: default_font_size(),
            editor_font_size: default_font_size(),
            font_weight: default_font_weight(),
            mono_font_weight: default_font_weight(),
            font_family: String::new(),
            mono_font_family: String::new(),
            webgl: true,
            show_dotfiles: false,
            show_git_dirs: false,
            editor_minimap: false,
            editor_line_wrap: true,
        }
    }
}

fn default_theme() -> String {
    "system".to_owned()
}

fn default_palette() -> String {
    "primer".to_owned()
}

fn default_font_size() -> u32 {
    14
}

fn default_font_weight() -> u32 {
    400
}

fn default_true() -> bool {
    true
}

fn normalize_font_weight(weight: u32) -> u32 {
    let clamped = weight.clamp(100, 900);
    ((clamped + 50) / 100) * 100
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
            .inspect(|cfg| {
                info!(
                    path = %path.display(),
                    shell = %cfg.resolve_shell().0,
                    theme = %cfg.ui.theme,
                    palette = %cfg.ui.palette,
                    "loaded config"
                );
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
    ///
    /// When the file already exists, inserts any keys present in
    /// [`DEFAULT_CONFIG_TEMPLATE`] that are absent from the file. Existing
    /// keys/values are never overwritten. Returns `true` when the file was
    /// created or modified.
    pub fn ensure_file(path: &Path) -> Result<bool> {
        if !path.is_file() {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .with_context(|| format!("create config dir {}", parent.display()))?;
            }
            fs::write(path, DEFAULT_CONFIG_TEMPLATE)
                .with_context(|| format!("write default config {}", path.display()))?;
            info!(path = %path.display(), "wrote default config.json");
            return Ok(true);
        }

        let text = fs::read_to_string(path)
            .with_context(|| format!("read config {}", path.display()))?;
        if text.trim().is_empty() {
            fs::write(path, DEFAULT_CONFIG_TEMPLATE)
                .with_context(|| format!("write default config {}", path.display()))?;
            info!(path = %path.display(), "wrote default config.json (was empty)");
            return Ok(true);
        }

        let defaults = parse_jsonc_value(DEFAULT_CONFIG_TEMPLATE)
            .context("parse default config template")?;
        let mut existing = match parse_jsonc_value(&text) {
            Ok(v) => v,
            Err(err) => {
                warn!(
                    path = %path.display(),
                    %err,
                    "config exists but could not be parsed — leaving file untouched"
                );
                return Ok(false);
            }
        };

        if !merge_missing_json(&mut existing, &defaults) {
            return Ok(false);
        }

        let output = render_merged_config_text(&text, &existing);
        fs::write(path, output)
            .with_context(|| format!("write hydrated config {}", path.display()))?;
        info!(
            path = %path.display(),
            "added missing default keys to config.json"
        );
        Ok(true)
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

        let palette = self.ui.palette.trim().to_ascii_lowercase();
        self.ui.palette = if KNOWN_PALETTES.contains(&palette.as_str()) {
            palette
        } else if palette.is_empty() {
            warn!("ui.palette is empty — falling back to primer");
            "primer".to_owned()
        } else {
            warn!("ui.palette `{palette}` is unknown — falling back to primer");
            "primer".to_owned()
        };

        self.ui.terminal_font_size = self.ui.terminal_font_size.clamp(10, 28);
        self.ui.editor_font_size = self.ui.editor_font_size.clamp(10, 28);
        self.ui.font_weight = normalize_font_weight(self.ui.font_weight);
        self.ui.mono_font_weight = normalize_font_weight(self.ui.mono_font_weight);
        self.ui.font_family = self.ui.font_family.trim().to_owned();
        self.ui.mono_font_family = self.ui.mono_font_family.trim().to_owned();

        if let Some(shell) = &mut self.terminal.shell {
            shell.command = shell.command.trim().to_owned();
            if shell.command.is_empty() {
                warn!("terminal.shell.command is empty — falling back to {DEFAULT_SHELL_COMMAND}");
                self.terminal.shell = None;
            }
        }

        self.shortkeys.retain(|sk| {
            let action = sk.action.trim();
            let chord = sk.shortkey.trim();
            if action.is_empty() || chord.is_empty() {
                warn!("dropping shortkey with empty action or shortkey");
                return false;
            }
            true
        });
        for sk in &mut self.shortkeys {
            sk.action = sk.action.trim().to_owned();
            sk.shortkey = sk.shortkey.trim().to_owned();
            if let Some(when) = sk.when.take() {
                let trimmed = when.trim().to_owned();
                sk.when = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
            }
        }
    }

    /// Effective shortcut list: user `shortkeys` when non-empty, else embedded defaults.
    pub fn effective_shortkeys(&self) -> Vec<Shortkey> {
        if !self.shortkeys.is_empty() {
            return self.shortkeys.clone();
        }
        match Self::parse(DEFAULT_CONFIG_TEMPLATE) {
            Ok(defaults) => defaults.shortkeys,
            Err(_) => Vec::new(),
        }
    }

    /// Write the embedded default catalog to a unique temp path (deleted on tab close).
    pub fn materialize_defaults_temp() -> Result<PathBuf> {
        let dir = std::env::temp_dir().join("fresh-gui-defaults");
        fs::create_dir_all(&dir)
            .with_context(|| format!("create defaults temp dir {}", dir.display()))?;
        let path = dir.join(format!(
            "defaults-{}-{}.json",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        let header = concat!(
            "// READ-ONLY CATALOG — temporary file; deleted when this tab closes.\n",
            "// Save is rejected. Copy keys into your user ~/.config/fresh-gui/config.json.\n",
            "//\n",
        );
        let mut body = String::from(header);
        body.push_str(DEFAULT_CONFIG_TEMPLATE);
        fs::write(&path, body)
            .with_context(|| format!("write defaults temp {}", path.display()))?;
        Ok(path)
    }

    /// True when `path` is the defaults open sentinel (not a real filesystem path).
    pub fn is_defaults_open_path(path: &str) -> bool {
        let trimmed = path.trim();
        trimmed == DEFAULT_SETTINGS_OPEN_PATH
            || trimmed.strip_suffix('/').unwrap_or(trimmed) == DEFAULT_SETTINGS_OPEN_PATH
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

/// Parse JSONC into a [`serde_json::Value`] (comments / trailing commas via strip).
fn parse_jsonc_value(text: &str) -> Result<serde_json::Value> {
    let json = strip_jsonc(text.trim());
    serde_json::from_str(&json).context("parse jsonc value")
}

/// Recursively insert keys from `defaults` that are missing in `existing`.
/// Never replaces an existing key (including `null` / wrong types).
/// Returns whether any key was inserted.
pub fn merge_missing_json(
    existing: &mut serde_json::Value,
    defaults: &serde_json::Value,
) -> bool {
    use serde_json::Value;
    match (existing, defaults) {
        (Value::Object(existing_map), Value::Object(defaults_map)) => {
            let mut changed = false;
            for (key, default_value) in defaults_map {
                match existing_map.get_mut(key) {
                    Some(existing_value) => {
                        changed |= merge_missing_json(existing_value, default_value);
                    }
                    None => {
                        existing_map.insert(key.clone(), default_value.clone());
                        changed = true;
                    }
                }
            }
            changed
        }
        _ => false,
    }
}

/// Write `merged` back, preserving comments/formatting from `existing_text` when possible
/// (same approach as Fresh’s JSONC CST reconcile).
fn render_merged_config_text(existing_text: &str, merged: &serde_json::Value) -> String {
    if let Some(text) = reconcile_preserving_comments(existing_text, merged) {
        return text;
    }
    serde_json::to_string_pretty(merged).unwrap_or_else(|_| merged.to_string())
}

fn reconcile_preserving_comments(existing: &str, clean: &serde_json::Value) -> Option<String> {
    use jsonc_parser::cst::CstRootNode;
    use serde_json::Value;

    let Value::Object(target) = clean else {
        return None;
    };
    let root = CstRootNode::parse(existing, &Default::default()).ok()?;
    root.value()?.as_object()?;
    let obj = root.object_value_or_set();
    reconcile_cst_object(&obj, target);
    Some(root.to_string())
}

fn reconcile_cst_object(
    obj: &jsonc_parser::cst::CstObject,
    target: &serde_json::Map<String, serde_json::Value>,
) {
    use serde_json::Value;

    // Do not remove user keys that are absent from defaults — only fill gaps.
    for (key, new_value) in target {
        match obj.get(key) {
            Some(prop) => {
                let current = prop.value().and_then(|n| n.to_serde_value());
                if current.as_ref() == Some(new_value) {
                    continue;
                }
                match (new_value, prop.value().and_then(|n| n.as_object())) {
                    (Value::Object(child_target), Some(child_obj)) => {
                        reconcile_cst_object(&child_obj, child_target);
                    }
                    _ => {
                        // Existing non-object value: leave it (never override).
                    }
                }
            }
            None => {
                obj.append(key, json_value_to_cst_input(new_value));
            }
        }
    }
}

fn json_value_to_cst_input(value: &serde_json::Value) -> jsonc_parser::cst::CstInputValue {
    use jsonc_parser::cst::CstInputValue;
    use serde_json::Value;
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(b) => CstInputValue::Bool(*b),
        Value::Number(n) => CstInputValue::Number(n.to_string()),
        Value::String(s) => CstInputValue::String(s.clone()),
        Value::Array(arr) => {
            CstInputValue::Array(arr.iter().map(json_value_to_cst_input).collect())
        }
        Value::Object(map) => CstInputValue::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json_value_to_cst_input(v)))
                .collect(),
        ),
    }
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
        assert_eq!(cfg.ui.palette, "primer");
        assert_eq!(cfg.ui.font_weight, 400);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let cfg = Config::load_from_path(Path::new("/no/such/fresh-gui-config.json")).unwrap();
        assert_eq!(cfg.resolve_shell().0, "zsh");
        assert_eq!(cfg.ui.theme, "system");
        assert_eq!(cfg.ui.palette, "primer");
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
        assert!(Config::ensure_file(&path).unwrap());
        assert!(path.is_file());
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.resolve_shell().0, "zsh");
        assert_eq!(cfg.ui.theme, "system");
        assert_eq!(cfg.ui.palette, "primer");
        assert!(!Config::ensure_file(&path).unwrap());
    }

    #[test]
    fn ensure_file_adds_missing_keys_without_overriding() {
        let dir = tempfile_dir();
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{
  // keep this comment
  "ui": {
    "theme": "light", // mine
    "terminalFontSize": 18
  }
}
"#,
        )
        .unwrap();

        assert!(Config::ensure_file(&path).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("keep this comment"),
            "comments should be preserved:\n{text}"
        );
        assert!(
            text.contains("\"theme\": \"light\""),
            "existing theme must not be overridden:\n{text}"
        );
        assert!(
            text.contains("\"terminalFontSize\": 18"),
            "existing font size must not be overridden:\n{text}"
        );
        assert!(
            text.contains("\"palette\""),
            "missing palette should be inserted:\n{text}"
        );
        assert!(
            text.contains("\"fontWeight\""),
            "missing fontWeight should be inserted:\n{text}"
        );
        assert!(
            text.contains("\"showDotfiles\""),
            "missing showDotfiles should be inserted:\n{text}"
        );
        assert!(
            text.contains("\"terminal\""),
            "missing terminal section should be inserted:\n{text}"
        );

        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.ui.theme, "light");
        assert_eq!(cfg.ui.terminal_font_size, 18);
        assert_eq!(cfg.ui.palette, "primer");
        assert_eq!(cfg.ui.font_weight, 400);
        assert_eq!(cfg.resolve_shell().0, "zsh");

        // Second call is a no-op.
        assert!(!Config::ensure_file(&path).unwrap());
    }

    #[test]
    fn merge_missing_json_only_fills_gaps() {
        use serde_json::json;
        let mut existing = json!({
            "ui": { "theme": "dark", "webgl": false },
            "extra": 1
        });
        let defaults = json!({
            "ui": {
                "theme": "system",
                "palette": "primer",
                "webgl": true
            },
            "terminal": { "shell": { "command": "zsh", "args": [] } }
        });
        assert!(merge_missing_json(&mut existing, &defaults));
        assert_eq!(existing["ui"]["theme"], "dark");
        assert_eq!(existing["ui"]["webgl"], false);
        assert_eq!(existing["ui"]["palette"], "primer");
        assert_eq!(existing["extra"], 1);
        assert_eq!(existing["terminal"]["shell"]["command"], "zsh");
        assert!(!merge_missing_json(&mut existing, &defaults));
    }

    #[test]
    fn default_template_parses() {
        let cfg = Config::parse(DEFAULT_CONFIG_TEMPLATE).unwrap();
        assert_eq!(cfg.ui.theme, "system");
        assert_eq!(cfg.ui.palette, "primer");
        assert_eq!(cfg.ui.font_weight, 400);
        assert_eq!(cfg.ui.mono_font_weight, 400);
        assert_eq!(cfg.resolve_shell().0, "zsh");
        assert!(!cfg.ui.show_dotfiles);
        assert!(!cfg.ui.show_git_dirs);
        assert!(!cfg.ui.editor_minimap);
        assert!(cfg.ui.editor_line_wrap);
        assert!(
            !cfg.shortkeys.is_empty(),
            "embedded defaults must list shortkeys"
        );
        assert!(
            cfg.shortkeys.iter().any(|s| s.action == "settings.open"),
            "defaults should include settings.open"
        );
    }

    #[test]
    fn effective_shortkeys_falls_back_when_empty() {
        let cfg = Config {
            ui: UiConfig::default(),
            terminal: TerminalConfig::default(),
            shortkeys: Vec::new(),
        };
        let effective = cfg.effective_shortkeys();
        assert!(!effective.is_empty());
        assert!(effective.iter().any(|s| s.action == "tab.new"));
    }

    #[test]
    fn parses_user_shortkeys() {
        let cfg = Config::parse(
            r#"{
              "shortkeys": [
                { "action": "tab.new", "shortkey": "Mod+N", "when": "global" }
              ]
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.shortkeys.len(), 1);
        assert_eq!(cfg.shortkeys[0].action, "tab.new");
        assert_eq!(cfg.shortkeys[0].shortkey, "Mod+N");
        assert_eq!(
            cfg.shortkeys[0].when.as_deref().unwrap_or("global"),
            "global"
        );
        assert_eq!(cfg.effective_shortkeys().len(), 1);
    }

    #[test]
    fn materialize_defaults_temp_writes_catalog() {
        let path = Config::materialize_defaults_temp().unwrap();
        assert!(path.is_file());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("READ-ONLY CATALOG"));
        assert!(text.contains("\"shortkeys\""));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn ensure_file_adds_shortkeys_when_missing() {
        let dir = tempfile_dir();
        let path = dir.join("config.json");
        fs::write(
            &path,
            r#"{
  "ui": { "theme": "dark" }
}
"#,
        )
        .unwrap();
        assert!(Config::ensure_file(&path).unwrap());
        let text = fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("\"shortkeys\""),
            "missing shortkeys should be inserted:\n{text}"
        );
        let cfg = Config::load_from_path(&path).unwrap();
        assert!(!cfg.shortkeys.is_empty());
    }

    #[test]
    fn parses_editor_minimap_flag() {
        let cfg = Config::parse(r#"{"ui":{"editorMinimap":true}}"#).unwrap();
        assert!(cfg.ui.editor_minimap);
    }

    #[test]
    fn parses_editor_line_wrap_flag() {
        let cfg = Config::parse(r#"{"ui":{"editorLineWrap":false}}"#).unwrap();
        assert!(!cfg.ui.editor_line_wrap);
    }

    #[test]
    fn parses_explorer_visibility_flags() {
        let cfg = Config::parse(
            r#"{
              "ui": { "showDotfiles": true, "showGitDirs": true }
            }"#,
        )
        .unwrap();
        assert!(cfg.ui.show_dotfiles);
        assert!(cfg.ui.show_git_dirs);
    }

    #[test]
    fn parses_palette_and_typography() {
        let cfg = Config::parse(
            r#"{
              "ui": {
                "palette": "Nord",
                "fontWeight": 550,
                "monoFontWeight": 700,
                "fontFamily": "IBM Plex Sans",
                "monoFontFamily": "IBM Plex Mono"
              }
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.ui.palette, "nord");
        assert_eq!(cfg.ui.font_weight, 600);
        assert_eq!(cfg.ui.mono_font_weight, 700);
        assert_eq!(cfg.ui.font_family, "IBM Plex Sans");
        assert_eq!(cfg.ui.mono_font_family, "IBM Plex Mono");
    }

    #[test]
    fn unknown_palette_falls_back_to_primer() {
        let cfg = Config::parse(r#"{"ui":{"palette":"octarine"}}"#).unwrap();
        assert_eq!(cfg.ui.palette, "primer");
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-config-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }
}

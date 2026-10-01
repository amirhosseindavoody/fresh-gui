//! JSON config for `fresh-gui` (+ host UI prefs).
//!
//! Mirrors Fresh editor’s user-facing shape for the terminal shell
//! (`terminal.shell.{command,args}`) and stores host chrome under `ui.*`.
//! Color packs under `ui.palette` reuse Fresh theme names where applicable
//! (`nord`, `dracula`, …) mapped onto host CSS tokens.
//!
//! Default path (Unix): `$XDG_CONFIG_HOME/fresh-gui/config.json`
//! (falls back to `~/.config/fresh-gui/config.json`).
//! Windows: `%APPDATA%\fresh-gui\config.json`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Filename under the config directory (same as Fresh).
pub const FILENAME: &str = "config.json";

/// Default shell when config omits `terminal.shell` or leaves it empty.
#[cfg(not(windows))]
pub const DEFAULT_SHELL_COMMAND: &str = "zsh";
#[cfg(windows)]
pub const DEFAULT_SHELL_COMMAND: &str = "powershell";

/// Documented starter file (JSONC) written on first settings open.
#[cfg(not(windows))]
pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../defaults/config.default.jsonc");
#[cfg(windows)]
pub const DEFAULT_CONFIG_TEMPLATE: &str = include_str!("../defaults/config.default.windows.jsonc");

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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub ui: UiConfig,
    #[serde(default)]
    pub terminal: TerminalConfig,
    /// Sparse Fresh editor overrides, preserving Fresh defaults for omitted fields.
    #[serde(default)]
    pub editor: fresh::partial_config::PartialEditorConfig,
    /// Fresh-compatible LSP server configuration keyed by Fresh language name.
    /// Each value accepts either one server object or an array of servers.
    #[serde(default)]
    pub lsp: std::collections::HashMap<String, fresh::types::LspLanguageConfig>,
    /// Fresh language definitions / file associations keyed by language id.
    /// These can add extensions or exact filenames for custom language ids.
    #[serde(default)]
    pub languages: std::collections::HashMap<String, fresh::partial_config::PartialLanguageConfig>,
    #[serde(default)]
    pub shortkeys: Vec<ShortkeyEntry>,
    /// Other valid Fresh PartialConfig fields stored in the same top-level
    /// document. Explicit host/Fresh fields above stay strongly typed.
    #[serde(flatten, skip_serializing)]
    pub fresh: fresh::partial_config::PartialConfig,
}

impl PartialEq for Config {
    fn eq(&self, other: &Self) -> bool {
        self.ui == other.ui
            && self.terminal == other.terminal
            && self.shortkeys == other.shortkeys
            && serde_json::to_value(&self.editor).ok() == serde_json::to_value(&other.editor).ok()
            && serde_json::to_value(&self.fresh).ok() == serde_json::to_value(&other.fresh).ok()
            && serde_json::to_value(&self.lsp).ok() == serde_json::to_value(&other.lsp).ok()
            && serde_json::to_value(&self.languages).ok()
                == serde_json::to_value(&other.languages).ok()
    }
}

/// A GPUI action name, keystroke, and optional focus context.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShortkeyEntry {
    pub action: String,
    pub shortkey: String,
    #[serde(default)]
    pub when: Option<String>,
}

/// Host UI settings sent on the ADE hello (GPUI chrome).
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
    /// interactive / OSC 7 setup for known Unix shells (`zsh`, `bash`, …).
    /// `powershell`, `pwsh`, and `cmd` are started with no extra arguments.
    #[serde(default)]
    pub args: Vec<String>,
}

impl Config {
    /// Apply Fresh-owned settings to an already resolved Fresh config.
    /// Fresh's `PartialConfig` merge semantics keep unspecified editor and
    /// language fields at their resolved defaults.
    pub fn apply_fresh(&self, fresh_config: &mut fresh::config::Config) {
        use fresh::partial_config::{Merge, PartialConfig};

        let mut overrides = self.fresh.clone();
        overrides.editor = Some(self.editor.clone());
        overrides.languages = Some(self.languages.clone());
        overrides.lsp = Some(self.lsp.clone());
        // An explicit global disable is meaningful even when the daemon has
        // configured servers. Otherwise derive whether the daemon has any
        // servers at all, preserving the default "off when unconfigured".
        overrides.lsp_enabled = self.fresh.lsp_enabled.or(Some(!self.lsp.is_empty()));
        // The value being mutated is the lower-precedence layer. Fresh's
        // Merge contract keeps values already present in `self`, so start
        // with the settings editor overrides and fill their gaps from the
        // already-resolved daemon/workspace config.
        let mut partial = overrides;
        partial.merge_from(&PartialConfig::from(&*fresh_config));
        // `fresh-gui` owns the daemon's LSP set. It intentionally replaces
        // Fresh's discovered servers rather than inheriting unrelated ones.
        partial.lsp = Some(self.lsp.clone());
        partial.lsp_enabled = self.fresh.lsp_enabled.or(Some(!self.lsp.is_empty()));
        *fresh_config = partial.resolve();
        // resolve() merges Fresh's built-in servers. ADE only authorizes the
        // explicitly configured daemon set, so replace it after resolution.
        fresh_config.lsp = self.lsp.clone();
    }

    /// Reconfigure the already supported language/server services without
    /// overriding the active project's editor preferences during a live reload.
    pub fn apply_fresh_services(&self, config: &mut fresh::config::Config) {
        let mut services = self.clone();
        services.editor = Default::default();
        // A service-only reload must still honor the user's global LSP kill
        // switch; clearing all typed Fresh settings used to silently turn it
        // back on whenever an LSP was configured.
        let lsp_enabled = services.fresh.lsp_enabled;
        services.fresh = Default::default();
        services.fresh.lsp_enabled = lsp_enabled;
        services.apply_fresh(config);
    }

    /// Apply Fresh's workspace and session layers above daemon user settings.
    /// Fresh exposes these layer loaders independently; using them preserves
    /// its precedence without reading unrelated native-user config locations.
    pub fn apply_fresh_project(
        &self,
        fresh_config: &mut fresh::config::Config,
        working_dir: &Path,
    ) -> Result<()> {
        use fresh::partial_config::{Merge, PartialConfig};

        let dir_context = fresh::config_io::DirectoryContext::for_testing(
            &std::env::temp_dir().join(format!("fresh-gui-settings-{}", uuid::Uuid::new_v4())),
        );
        let resolver =
            fresh::config_io::ConfigResolver::new(dir_context, working_dir.to_path_buf());
        let mut higher = resolver
            .load_session_layer()
            .map_err(anyhow::Error::new)?
            .unwrap_or_default();
        if let Some(project) = resolver.load_project_layer().map_err(anyhow::Error::new)? {
            higher.merge_from(&project);
        }
        // Keep server commands and associations owned by the daemon. Project
        // layers may only disable matching configured servers; they cannot
        // add a server (which could spawn an unreviewed process) or replace a
        // daemon command. Fresh's ordinary map merge would replace a whole
        // language entry, so capture these booleans before merging the lower
        // daemon config and apply them to the configured set afterward.
        let workspace_lsp = higher.lsp.clone();
        let daemon_lsp = fresh_config.lsp.clone();
        higher.merge_from(&PartialConfig::from(&*fresh_config));
        let mut resolved = higher.resolve();
        resolved.lsp = apply_workspace_lsp_enablement(daemon_lsp, workspace_lsp.as_ref());
        // An explicit daemon-level false is a global opt-out and has higher
        // precedence than project/session settings.
        if self.fresh.lsp_enabled == Some(false) {
            resolved.lsp_enabled = false;
        }
        *fresh_config = resolved;
        Ok(())
    }

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

        let text =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
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
        let mut cfg: Config = jsonc_parser::parse_to_serde_value(trimmed, &Default::default())
            .context("parse config jsonc")?;
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

        let text =
            fs::read_to_string(path).with_context(|| format!("read config {}", path.display()))?;
        if text.trim().is_empty() {
            fs::write(path, DEFAULT_CONFIG_TEMPLATE)
                .with_context(|| format!("write default config {}", path.display()))?;
            info!(path = %path.display(), "wrote default config.json (was empty)");
            return Ok(true);
        }

        let defaults =
            parse_jsonc_value(DEFAULT_CONFIG_TEMPLATE).context("parse default config template")?;
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
    }

    /// `(command, args)` configured for a new PTY when the client did not override `shell`.
    ///
    /// Does not check that the executable exists. Spawn probes the command and,
    /// falls back when it is missing ([`crate::shell_resolve`]).
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

/// Apply only workspace enable/disable controls to the daemon-owned server
/// set. The workspace may use a compact `{ "enabled": false }` entry, a
/// server array, or identify servers by name/command. It cannot introduce a
/// command or turn on a server disabled by the daemon.
fn apply_workspace_lsp_enablement(
    mut configured: std::collections::HashMap<String, fresh::types::LspLanguageConfig>,
    workspace: Option<&std::collections::HashMap<String, fresh::types::LspLanguageConfig>>,
) -> std::collections::HashMap<String, fresh::types::LspLanguageConfig> {
    let Some(workspace) = workspace else {
        return configured;
    };

    for (language, servers) in &mut configured {
        let Some(controls) = workspace.get(language) else {
            continue;
        };
        let controls = controls.as_slice();
        let servers = servers.as_mut_slice();
        let server_count = servers.len();
        for (index, server) in servers.iter_mut().enumerate() {
            let matching_control = controls
                .iter()
                .find(|control| {
                    control.name.is_some() && control.name == server.name
                        || !control.command.is_empty() && control.command == server.command
                })
                .or_else(|| {
                    let positional = if controls.len() == server_count {
                        controls.get(index)
                    } else if controls.len() == 1 {
                        controls.first()
                    } else {
                        None
                    };
                    positional.filter(|control| {
                        control.name.is_none() && control.command.is_empty()
                    })
                });
            if let Some(control) = matching_control {
                server.enabled &= control.enabled;
            }
        }
    }

    configured
}

/// Unix: `$XDG_CONFIG_HOME/fresh-gui/config.json` or `~/.config/fresh-gui/config.json`.
/// Windows: `%APPDATA%\fresh-gui\config.json`.
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
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            let appdata = appdata.trim();
            if !appdata.is_empty() {
                return PathBuf::from(appdata).join("fresh-gui");
            }
        }
    }
    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
        .join("fresh-gui")
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
}

/// Parse JSONC into a [`serde_json::Value`] (comments / trailing commas via strip).
fn parse_jsonc_value(text: &str) -> Result<serde_json::Value> {
    jsonc_parser::parse_to_serde_value(text.trim(), &Default::default())
        .context("parse jsonc value")
}

/// Recursively insert keys from `defaults` that are missing in `existing`.
/// Never replaces an existing key (including `null` / wrong types).
/// Returns whether any key was inserted.
pub fn merge_missing_json(existing: &mut serde_json::Value, defaults: &serde_json::Value) -> bool {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn default_resolves_to_zsh_and_system_theme() {
        let cfg = Config::default();
        let (cmd, args) = cfg.resolve_shell();
        assert_eq!(cmd, DEFAULT_SHELL_COMMAND);
        assert!(args.is_empty());
        assert_eq!(cfg.ui.theme, "system");
        assert_eq!(cfg.ui.palette, "primer");
        assert_eq!(cfg.ui.font_weight, 400);
    }

    #[test]
    fn missing_file_uses_defaults() {
        let cfg = Config::load_from_path(Path::new("/no/such/fresh-gui-config.json")).unwrap();
        assert_eq!(cfg.resolve_shell().0, DEFAULT_SHELL_COMMAND);
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
        assert_eq!(cfg.resolve_shell().0, DEFAULT_SHELL_COMMAND);
    }

    #[test]
    fn ensure_file_writes_template() {
        let dir = tempfile_dir();
        let path = dir.join("nested").join("config.json");
        assert!(Config::ensure_file(&path).unwrap());
        assert!(path.is_file());
        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.resolve_shell().0, DEFAULT_SHELL_COMMAND);
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
        assert!(
            text.contains("\"shortkeys\""),
            "missing shortkeys should be inserted:\n{text}"
        );

        let cfg = Config::load_from_path(&path).unwrap();
        assert_eq!(cfg.ui.theme, "light");
        assert_eq!(cfg.ui.terminal_font_size, 18);
        assert_eq!(cfg.ui.palette, "primer");
        assert_eq!(cfg.ui.font_weight, 400);
        assert_eq!(cfg.resolve_shell().0, DEFAULT_SHELL_COMMAND);
        assert_eq!(cfg.shortkeys.len(), 51);

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
        assert_eq!(cfg.resolve_shell().0, DEFAULT_SHELL_COMMAND);
        #[cfg(unix)]
        {
            assert!(
                DEFAULT_CONFIG_TEMPLATE.contains("falls back"),
                "template should document the Unix shell fallback"
            );
            assert!(DEFAULT_CONFIG_TEMPLATE.contains("$SHELL"));
        }
        assert!(!cfg.ui.show_dotfiles);
        assert!(!cfg.ui.show_git_dirs);
        assert!(!cfg.ui.editor_minimap);
        assert!(cfg.ui.editor_line_wrap);
        assert_eq!(cfg.shortkeys.len(), 51);
        assert_eq!(cfg.shortkeys[0].action, "NewTerminal");
        assert_eq!(cfg.shortkeys[0].shortkey, "ctrl-t");
        assert_eq!(cfg.shortkeys[0].when, None);
        assert!(cfg.shortkeys.iter().any(|key| {
            key.action == "CopyExplorer" && key.when.as_deref() == Some("Explorer")
        }));
        assert!(cfg.shortkeys.iter().any(|key| {
            key.action == "ZoomInContent" && key.shortkey == "ctrl-=" && key.when.is_none()
        }));
        assert!(cfg.shortkeys.iter().any(|key| {
            key.action == "ZoomInUi" && key.shortkey == "ctrl-shift-=" && key.when.is_none()
        }));
    }

    #[test]
    fn shortkeys_deserialize_with_optional_context() {
        let cfg = Config::parse(r#"{"shortkeys":[{"action":"NewTerminal","shortkey":"ctrl-t"},{"action":"CopyExplorer","shortkey":"ctrl-c","when":"Explorer"}]}"#).unwrap();
        assert_eq!(cfg.shortkeys.len(), 2);
        assert_eq!(cfg.shortkeys[0].when, None);
        assert_eq!(cfg.shortkeys[1].when.as_deref(), Some("Explorer"));
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

    #[test]
    fn parses_single_and_multiple_fresh_lsp_servers_and_language_associations() {
        let cfg = Config::parse(
            r#"{
              "lsp": {
                "toml": { "command": "/opt/bin/tombi", "args": ["lsp"] },
                "python": [
                  { "command": "ruff", "args": ["server"], "only_features": ["diagnostics", "format"] },
                  { "command": "ty", "args": ["server"], "only_features": ["diagnostics"] }
                ]
              },
              "languages": {
                "my_lang": { "extensions": ["ml"], "filenames": ["Build.my"] }
              }
            }"#,
        )
        .unwrap();

        let toml = cfg.lsp.get("toml").unwrap().as_slice();
        assert_eq!(toml.len(), 1);
        assert_eq!(toml[0].command, "/opt/bin/tombi");
        assert_eq!(toml[0].args(), ["lsp"]);
        assert!(toml[0].enabled);

        let python = cfg.lsp.get("python").unwrap().as_slice();
        assert_eq!(python.len(), 2);
        assert_eq!(python[0].command, "ruff");
        assert_eq!(python[0].args(), ["server"]);
        assert_eq!(python[1].command, "ty");
        assert_eq!(python[1].args(), ["server"]);
        assert_eq!(python[0].only_features.as_ref().unwrap().len(), 2);

        let custom_language = cfg.languages.get("my_lang").unwrap();
        assert_eq!(custom_language.extensions.as_deref().unwrap(), ["ml"]);
        assert_eq!(custom_language.filenames.as_deref().unwrap(), ["Build.my"]);
    }

    #[test]
    fn jsonc_config_retains_unicode_values_and_accepts_trailing_commas() {
        let config = Config::parse(
            r#"{
            // Native and Fresh fields use the same JSONC parser.
            "ui": {"fontFamily": "日本語",},
            "languages": {"custom": {"filenames": ["Build.テスト"],}},
        }"#,
        )
        .unwrap();
        assert_eq!(config.ui.font_family, "日本語");
        assert_eq!(
            config.languages["custom"].filenames.as_ref().unwrap()[0],
            "Build.テスト"
        );
    }

    #[test]
    fn sparse_fresh_editor_and_language_overrides_keep_other_defaults() {
        let cfg = Config::parse(
            r#"{"editor":{"tab_size":9},"languages":{"rust":{"use_tabs":true,"tab_size":8}}}"#,
        )
        .unwrap();
        let mut fresh = fresh::config::Config::default();
        let default_line_wrap = fresh.editor.line_wrap;
        let default_tab_size = fresh.editor.tab_size;
        let default_rust_wrap = fresh.languages.get("rust").unwrap().line_wrap;
        let default_rust_tabs = fresh.languages.get("rust").unwrap().use_tabs;
        let default_rust_tab_size = fresh.languages.get("rust").unwrap().tab_size;
        cfg.apply_fresh(&mut fresh);
        assert_eq!(fresh.editor.tab_size, 9);
        assert_ne!(fresh.editor.tab_size, default_tab_size);
        assert_eq!(fresh.editor.line_wrap, default_line_wrap);
        let rust = fresh.languages.get("rust").unwrap();
        assert_eq!(rust.use_tabs, Some(true));
        assert_ne!(rust.use_tabs, default_rust_tabs);
        assert_eq!(rust.tab_size, Some(8));
        assert_ne!(rust.tab_size, default_rust_tab_size);
        assert_eq!(rust.line_wrap, default_rust_wrap);
    }

    #[test]
    fn applies_other_typed_fresh_partial_config_fields() {
        let cfg = Config::parse(r#"{"theme":"noir","editor":{"restore_previous_session":false}}"#)
            .unwrap();
        let mut fresh = fresh::config::Config::default();
        cfg.apply_fresh(&mut fresh);
        assert_eq!(fresh.theme.0, "noir");
        assert!(!fresh.editor.restore_previous_session);
    }

    #[test]
    fn explicit_global_lsp_disable_survives_user_and_service_layers() {
        let cfg = Config::parse(
            r#"{"lsp_enabled":false,"lsp":{"rust":{"command":"rust-analyzer"}}}"#,
        )
        .unwrap();
        let mut fresh = fresh::config::Config::default();
        cfg.apply_fresh(&mut fresh);
        assert!(!fresh.lsp_enabled);
        cfg.apply_fresh_services(&mut fresh);
        assert!(!fresh.lsp_enabled);
    }

    #[test]
    fn workspace_can_disable_daemon_server_but_cannot_add_or_enable_servers() {
        let root = tempfile_dir();
        std::fs::create_dir_all(root.join(".fresh")).unwrap();
        std::fs::write(
            root.join(".fresh/config.json"),
            r#"{"lsp":{"rust":{"enabled":false},"python":{"command":"python-lsp"}}}"#,
        )
        .unwrap();

        let host = Config::parse(
            r#"{"lsp":{"rust":{"command":"rust-analyzer"}},"lsp_enabled":true}"#,
        )
        .unwrap();
        let mut fresh = fresh::config::Config::default();
        host.apply_fresh(&mut fresh);
        host.apply_fresh_project(&mut fresh, &root).unwrap();

        let rust = fresh.lsp.get("rust").unwrap().as_slice();
        assert_eq!(rust.len(), 1);
        assert_eq!(rust[0].command, "rust-analyzer");
        assert!(!rust[0].enabled);
        assert!(!fresh.lsp.contains_key("python"));
        assert!(fresh.lsp_enabled);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_language_formatter_and_format_on_save_override_daemon_layer() {
        let root = tempfile_dir();
        std::fs::create_dir_all(root.join(".fresh")).unwrap();
        std::fs::write(
            root.join(".fresh/config.json"),
            r#"{"languages":{"rust":{"formatter":{"command":"rustfmt","args":["--edition","2024"]},"format_on_save":true}}}"#,
        )
        .unwrap();

        let host = Config::parse(
            r#"{"languages":{"rust":{"formatter":{"command":"oldfmt"},"format_on_save":false}}}"#,
        )
        .unwrap();
        let mut fresh = fresh::config::Config::default();
        host.apply_fresh(&mut fresh);
        host.apply_fresh_project(&mut fresh, &root).unwrap();
        let rust = fresh.languages.get("rust").unwrap();
        assert_eq!(rust.formatter.as_ref().unwrap().command, "rustfmt");
        assert_eq!(rust.formatter.as_ref().unwrap().args, ["--edition", "2024"]);
        assert!(rust.format_on_save);

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn project_and_session_layers_override_daemon_user_settings() {
        let root =
            std::env::temp_dir().join(format!("fresh-gui-layer-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".fresh")).unwrap();
        std::fs::write(
            root.join(".fresh/config.json"),
            r#"{"editor":{"tab_size":9}}"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".fresh/session.json"),
            r#"{"editor":{"use_tabs":true}}"#,
        )
        .unwrap();

        let host = Config::parse(r#"{"editor":{"tab_size":4,"use_tabs":false}}"#).unwrap();
        let mut fresh = fresh::config::Config::default();
        host.apply_fresh(&mut fresh);
        host.apply_fresh_project(&mut fresh, &root).unwrap();
        assert_eq!(fresh.editor.tab_size, 9);
        assert!(fresh.editor.use_tabs);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn default_config_does_not_start_any_lsp_server() {
        let cfg = Config::default();
        assert!(cfg.lsp.is_empty());
        assert!(cfg.languages.is_empty());
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

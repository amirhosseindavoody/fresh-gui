//! UI preferences on the machine running the window.
//!
//! The daemon's Hello/ConfigUpdated carries its own config. Over SSH that file
//! is on another machine, so a client-side `ui` object is overlaid here.

use std::path::PathBuf;

use anyhow::{Context, Result};
use fresh_gui_protocol::HelloUi;
use serde_json::Value;

fn client_config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(xdg).join("fresh-gui/config.json");
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|value| !value.is_empty()) {
        return PathBuf::from(appdata).join("fresh-gui/config.json");
    }
    PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()))
        .join(".config/fresh-gui/config.json")
}

fn overlay_ui(base: &HelloUi, local: &Value) -> Result<HelloUi> {
    let mut merged = serde_json::to_value(base)?;
    let Some(settings) = local.get("ui").and_then(Value::as_object) else {
        return Ok(base.clone());
    };
    let target = merged.as_object_mut().expect("HelloUi serializes as an object");
    for (key, value) in settings {
        target.insert(key.clone(), value.clone());
    }
    serde_json::from_value(merged).context("client ui settings")
}

/// Read the local UI overlay afresh, including when the remote daemon reloads.
pub fn load_ui(base: &HelloUi) -> Result<HelloUi> {
    let path = client_config_path();
    if !path.is_file() {
        return Ok(base.clone());
    }
    let contents = std::fs::read_to_string(&path)
        .with_context(|| format!("read client config {}", path.display()))?;
    let value: Value = jsonc_parser::parse_to_serde_value(
        &contents,
        &jsonc_parser::ParseOptions::default(),
    )
    .with_context(|| format!("parse client config {}", path.display()))?;
    overlay_ui(base, &value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_ui_overrides_only_selected_daemon_fields() {
        let base: HelloUi = serde_json::from_value(serde_json::json!({
            "theme": "dark", "terminalFontSize": 18, "editorLineWrap": false
        })).unwrap();
        let ui = overlay_ui(&base, &serde_json::json!({"ui": {"theme": "light"}})).unwrap();
        assert_eq!(ui.theme, "light");
        assert_eq!(ui.terminal_font_size, 18);
        assert!(!ui.editor_line_wrap);
    }
}

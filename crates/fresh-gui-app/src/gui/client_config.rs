//! UI preferences on the machine running the window.
//!
//! The daemon's Hello/ConfigUpdated carries its own config. Over SSH that file
//! is on another machine, so a client-side `ui` object is overlaid here.

use std::path::PathBuf;

use anyhow::{Context, Result};
use fresh_gui_protocol::HelloUi;
use serde_json::Value;

pub fn client_config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(xdg).join("fresh-gui/config.json");
    }
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA").filter(|value| !value.is_empty()) {
        return PathBuf::from(appdata).join("fresh-gui/config.json");
    }
    PathBuf::from(
        std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .unwrap_or_else(|| ".".into()),
    )
    .join(".config/fresh-gui/config.json")
}

fn overlay_ui(base: &HelloUi, local: &Value) -> Result<HelloUi> {
    let mut merged = serde_json::to_value(base)?;
    let Some(settings) = local.get("ui").and_then(Value::as_object) else {
        return Ok(base.clone());
    };
    let target = merged
        .as_object_mut()
        .expect("HelloUi serializes as an object");
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
    let value: Value =
        jsonc_parser::parse_to_serde_value(&contents, &jsonc_parser::ParseOptions::default())
            .with_context(|| format!("parse client config {}", path.display()))?;
    overlay_ui(base, &value)
}

/// Local presentation settings never use the remote filesystem.
pub fn read_document() -> Result<String> {
    let path = client_config_path();
    if !path.exists() {
        return Ok("{}".into());
    }
    std::fs::read_to_string(&path).context("read local settings")
}

pub fn patch_document(base: &str, path: &[String], value: Option<&Value>) -> Result<String> {
    let target = client_config_path();
    anyhow::ensure!(
        read_document()? == base,
        "Local settings changed; reload before saving"
    );
    anyhow::ensure!(
        path.first().map(String::as_str) == Some("ui"),
        "Only UI settings belong to the local client"
    );
    let text = fresh_gui_protocol::settings::patch_jsonc(base, path, value)?;
    let value = fresh_gui_protocol::settings::read_jsonc(&text)?;
    let defaults: HelloUi = serde_json::from_value(serde_json::json!({}))?;
    overlay_ui(&defaults, &value)?;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, &text).context("write local settings")?;
    Ok(text)
}

/// The local JSON editor shares the same stale-write guard as typed controls.
pub fn write_document(base: &str, text: &str) -> Result<String> {
    anyhow::ensure!(
        read_document()? == base,
        "Local settings changed; reload before saving"
    );
    let value = fresh_gui_protocol::settings::read_jsonc(text)?;
    let defaults: HelloUi = serde_json::from_value(serde_json::json!({}))?;
    overlay_ui(&defaults, &value)?;
    let target = client_config_path();
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&target, text).context("write local settings JSON")?;
    Ok(text.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_ui_overrides_only_selected_daemon_fields() {
        let base: HelloUi = serde_json::from_value(serde_json::json!({
            "theme": "dark", "terminalFontSize": 18, "editorLineWrap": false
        }))
        .unwrap();
        let ui = overlay_ui(&base, &serde_json::json!({"ui": {"theme": "light"}})).unwrap();
        assert_eq!(ui.theme, "light");
        assert_eq!(ui.terminal_font_size, 18);
        assert!(!ui.editor_line_wrap);
    }
}

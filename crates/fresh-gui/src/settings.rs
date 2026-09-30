//! Daemon-authoritative settings reads and JSONC patches.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fresh_gui_protocol::Message;
use serde_json::Value;

use crate::{config::Config, workspace::WorkspaceStore};

const WORKSPACE_CONFIG: &str = ".fresh/config.json";

pub async fn target_path(
    config_path: &Path,
    workspaces: &WorkspaceStore,
    workspace_id: Option<&str>,
) -> Result<PathBuf> {
    match workspace_id {
        Some(id) => {
            let root = workspaces
                .root_of(id)
                .await
                .with_context(|| format!("unknown workspace {id}"))?;
            Ok(PathBuf::from(root).join(WORKSPACE_CONFIG))
        }
        None => Ok(config_path.to_path_buf()),
    }
}

pub fn read_snapshot(
    path: &Path,
    workspace_id: Option<String>,
    request_id: String,
) -> Result<Message> {
    let text = if path.is_file() {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        "{}\n".to_owned()
    };
    let defaults = defaults_value();
    Ok(Message::SettingsSnapshot {
        request_id,
        workspace_id,
        path: path.display().to_string(),
        text,
        defaults,
    })
}

pub fn apply_patch(
    path: &Path,
    base_text: &str,
    parts: &[String],
    value: Option<Value>,
    workspace: bool,
) -> Result<String> {
    static PATCH_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = PATCH_LOCK
        .lock()
        .map_err(|_| anyhow::anyhow!("settings write lock poisoned"))?;
    let current = if path.is_file() {
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?
    } else {
        "{}\n".to_owned()
    };
    if current != base_text {
        bail!("settings changed on disk; reload before saving");
    }
    if parts.is_empty() {
        bail!("settings patch path cannot be empty");
    }
    if workspace
        && matches!(
            parts.first().map(String::as_str),
            Some("ui" | "terminal" | "shortkeys")
        )
    {
        bail!("host settings cannot be stored in a workspace config");
    }
    if let Some(setting) = fresh_gui_protocol::settings::catalog()
        .iter()
        .find(|setting| {
            setting.path.len() == parts.len()
                && setting
                    .path
                    .iter()
                    .zip(parts)
                    .all(|(expected, actual)| *expected == actual)
        })
        && let Some(value) = &value
    {
        fresh_gui_protocol::settings::validate_value(setting, value)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    let text = fresh_gui_protocol::settings::patch_jsonc(&current, parts, value.as_ref())
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    // Validate the result of the actual JSONC edit. This covers array-valued
    // settings such as `shortkeys`, including replacing the whole binding list.
    let candidate = fresh_gui_protocol::settings::read_jsonc(&text)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    validate(&candidate, workspace)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
    }
    std::fs::write(path, &text).with_context(|| format!("write {}", path.display()))?;
    Ok(text)
}

fn validate(root: &Value, workspace: bool) -> Result<()> {
    let object = root
        .as_object()
        .context("settings root must be an object")?;
    if workspace {
        if ["ui", "terminal", "shortkeys"]
            .iter()
            .any(|key| object.contains_key(*key))
        {
            bail!("workspace config contains daemon or host-only settings");
        }
        serde_json::from_value::<fresh::partial_config::PartialConfig>(root.clone())
            .context("invalid Fresh workspace settings")?;
    } else {
        let host: Config = serde_json::from_value(root.clone()).context("invalid host settings")?;
        // Fresh fields coexist at the top level with host-specific fields.
        let mut fresh_value = object.clone();
        for key in ["ui", "terminal", "shortkeys"] {
            fresh_value.remove(key);
        }
        serde_json::from_value::<fresh::partial_config::PartialConfig>(Value::Object(fresh_value))
            .context("invalid Fresh settings")?;
        let _ = host;
    }
    Ok(())
}

/// Values inherited when removing an override from the selected layer.
pub fn layer_defaults(config_path: &Path, workspace: bool) -> Result<Value> {
    if !workspace {
        return Ok(defaults_value());
    }
    let config = Config::load_from_path(config_path)?;
    let mut fresh = fresh::config::Config::default();
    config.apply_fresh(&mut fresh);
    serde_json::to_value(fresh).context("serialize inherited Fresh settings")
}

pub fn defaults_value() -> Value {
    let mut fresh = serde_json::to_value(fresh::config::Config::default()).unwrap_or(Value::Null);
    let host = serde_json::to_value(Config::default()).unwrap_or(Value::Null);
    if let (Some(target), Some(host)) = (fresh.as_object_mut(), host.as_object()) {
        for key in ["ui", "terminal", "shortkeys"] {
            if let Some(value) = host.get(key) {
                target.insert(key.to_owned(), value.clone());
            }
        }
    }
    if let Some(target) = fresh.as_object_mut()
        && let Ok(template) =
            fresh_gui_protocol::settings::read_jsonc(crate::config::DEFAULT_CONFIG_TEMPLATE)
        && let Some(bindings) = template.get("shortkeys")
    {
        target.insert("shortkeys".into(), bindings.clone());
    }
    fresh
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_preserves_comments_and_unknown_fields() {
        let dir = std::env::temp_dir().join(format!("fresh-gui-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let source = "{\n  // keep me\n  \"unknown\": 3,\n  \"editor\": {\"line_wrap\": true}\n}\n";
        std::fs::write(&path, source).unwrap();
        let updated = apply_patch(
            &path,
            source,
            &["editor".into(), "line_wrap".into()],
            Some(Value::Bool(false)),
            true,
        )
        .unwrap();
        assert!(updated.contains("// keep me"));
        assert!(updated.contains("\"unknown\": 3"));
        assert!(updated.contains("\"line_wrap\": false"));
        let reset = apply_patch(
            &path,
            &updated,
            &["editor".into(), "line_wrap".into()],
            None,
            true,
        )
        .unwrap();
        assert!(!reset.contains("\"line_wrap\""));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_reset_inherits_daemon_user_values_and_native_defaults() {
        let root =
            std::env::temp_dir().join(format!("fresh-gui-defaults-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let config_path = root.join("config.json");
        std::fs::write(
            &config_path,
            r#"{"editor":{"tab_size":9},"languages":{"rust":{"use_tabs":true}}}"#,
        )
        .unwrap();
        let inherited = layer_defaults(&config_path, true).unwrap();
        assert_eq!(inherited["editor"]["tab_size"], 9);
        assert_eq!(inherited["languages"]["rust"]["use_tabs"], true);
        let native = defaults_value();
        assert!(
            native["shortkeys"]
                .as_array()
                .is_some_and(|v| !v.is_empty())
        );
        assert_eq!(native["ui"]["editorFontSize"], 14);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compare_and_swap_rejects_stale_text() {
        let dir = std::env::temp_dir().join(format!("fresh-gui-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, "{\"unknown\":1}\n").unwrap();
        let result = apply_patch(
            &path,
            "{}\n",
            &["unknown".into()],
            Some(Value::from(2)),
            false,
        );
        assert!(result.unwrap_err().to_string().contains("changed on disk"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn concurrent_patches_from_one_snapshot_allow_only_one_writer() {
        let dir = std::env::temp_dir().join(format!("fresh-gui-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let source = "{}\n";
        std::fs::write(&path, source).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));
        let workers: Vec<_> = ["one", "two"]
            .into_iter()
            .map(|label| {
                let path = path.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    apply_patch(
                        &path,
                        source,
                        &[label.to_owned()],
                        Some(Value::Bool(true)),
                        false,
                    )
                })
            })
            .collect();
        barrier.wait();
        let successes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .filter(Result::is_ok)
            .count();
        assert_eq!(successes, 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn invalid_typed_values_leave_config_unchanged() {
        let dir = std::env::temp_dir().join(format!("fresh-gui-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let source = "{\"editor\":{\"tab_size\":4,\"line_wrap\":true}}\n";
        std::fs::write(&path, source).unwrap();
        for (parts, value) in [
            (
                vec!["editor".to_owned(), "tab_size".to_owned()],
                Value::from(33),
            ),
            (
                vec!["editor".to_owned(), "line_wrap".to_owned()],
                Value::from(3),
            ),
        ] {
            assert!(apply_patch(&path, source, &parts, Some(value), false).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn patches_shortkeys_as_an_array() {
        let dir = std::env::temp_dir().join(format!("fresh-gui-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let source = "{}\n";
        std::fs::write(&path, source).unwrap();
        let bindings = Value::Array(vec![serde_json::json!({
            "action": "editor.save",
            "shortkey": "ctrl-s",
            "when": "editor"
        })]);
        let saved =
            apply_patch(&path, source, &["shortkeys".into()], Some(bindings), false).unwrap();
        let parsed = fresh_gui_protocol::settings::read_jsonc(&saved).unwrap();
        assert_eq!(parsed["shortkeys"][0]["shortkey"], "ctrl-s");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn workspace_rejects_host_ui_fields() {
        let dir = std::env::temp_dir().join(format!("fresh-gui-settings-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        let source = "{}\n";
        std::fs::write(&path, source).unwrap();
        assert!(
            apply_patch(
                &path,
                source,
                &["ui".into(), "theme".into()],
                Some(Value::String("dark".into())),
                true
            )
            .is_err()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

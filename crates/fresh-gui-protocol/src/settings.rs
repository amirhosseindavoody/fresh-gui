//! Typed metadata and loss-minimizing JSONC helpers for the settings editor.

use jsonc_parser::{
    ParseOptions,
    cst::{CstArray, CstContainerNode, CstInputValue, CstNode, CstObject, CstRootNode},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingValueType {
    Boolean,
    Integer,
    OptionalInteger,
    Number,
    String,
    Object,
    Array,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingOwner {
    LocalClient,
    Daemon,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingScope {
    Global,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingEffect {
    Immediate,
    Restart,
    NextBuffer,
    NextSave,
    NextTerminal,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SettingDefinition {
    pub path: &'static [&'static str],
    pub value_type: SettingValueType,
    /// `None` means the authoritative default is supplied by Fresh at runtime.
    pub default: Option<Value>,
    pub owner: SettingOwner,
    pub scope: SettingScope,
    pub effect: SettingEffect,
    pub minimum: Option<u64>,
    pub maximum: Option<u64>,
    pub choices: Vec<String>,
}

fn fresh(path: &'static [&'static str], value_type: SettingValueType) -> SettingDefinition {
    SettingDefinition {
        path,
        value_type,
        default: None,
        owner: SettingOwner::Daemon,
        scope: SettingScope::Workspace,
        effect: SettingEffect::Restart,
        minimum: None,
        maximum: None,
        choices: Vec::new(),
    }
}
fn local(
    path: &'static [&'static str],
    value_type: SettingValueType,
    default: Option<Value>,
) -> SettingDefinition {
    SettingDefinition {
        path,
        value_type,
        default,
        owner: SettingOwner::LocalClient,
        scope: SettingScope::Global,
        effect: SettingEffect::Immediate,
        minimum: None,
        maximum: None,
        choices: Vec::new(),
    }
}
fn daemon(
    path: &'static [&'static str],
    value_type: SettingValueType,
    default: Option<Value>,
) -> SettingDefinition {
    SettingDefinition {
        path,
        value_type,
        default,
        owner: SettingOwner::Daemon,
        scope: SettingScope::Global,
        effect: SettingEffect::Immediate,
        minimum: None,
        maximum: None,
        choices: Vec::new(),
    }
}
fn bounded(mut setting: SettingDefinition, minimum: u64, maximum: u64) -> SettingDefinition {
    setting.minimum = Some(minimum);
    setting.maximum = Some(maximum);
    setting
}
fn choices(mut setting: SettingDefinition, values: &[&str]) -> SettingDefinition {
    setting.choices = values.iter().map(|value| (*value).to_owned()).collect();
    setting
}

/// Settings currently represented by native controls. Fresh's own `Config::default()`
/// remains the source of truth for editor defaults; this catalog intentionally does not
/// copy a second set of Fresh defaults into the protocol crate.
pub fn catalog() -> Vec<SettingDefinition> {
    use SettingValueType::*;
    vec![
        choices(
            local(
                &["ui", "theme"],
                String,
                Some(Value::String("system".into())),
            ),
            &["system", "light", "dark"],
        ),
        bounded(
            local(&["ui", "terminalFontSize"], Integer, Some(Value::from(14))),
            10,
            28,
        ),
        bounded(
            local(&["ui", "editorFontSize"], Integer, Some(Value::from(14))),
            10,
            28,
        ),
        local(
            &["ui", "fontFamily"],
            String,
            Some(Value::String(std::string::String::new())),
        ),
        local(
            &["ui", "monoFontFamily"],
            String,
            Some(Value::String(std::string::String::new())),
        ),
        daemon(&["ui", "showDotfiles"], Boolean, Some(Value::Bool(false))),
        daemon(&["ui", "showGitDirs"], Boolean, Some(Value::Bool(false))),
        local(&["ui", "editorLineWrap"], Boolean, Some(Value::Bool(true))),
        SettingDefinition {
            effect: SettingEffect::NextTerminal,
            ..daemon(&["terminal", "shell", "command"], String, None)
        },
        SettingDefinition {
            effect: SettingEffect::NextTerminal,
            ..daemon(&["terminal", "shell", "args"], Array, None)
        },
        fresh(&["editor", "line_numbers"], Boolean),
        fresh(&["editor", "line_wrap"], Boolean),
        fresh(&["editor", "wrap_column"], OptionalInteger),
        fresh(&["editor", "use_tabs"], Boolean),
        bounded(fresh(&["editor", "tab_size"], Integer), 1, 32),
        fresh(&["editor", "auto_indent"], Boolean),
    ]
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("invalid JSONC: {0}")]
    Parse(#[from] jsonc_parser::errors::ParseError),
    #[error("settings document root must be an object")]
    RootNotObject,
    #[error("invalid settings path: {0}")]
    InvalidPath(String),
    #[error("value has the wrong type for this setting")]
    InvalidValue,
}

pub fn read_jsonc(text: &str) -> Result<Value, SettingsError> {
    let root = CstRootNode::parse(text, &ParseOptions::default())?;
    let value = root.to_serde_value().ok_or(SettingsError::RootNotObject)?;
    if value.is_object() {
        Ok(value)
    } else {
        Err(SettingsError::RootNotObject)
    }
}

/// Change one object-valued path while retaining unrelated comments and formatting.
/// A missing value removes the leaf, allowing Fresh's default to take effect.
pub fn patch_jsonc(
    text: &str,
    path: &[String],
    value: Option<&Value>,
) -> Result<String, SettingsError> {
    if path.is_empty() {
        return Err(SettingsError::InvalidPath("empty".into()));
    }
    read_jsonc(text)?;
    let root = CstRootNode::parse(text, &ParseOptions::default())?;
    let mut container = SettingsContainer::Object(root.object_value_or_set());
    for (position, key) in path[..path.len() - 1].iter().enumerate() {
        container = match container {
            SettingsContainer::Object(object) => {
                let next_is_index = path[position + 1].parse::<usize>().is_ok();
                if object.get(key).is_none() {
                    if next_is_index {
                        SettingsContainer::Array(object.array_value_or_set(key))
                    } else {
                        SettingsContainer::Object(object.object_value_or_set(key))
                    }
                } else {
                    let prop = object.get(key).unwrap();
                    let node = prop
                        .value()
                        .ok_or_else(|| SettingsError::InvalidPath(path.join(".")))?;
                    container_from_node(node)
                        .ok_or_else(|| SettingsError::InvalidPath(path.join(".")))?
                }
            }
            SettingsContainer::Array(array) => {
                let index = key
                    .parse::<usize>()
                    .map_err(|_| SettingsError::InvalidPath(path.join(".")))?;
                let node = array
                    .elements()
                    .get(index)
                    .cloned()
                    .ok_or_else(|| SettingsError::InvalidPath(path.join(".")))?;
                container_from_node(node)
                    .ok_or_else(|| SettingsError::InvalidPath(path.join(".")))?
            }
        };
    }
    let leaf = path.last().unwrap();
    match container {
        SettingsContainer::Object(object) => {
            if let Some(value) = value {
                let input = to_cst(value);
                match object.get(leaf) {
                    Some(prop) => match prop.value() {
                        Some(node) => reconcile_node(node, value),
                        None => prop.set_value(input),
                    },
                    None => {
                        object.append(leaf, input);
                    }
                }
            } else if let Some(prop) = object.get(leaf) {
                prop.remove();
            }
        }
        SettingsContainer::Array(array) => {
            let index = leaf
                .parse::<usize>()
                .map_err(|_| SettingsError::InvalidPath(path.join(".")))?;
            let nodes = array.elements();
            if let Some(node) = nodes.get(index).cloned() {
                if let Some(value) = value {
                    reconcile_node(node, value);
                } else {
                    node.remove();
                }
            } else if value.is_some() && index == nodes.len() {
                array.append(to_cst(value.unwrap()));
            } else if index > nodes.len() || value.is_some() {
                return Err(SettingsError::InvalidPath(path.join(".")));
            }
        }
    }
    Ok(root.to_string())
}

enum SettingsContainer {
    Object(CstObject),
    Array(CstArray),
}
fn container_from_node(node: CstNode) -> Option<SettingsContainer> {
    match node {
        CstNode::Container(CstContainerNode::Object(object)) => {
            Some(SettingsContainer::Object(object))
        }
        CstNode::Container(CstContainerNode::Array(array)) => Some(SettingsContainer::Array(array)),
        _ => None,
    }
}
fn reconcile_node(node: CstNode, value: &Value) {
    if let (Some(object), Value::Object(replacement)) = (node.as_object(), value) {
        for (key, next_value) in replacement {
            if let Some(prop) = object.get(key) {
                if let Some(current) = prop.value() {
                    reconcile_node(current, next_value);
                } else {
                    prop.set_value(to_cst(next_value));
                }
            } else {
                object.append(key, to_cst(next_value));
            }
        }
        return;
    }
    replace_node(node, to_cst(value));
}

fn replace_node(node: CstNode, value: CstInputValue) {
    match node {
        CstNode::Container(container) => match container {
            CstContainerNode::Root(node) => {
                node.set_value(value);
            }
            CstContainerNode::Object(node) => {
                node.replace_with(value);
            }
            CstContainerNode::ObjectProp(node) => {
                node.set_value(value);
            }
            CstContainerNode::Array(node) => {
                node.replace_with(value);
            }
        },
        CstNode::Leaf(leaf) => match leaf {
            jsonc_parser::cst::CstLeafNode::BooleanLit(node) => {
                node.replace_with(value);
            }
            jsonc_parser::cst::CstLeafNode::NullKeyword(node) => {
                node.replace_with(value);
            }
            jsonc_parser::cst::CstLeafNode::NumberLit(node) => {
                node.replace_with(value);
            }
            jsonc_parser::cst::CstLeafNode::StringLit(node) => {
                node.replace_with(value);
            }
            jsonc_parser::cst::CstLeafNode::WordLit(node) => {
                node.replace_with(value);
            }
            _ => {}
        },
    }
}

fn to_cst(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(v) => CstInputValue::Bool(*v),
        Value::Number(v) => CstInputValue::Number(v.to_string()),
        Value::String(v) => CstInputValue::String(v.clone()),
        Value::Array(values) => CstInputValue::Array(values.iter().map(to_cst).collect()),
        Value::Object(values) => {
            CstInputValue::Object(values.iter().map(|(k, v)| (k.clone(), to_cst(v))).collect())
        }
    }
}

/// Validate a JSON value against the catalog's primitive type.
pub fn validate_value(setting: &SettingDefinition, value: &Value) -> Result<(), SettingsError> {
    let valid = match setting.value_type {
        SettingValueType::Boolean => value.is_boolean(),
        SettingValueType::Integer => value.as_i64().is_some() || value.as_u64().is_some(),
        SettingValueType::OptionalInteger => {
            value.is_null() || value.as_i64().is_some() || value.as_u64().is_some()
        }
        SettingValueType::Number => value.is_number(),
        SettingValueType::String => value.is_string(),
        SettingValueType::Object => value.is_object(),
        SettingValueType::Array => value.is_array(),
    };
    if !valid {
        return Err(SettingsError::InvalidValue);
    }
    if let Some(number) = value.as_i64() {
        if number < 0 && setting.minimum.is_some() {
            return Err(SettingsError::InvalidValue);
        }
    }
    if let Some(number) = value.as_u64() {
        if setting.minimum.is_some_and(|min| number < min)
            || setting.maximum.is_some_and(|max| number > max)
        {
            return Err(SettingsError::InvalidValue);
        }
    }
    if !setting.choices.is_empty()
        && !value
            .as_str()
            .is_some_and(|v| setting.choices.iter().any(|choice| choice == v))
    {
        return Err(SettingsError::InvalidValue);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patch_keeps_comments_and_unknown_keys_and_remove_resets() {
        let src =
            "{\n  // keep this\n  \"unknown\": 42,\n  \"editor\": { \"line_wrap\": false }\n}\n";
        let path = vec!["editor".into(), "line_wrap".into()];
        let patched = patch_jsonc(src, &path, Some(&Value::Bool(true))).unwrap();
        assert!(patched.contains("// keep this"));
        assert_eq!(read_jsonc(&patched).unwrap()["unknown"], 42);
        assert_eq!(read_jsonc(&patched).unwrap()["editor"]["line_wrap"], true);
        let reset = patch_jsonc(&patched, &path, None).unwrap();
        assert!(reset.contains("// keep this"));
        assert!(
            read_jsonc(&reset).unwrap()["editor"]
                .get("line_wrap")
                .is_none()
        );
    }

    #[test]
    fn catalog_separates_host_and_fresh_ownership() {
        let entries = catalog();
        assert_eq!(
            entries
                .iter()
                .find(|e| e.path == ["ui", "theme"])
                .unwrap()
                .owner,
            SettingOwner::LocalClient
        );
        assert_eq!(
            entries
                .iter()
                .find(|e| e.path == ["editor", "line_wrap"])
                .unwrap()
                .owner,
            SettingOwner::Daemon
        );
    }

    #[test]
    fn array_entry_patch_preserves_neighboring_unknown_data() {
        let src = "{\n  \"shortkeys\": [\n    { \"action\": \"Open\", \"shortkey\": \"ctrl-o\", \"future\": 5 },\n    { \"action\": \"Save\", \"shortkey\": \"ctrl-s\" }\n  ]\n}";
        let path = vec!["shortkeys".into(), "0".into(), "shortkey".into()];
        let patched = patch_jsonc(src, &path, Some(&Value::String("ctrl-p".into()))).unwrap();
        assert_eq!(
            read_jsonc(&patched).unwrap()["shortkeys"][0]["shortkey"],
            "ctrl-p"
        );
        assert_eq!(read_jsonc(&patched).unwrap()["shortkeys"][0]["future"], 5);
        assert_eq!(
            read_jsonc(&patched).unwrap()["shortkeys"][1]["action"],
            "Save"
        );
    }

    #[test]
    fn object_entry_patch_keeps_internal_comments_and_unmentioned_keys() {
        let src = "{\n  \"shortkeys\": [\n    {\n      // keep action note\n      \"action\": \"Open\",\n      \"shortkey\": \"ctrl-o\",\n      // keep context note\n      \"when\": \"Editor\",\n      \"future\": true\n    }\n  ]\n}";
        let entry = serde_json::json!({"action":"Save", "shortkey":"ctrl-s"});
        let patched = patch_jsonc(src, &["shortkeys".into(), "0".into()], Some(&entry)).unwrap();
        assert!(patched.contains("// keep action note"));
        assert!(patched.contains("// keep context note"));
        let value = read_jsonc(&patched).unwrap();
        assert_eq!(value["shortkeys"][0]["action"], "Save");
        assert_eq!(value["shortkeys"][0]["shortkey"], "ctrl-s");
        assert_eq!(value["shortkeys"][0]["when"], "Editor");
        assert_eq!(value["shortkeys"][0]["future"], true);
    }
}

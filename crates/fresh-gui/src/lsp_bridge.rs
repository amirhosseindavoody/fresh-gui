//! Wire helpers for the daemon's revision-aware LSP request bridge.

use fresh_gui_protocol::{LspRequestFeature, LspServerResponse};
use serde_json::{Value, json};

pub(crate) fn method(feature: LspRequestFeature) -> &'static str {
    match feature {
        LspRequestFeature::Capabilities => "",
        LspRequestFeature::Completion => "textDocument/completion",
        LspRequestFeature::Hover => "textDocument/hover",
        LspRequestFeature::SignatureHelp => "textDocument/signatureHelp",
        LspRequestFeature::CompletionResolve => "completionItem/resolve",
    }
}

pub(crate) fn position_params(
    uri: &str,
    line: u32,
    character: u32,
    trigger: Option<&str>,
    signature: bool,
) -> Value {
    let context = if signature {
        json!({
            "triggerKind": if trigger.is_some() { 2 } else { 1 },
            "triggerCharacter": trigger,
            "isRetrigger": false,
        })
    } else {
        json!({
            "triggerKind": if trigger.is_some() { 2 } else { 1 },
            "triggerCharacter": trigger,
        })
    };
    let mut params = json!({
        "textDocument": { "uri": uri },
        "position": { "line": line, "character": character },
    });
    // Explicit requests use Invoked; a character trigger is sent only after
    // the caller verifies that this server advertised it.
    params["context"] = context;
    params
}

pub(crate) fn resolve_item_params(item: Value) -> Value {
    item.get("data")
        .and_then(find_original_completion)
        .cloned()
        .unwrap_or(item)
}

fn find_original_completion(value: &Value) -> Option<&Value> {
    let object = value.as_object()?;
    if let Some(original) = object.get("_fresh_original") {
        Some(find_original_completion(original).unwrap_or(original))
    } else if object.contains_key("label") {
        let data = object.get("data")?.as_object()?;
        if data.contains_key("_fresh_cursor_offset") {
            let original = data.get("_fresh_original")?;
            Some(find_original_completion(original).unwrap_or(original))
        } else {
            None
        }
    } else {
        None
    }
}

/// Expand Fresh snippet syntax in completion items before they reach the
/// host editor, while preserving `$0` as a byte offset for its cursor update.
pub(crate) fn normalize_completion_result(value: Value) -> Value {
    fn normalize_item(item: &mut Value, defaults: Option<&Value>) {
        let Some(obj) = item.as_object_mut() else {
            return;
        };
        if let Some(defaults) = defaults {
            if obj.get("insertTextFormat").is_none()
                && let Some(format) = defaults.get("insertTextFormat")
            {
                obj.insert("insertTextFormat".into(), format.clone());
            }
            if obj.get("data").is_none()
                && let Some(data) = defaults.get("data")
            {
                obj.insert("data".into(), data.clone());
            }
        }
        let mut original_item = Value::Object(obj.clone());

        // CompletionItem.textEditText is newer than the pinned lsp-types
        // model. Fold it into the supported fields before GPUI deserializes it.
        let text_edit_text = obj
            .remove("textEditText")
            .and_then(|text| text.as_str().map(str::to_owned));
        if obj.get("textEdit").is_none()
            && let Some(text) = text_edit_text.as_ref()
        {
            if let Some(edit_range) = defaults.and_then(|defaults| defaults.get("editRange")) {
                let text_edit = if edit_range.get("insert").is_some()
                    && edit_range.get("replace").is_some()
                {
                    json!({"insert": edit_range["insert"], "replace": edit_range["replace"], "newText": text})
                } else {
                    json!({"range": edit_range, "newText": text})
                };
                obj.insert("textEdit".into(), text_edit);
            } else {
                obj.insert("insertText".into(), json!(text));
            }
        }
        if let Some(original) = original_item.as_object_mut()
            && original.get("textEdit").is_none()
        {
            if let Some(edit) = obj.get("textEdit") {
                original.insert("textEdit".into(), edit.clone());
            } else if let Some(text) = text_edit_text.as_ref() {
                original.insert("insertText".into(), json!(text));
            }
        }

        let snippet = obj
            .get("textEdit")
            .and_then(|edit| edit.get("newText"))
            .and_then(Value::as_str)
            .or(text_edit_text.as_deref())
            .or_else(|| obj.get("insertText").and_then(Value::as_str))
            .or_else(|| obj.get("label").and_then(Value::as_str));
        let is_snippet = obj.get("insertTextFormat").and_then(Value::as_u64) == Some(2)
            && snippet.is_some_and(fresh::primitives::snippet::is_snippet);
        if !is_snippet && text_edit_text.is_none() {
            return;
        }

        let (replacement, cursor_offset) = if is_snippet {
            let expanded =
                fresh::primitives::snippet::expand_snippet(snippet.expect("snippet checked"));
            (expanded.text, Some(expanded.cursor_offset))
        } else {
            (snippet.unwrap_or_default().to_owned(), None)
        };
        let mut metadata = serde_json::Map::new();
        if let Some(offset) = cursor_offset {
            metadata.insert("_fresh_cursor_offset".into(), json!(offset));
        }
        metadata.insert("_fresh_original".into(), original_item);
        obj.insert("data".into(), Value::Object(metadata));
        if obj.get("textEdit").is_none() {
            obj.insert("insertText".into(), json!(replacement));
        } else {
            if let Some(edit) = obj.get_mut("textEdit").and_then(Value::as_object_mut) {
                edit.insert("newText".into(), json!(replacement));
            }
            if obj.contains_key("insertText") {
                obj.insert("insertText".into(), json!(replacement));
            }
        }
        if is_snippet {
            obj.insert("insertTextFormat".into(), json!(1));
        }
    }

    let mut value = value;
    if value
        .as_object()
        .is_some_and(|obj| obj.contains_key("label"))
    {
        normalize_item(&mut value, None);
        return value;
    }
    match &mut value {
        Value::Array(items) => items.iter_mut().for_each(|item| normalize_item(item, None)),
        Value::Object(obj) if obj.contains_key("items") => {
            let defaults = obj.get("itemDefaults").cloned();
            if let Some(items) = obj.get_mut("items").and_then(Value::as_array_mut) {
                for item in items {
                    normalize_item(item, defaults.as_ref());
                }
            }
        }
        _ => {}
    }
    value
}

pub(crate) fn one_response(server: String, result: Value) -> LspServerResponse {
    LspServerResponse { server, result }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_snippets_preserve_original_and_cursor_offset_for_resolve() {
        let input = json!([{
            "label": "call",
            "insertText": "call($1)\n$0",
            "insertTextFormat": 2
        }]);
        let normalized = normalize_completion_result(input);
        assert_eq!(normalized[0]["insertText"], "call()\n");
        assert_eq!(normalized[0]["insertTextFormat"], 1);
        assert_eq!(normalized[0]["data"]["_fresh_cursor_offset"], 7);
        let original = resolve_item_params(normalized[0].clone());
        assert_eq!(original["insertTextFormat"], 2);
    }

    #[test]
    fn text_edit_text_uses_defaults_and_restores_full_original_item_for_resolve() {
        let input = json!({
            "items": [{
                "label": "call",
                "textEditText": "call($1)\n$0",
                "insertText": "call($1)\n$0"
            }],
            "itemDefaults": {
                "insertTextFormat": 2,
                "editRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 4}},
                "data": {"resolveKey": "kept"}
            }
        });
        let normalized = normalize_completion_result(input);
        let item = &normalized["items"][0];
        assert_eq!(item["textEdit"]["newText"], "call()\n");
        assert_eq!(item["insertText"], "call()\n");
        assert_eq!(item["insertTextFormat"], 1);
        assert_eq!(item["data"]["_fresh_cursor_offset"], 7);
        assert_eq!(
            item["data"]["_fresh_original"]["data"]["resolveKey"],
            "kept"
        );
        assert_eq!(
            item["data"]["_fresh_original"]["textEditText"],
            "call($1)\n$0"
        );

        let original = resolve_item_params(item.clone());
        assert_eq!(original["textEditText"], "call($1)\n$0");
        assert_eq!(original["textEdit"]["newText"], "call($1)\n$0");
        assert_eq!(original["textEdit"]["range"]["end"]["character"], 4);
        assert_eq!(original["insertText"], "call($1)\n$0");
        assert_eq!(original["data"]["resolveKey"], "kept");

        let gui_wrapped = json!({
            "label": item["label"],
            "data": {"_fresh_server": "Rust", "_fresh_cursor_offset": 7, "_fresh_original": item}
        });
        let resolved_from_gui = resolve_item_params(gui_wrapped);
        assert_eq!(resolved_from_gui["textEditText"], "call($1)\n$0");
        assert_eq!(resolved_from_gui["data"]["resolveKey"], "kept");
    }

    #[test]
    fn text_edit_text_without_default_range_becomes_insert_text() {
        let input = json!({"label": "item", "textEditText": "expanded", "insertTextFormat": 1});
        let normalized = normalize_completion_result(input);
        assert_eq!(normalized["insertText"], "expanded");
        assert!(normalized.get("textEdit").is_none());
        assert_eq!(
            resolve_item_params(normalized.clone())["textEditText"],
            "expanded"
        );
    }

    #[test]
    fn item_data_and_item_specific_format_override_defaults() {
        let input = json!({
            "items": [{
                "label": "plain",
                "textEditText": "plain",
                "insertTextFormat": 1,
                "data": {"itemKey": "preserved"}
            }],
            "itemDefaults": {
                "insertTextFormat": 2,
                "editRange": {
                    "insert": {"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 1}},
                    "replace": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 2}}
                },
                "data": {"defaultKey": "must-not-replace-item-data"}
            }
        });
        let normalized = normalize_completion_result(input);
        let item = &normalized["items"][0];
        assert_eq!(item["insertTextFormat"], 1);
        assert_eq!(item["textEdit"]["insert"]["start"]["character"], 1);
        assert_eq!(item["textEdit"]["newText"], "plain");
        assert_eq!(
            item["data"]["_fresh_original"]["data"]["itemKey"],
            "preserved"
        );
        assert!(
            item["data"]["_fresh_original"]["data"]
                .get("defaultKey")
                .is_none()
        );
        assert_eq!(
            resolve_item_params(item.clone())["data"]["itemKey"],
            "preserved"
        );
    }
}

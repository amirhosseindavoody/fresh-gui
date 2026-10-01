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

pub(crate) fn position_params(uri: &str, line: u32, character: u32, trigger: Option<&str>, signature: bool) -> Value {
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
    if signature {
        params["context"] = context;
    } else {
        // LSP completion requires a context; explicit requests use Invoked.
        params["context"] = context;
    }
    params
}

pub(crate) fn resolve_item_params(item: Value) -> Value {
    item.get("data").and_then(find_original_completion).cloned().unwrap_or(item)
}

fn find_original_completion(value: &Value) -> Option<&Value> {
    let object = value.as_object()?;
    if let Some(original) = object.get("_fresh_original") {
        Some(find_original_completion(original).unwrap_or(original))
    } else {
        None
    }
}

/// Expand Fresh snippet syntax in completion items before they reach the
/// host editor, while preserving `$0` as a byte offset for its cursor update.
pub(crate) fn normalize_completion_result(value: Value) -> Value {
    fn normalize_item(item: &mut Value) {
        let Some(obj) = item.as_object_mut() else { return };
        if obj.get("insertTextFormat").and_then(Value::as_u64) != Some(2) {
            return;
        }
        let snippet = obj
            .get("textEdit").and_then(|edit| edit.get("newText")).and_then(Value::as_str)
            .or_else(|| obj.get("insertText").and_then(Value::as_str))
            .or_else(|| obj.get("label").and_then(Value::as_str));
        let Some(snippet) = snippet else { return };
        if !fresh::primitives::snippet::is_snippet(snippet) {
            return;
        }
        let expanded = fresh::primitives::snippet::expand_snippet(snippet);
        let original_item = Value::Object(obj.clone());
        obj.insert("data".into(), json!({"_fresh_cursor_offset": expanded.cursor_offset, "_fresh_original": original_item}));
        if let Some(edit) = obj.get_mut("textEdit").and_then(Value::as_object_mut) {
            edit.insert("newText".into(), json!(expanded.text));
        } else {
            obj.insert("insertText".into(), json!(expanded.text));
        }
        obj.insert("insertTextFormat".into(), json!(1));
    }

    let mut value = value;
    match &mut value {
        Value::Array(items) => items.iter_mut().for_each(normalize_item),
        Value::Object(obj) => {
            if obj.contains_key("label") {
                // Normalize an individual completion item (completion/resolve).
                if obj.get("insertTextFormat").and_then(Value::as_u64) == Some(2) {
                    let snippet = obj
                        .get("textEdit").and_then(|edit| edit.get("newText")).and_then(Value::as_str)
                        .or_else(|| obj.get("insertText").and_then(Value::as_str))
                        .or_else(|| obj.get("label").and_then(Value::as_str));
                    if let Some(snippet) = snippet.filter(|snippet| fresh::primitives::snippet::is_snippet(snippet)) {
                        let expanded = fresh::primitives::snippet::expand_snippet(snippet);
                        let original_item = Value::Object(obj.clone());
                        obj.insert("data".into(), json!({"_fresh_cursor_offset": expanded.cursor_offset, "_fresh_original": original_item}));
                        if let Some(edit) = obj.get_mut("textEdit").and_then(Value::as_object_mut) {
                            edit.insert("newText".into(), json!(expanded.text));
                        } else { obj.insert("insertText".into(), json!(expanded.text)); }
                        obj.insert("insertTextFormat".into(), json!(1));
                    }
                }
            } else if obj.contains_key("items") {
                let default_format = obj.get("itemDefaults").and_then(|defaults| defaults.get("insertTextFormat")).cloned();
                if let Some(items) = obj.get_mut("items").and_then(Value::as_array_mut) {
                    for item in items {
                        if let Some(format) = default_format.as_ref() {
                            if item.get("insertTextFormat").is_none() {
                                if let Some(item) = item.as_object_mut() { item.insert("insertTextFormat".into(), format.clone()); }
                            }
                        }
                        normalize_item(item);
                    }
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
}

//! Thin adapters between gpui-component's editor LSP hooks and Fresh's ADE
//! request bridge. Fresh remains responsible for routing requests to servers;
//! this module only normalizes responses for the native editor widgets.

use std::{cell::RefCell, collections::HashSet, ops::Range, rc::Rc, time::Duration};

use anyhow::{Result, anyhow};
use gpui::{App, FutureExt as _, Task, WeakEntity, Window};
use gpui_kit::component::input::{CompletionProvider, HoverProvider, Rope};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionResponse, CompletionTextEdit, Hover,
    HoverContents, MarkupContent, MarkupKind, Position, SignatureHelp, TextEdit,
};
use serde_json::{Value as JsonValue, json};

use super::pane::EditorPanel;
use fresh_gui_protocol::{LspRequestFeature as LspFeature, LspResult};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

/// Information used by the pane to recognize a GPUI completion after its
/// input event and restore the LSP-provided caret position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompletionPlan {
    pub before: String,
    pub after: String,
    pub cursor: usize,
    pub request_offset: usize,
}

/// Provider object installed on gpui-component's built-in completion and hover
/// hooks. The returned task waits only on the ADE response channel; filesystem
/// and language-server work stays on the daemon.
pub(crate) struct RemoteLsp {
    panel: WeakEntity<EditorPanel>,
    completion_triggers: Rc<RefCell<Vec<String>>>,
}

impl RemoteLsp {
    pub(crate) fn new(panel: WeakEntity<EditorPanel>) -> Rc<Self> {
        Rc::new(Self {
            panel,
            completion_triggers: Rc::default(),
        })
    }

    pub(crate) fn update_triggers(&self, result: &LspResult) {
        *self.completion_triggers.borrow_mut() = result.completion_triggers.clone();
    }
}

impl CompletionProvider for RemoteLsp {
    fn completions(
        &self,
        rope: &Rope,
        offset: usize,
        trigger: CompletionContext,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<CompletionResponse>> {
        let text = rope.to_string();
        let trigger_character = trigger
            .trigger_character
            .filter(|trigger| trigger.chars().count() == 1)
            .or_else(|| {
                text.get(..offset)
                    .and_then(|prefix| prefix.chars().next_back())
                    .map(|c| c.to_string())
            });
        let executor = cx.background_executor().clone();
        let panel = self.panel.clone();
        cx.spawn(async move |cx| {
            let receiver = panel
                .update(cx, |panel, cx| {
                    panel.queue_lsp(
                        LspFeature::Completion,
                        offset,
                        trigger_character,
                        text.clone(),
                        cx,
                    )
                })
                .map_err(|_| anyhow!("editor panel is no longer available"))?;
            let response = receiver
                .recv()
                .with_timeout(REQUEST_TIMEOUT, &executor)
                .await
                .map_err(|_| anyhow!("LSP completion request timed out"))?
                .map_err(|_| anyhow!("LSP completion response channel closed"))?;
            let (items, plans) = normalize_completions(&text, offset, &response);
            let current = panel
                .update(cx, |panel, cx| {
                    panel.lsp_result_matches(&response, &text, Some(offset), cx)
                })
                .unwrap_or(false);
            if !current {
                return Ok(CompletionResponse::Array(Vec::new()));
            }
            let _ = panel.update(cx, |panel, cx| panel.set_completion_plans(plans, cx));
            Ok(CompletionResponse::Array(items))
        })
    }

    fn is_completion_trigger(&self, _offset: usize, new_text: &str, _cx: &mut App) -> bool {
        let last = new_text.chars().next_back();
        last.is_some_and(|ch| ch == '_' || ch.is_alphanumeric())
            || self
                .completion_triggers
                .borrow()
                .iter()
                .any(|trigger| !trigger.is_empty() && new_text.ends_with(trigger))
    }
}

impl HoverProvider for RemoteLsp {
    fn hover(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Option<Hover>>> {
        let text = text.to_string();
        let executor = cx.background_executor().clone();
        let panel = self.panel.clone();
        cx.spawn(async move |_cx| {
            let receiver = panel
                .update(_cx, |panel, cx| {
                    panel.queue_lsp(LspFeature::Hover, offset, None, text.clone(), cx)
                })
                .map_err(|_| anyhow!("editor panel is no longer available"))?;
            let response = receiver
                .recv()
                .with_timeout(REQUEST_TIMEOUT, &executor)
                .await
                .map_err(|_| anyhow!("LSP hover request timed out"))?
                .map_err(|_| anyhow!("LSP hover response channel closed"))?;
            let current = panel
                .update(_cx, |panel, cx| {
                    panel.lsp_result_matches(&response, &text, None, cx)
                })
                .unwrap_or(false);
            Ok(current.then(|| merge_hover(&response)).flatten())
        })
    }
}

/// Convert Fresh's merged server payloads to the single-edit representation
/// accepted by gpui-component. Additional edits are composed into one spanning
/// replacement so accepting the item reaches the pane as one #141 range edit.
pub(crate) fn normalize_completions(
    text: &str,
    offset: usize,
    result: &LspResult,
) -> (Vec<CompletionItem>, Vec<CompletionPlan>) {
    if result.stale || result.feature != LspFeature::Completion || result.offset != offset {
        return (Vec::new(), Vec::new());
    }
    let Some(prefix) = completion_prefix(text, offset) else {
        return (Vec::new(), Vec::new());
    };

    let mut normalized = Vec::new();
    for response in &result.responses {
        let (mut items, defaults) = completion_items(&response.result);
        for mut item in items.drain(..) {
            apply_item_defaults(&mut item, defaults.as_ref());
            let Some((item, plan, key)) = normalize_item(text, offset, item, &response.server)
            else {
                continue;
            };
            if !completion_matches_prefix(&item, &prefix) {
                continue;
            }
            let item = sanitize_completion_highlight(item, &prefix);
            normalized.push((item, plan, key));
        }
    }

    // LSP sortText is the server's stable preference. Retain source order for
    // ties, and remove only entries with the same label and resulting edit.
    normalized.sort_by(|a, b| {
        a.0.sort_text
            .as_deref()
            .unwrap_or(&a.0.label)
            .cmp(b.0.sort_text.as_deref().unwrap_or(&b.0.label))
    });
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    let mut plans = Vec::new();
    for (item, plan, key) in normalized {
        if seen.insert(key) {
            items.push(item);
            plans.push(plan);
            if items.len() == 100 {
                break;
            }
        }
    }
    (items, plans)
}

fn completion_prefix(text: &str, offset: usize) -> Option<String> {
    let prefix = text.get(..offset)?;
    Some(
        prefix
            .char_indices()
            .rev()
            .take_while(|(_, ch)| is_completion_word_char(*ch))
            .last()
            .map_or_else(String::new, |(byte, _)| prefix[byte..].to_owned()),
    )
}

fn completion_matches_prefix(item: &CompletionItem, prefix: &str) -> bool {
    let prefix = prefix.to_lowercase();
    prefix.is_empty()
        || item.label.to_lowercase().starts_with(&prefix)
        || item
            .filter_text
            .as_ref()
            .is_some_and(|filter| filter.to_lowercase().starts_with(&prefix))
}

/// The stock menu uses `filter_text.len()` as a byte range into the visible
/// label. Preserve matching against the server's filter text above, then set a
/// safe visible-label prefix so Unicode labels cannot produce invalid ranges.
fn sanitize_completion_highlight(mut item: CompletionItem, prefix: &str) -> CompletionItem {
    let len = item
        .label
        .get(..prefix.len())
        .filter(|candidate| candidate.to_lowercase().starts_with(&prefix.to_lowercase()))
        .map_or(0, str::len);
    item.filter_text = Some(item.label[..len].to_owned());
    item
}

fn completion_items(value: &JsonValue) -> (Vec<CompletionItem>, Option<JsonValue>) {
    let Some(value) = value.as_object() else {
        return (
            serde_json::from_value(value.clone()).unwrap_or_default(),
            None,
        );
    };
    let defaults = value.get("itemDefaults").cloned();
    let Some(items) = value.get("items") else {
        return (Vec::new(), defaults);
    };
    (
        serde_json::from_value(items.clone()).unwrap_or_default(),
        defaults,
    )
}

fn apply_item_defaults(item: &mut CompletionItem, defaults: Option<&JsonValue>) {
    let Some(defaults) = defaults.and_then(JsonValue::as_object) else {
        return;
    };
    if item.text_edit.is_none()
        && let Some(edit_range) = defaults.get("editRange")
    {
        let range_value = edit_range.get("replace").unwrap_or(edit_range);
        if let Ok(range) = serde_json::from_value(range_value.clone()) {
            let new_text = defaults
                .get("textEditText")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
                .or_else(|| item.insert_text.clone())
                .unwrap_or_else(|| item.label.clone());
            item.text_edit = Some(CompletionTextEdit::Edit(TextEdit { range, new_text }));
        }
    }
    if item.insert_text_format.is_none() {
        item.insert_text_format = defaults
            .get("insertTextFormat")
            .and_then(|value| serde_json::from_value(value.clone()).ok());
    }
    if item.data.is_none() {
        item.data = defaults.get("data").cloned();
    }
}

fn normalize_item(
    original: &str,
    offset: usize,
    mut item: CompletionItem,
    server: &str,
) -> Option<(CompletionItem, CompletionPlan, (String, usize, String))> {
    let source_item = item
        .data
        .as_ref()
        .and_then(|data| data.get("_fresh_original"))
        .cloned()
        .or_else(|| serde_json::to_value(&item).ok())?;
    let (primary_range, insertion) = match item.text_edit.as_ref() {
        Some(CompletionTextEdit::Edit(edit)) => (
            position_range_to_bytes(original, &edit.range.start, &edit.range.end)?,
            edit.new_text.clone(),
        ),
        Some(CompletionTextEdit::InsertAndReplace(edit)) => (
            position_range_to_bytes(original, &edit.replace.start, &edit.replace.end)?,
            edit.new_text.clone(),
        ),
        None => {
            if offset > original.len() || !original.is_char_boundary(offset) {
                return None;
            }
            let start = original[..offset]
                .char_indices()
                .rev()
                .take_while(|(_, ch)| is_completion_word_char(*ch))
                .last()
                .map_or(offset, |(byte, _)| byte);
            (
                start..offset,
                item.insert_text
                    .clone()
                    .unwrap_or_else(|| item.label.clone()),
            )
        }
    };

    // gpui-component inserts text literally. Fresh expands snippets on the
    // daemon and annotates the item with this byte offset when one is present.
    let caret_in_insertion = fresh_cursor_offset(&item).unwrap_or(insertion.len());
    if caret_in_insertion > insertion.len() || !insertion.is_char_boundary(caret_in_insertion) {
        return None;
    }

    let mut edits = vec![(primary_range.clone(), insertion.clone(), true)];
    for edit in item.additional_text_edits.as_deref().unwrap_or_default() {
        let range = position_range_to_bytes(original, &edit.range.start, &edit.range.end)?;
        edits.push((range, edit.new_text.clone(), false));
    }
    edits.sort_by_key(|edit| (edit.0.start, edit.0.end));
    for pair in edits.windows(2) {
        if pair[0].0.end > pair[1].0.start
            || (pair[0].0.start == pair[1].0.start && pair[0].0.end == pair[0].0.start)
        {
            return None;
        }
    }

    let span_start = edits.first()?.0.start;
    let span_end = edits.last()?.0.end;
    let mut result_text = original.to_owned();
    for (range, replacement, _) in edits.iter().rev() {
        result_text.replace_range(range.clone(), replacement);
    }
    let delta = isize::try_from(result_text.len()).ok()? - isize::try_from(original.len()).ok()?;
    let final_span_end =
        usize::try_from(isize::try_from(span_end).ok()?.checked_add(delta)?).ok()?;
    let replacement = result_text.get(span_start..final_span_end)?.to_owned();

    let primary_delta_before = edits
        .iter()
        .filter(|(range, _, primary)| !*primary && range.end <= primary_range.start)
        .try_fold(0isize, |sum, (range, value, _)| {
            sum.checked_add(isize::try_from(value.len()).ok()? - isize::try_from(range.len()).ok()?)
        })?;
    let final_primary_start = usize::try_from(
        isize::try_from(primary_range.start)
            .ok()?
            .checked_add(primary_delta_before)?,
    )
    .ok()?;
    let cursor = final_primary_start.checked_add(caret_in_insertion)?;
    if cursor > result_text.len() || !result_text.is_char_boundary(cursor) {
        return None;
    }

    let merged_text = replacement.clone();
    let merged_range = TextEdit {
        range: bytes_range_to_component_position(original, span_start..span_end)?,
        new_text: replacement,
    };
    item.text_edit = Some(CompletionTextEdit::Edit(merged_range));
    item.additional_text_edits = None;
    // A command requires a separate client-to-daemon action path. Do not let
    // the stock GPUI completion menu silently imply that it was executed.
    item.command = None;

    // Keep resolve metadata intact while recording which Fresh-routed server
    // owns it. The daemon unwraps this envelope before completion/resolve.
    item.data = Some(json!({
        "_fresh_server": server,
        "_fresh_cursor_offset": caret_in_insertion,
        "_fresh_original": source_item,
    }));

    let plan = CompletionPlan {
        before: original.to_owned(),
        after: result_text,
        cursor,
        request_offset: offset,
    };
    let key = (item.label.clone(), span_start, merged_text);
    Some((item, plan, key))
}

fn is_completion_word_char(ch: char) -> bool {
    ch == '_' || ch.is_alphanumeric()
}

fn fresh_cursor_offset(item: &CompletionItem) -> Option<usize> {
    item.data
        .as_ref()?
        .get("_fresh_cursor_offset")
        .and_then(JsonValue::as_u64)
        .and_then(|n| usize::try_from(n).ok())
}

/// Strictly map an LSP UTF-16 range into UTF-8 byte offsets. A position inside
/// a surrogate pair or outside the loaded text is rejected rather than clamped.
fn position_range_to_bytes(text: &str, start: &Position, end: &Position) -> Option<Range<usize>> {
    let start = utf16_position_to_byte(text, start)?;
    let end = utf16_position_to_byte(text, end)?;
    (start <= end).then_some(start..end)
}

fn utf16_position_to_byte(text: &str, position: &Position) -> Option<usize> {
    let line = usize::try_from(position.line).ok()?;
    let character = usize::try_from(position.character).ok()?;
    let mut line_start = 0;
    for _ in 0..line {
        let newline = text.get(line_start..)?.find('\n')?;
        line_start += newline + 1;
    }
    let rest = text.get(line_start..)?;
    let content_end = rest.find('\n').unwrap_or(rest.len());
    let content = rest
        .get(..content_end)?
        .strip_suffix('\r')
        .unwrap_or(&rest[..content_end]);
    let mut units = 0;
    for (byte, ch) in content.char_indices() {
        if units == character {
            return Some(line_start + byte);
        }
        let width = ch.len_utf16();
        if character > units && character < units + width {
            return None;
        }
        units += width;
    }
    (units == character).then_some(line_start + content.len())
}

/// gpui-component 0.6.6's stock `insert_completion` routes LSP edit columns
/// through `RopeExt::position_to_offset`, whose columns count Unicode scalar
/// values rather than UTF-16 units. Emit those component-specific columns here
/// after consuming the server's true UTF-16 coordinates above.
fn bytes_range_to_component_position(text: &str, range: Range<usize>) -> Option<lsp_types::Range> {
    Some(lsp_types::Range {
        start: byte_to_component_position(text, range.start)?,
        end: byte_to_component_position(text, range.end)?,
    })
}

fn byte_to_component_position(text: &str, offset: usize) -> Option<Position> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let prefix = text.get(..offset)?;
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx + 1);
    let character = text.get(line_start..offset)?.chars().count();
    Some(Position::new(
        u32::try_from(line).ok()?,
        u32::try_from(character).ok()?,
    ))
}

#[cfg(test)]
fn byte_to_utf16_position(text: &str, offset: usize) -> Option<Position> {
    if offset > text.len() || !text.is_char_boundary(offset) {
        return None;
    }
    let prefix = text.get(..offset)?;
    let line = prefix.bytes().filter(|byte| *byte == b'\n').count();
    let line_start = prefix.rfind('\n').map_or(0, |idx| idx + 1);
    let character = text.get(line_start..offset)?.encode_utf16().count();
    Some(Position::new(
        u32::try_from(line).ok()?,
        u32::try_from(character).ok()?,
    ))
}

/// Merge all non-empty server hovers into markdown with an explicit source
/// label, so multi-server routing remains visible to the user.
pub(crate) fn merge_hover(result: &LspResult) -> Option<Hover> {
    if result.stale || result.feature != LspFeature::Hover {
        return None;
    }
    let sections = result
        .responses
        .iter()
        .filter_map(|response| {
            let hover: Hover = serde_json::from_value(response.result.clone()).ok()?;
            let contents = hover_text(hover.contents);
            (!contents.trim().is_empty()).then_some((response.server.as_str(), contents))
        })
        .map(|(server, contents)| format!("### {server}\n\n{contents}"))
        .collect::<Vec<_>>();
    (!sections.is_empty()).then(|| Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: sections.join("\n\n---\n\n"),
        }),
        range: None,
    })
}

fn hover_text(contents: HoverContents) -> String {
    match contents {
        HoverContents::Scalar(marked) => match marked {
            lsp_types::MarkedString::String(text) => text,
            lsp_types::MarkedString::LanguageString(code) => {
                format!("```{}\n{}\n```", code.language, code.value)
            }
        },
        HoverContents::Array(marked) => marked
            .into_iter()
            .map(|item| match item {
                lsp_types::MarkedString::String(text) => text,
                lsp_types::MarkedString::LanguageString(code) => {
                    format!("```{}\n{}\n```", code.language, code.value)
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n"),
        HoverContents::Markup(markup) => markup.value,
    }
}

/// Text for the compact signature bar used by the pane. Active parameter
/// offsets are UTF-16 offsets per the LSP type; invalid ranges are ignored.
pub(crate) fn signature_text(result: &LspResult) -> Option<String> {
    if result.stale || result.feature != LspFeature::SignatureHelp {
        return None;
    }
    let mut sections = Vec::new();
    for response in &result.responses {
        let Ok(help) = serde_json::from_value::<SignatureHelp>(response.result.clone()) else {
            continue;
        };
        let index = help.active_signature.unwrap_or(0) as usize;
        let Some(signature) = help.signatures.get(index) else {
            continue;
        };
        let label = signature.label.clone();
        let active = signature.active_parameter.or(help.active_parameter);
        let active_label = active
            .and_then(|index| signature.parameters.as_ref()?.get(index as usize))
            .and_then(|parameter| match &parameter.label {
                lsp_types::ParameterLabel::Simple(label) => Some(label.clone()),
                lsp_types::ParameterLabel::LabelOffsets([start, end]) => {
                    let start = utf16_position_to_byte(&label, &Position::new(0, *start))?;
                    let end = utf16_position_to_byte(&label, &Position::new(0, *end))?;
                    label.get(start..end).map(str::to_owned)
                }
            });
        sections.push(if result.responses.len() > 1 {
            format!(
                "{}: {}{}",
                response.server,
                label,
                active_label
                    .map(|p| format!("  [active: {p}]"))
                    .unwrap_or_default()
            )
        } else {
            format!(
                "{}{}",
                label,
                active_label
                    .map(|p| format!("  [active: {p}]"))
                    .unwrap_or_default()
            )
        });
    }
    (!sections.is_empty()).then(|| sections.join("  ·  "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fresh_gui_protocol::LspServerResponse;
    use gpui::{AppContext as _, Render, TestAppContext, VisualTestContext};
    use gpui_kit::component::input::{Editor, EditorState};

    struct EditorHarness {
        state: gpui::Entity<EditorState>,
    }

    impl Render for EditorHarness {
        fn render(
            &mut self,
            _: &mut Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            Editor::new(&self.state)
        }
    }

    fn result(feature: LspFeature, offset: usize, value: JsonValue) -> LspResult {
        LspResult {
            request_id: 7,
            buffer_id: "b".into(),
            view_id: "v".into(),
            rev: 3,
            offset,
            feature,
            navigation_targets: Vec::new(),
            responses: vec![LspServerResponse {
                server: "rust-analyzer".into(),
                result: value,
            }],
            completion_triggers: vec![],
            signature_triggers: vec![],
            status: None,
            stale: false,
        }
    }

    #[test]
    fn utf16_positions_map_after_astral_unicode() {
        let text = "a🎉name";
        assert_eq!(utf16_position_to_byte(text, &Position::new(0, 3)), Some(5));
        assert_eq!(utf16_position_to_byte(text, &Position::new(0, 2)), None);
    }

    #[test]
    fn completion_auto_import_edits_become_one_text_edit() {
        let text = "use std::fmt;\n\nfn main() { pri }";
        let cursor = text.find("pri").unwrap() + 3;
        let start = byte_to_utf16_position(text, cursor - 3).unwrap();
        let end = byte_to_utf16_position(text, cursor).unwrap();
        let import_start = byte_to_utf16_position(text, "use std::fmt;\n".len()).unwrap();
        let value = json!([{
            "label":"println!",
            "textEdit":{"range":{"start":start,"end":end},"newText":"println!()"},
            "additionalTextEdits":[{"range":{"start":import_start,"end":import_start},"newText":"use std::io;\n"}]
        }]);
        let (items, plans) =
            normalize_completions(text, cursor, &result(LspFeature::Completion, cursor, value));
        assert_eq!(items.len(), 1);
        assert!(items[0].additional_text_edits.is_none());
        assert_eq!(plans[0].before, text);
        assert_eq!(
            plans[0].after,
            "use std::fmt;\nuse std::io;\n\nfn main() { println!() }"
        );
    }

    #[test]
    fn fresh_snippet_cursor_metadata_is_restored_in_plan() {
        let text = "fn main() { pri }";
        let cursor = text.find("pri").unwrap() + 3;
        let start = byte_to_utf16_position(text, cursor - 3).unwrap();
        let end = byte_to_utf16_position(text, cursor).unwrap();
        let value = json!([{
            "label":"call",
            "filterText":"pri",
            "textEdit":{"range":{"start":start,"end":end},"newText":"call(arg)"},
            "data":{"_fresh_cursor_offset":5}
        }]);
        let (_, plans) =
            normalize_completions(text, cursor, &result(LspFeature::Completion, cursor, value));
        assert_eq!(plans[0].cursor, text.find("pri").unwrap() + 5);
    }

    #[test]
    fn malformed_surrogate_or_overlapping_additional_edit_is_rejected() {
        let text = "a🎉x";
        let value = json!([{
            "label":"x",
            "textEdit":{"range":{"start":{"line":0,"character":2},"end":{"line":0,"character":4}},"newText":"X"}
        }]);
        let (items, _) = normalize_completions(
            text,
            text.len(),
            &result(LspFeature::Completion, text.len(), value),
        );
        assert!(items.is_empty());
    }

    #[test]
    fn completion_list_defaults_supply_edit_range() {
        let text = "pri";
        let value = json!({
            "isIncomplete":false,
            "itemDefaults":{"editRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}}},
            "items":[{"label":"print","insertText":"print!()"}]
        });
        let (items, _) = normalize_completions(text, 3, &result(LspFeature::Completion, 3, value));
        assert_eq!(items.len(), 1);
        assert!(matches!(
            items[0].text_edit,
            Some(CompletionTextEdit::Edit(_))
        ));
    }

    #[test]
    fn stale_or_mismatched_completion_results_are_ignored() {
        let value = json!([{"label":"print"}]);
        let mut stale = result(LspFeature::Completion, 3, value.clone());
        stale.stale = true;
        assert!(normalize_completions("pri", 3, &stale).0.is_empty());
        assert!(
            normalize_completions("pri", 2, &result(LspFeature::Hover, 2, value))
                .0
                .is_empty()
        );
    }

    #[test]
    fn completion_list_text_edit_text_overrides_item_insert_text() {
        let value = json!({
            "itemDefaults": {
                "editRange":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},
                "textEditText":"print!()"
            },
            "items":[{"label":"print","insertText":"ignored"}]
        });
        let (items, _) = normalize_completions("pri", 3, &result(LspFeature::Completion, 3, value));
        let Some(CompletionTextEdit::Edit(edit)) =
            items.first().and_then(|item| item.text_edit.as_ref())
        else {
            panic!("expected default text edit");
        };
        assert_eq!(edit.new_text, "print!()");
    }

    #[test]
    fn completions_keep_distinct_server_edits_and_hover_sections() {
        let text = "fo";
        let value = |replacement: &str| {
            json!([{"label":"foo","textEdit":{
                "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},
                "newText":replacement
            }}])
        };
        let mut response = result(LspFeature::Completion, 2, value("foo"));
        response.responses.push(LspServerResponse {
            server: "other-server".into(),
            result: value("food"),
        });
        let (items, _) = normalize_completions(text, 2, &response);
        assert_eq!(items.len(), 2);

        let mut hover = result(
            LspFeature::Hover,
            2,
            json!({"contents":{"kind":"plaintext","value":"first"}}),
        );
        hover.responses.push(LspServerResponse {
            server: "other-server".into(),
            result: json!({"contents":{"kind":"plaintext","value":"second"}}),
        });
        let rendered = merge_hover(&hover).unwrap();
        let HoverContents::Markup(markup) = rendered.contents else {
            panic!("merged markdown")
        };
        assert!(markup.value.contains("### rust-analyzer\n\nfirst"));
        assert!(markup.value.contains("### other-server\n\nsecond"));
    }

    #[test]
    fn completion_filter_text_matches_and_native_highlights_are_utf8_safe() {
        let text = "pri";
        let value = json!([{
            "label":"displayName",
            "filterText":"printable",
            "textEdit":{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"displayName"}
        }]);
        let (items, _) = normalize_completions(text, 3, &result(LspFeature::Completion, 3, value));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filter_text.as_deref(), Some(""));
        assert_eq!(
            items[0].data.as_ref().unwrap()["_fresh_original"]["filterText"],
            "printable"
        );

        let unicode_text = "ñ";
        let value = json!([{
            "label":"🛠tool",
            "filterText":"ñ",
            "textEdit":{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"🛠tool"}
        }]);
        let (items, _) = normalize_completions(
            unicode_text,
            unicode_text.len(),
            &result(LspFeature::Completion, unicode_text.len(), value),
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filter_text.as_deref(), Some(""));
        let value = json!([{
            "label":"🛠tool",
            "filterText":"printable",
            "textEdit":{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"🛠tool"}
        }]);
        let (items, _) = normalize_completions(text, 3, &result(LspFeature::Completion, 3, value));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].filter_text.as_deref(), Some(""));
    }

    #[test]
    fn signature_active_parameter_is_plain_text_and_invalid_index_is_ignored() {
        let value = json!({
            "activeSignature":0,"activeParameter":9,
            "signatures":[{"label":"call(value)","activeParameter":0,
                "parameters":[{"label":[5,10]}]}]
        });
        assert_eq!(
            signature_text(&result(LspFeature::SignatureHelp, 0, value)).as_deref(),
            Some("call(value)  [active: value]")
        );
        let value = json!({"signatures":[{"label":"call(value)","parameters":[]}]});
        assert_eq!(
            signature_text(&result(LspFeature::SignatureHelp, 0, value)).as_deref(),
            Some("call(value)")
        );
    }

    #[gpui::test]
    fn gpui_completion_inserts_composed_edit_as_one_range_transaction(cx: &mut TestAppContext) {
        cx.update(crate::gui::init);
        let text = "🎉use std::fmt;\n\nfn main() { pri }";
        let cursor = text.find("pri").unwrap() + 3;
        let start = byte_to_utf16_position(text, cursor - 3).unwrap();
        let end = byte_to_utf16_position(text, cursor).unwrap();
        let import = byte_to_utf16_position(text, "🎉use std::fmt;\n".len()).unwrap();
        let response = result(
            LspFeature::Completion,
            cursor,
            json!([{
                "label":"println!",
                "textEdit":{"range":{"start":start,"end":end},"newText":"println!()"},
                "additionalTextEdits":[{"range":{"start":import,"end":import},"newText":"use std::io;\n"}]
            }]),
        );
        let (items, plans) = normalize_completions(text, cursor, &response);
        let item = items.into_iter().next().expect("normalized completion");
        let plan = plans.first().expect("cursor plan").clone();
        let mut state = None;
        let (_, app) = cx.add_window_view(|window, cx| {
            let editor = cx.new(|cx| EditorState::new(window, cx).default_value(text));
            state = Some(editor.clone());
            EditorHarness { state: editor }
        });
        let state = state.expect("editor state");
        VisualTestContext::update(app, |window, cx| {
            state.update(cx, |state, cx| {
                state.insert_completion(&item, cursor..cursor, window, cx);
                state.set_selected_range(plan.cursor..plan.cursor, cx);
                assert_eq!(state.text().to_string(), plan.after);
            });
        });
        let mut sync = fresh_gui_client::edit_sync::EditSync::new(text.to_owned(), 9);
        let (revision, edits) = sync.begin_edit(&plan.after).expect("one committed draft");
        assert_eq!(revision, 9);
        assert_eq!(edits.len(), 1);
        let mut reconstructed = text.to_owned();
        for edit in edits {
            reconstructed.replace_range(edit.start..edit.end, &edit.text);
        }
        assert_eq!(reconstructed, plan.after);
    }
}

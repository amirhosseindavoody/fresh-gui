//! Native rename/code-action UI and revision checked workspace edit preview.
use super::*;
use fresh_gui_protocol::{
    CAP_LSP_WORKSPACE_EDITS, LspRequestFeature as Feature, LspResult, WorkspaceBufferUpdate,
    WorkspaceEditPreview,
};
use gpui_kit::component::scroll::ScrollableElement as _;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_EDIT: AtomicU64 = AtomicU64::new(1);

#[derive(Default)]
pub(super) struct WorkspaceEdits {
    pub open: bool,
    mode: Mode,
    input: Option<Entity<InputState>>,
    input_subscription: Option<Subscription>,
    pub(super) source: Option<Entity<EditorPanel>>,
    source_text: String,
    offset: usize,
    rev: u64,
    server: Option<String>,
    actions: Vec<ServerAction>,
    preview: Option<WorkspaceEditPreview>,
    preview_valid: bool,
    preview_request_id: Option<String>,
    apply_request_id: Option<String>,
    applying: bool,
    deferred_command: Option<CommandData>,
    loading: bool,
    generation: u64,
    workspace_id: String,
    pub(super) server_initiated: bool,
    status: String,
}

#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Mode {
    #[default]
    Closed,
    Rename,
    Actions,
    Preview,
}

#[derive(Clone, Debug, PartialEq)]
struct ServerAction {
    server: String,
    item: Value,
}

#[derive(Clone, Debug, PartialEq)]
struct CommandData {
    command: String,
    arguments: Vec<Value>,
    server: String,
}

fn normalize_actions(responses: &[fresh_gui_protocol::LspServerResponse]) -> Vec<ServerAction> {
    responses
        .iter()
        .flat_map(|response| {
            response
                .result
                .as_array()
                .into_iter()
                .flatten()
                .cloned()
                .map(|item| ServerAction {
                    server: response.server.clone(),
                    item,
                })
        })
        .collect()
}

fn normalize_command(value: &Value, server: &str) -> Option<CommandData> {
    let command = value.get("command");
    let command = match command {
        Some(Value::String(command)) => (command.as_str(), value.get("arguments")),
        Some(Value::Object(command)) => (
            command.get("command")?.as_str()?,
            command.get("arguments").or_else(|| value.get("arguments")),
        ),
        _ => return None,
    };
    Some(CommandData {
        command: command.0.to_owned(),
        arguments: command
            .1
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
        server: server.to_owned(),
    })
}

fn is_direct_apply_ack(expected: Option<&str>, received: &str) -> bool {
    !received.is_empty() && expected == Some(received)
}

fn has_lsp_value(action: &Value, key: &str) -> bool {
    action.get(key).is_some_and(|value| !value.is_null())
}

impl WorkspaceEdits {
    fn id() -> String {
        format!(
            "workspace-edit-{}",
            NEXT_EDIT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn active_snapshot(
        ws: &Workspace,
        cx: &App,
    ) -> Option<(Entity<EditorPanel>, String, usize, u64)> {
        let ActiveSurface::Editor(path) = ws.active.as_ref()? else {
            return None;
        };
        let panel = ws.editors.get(path)?.clone();
        let (text, offset) = panel.read(cx).navigation_snapshot(cx);
        if !panel.read(cx).lsp_snapshot_matches(&text, Some(offset), cx) {
            return None;
        }
        let rev = panel.read(cx).revision();
        Some((panel, text, offset, rev))
    }

    fn start(
        ws: &mut Workspace,
        feature: Feature,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        if !ws.capabilities.iter().any(|c| c == CAP_LSP_WORKSPACE_EDITS) {
            ws.status =
                "Rename and code actions require daemon capability lsp.workspace-edits".into();
            cx.notify();
            return;
        }
        if ws.workspace_edits.open || ws.workspace_edits.preview.is_some() {
            let mut prior = std::mem::take(&mut ws.workspace_edits);
            prior.dismiss(ws, cx);
            ws.workspace_edits = prior;
        }
        let Some((panel, text, offset, rev)) = Self::active_snapshot(ws, cx) else {
            ws.status = "Select a synchronized editor before requesting an LSP edit".into();
            cx.notify();
            return;
        };
        let request_item = (feature == Feature::CodeActions).then(|| {
            let selection = panel.read(cx).navigation_selection(cx);
            json!({
                "startOffset": selection.anchor.min(selection.head),
                "endOffset": selection.anchor.max(selection.head)
            })
        });
        let receiver = panel.update(cx, |p, cx| {
            p.queue_lsp_target(feature, offset, None, text.clone(), request_item, None, cx)
        });
        let generation = ws.workspace_edits.generation.wrapping_add(1);
        ws.workspace_edits = WorkspaceEdits {
            open: true,
            mode: if feature == Feature::PrepareRename {
                Mode::Rename
            } else {
                Mode::Actions
            },
            source: Some(panel),
            source_text: text,
            offset,
            rev,
            generation,
            workspace_id: ws.workspace_id_or_empty(),
            status: "Requesting language server…".into(),
            loading: true,
            ..Default::default()
        };
        if feature == Feature::PrepareRename {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("New symbol name"));
            let existing = word_at(&ws.workspace_edits.source_text, offset);
            if !existing.is_empty() {
                input.update(cx, |input, cx| input.set_value(&existing, window, cx));
            }
            let input_subscription = cx.subscribe(&input, |ws, input, event: &InputEvent, cx| {
                if matches!(event, InputEvent::PressEnter { .. })
                    && ws.workspace_edits.mode == Mode::Rename
                    && !ws.workspace_edits.loading
                    && ws.workspace_edits.server.is_some()
                {
                    let name = input.read(cx).value().to_string();
                    let mut state = std::mem::take(&mut ws.workspace_edits);
                    state.send_rename(ws, name, cx);
                    ws.workspace_edits = state;
                }
            });
            input.update(cx, |input, cx| input.focus(window, cx));
            ws.workspace_edits.input = Some(input);
            ws.workspace_edits.input_subscription = Some(input_subscription);
        }
        ws.workspace_edits.await_result(receiver, cx);
        cx.notify();
    }

    fn handle_lsp_result(
        &mut self,
        result: LspResult,
        ws: &mut Workspace,
        cx: &mut Context<Workspace>,
    ) -> bool {
        if !matches!(
            result.feature,
            Feature::PrepareRename
                | Feature::Rename
                | Feature::CodeActions
                | Feature::CodeActionResolve
                | Feature::ExecuteCommand
        ) {
            return false;
        }
        let current = self.source.as_ref().is_some_and(|panel| {
            let (text, offset) = panel.read(cx).navigation_snapshot(cx);
            panel.read(cx).buffer_id() == result.buffer_id
                && panel.read(cx).revision() == result.rev
                && panel.read(cx).lsp_snapshot_matches(&text, Some(offset), cx)
                && text == self.source_text
                && offset == self.offset
        });
        if result.stale || !current {
            self.loading = false;
            self.status = "LSP edit discarded because the editor changed".into();
            self.open = true;
            cx.notify();
            return true;
        }
        self.rev = result.rev;
        if let Some(status) = result.status.as_deref() {
            self.loading = false;
            self.status = status.to_owned();
            self.open = true;
            cx.notify();
            return true;
        }
        self.loading = false;
        match result.feature {
            Feature::PrepareRename => {
                let Some(response) = result.responses.iter().find(|r| !r.result.is_null()) else {
                    self.dismiss(ws, cx);
                    ws.status = "The language server cannot rename this symbol".into();
                    return true;
                };
                self.server = Some(response.server.clone());
                self.open = true;
                self.mode = Mode::Rename;
                self.status = "Enter the new symbol name".into();
            }
            Feature::Rename => {
                let edit = result
                    .responses
                    .iter()
                    .find(|r| !r.result.is_null())
                    .map(|r| r.result.clone());
                if let Some(edit) = edit {
                    self.prepare(ws, edit, None, cx);
                } else {
                    self.status = "The language server returned no rename edits".into();
                    self.open = true;
                }
            }
            Feature::CodeActions => {
                self.actions = normalize_actions(&result.responses);
                self.open = true;
                self.mode = Mode::Actions;
                self.status = if self.actions.is_empty() {
                    "No code actions available"
                } else {
                    "Choose a code action"
                }
                .into();
            }
            Feature::CodeActionResolve => {
                if let Some(response) = result.responses.iter().find(|r| !r.result.is_null()) {
                    self.consume_action(
                        ws,
                        ServerAction {
                            server: response.server.clone(),
                            item: response.result.clone(),
                        },
                        false,
                        cx,
                    );
                }
            }
            Feature::ExecuteCommand => {
                if self.preview.is_none() {
                    self.open = false;
                    self.mode = Mode::Closed;
                }
                ws.status = result
                    .status
                    .unwrap_or_else(|| "Language server command completed".into())
                    .into();
            }
            _ => {}
        }
        cx.notify();
        true
    }

    fn prepare(&mut self, ws: &Workspace, edit: Value, command: Option<CommandData>, cx: &App) {
        let Some(source) = &self.source else {
            return;
        };
        let request_id = Self::id();
        if let Some(command) = command {
            self.deferred_command = Some(command);
        }
        let buffer_id = source.read(cx).buffer_id().to_owned();
        ws.ade.send(AdeCmd::WorkspaceEditPrepare {
            request_id: request_id.clone(),
            buffer_id,
            base_rev: self.rev,
            edit,
        });
        self.preview_request_id = Some(request_id);
        self.status = "Preparing revision checked workspace edit…".into();
        self.open = true;
        self.mode = Mode::Preview;
    }

    pub(super) fn preview(
        &mut self,
        request_id: String,
        preview: WorkspaceEditPreview,
        ws: &Workspace,
        cx: &mut Context<Workspace>,
    ) {
        if request_id.is_empty()
            && (self.applying
                || self.preview.is_some()
                || self
                    .preview_request_id
                    .as_deref()
                    .is_some_and(|id| !id.is_empty()))
        {
            ws.ade.send(AdeCmd::WorkspaceEditCancel {
                buffer_id: preview.buffer_id,
                token: preview.token,
            });
            self.status = "An additional server edit was rejected while this preview is pending; request that action again".into();
            cx.notify();
            return;
        }
        if request_id.is_empty() && self.preview_request_id.as_deref().is_none_or(str::is_empty) {
            let Some(source) = ws.editor_by_buffer(&preview.buffer_id, cx) else {
                ws.ade.send(AdeCmd::WorkspaceEditCancel {
                    buffer_id: preview.buffer_id,
                    token: preview.token,
                });
                return;
            };
            let (text, offset) = source.read(cx).navigation_snapshot(cx);
            self.generation = self.generation.wrapping_add(1);
            self.source = Some(source.clone());
            self.source_text = text;
            self.offset = offset;
            self.rev = source.read(cx).revision();
            self.workspace_id = ws.workspace_id_or_empty();
            self.mode = Mode::Preview;
            self.open = true;
            self.server_initiated = true;
            self.deferred_command = None;
            self.apply_request_id = None;
            self.loading = false;
        }
        if self.mode != Mode::Preview
            || self
                .preview_request_id
                .as_deref()
                .is_some_and(|id| !request_id.is_empty() && id != request_id.as_str())
            || self.workspace_id != ws.workspace_id_or_empty()
            || self
                .source
                .as_ref()
                .is_none_or(|p| p.read(cx).buffer_id() != preview.buffer_id)
        {
            ws.ade.send(AdeCmd::WorkspaceEditCancel {
                buffer_id: preview.buffer_id,
                token: preview.token,
            });
            return;
        }
        self.status = format!(
            "Review {} file{} before applying",
            preview.files.len(),
            if preview.files.len() == 1 { "" } else { "s" }
        );
        self.preview = Some(preview);
        self.preview_request_id = Some(request_id);
        self.open = true;
        self.preview_valid = match self.validate_preview(ws, cx) {
            Ok(()) => true,
            Err(reason) => {
                self.status = reason;
                false
            }
        };
        cx.notify();
    }

    fn validate_preview(&self, ws: &Workspace, cx: &App) -> Result<(), String> {
        let preview = self
            .preview
            .as_ref()
            .ok_or_else(|| "Waiting for edit preview".to_owned())?;
        if self.workspace_id != ws.workspace_id_or_empty() {
            return Err("Workspace changed; cancel and request the edit again".into());
        }
        if !self.server_initiated {
            let Some(ActiveSurface::Editor(active_path)) = ws.active.as_ref() else {
                return Err("Return to the source editor before applying the edit".into());
            };
            if ws
                .editors
                .get(active_path)
                .is_none_or(|panel| panel.read(cx).buffer_id() != preview.buffer_id)
            {
                return Err("The source editor changed; cancel and request the edit again".into());
            }
        }
        for file in &preview.files {
            let Some(buffer_id) = &file.buffer_id else {
                continue;
            };
            let panel = ws
                .editor_by_buffer(buffer_id, cx)
                .ok_or_else(|| format!("{} is no longer open", file.path))?;
            let panel = panel.read(cx);
            let (text, _) = panel.navigation_snapshot(cx);
            if !panel.lsp_snapshot_matches(&text, None, cx) {
                return Err(format!("{} is not synchronized with the daemon", file.path));
            }
            if file.base_rev.is_some_and(|rev| panel.revision() != rev) || text != file.before {
                return Err(format!(
                    "{} changed after the preview; refresh the edit",
                    file.path
                ));
            }
            if panel.is_dirty() && text != file.before {
                return Err(format!(
                    "{} has unsaved changes not present in the preview",
                    file.path
                ));
            }
        }
        let source = self
            .source
            .as_ref()
            .ok_or_else(|| "The source editor was closed".to_owned())?;
        let (source_text, source_offset) = source.read(cx).navigation_snapshot(cx);
        if source.read(cx).revision() != self.rev
            || source_text != self.source_text
            || source_offset != self.offset
            || !source
                .read(cx)
                .lsp_snapshot_matches(&source_text, Some(source_offset), cx)
        {
            return Err(
                "The source editor changed or has pending edits; request a new preview".into(),
            );
        }
        Ok(())
    }

    pub(super) fn applied(
        &mut self,
        request_id: String,
        updates: Vec<WorkspaceBufferUpdate>,
        ws: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        for update in &updates {
            if let Some(panel) = ws.editor_by_buffer(&update.buffer_id, cx) {
                let update = update.clone();
                panel.update(cx, |panel, cx| {
                    panel.apply_workspace_edit_snapshot(update.rev, update.text, window, cx)
                });
            }
        }
        let explorer_root = ws.explorer_root.clone();
        ws.list_dir(&explorer_root);
        ws.refresh_git();
        if ws.workspace_id_or_empty() != self.workspace_id {
            return;
        }
        let direct = self.applying
            && is_direct_apply_ack(self.apply_request_id.as_deref(), &request_id)
            && self.workspace_id == ws.workspace_id_or_empty();
        let correlated_broadcast = self.applying
            && request_id.is_empty()
            && self.empty_broadcast_matches(&updates)
            && self.workspace_id == ws.workspace_id_or_empty();
        if !direct && !correlated_broadcast {
            return;
        }
        if !direct {
            self.status = "Edit published; waiting for the apply acknowledgement…".into();
            cx.notify();
            return;
        }
        self.applying = false;
        self.preview_valid = false;
        self.preview = None;
        self.preview_request_id = None;
        if let Some(command) = self.deferred_command.take() {
            if let Some(source) = &self.source {
                let (text, offset) = source.read(cx).navigation_snapshot(cx);
                self.source_text = text;
                self.offset = offset;
                self.rev = source.read(cx).revision();
            }
            self.queue_command(ws, command, cx);
        } else {
            self.open = false;
            self.mode = Mode::Closed;
            ws.status = "Workspace edit applied".into();
        }
        cx.notify();
    }

    fn consume_action(
        &mut self,
        ws: &mut Workspace,
        action: ServerAction,
        allow_resolve: bool,
        cx: &mut Context<Workspace>,
    ) {
        if has_lsp_value(&action.item, "disabled") {
            return;
        }
        if allow_resolve
            && has_lsp_value(&action.item, "data")
            && !has_lsp_value(&action.item, "edit")
            && !has_lsp_value(&action.item, "command")
        {
            let Some(panel) = &self.source else {
                return;
            };
            let receiver = panel.update(cx, |p, cx| {
                p.queue_lsp_target(
                    Feature::CodeActionResolve,
                    self.offset,
                    None,
                    self.source_text.clone(),
                    Some(action.item.clone()),
                    Some(action.server.clone()),
                    cx,
                )
            });
            self.open = false;
            self.await_result(receiver, cx);
            return;
        }
        let edit = action
            .item
            .get("edit")
            .filter(|edit| !edit.is_null())
            .cloned();
        let command = normalize_command(&action.item, &action.server);
        if let Some(edit) = edit {
            self.prepare(ws, edit, command, cx);
        } else if let Some(command) = command {
            self.queue_command(ws, command, cx);
        } else {
            self.open = false;
            self.mode = Mode::Closed;
        }
    }

    fn queue_command(
        &mut self,
        ws: &mut Workspace,
        command: CommandData,
        cx: &mut Context<Workspace>,
    ) {
        let Some(panel) = &self.source else {
            return;
        };
        let (text, offset) = panel.read(cx).navigation_snapshot(cx);
        if !panel.read(cx).lsp_snapshot_matches(&text, Some(offset), cx)
            || text != self.source_text
            || offset != self.offset
            || panel.read(cx).revision() != self.rev
        {
            ws.status =
                "Cannot run language server command because the source buffer is unsynchronized"
                    .into();
            return;
        }
        let item = json!({ "command": command.command, "arguments": command.arguments });
        let receiver = panel.update(cx, |panel, cx| {
            panel.queue_lsp_target(
                Feature::ExecuteCommand,
                offset,
                None,
                text,
                Some(item),
                Some(command.server),
                cx,
            )
        });
        self.open = true;
        self.loading = true;
        self.status = "Running language server command…".into();
        self.await_result(receiver, cx);
    }

    fn await_result(
        &mut self,
        receiver: async_channel::Receiver<LspResult>,
        cx: &mut Context<Workspace>,
    ) {
        let generation = self.generation;
        let workspace_id = self.workspace_id.clone();
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            let response = receiver.recv().await;
            let _ = weak.update(cx, |ws, cx| {
                let mut state = std::mem::take(&mut ws.workspace_edits);
                if state.generation == generation
                    && state.workspace_id == workspace_id
                    && ws.workspace_id_or_empty() == workspace_id
                {
                    match response {
                        Ok(result) => {
                            state.handle_lsp_result(result, ws, cx);
                        }
                        Err(_) => {
                            state.loading = false;
                            state.open = true;
                            state.status =
                                "Language server request was cancelled or timed out".into();
                            cx.notify();
                        }
                    }
                }
                ws.workspace_edits = state;
            });
        })
        .detach();
    }

    fn dismiss(&mut self, ws: &Workspace, cx: &mut Context<Workspace>) {
        if let Some(source) = &self.source {
            source.update(cx, |panel, _| panel.cancel_workspace_lsp());
        }
        if let Some(preview) = self.preview.take() {
            ws.ade.send(AdeCmd::WorkspaceEditCancel {
                buffer_id: preview.buffer_id,
                token: preview.token,
            });
        }
        self.generation = self.generation.wrapping_add(1);
        self.open = false;
        self.mode = Mode::Closed;
        self.applying = false;
        self.actions.clear();
        self.preview_request_id = None;
        self.apply_request_id = None;
        self.loading = false;
        self.server_initiated = false;
        self.deferred_command = None;
        cx.notify();
    }

    fn cancel(&mut self, ws: &Workspace, cx: &mut Context<Workspace>) {
        if self.open || self.preview.is_some() || self.applying || self.source.is_some() {
            self.dismiss(ws, cx);
        }
    }

    fn failed(
        &mut self,
        ws: &Workspace,
        code: &str,
        message: &str,
        cx: &mut Context<Workspace>,
    ) -> bool {
        let request_matches = self
            .apply_request_id
            .as_deref()
            .is_some_and(|id| message.contains(id))
            || self
                .preview_request_id
                .as_deref()
                .is_some_and(|id| message.contains(id));
        let code_matches = {
            let code = code.to_ascii_lowercase();
            code.contains("workspace_edit")
                || code.contains("workspace-edit")
                || code.contains("workspace.edit")
        };
        let relevant =
            (self.applying || self.mode == Mode::Preview) && (request_matches || code_matches);
        if !relevant {
            return false;
        }
        if let Some(preview) = self.preview.take() {
            ws.ade.send(AdeCmd::WorkspaceEditCancel {
                buffer_id: preview.buffer_id,
                token: preview.token,
            });
        }
        self.applying = false;
        self.preview_valid = false;
        self.loading = false;
        self.open = true;
        self.status = format!("Workspace edit failed ({code}): {message}");
        cx.notify();
        true
    }

    pub(super) fn render(&self, cx: &mut Context<Workspace>) -> impl IntoElement {
        let mut rows = Vec::new();
        match self.mode {
            Mode::Rename => {
                rows.push(
                    div()
                        .font_semibold()
                        .child("Rename symbol")
                        .into_any_element(),
                );
                if let Some(input) = &self.input {
                    rows.push(Input::new(input).into_any_element());
                }
                let weak = cx.entity().downgrade();
                rows.push(
                    Button::new("rename-symbol-apply")
                        .primary()
                        .label("Rename")
                        .disabled(self.loading || self.server.is_none())
                        .on_click(move |_, _, cx| {
                            let _ = weak.update(cx, |ws, cx| {
                                let name = ws
                                    .workspace_edits
                                    .input
                                    .as_ref()
                                    .map(|i| i.read(cx).value().to_string())
                                    .unwrap_or_default();
                                let mut state = std::mem::take(&mut ws.workspace_edits);
                                state.send_rename(ws, name, cx);
                                ws.workspace_edits = state;
                            });
                        })
                        .into_any_element(),
                );
            }
            Mode::Actions => {
                rows.push(
                    div()
                        .text_sm()
                        .child(self.status.clone())
                        .into_any_element(),
                );
                for (idx, action) in self.actions.iter().enumerate() {
                    let title = action
                        .item
                        .get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("Code action");
                    let disabled = has_lsp_value(&action.item, "disabled");
                    let reason = action
                        .item
                        .get("disabled")
                        .and_then(|value| value.get("reason"))
                        .and_then(Value::as_str);
                    let label = format!(
                        "{} · {}{}",
                        action.server,
                        title,
                        reason
                            .map(|reason| format!(" — {reason}"))
                            .unwrap_or_default()
                    );
                    let weak = cx.entity().downgrade();
                    rows.push(
                        Button::new(format!("lsp-code-action-{idx}"))
                            .label(label)
                            .disabled(disabled)
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |ws, cx| {
                                    let mut state = std::mem::take(&mut ws.workspace_edits);
                                    state.choose_action(idx, ws, cx);
                                    ws.workspace_edits = state;
                                });
                            })
                            .into_any_element(),
                    );
                }
            }
            Mode::Preview => {
                rows.push(
                    div()
                        .text_sm()
                        .child(self.status.clone())
                        .into_any_element(),
                );
                if let Some(preview) = &self.preview {
                    for file in &preview.files {
                        rows.push(
                            v_flex()
                                .gap_1()
                                .border_t_1()
                                .border_color(cx.theme().border)
                                .child(
                                    div()
                                        .font_semibold()
                                        .child(format!("{} · {}", file.path, file.operation)),
                                )
                                .child(h_flex().gap_2().children([
                                    diff_column("Before", &file.before, cx),
                                    diff_column("After", &file.after, cx),
                                ]))
                                .into_any_element(),
                        );
                    }
                    let can_apply = self.preview_valid && !self.applying;
                    let weak = cx.entity().downgrade();
                    rows.push(
                        Button::new("workspace-edit-apply")
                            .primary()
                            .label(if self.applying {
                                "Applying…"
                            } else {
                                "Apply all"
                            })
                            .disabled(!can_apply)
                            .on_click(move |_, _, cx| {
                                let _ = weak.update(cx, |ws, cx| {
                                    let mut state = std::mem::take(&mut ws.workspace_edits);
                                    state.apply(ws, cx);
                                    ws.workspace_edits = state;
                                });
                            })
                            .into_any_element(),
                    );
                }
            }
            Mode::Closed => {}
        }
        if !self.status.is_empty() && self.mode != Mode::Actions && self.mode != Mode::Preview {
            rows.push(
                div()
                    .text_sm()
                    .child(self.status.clone())
                    .into_any_element(),
            );
        }
        let weak = cx.entity().downgrade();
        rows.push(
            Button::new("workspace-edit-cancel")
                .ghost()
                .label("Cancel")
                .on_click(move |_, _, cx| {
                    let _ = weak.update(cx, |ws, cx| {
                        let mut state = std::mem::take(&mut ws.workspace_edits);
                        state.dismiss(ws, cx);
                        ws.workspace_edits = state;
                    });
                })
                .into_any_element(),
        );
        let weak = cx.entity().downgrade();
        v_flex()
            .id("workspace-edit-overlay")
            .absolute()
            .inset_0()
            .items_center()
            .pt(px(60.))
            .bg(cx.theme().background.opacity(0.45))
            .capture_key_down(move |event: &KeyDownEvent, _, cx| {
                if event.keystroke.key.eq_ignore_ascii_case("escape") {
                    let _ = weak.update(cx, |ws, cx| {
                        let mut state = std::mem::take(&mut ws.workspace_edits);
                        state.dismiss(ws, cx);
                        ws.workspace_edits = state;
                    });
                    cx.stop_propagation();
                }
            })
            .child(
                v_flex()
                    .w(px(720.))
                    .max_h(px(620.))
                    .overflow_y_scrollbar()
                    .gap_2()
                    .p_3()
                    .rounded(cx.theme().radius)
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .children(rows),
            )
    }

    fn empty_broadcast_matches(&self, updates: &[WorkspaceBufferUpdate]) -> bool {
        let Some(preview) = &self.preview else {
            return false;
        };
        let expected = preview
            .files
            .iter()
            .filter_map(|file| {
                file.buffer_id
                    .as_ref()
                    .map(|id| (id, &file.after, file.base_rev))
            })
            .collect::<Vec<_>>();
        !expected.is_empty()
            && expected.len() == updates.len()
            && expected.iter().all(|(id, text, base_rev)| {
                updates.iter().any(|update| {
                    &update.buffer_id == *id
                        && &update.text == *text
                        && base_rev.is_none_or(|rev| update.rev > rev)
                })
            })
    }

    fn apply(&mut self, ws: &mut Workspace, cx: &mut Context<Workspace>) {
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        if self.applying {
            return;
        }
        if let Err(reason) = self.validate_preview(ws, cx) {
            self.status = reason;
            self.preview_valid = false;
            cx.notify();
            return;
        }
        let request_id = Self::id();
        self.apply_request_id = Some(request_id.clone());
        self.applying = true;
        ws.ade.send(AdeCmd::WorkspaceEditApply {
            request_id,
            buffer_id: preview.buffer_id.clone(),
            token: preview.token.clone(),
        });
        self.status = "Applying all files atomically…".into();
    }

    fn send_rename(&mut self, _ws: &mut Workspace, name: String, cx: &mut Context<Workspace>) {
        if self.loading || self.server.is_none() {
            return;
        }
        if name.trim().is_empty() {
            self.status = "Enter a new symbol name".into();
            self.open = true;
            return;
        }
        let Some(panel) = &self.source else {
            return;
        };
        let receiver = panel.update(cx, |p, cx| {
            p.queue_lsp_target(
                Feature::Rename,
                self.offset,
                None,
                self.source_text.clone(),
                Some(json!({"newName":name})),
                self.server.clone(),
                cx,
            )
        });
        self.open = true;
        self.loading = true;
        self.status = "Requesting rename edits…".into();
        self.await_result(receiver, cx);
    }

    fn choose_action(&mut self, idx: usize, ws: &mut Workspace, cx: &mut Context<Workspace>) {
        let Some(action) = self.actions.get(idx).cloned() else {
            return;
        };
        self.consume_action(ws, action, true, cx);
    }
}

fn diff_column(title: &'static str, text: &str, cx: &App) -> AnyElement {
    let preview = text.to_owned();
    v_flex()
        .flex_1()
        .min_w_0()
        .child(div().text_xs().font_semibold().child(title))
        .child(
            div()
                .max_h(px(180.))
                .overflow_y_scrollbar()
                .p_1()
                .bg(cx.theme().muted)
                .font_family(cx.theme().mono_font_family.clone())
                .text_xs()
                .child(preview),
        )
        .into_any_element()
}

fn word_at(text: &str, offset: usize) -> String {
    let offset = offset.min(text.len());
    let Some(prefix) = text.get(..offset) else {
        return String::new();
    };
    let Some(suffix) = text.get(offset..) else {
        return String::new();
    };
    let start = prefix
        .char_indices()
        .rev()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(offset, |(i, _)| i);
    let end = suffix
        .char_indices()
        .take_while(|(_, c)| c.is_alphanumeric() || *c == '_')
        .last()
        .map_or(offset, |(i, c)| offset + i + c.len_utf8());
    text.get(start..end).unwrap_or_default().to_owned()
}

pub(super) fn cancel(ws: &mut Workspace, cx: &mut Context<Workspace>) {
    let mut state = std::mem::take(&mut ws.workspace_edits);
    state.cancel(ws, cx);
    ws.workspace_edits = state;
}

pub(super) fn failed(
    ws: &mut Workspace,
    code: &str,
    message: &str,
    cx: &mut Context<Workspace>,
) -> bool {
    let mut state = std::mem::take(&mut ws.workspace_edits);
    let handled = state.failed(ws, code, message, cx);
    ws.workspace_edits = state;
    handled
}

pub(super) fn begin_rename(ws: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    WorkspaceEdits::start(ws, Feature::PrepareRename, window, cx);
}

pub(super) fn begin_code_actions(
    ws: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    WorkspaceEdits::start(ws, Feature::CodeActions, window, cx);
}

#[cfg(test)]
mod tests {
    use super::{CommandData, is_direct_apply_ack, normalize_actions, normalize_command, word_at};
    use serde_json::json;

    #[test]
    fn code_action_commands_accept_both_lsp_forms() {
        let direct = json!({"command":"organize","arguments":[1]});
        assert_eq!(
            normalize_command(&direct, "rust-analyzer"),
            Some(CommandData {
                command: "organize".into(),
                arguments: vec![json!(1)],
                server: "rust-analyzer".into(),
            })
        );
        let nested = json!({"command":{"title":"Organize","command":"organize","arguments":[2]}});
        assert_eq!(
            normalize_command(&nested, "tsserver").unwrap().arguments,
            vec![json!(2)]
        );
    }

    #[test]
    fn actions_keep_server_and_disabled_reason_for_picker() {
        let responses = vec![fresh_gui_protocol::LspServerResponse {
            server: "one".into(),
            result: json!([
                {"title":"Fix","data":{"x":1}},
                {"title":"Unavailable","disabled":{"reason":"not safe"}}
            ]),
        }];
        let actions = normalize_actions(&responses);
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[0].server, "one");
        assert_eq!(actions[1].item["disabled"]["reason"], "not safe");
    }

    #[test]
    fn word_at_handles_boundaries_and_unicode() {
        assert_eq!(word_at("let éclair = 1", 8), "éclair");
        assert_eq!(word_at("hello!", 6), "");
    }

    #[test]
    fn empty_broadcast_never_counts_as_the_apply_ack() {
        assert!(is_direct_apply_ack(Some("req-1"), "req-1"));
        assert!(!is_direct_apply_ack(Some("req-1"), ""));
        assert!(!is_direct_apply_ack(Some("req-1"), "req-2"));
    }
}

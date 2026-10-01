//! Native navigation presentation; Fresh and the daemon own LSP and file IO.
use super::*;
use fresh_gui_client::navigation::{EditorLocation, NavigationHistory};
use fresh_gui_protocol::{CAP_LSP_NAVIGATION, LspNavigationTarget};

pub(crate) fn navigation_feature(feature: LspRequestFeature) -> bool {
    matches!(
        feature,
        LspRequestFeature::Definition
            | LspRequestFeature::Declaration
            | LspRequestFeature::TypeDefinition
            | LspRequestFeature::Implementation
            | LspRequestFeature::References
            | LspRequestFeature::DocumentSymbols
            | LspRequestFeature::WorkspaceSymbols
    )
}

#[derive(Default)]
pub(super) struct Navigation {
    generation: u64,
    open: bool,
    feature: Option<LspRequestFeature>,
    source: Option<Entity<EditorPanel>>,
    targets: Vec<LspNavigationTarget>,
    accepted: Option<(fresh_gui_protocol::LspResult, String, usize)>,
    origin: Option<EditorLocation>,
    pending: HashMap<String, PendingLocation>,
    histories: HashMap<String, NavigationHistory>,
    status: String,
}

struct PendingLocation {
    origin: Option<EditorLocation>,
    offset: Option<usize>,
    workspace: String,
}

impl Navigation {
    pub(super) fn is_open(&self) -> bool {
        self.open
    }
}

impl Workspace {
    fn current_location(&self, cx: &App) -> Option<EditorLocation> {
        let Some(ActiveSurface::Editor(path)) = &self.active else {
            return None;
        };
        self.editors
            .get(path)
            .map(|panel| panel.read(cx).navigation_location(cx))
    }

    pub(super) fn dismiss_navigation(&mut self, cx: &mut Context<Self>) {
        self.navigation.generation = self.navigation.generation.wrapping_add(1);
        self.navigation.open = false;
        self.navigation.targets.clear();
        self.navigation.accepted = None;
        if let Some(panel) = self.navigation.source.take() {
            panel.update(cx, |panel, _| panel.cancel_navigation());
        }
        cx.notify();
    }

    fn cancel_navigation_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let source = self.navigation.source.clone();
        self.dismiss_navigation(cx);
        if let Some(panel) = source {
            panel.update(cx, |panel, cx| panel.focus_navigation_source(window, cx));
        }
        self.status = "Navigation cancelled".into();
    }

    pub(super) fn forget_navigation_workspace(&mut self, workspace: &str) {
        self.navigation.histories.remove(workspace);
    }

    fn start_navigation(
        &mut self,
        feature: LspRequestFeature,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dismiss_navigation(cx);
        if !self
            .capabilities
            .iter()
            .any(|cap| cap == CAP_LSP_NAVIGATION)
        {
            self.status = "Symbol navigation requires a daemon with lsp.navigation; upgrade and restart the daemon".into();
            return;
        }
        let Some(ActiveSurface::Editor(path)) = &self.active else {
            self.status = "Select an editor to navigate symbols".into();
            return;
        };
        let Some(panel) = self.editors.get(path).cloned() else {
            return;
        };
        let (text, offset) = panel.read(cx).navigation_snapshot(cx);
        if !panel.read(cx).lsp_snapshot_matches(&text, Some(offset), cx) {
            self.status = "Symbol requests are unavailable for paged, disconnected, or unsynchronized editors".into();
            return;
        }
        self.navigation.open = true;
        self.navigation.feature = Some(feature);
        self.navigation.origin = Some(panel.read(cx).navigation_location(cx));
        self.navigation.source = Some(panel);
        self.navigation.status = "Loading…".into();
        self.palette_open = false;
        self.goto_open = false;
        self.navigation_state.update(cx, |state, cx| {
            state.set_query("", window, cx);
            state.focus(window, cx);
        });
        self.request_navigation(String::new(), false, cx);
        cx.notify();
    }

    /// Query changes cancel the preceding bridge request before the debounce.
    fn request_navigation(&mut self, query: String, debounce: bool, cx: &mut Context<Self>) {
        if !self.navigation.open {
            return;
        }
        let Some(panel) = self.navigation.source.clone() else {
            return;
        };
        let Some(feature) = self.navigation.feature else {
            return;
        };
        panel.update(cx, |panel, _| panel.cancel_navigation());
        self.navigation.generation = self.navigation.generation.wrapping_add(1);
        let generation = self.navigation.generation;
        self.navigation.targets.clear();
        self.navigation.accepted = None;
        self.navigation.status = "Loading…".into();
        let workspace = self.workspace_id_or_empty();
        let timer = cx
            .background_executor()
            .timer(std::time::Duration::from_millis(if debounce {
                180
            } else {
                0
            }));
        cx.spawn(async move |this, cx| {
            timer.await;
            let request = this
                .update(cx, |this, cx| {
                    if !this.navigation.open
                        || this.navigation.generation != generation
                        || this.workspace_id_or_empty() != workspace
                    {
                        return None;
                    }
                    if this.current_location(cx).is_none_or(|location| {
                        location.view_id != panel.read(cx).navigation_location(cx).view_id
                    }) {
                        this.navigation.status =
                            "Navigation cancelled because the active editor changed".into();
                        cx.notify();
                        return None;
                    }
                    this.navigation.origin = Some(panel.read(cx).navigation_location(cx));
                    let (text, offset) = panel.read(cx).navigation_snapshot(cx);
                    let receiver = panel.update(cx, |panel, cx| {
                        panel.queue_lsp_payload(
                            feature,
                            offset,
                            None,
                            text.clone(),
                            Some(serde_json::json!({"query":query})),
                            cx,
                        )
                    });
                    Some((receiver, text, offset))
                })
                .ok()
                .flatten();
            let Some((receiver, text, offset)) = request else {
                return;
            };
            let result = receiver.recv().await;
            let _ = this.update(cx, |this, cx| {
                if !this.navigation.open
                    || this.navigation.generation != generation
                    || this.workspace_id_or_empty() != workspace
                {
                    return;
                }
                match result {
                    Ok(result)
                        if panel
                            .read(cx)
                            .lsp_result_matches(&result, &text, Some(offset), cx)
                            && this.current_location(cx).is_some_and(|location| {
                                location.view_id == panel.read(cx).navigation_location(cx).view_id
                            }) =>
                    {
                        this.navigation.accepted = Some((result.clone(), text.clone(), offset));
                        this.navigation.targets = result.navigation_targets;
                        this.navigation.status = result.status.unwrap_or_else(|| {
                            if this.navigation.targets.is_empty() {
                                "No destinations found".into()
                            } else {
                                format!("{} destinations", this.navigation.targets.len())
                            }
                        });
                        // Symbol lists and references always remain browsable.
                        if this.navigation.targets.len() == 1
                            && !matches!(
                                feature,
                                LspRequestFeature::References
                                    | LspRequestFeature::DocumentSymbols
                                    | LspRequestFeature::WorkspaceSymbols
                            )
                        {
                            let target = this.navigation.targets[0].clone();
                            this.open_navigation_target(target, cx);
                        }
                    }
                    Ok(_) => {
                        this.navigation.status =
                            "Navigation cancelled because the editor changed".into()
                    }
                    Err(_) => this.navigation.status = "Navigation cancelled or timed out".into(),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn open_navigation_target(&mut self, target: LspNavigationTarget, cx: &mut Context<Self>) {
        let current = self
            .navigation
            .source
            .as_ref()
            .zip(self.navigation.accepted.as_ref())
            .is_some_and(|(panel, (result, text, offset))| {
                panel
                    .read(cx)
                    .lsp_result_matches(result, text, Some(*offset), cx)
                    && self.current_location(cx).is_some_and(|location| {
                        location.view_id == panel.read(cx).navigation_location(cx).view_id
                    })
            });
        if !current {
            self.navigation.targets.clear();
            self.navigation.status = "Navigation cancelled because the editor changed".into();
            cx.notify();
            return;
        }
        let request_id = next_id("nav-open");
        self.navigation.pending.insert(
            request_id.clone(),
            PendingLocation {
                origin: self.navigation.origin.clone(),
                offset: None,
                workspace: self.workspace_id_or_empty(),
            },
        );
        self.pending_editors.insert(request_id.clone(), true);
        self.ade.send(AdeCmd::OpenLocation {
            request_id,
            uri: target.uri,
            line: target.line,
            character: target.character,
        });
        self.dismiss_navigation(cx);
    }

    pub(super) fn location_opened(
        &mut self,
        request_id: &str,
        buffer_id: &str,
        path: &str,
        offset: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(pending) = self.navigation.pending.remove(request_id) else {
            return;
        };
        if pending.workspace != self.workspace_id_or_empty() {
            return;
        }
        if let Some(panel) = self.editor_by_buffer(buffer_id, cx) {
            panel.update(cx, |panel, cx| panel.reveal_byte(offset, window, cx));
            let mut destination = panel.read(cx).navigation_location(cx);
            destination.path = path.to_string();
            destination.offset = offset;
            if let Some(origin) = pending.origin {
                self.navigation
                    .histories
                    .entry(pending.workspace)
                    .or_default()
                    .record_jump(origin, destination);
            }
        }
    }

    pub(super) fn history_editor_opened(
        &mut self,
        request_id: &str,
        path: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(offset) = self
            .navigation
            .pending
            .get(request_id)
            .and_then(|pending| pending.offset)
        {
            if let Some(panel) = self.editors.get(path) {
                panel.update(cx, |panel, cx| panel.reveal_byte(offset, window, cx));
            }
            self.navigation.pending.remove(request_id);
        }
    }

    fn navigate_history(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.dismiss_navigation(cx);
        let Some(current) = self.current_location(cx) else {
            return;
        };
        let workspace = self.workspace_id_or_empty();
        let history = self
            .navigation
            .histories
            .entry(workspace.clone())
            .or_default();
        let destination = if forward {
            history.forward(current)
        } else {
            history.back(current)
        };
        let Some(destination) = destination else {
            self.status = "No further navigation history".into();
            return;
        };
        let panel = self
            .editors
            .values()
            .find(|panel| panel.read(cx).navigation_location(cx).view_id == destination.view_id)
            .cloned()
            .or_else(|| self.editors.get(&destination.path).cloned());
        if let Some(panel) = panel {
            self.select_entity(&panel, window, cx);
            panel.update(cx, |panel, cx| {
                panel.reveal_byte(destination.offset, window, cx)
            });
        } else {
            let request_id = next_id("nav-history");
            self.navigation.pending.insert(
                request_id.clone(),
                PendingLocation {
                    origin: None,
                    offset: Some(destination.offset),
                    workspace,
                },
            );
            self.pending_editors.insert(request_id.clone(), true);
            self.ade.send(AdeCmd::OpenEditor {
                request_id,
                path: destination.path,
                preview: false,
                line: None,
                column: None,
            });
        }
        cx.notify();
    }

    pub(super) fn clear_navigation_pending(&mut self, cx: &mut Context<Self>) {
        self.dismiss_navigation(cx);
        self.navigation.pending.clear();
    }

    pub(super) fn navigation_open_error(&mut self, message: &str) {
        if let Some((request_id, _)) = split_request_message(message) {
            self.navigation.pending.remove(request_id);
            self.pending_editors.remove(request_id);
        }
    }

    pub(super) fn on_definition(
        &mut self,
        _: &GoToDefinition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_navigation(LspRequestFeature::Definition, window, cx);
    }
    pub(super) fn on_declaration(
        &mut self,
        _: &GoToDeclaration,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_navigation(LspRequestFeature::Declaration, window, cx);
    }
    pub(super) fn on_type_definition(
        &mut self,
        _: &GoToTypeDefinition,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_navigation(LspRequestFeature::TypeDefinition, window, cx);
    }
    pub(super) fn on_implementation(
        &mut self,
        _: &GoToImplementation,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_navigation(LspRequestFeature::Implementation, window, cx);
    }
    pub(super) fn on_references(
        &mut self,
        _: &FindReferences,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_navigation(LspRequestFeature::References, window, cx);
    }
    pub(super) fn on_document_symbols(
        &mut self,
        _: &DocumentSymbols,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_navigation(LspRequestFeature::DocumentSymbols, window, cx);
    }
    pub(super) fn on_workspace_symbols(
        &mut self,
        _: &WorkspaceSymbols,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_navigation(LspRequestFeature::WorkspaceSymbols, window, cx);
    }
    pub(super) fn on_navigate_back(
        &mut self,
        _: &NavigateBack,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_history(false, window, cx);
    }
    pub(super) fn on_navigate_forward(
        &mut self,
        _: &NavigateForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_history(true, window, cx);
    }

    pub(super) fn render_navigation(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let workspace_symbols =
            self.navigation.feature == Some(LspRequestFeature::WorkspaceSymbols);
        let targets = self.navigation.targets.clone();
        let generation = self.navigation.generation;
        let view = cx.entity();
        let query_view = view.clone();
        let cancel = view.clone();
        div()
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key.eq_ignore_ascii_case("escape") {
                    this.cancel_navigation_picker(window, cx);
                    cx.stop_propagation();
                }
            }))
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .pt_12()
            .bg(gpui::black().opacity(0.3))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| this.cancel_navigation_picker(window, cx)),
            )
            .child(
                v_flex()
                    .w(self.ui_px(680.))
                    .max_h(self.ui_px(540.))
                    .gap_2()
                    .p_2()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        h_flex()
                            .gap_2()
                            .items_center()
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .child(self.navigation.status.clone()),
                            )
                            .child(
                                Button::new("cancel-navigation")
                                    .ghost()
                                    .small()
                                    .label("Cancel")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.cancel_navigation_picker(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        Command::new(&self.navigation_state)
                            .bordered(true)
                            .placeholder(if workspace_symbols {
                                "Search workspace symbols…"
                            } else {
                                "Filter destinations…"
                            })
                            .filterable(!workspace_symbols)
                            .items(self.navigation.targets.iter().map(|target| {
                                CommandItem::new().label(format!(
                                    "{} — {}:{}:{}",
                                    target.name.as_deref().unwrap_or("Location"),
                                    target.uri,
                                    target.line.saturating_add(1),
                                    target.character.saturating_add(1)
                                ))
                            }))
                            .on_query(move |query, _, cx| {
                                if workspace_symbols {
                                    query_view.update(cx, |this, cx| {
                                        if this.navigation.feature
                                            == Some(LspRequestFeature::WorkspaceSymbols)
                                        {
                                            this.request_navigation(query.to_string(), true, cx);
                                        }
                                    });
                                }
                            })
                            .on_confirm(move |index, _, cx| {
                                view.update(cx, |this, cx| {
                                    if this.navigation.generation == generation
                                        && let Some(target) = targets.get(index.row).cloned()
                                    {
                                        this.open_navigation_target(target, cx);
                                    }
                                });
                            })
                            .on_cancel(move |window, cx| {
                                cancel.update(cx, |this, cx| {
                                    this.cancel_navigation_picker(window, cx)
                                });
                            }),
                    ),
            )
    }
}

//! Fresh-backed search presentation. Matching runs on ADE; reviewed text changes
//! use EditorPanel's revisioned range-edit synchronizer.
use super::*;
use crate::gui::actions::{
    ClearSearchHighlights, NextSearchMatch, PreviousSearchMatch, QueryReplace,
};
use fresh_gui_client::search::{SearchReview, apply_matches};
use fresh_gui_protocol::{Message, SearchMatch, SearchOptions};

pub(super) struct SearchUi {
    pub open: bool,
    replace: bool,
    query: Entity<InputState>,
    replacement: Entity<InputState>,
    options: SearchOptions,
    scope: Option<ByteRange>,
    scope_text: String,
    selection_candidate: Option<ByteRange>,
    matches: Vec<SearchMatch>,
    current: Option<usize>,
    result_text: String,
    pending: Option<(String, String)>,
    error: Option<String>,
    capped: bool,
    pub review: Option<SearchReview>,
    decorations: TextDecorationCollection,
    refresh_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SearchUi {
    pub fn new(
        editor: &Entity<EditorState>,
        window: &mut Window,
        cx: &mut Context<EditorPanel>,
    ) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Find"));
        let replacement =
            cx.new(|cx| InputState::new(window, cx).placeholder("Replace with (empty deletes)"));
        let subscriptions = [&query, &replacement]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.refresh_search(cx);
                    }
                })
            })
            .collect();
        let decorations = editor.update(cx, |state, cx| {
            state.create_decorations_collection(Vec::new(), cx)
        });
        Self {
            open: false,
            replace: false,
            query,
            replacement,
            options: SearchOptions::default(),
            scope: None,
            scope_text: String::new(),
            selection_candidate: None,
            matches: Vec::new(),
            current: None,
            result_text: String::new(),
            pending: None,
            error: None,
            capped: false,
            review: None,
            decorations,
            refresh_task: None,
            _subscriptions: subscriptions,
        }
    }
}

impl EditorPanel {
    pub fn configure_search(&mut self, options: Option<SearchOptions>, cx: &mut Context<Self>) {
        let options = options.unwrap_or_default();
        if self.search.options != options {
            self.search.options = options;
            if self.search.review.is_none() {
                self.refresh_search(cx);
            }
        }
    }

    pub(super) fn reset_search_scope(&mut self, cx: &mut Context<Self>) {
        self.search.scope = None;
        self.search.selection_candidate = None;
        self.search.review = None;
        self.refresh_search(cx);
    }

    pub fn open_search(&mut self, replace: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.review.is_some() {
            self.review_decision('q', window, cx);
        }
        self.commit_markdown_inline_edit(window, cx);
        let selection = self.byte_selection(cx);
        let start = selection.anchor.min(selection.head);
        let end = selection.anchor.max(selection.head);
        self.search.selection_candidate = (start != end).then_some(ByteRange {
            start,
            len: end - start,
        });
        self.markdown_preview = false;
        self.search.open = true;
        self.search.replace = replace;
        self.editor.update(cx, |state, cx| state.close_search(cx));
        if self.search.query.read(cx).value().is_empty() {
            let selected = self.editor.read(cx).selected_value().to_string();
            if !selected.is_empty() && !selected.contains('\n') {
                self.search
                    .query
                    .update(cx, |input, cx| input.set_value(selected, window, cx));
            }
        }
        self.search
            .query
            .update(cx, |input, cx| input.focus(window, cx));
        self.refresh_search(cx);
        cx.notify();
    }

    /// Invalidate immediately, then debounce requests. A stale result cannot
    /// enable replacement while the text or query is being edited.
    pub(super) fn refresh_search(&mut self, cx: &mut Context<Self>) {
        let text = self.current_text(cx);
        if let Some(scope) = self.search.scope
            && self.search.scope_text != text
        {
            let selection = map_selection_through_edits(
                &self.search.scope_text,
                &text,
                ByteSelection {
                    anchor: scope.start,
                    head: scope.start + scope.len,
                },
            );
            self.search.scope = Some(ByteRange {
                start: selection.anchor,
                len: selection.head.saturating_sub(selection.anchor),
            });
        }
        self.search.scope_text = text;
        self.search.pending = None;
        self.search.error = None;
        self.search.matches.clear();
        self.search.current = None;
        self.search.decorations.set(Vec::new(), cx);
        if self.search.review.is_some() {
            self.search.review = None;
            self.search.error = Some("Draft changed during review; no replacements applied".into());
        }
        self.search.refresh_task = None;
        if self.search.query.read(cx).value().is_empty() {
            cx.notify();
            return;
        }
        let timer = cx
            .background_executor()
            .timer(std::time::Duration::from_millis(100));
        self.search.refresh_task = Some(cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| this.send_search(cx));
        }));
        cx.notify();
    }

    fn send_search(&mut self, cx: &mut Context<Self>) {
        let text = self.current_text(cx);
        if !self.transport_connected || self.closed {
            self.search.error = Some("Search requires a connected daemon".into());
            cx.notify();
            return;
        }
        let request_id = self.next_edit_request("search");
        self.search.pending = Some((request_id.clone(), text.clone()));
        self.search.error = None;
        self.ade.send(AdeCmd::Search(Message::BufferSearch {
            request_id,
            text,
            query: self.search.query.read(cx).value().to_string(),
            replacement: self.search.replacement.read(cx).value().to_string(),
            options: self.search.options.clone(),
            scope: self.search.scope,
        }));
        cx.notify();
    }

    pub fn apply_search_result(
        &mut self,
        request_id: &str,
        matches: &[SearchMatch],
        error: Option<&str>,
        capped: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self
            .search
            .pending
            .as_ref()
            .is_some_and(|(id, _)| id == request_id)
        {
            return;
        }
        let (_, text) = self.search.pending.take().expect("checked pending search");
        if text != self.current_text(cx) {
            self.refresh_search(cx);
            return;
        }
        self.search.result_text = text;
        self.search.error = error.map(str::to_owned);
        self.search.capped = capped;
        self.search.matches = matches.to_vec();
        let cursor = self.byte_selection(cx).head;
        self.search.current = (!matches.is_empty())
            .then(|| matches.iter().position(|m| m.start >= cursor).unwrap_or(0));
        let color = cx.theme().warning.opacity(0.25);
        self.search.decorations.set(
            matches
                .iter()
                .map(|m| {
                    TextDecoration::new(
                        m.start..m.end,
                        gpui::HighlightStyle {
                            background_color: Some(color),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            cx,
        );
        if self.search.open && !self.editor.read(cx).focus_handle(cx).is_focused(window) {
            self.reveal_search_match(window, cx);
        }
        cx.notify();
    }

    fn reveal_search_match(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let matched = self
            .search
            .review
            .as_ref()
            .and_then(SearchReview::current)
            .or_else(|| self.search.current.and_then(|i| self.search.matches.get(i)));
        if let Some(matched) = matched {
            let range = matched.start..matched.end;
            // set_selected_range requests the editor to reveal the cursor and
            // preserves the toolbar's focus for incremental typing.
            self.editor
                .update(cx, |state, cx| state.set_selected_range(range, cx));
        }
    }

    pub fn navigate_search(&mut self, previous: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.search.result_text != self.current_text(cx)
            || self.search.matches.is_empty()
            || self.search.review.is_some()
        {
            return;
        }
        let len = self.search.matches.len();
        let index = self.search.current.unwrap_or(0);
        self.search.current = Some(if previous {
            (index + len - 1) % len
        } else {
            (index + 1) % len
        });
        self.reveal_search_match(window, cx);
        cx.notify();
    }

    fn search_can_replace(&self, cx: &App) -> bool {
        self.range_edits
            && self.transport_connected
            && !self.closed
            && !self.conflict
            && !self.sync_paused
            && self.page_request.is_none()
            && self.sync_request_id.is_none()
            && self.edit_request_id.is_none()
            && self.save_request_id.is_none()
            && !self.format_inflight
            && !self.action_inflight
            && self.external_request.is_none()
            && self.search.pending.is_none()
            && self.search.error.is_none()
            && !self.search.capped
            && !self.search.matches.is_empty()
            && self.search.result_text == self.current_text(cx)
            && self
                .edit_sync
                .as_ref()
                .is_some_and(|sync| sync.acknowledged().0 == self.current_text(cx))
    }

    fn commit_search_text(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        if text == self.current_text(cx) {
            return;
        }
        let selection =
            map_selection_through_edits(&self.current_text(cx), &text, self.byte_selection(cx));
        self.set_editor_text_and_selection(&text, Some(selection), window, cx);
        self.dirty = true;
        // Flush immediately: one range transaction, separated from later typing.
        self.flush_pending(cx);
        self.refresh_search(cx);
    }

    fn replace_search(&mut self, all: bool, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_can_replace(cx) || self.search.review.is_some() {
            return;
        }
        let matches = if all {
            self.search.matches.clone()
        } else {
            self.search
                .current
                .and_then(|i| self.search.matches.get(i))
                .cloned()
                .into_iter()
                .collect()
        };
        if let Some(text) = apply_matches(&self.search.result_text, &matches) {
            self.commit_search_text(text, window, cx);
        }
    }

    fn start_search_review(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.search_can_replace(cx) {
            return;
        }
        self.search.review = Some(SearchReview::new(
            self.search.result_text.clone(),
            self.search.matches.clone(),
        ));
        self.reveal_search_match(window, cx);
        self.editor.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    fn review_decision(&mut self, decision: char, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = self.search.review.as_mut() else {
            return;
        };
        match decision {
            'y' => review.accept(),
            'n' => review.skip(),
            '!' => review.accept_all(),
            'q' => {}
            _ => return,
        }
        if decision == 'q' || review.is_done() {
            let review = self.search.review.take().expect("review exists");
            if review.matches_text(&self.current_text(cx)) {
                if let Some(text) = review.replacements() {
                    self.commit_search_text(text, window, cx);
                }
            } else {
                self.search.error =
                    Some("Draft changed; reviewed replacements were not applied".into());
            }
        } else {
            self.reveal_search_match(window, cx);
        }
        cx.notify();
    }

    pub fn clear_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.review_decision('q', window, cx);
        self.search.open = false;
        self.search.pending = None;
        self.search.refresh_task = None;
        self.search.matches.clear();
        self.search.current = None;
        self.search.error = None;
        self.search
            .query
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.search.decorations.set(Vec::new(), cx);
        self.editor.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    fn save_search_options(&mut self, reset: bool, cx: &mut Context<Self>) {
        if reset {
            self.search.options = SearchOptions::default();
        }
        let options = (!reset).then(|| self.search.options.clone());
        let workspace = self.workspace.clone();
        // Updating every pane while this pane is borrowed would re-enter GPUI.
        cx.defer(move |cx| {
            let _ = workspace.update(cx, |ws, cx| ws.set_search_options(options, cx));
        });
        self.refresh_search(cx);
    }

    pub(super) fn on_query_replace(
        &mut self,
        _: &QueryReplace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_search(true, window, cx);
        cx.stop_propagation();
    }
    pub(super) fn on_clear_search(
        &mut self,
        _: &ClearSearchHighlights,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.clear_search(window, cx);
        cx.stop_propagation();
    }
    pub(super) fn on_next_match(
        &mut self,
        _: &NextSearchMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_search(false, window, cx);
        cx.stop_propagation();
    }
    pub(super) fn on_previous_match(
        &mut self,
        _: &PreviousSearchMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.navigate_search(true, window, cx);
        cx.stop_propagation();
    }

    pub(super) fn search_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        if self.search.review.is_some() {
            let decision = if event.keystroke.key_char.as_deref() == Some("!") {
                Some('!')
            } else {
                match key {
                    "y" => Some('y'),
                    "n" => Some('n'),
                    "!" => Some('!'),
                    "q" | "escape" => Some('q'),
                    _ => None,
                }
            };
            if let Some(decision) = decision {
                self.review_decision(decision, window, cx);
                cx.stop_propagation();
            }
        } else if self.search.open
            && (key == "escape" || key == "enter")
            && (self
                .search
                .query
                .read(cx)
                .focus_handle(cx)
                .is_focused(window)
                || self
                    .search
                    .replacement
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window))
        {
            self.search.open = false;
            self.editor.update(cx, |state, cx| state.focus(window, cx));
            cx.stop_propagation();
            cx.notify();
        }
    }

    pub(super) fn render_search(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let reviewing = self.search.review.is_some();
        let can_replace = self.search_can_replace(cx) && !reviewing;
        let current = self
            .search
            .review
            .as_ref()
            .and_then(SearchReview::current_index)
            .or(self.search.current);
        let count = format!(
            "{} / {}{}{}",
            current.map_or(0, |i| i + 1),
            self.search.matches.len(),
            if self.search.capped { "+ (limit)" } else { "" },
            if self.page.is_some() {
                " · loaded page"
            } else {
                ""
            }
        );
        let mut bar = v_flex()
            .w_full()
            .p_2()
            .gap_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .child(Input::new(&self.search.query).small().disabled(reviewing)),
                    )
                    .child(
                        Button::new("search-case")
                            .ghost()
                            .xsmall()
                            .label("Aa")
                            .tooltip("Case sensitive")
                            .selected(self.search.options.case_sensitive)
                            .disabled(reviewing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search.options.case_sensitive =
                                    !this.search.options.case_sensitive;
                                this.save_search_options(false, cx);
                            })),
                    )
                    .child(
                        Button::new("search-word")
                            .ghost()
                            .xsmall()
                            .label("Word")
                            .tooltip("Whole word")
                            .selected(self.search.options.whole_word)
                            .disabled(reviewing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search.options.whole_word = !this.search.options.whole_word;
                                this.save_search_options(false, cx);
                            })),
                    )
                    .child(
                        Button::new("search-regex")
                            .ghost()
                            .xsmall()
                            .label(".*")
                            .tooltip("Regular expression")
                            .selected(self.search.options.use_regex)
                            .disabled(reviewing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search.options.use_regex = !this.search.options.use_regex;
                                this.save_search_options(false, cx);
                            })),
                    )
                    .child(
                        Button::new("search-selection")
                            .ghost()
                            .xsmall()
                            .label("Selection")
                            .selected(self.search.scope.is_some())
                            .disabled(reviewing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this.search.scope.is_some() {
                                    this.search.scope = None;
                                } else {
                                    let candidate = this.search.selection_candidate;
                                    if candidate.is_none() {
                                        this.search.error = Some(
                                            "Select text before enabling selection scope".into(),
                                        );
                                        cx.notify();
                                        return;
                                    }
                                    this.search.scope = candidate;
                                    this.search.scope_text = this.current_text(cx);
                                }
                                this.refresh_search(cx);
                            })),
                    )
                    .child(
                        Button::new("search-defaults")
                            .ghost()
                            .xsmall()
                            .label("Reset options")
                            .disabled(reviewing)
                            .on_click(
                                cx.listener(|this, _, _, cx| this.save_search_options(true, cx)),
                            ),
                    )
                    .child(
                        Button::new("search-close")
                            .ghost()
                            .xsmall()
                            .label("Close")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.review_decision('q', window, cx);
                                this.search.open = false;
                                this.editor.update(cx, |state, cx| state.focus(window, cx));
                                cx.notify();
                            })),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().text_xs().child(count))
                    .child(
                        Button::new("search-prev")
                            .ghost()
                            .xsmall()
                            .label("Previous")
                            .disabled(reviewing)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_search(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("search-next")
                            .ghost()
                            .xsmall()
                            .label("Next")
                            .disabled(reviewing)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_search(false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("search-replace-mode")
                            .ghost()
                            .xsmall()
                            .label("Replace…")
                            .disabled(reviewing)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search.replace = !this.search.replace;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("search-clear")
                            .ghost()
                            .xsmall()
                            .label("Clear highlights")
                            .on_click(
                                cx.listener(|this, _, window, cx| this.clear_search(window, cx)),
                            ),
                    ),
            );
        if self.search.replace {
            bar = bar.child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div().flex_1().child(
                            Input::new(&self.search.replacement)
                                .small()
                                .disabled(reviewing),
                        ),
                    )
                    .child(
                        Button::new("search-replace-one")
                            .ghost()
                            .xsmall()
                            .label("Replace")
                            .disabled(!can_replace)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.replace_search(false, window, cx)
                            })),
                    )
                    .child(
                        Button::new("search-replace-all")
                            .ghost()
                            .xsmall()
                            .label("Replace all")
                            .disabled(!can_replace)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.replace_search(true, window, cx)
                            })),
                    )
                    .child(
                        Button::new("search-review")
                            .ghost()
                            .xsmall()
                            .label("Query replace")
                            .disabled(!can_replace)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.start_search_review(window, cx)
                            })),
                    ),
            );
        }
        if reviewing {
            bar = bar.child(
                h_flex()
                    .gap_1()
                    .child(
                        div()
                            .text_xs()
                            .child("Review: y accept · n skip · ! all · q cancel"),
                    )
                    .child(
                        Button::new("query-accept")
                            .ghost()
                            .xsmall()
                            .label("Accept")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.review_decision('y', window, cx)
                            })),
                    )
                    .child(
                        Button::new("query-skip")
                            .ghost()
                            .xsmall()
                            .label("Skip")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.review_decision('n', window, cx)
                            })),
                    )
                    .child(
                        Button::new("query-all")
                            .ghost()
                            .xsmall()
                            .label("All")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.review_decision('!', window, cx)
                            })),
                    )
                    .child(
                        Button::new("query-cancel")
                            .ghost()
                            .xsmall()
                            .label("Cancel")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.review_decision('q', window, cx)
                            })),
                    ),
            );
        }
        if self.search.pending.is_some() {
            bar = bar.child(div().text_xs().child("Searching…"));
        }
        if !self.range_edits && self.search.replace {
            bar = bar.child(
                div()
                    .text_xs()
                    .child("Replacement requires a daemon with editor.range-edits"),
            );
        }
        if let Some(error) = &self.search.error {
            bar = bar.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.clone()),
            );
        }
        bar
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use gpui::TestAppContext;

    fn matched(start: usize, end: usize, replacement: &str) -> SearchMatch {
        SearchMatch {
            start,
            end,
            replacement: replacement.into(),
        }
    }

    #[gpui::test]
    fn stale_search_responses_and_changed_drafts_cannot_replace(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (workspace, test_cx) = cx.add_window_view(|window, cx| {
            Workspace::new(
                crate::gui::connect::parse_connect_target("ws://", None),
                window,
                cx,
            )
        });
        let (ade, commands) = AdeHandle::test_channel();
        let panel = test_cx.update(|window, cx| {
            cx.new(|cx| {
                EditorPanel::new(
                    "buffer".into(),
                    "test.txt".into(),
                    None,
                    None,
                    None,
                    true,
                    None,
                    ade,
                    workspace.downgrade(),
                    TabStripMetrics::default(),
                    window,
                    cx,
                )
            })
        });
        panel.update_in(test_cx, |panel, window, cx| {
            panel.set_editor_text_and_selection("é cat cat", None, window, cx);
            panel.configure_range_edits(true, cx);
            panel.edit_sync = Some(EditSync::new("é cat cat".into(), 4));
            panel.search.open = true;
            panel.search.pending = Some(("new-search".into(), "é cat cat".into()));
            panel.apply_search_result(
                "old-search",
                &[matched(3, 6, "wrong")],
                None,
                false,
                window,
                cx,
            );
            assert!(panel.search.matches.is_empty());
            assert_eq!(panel.search.pending.as_ref().unwrap().0, "new-search");
            panel.apply_search_result(
                "new-search",
                &[matched(3, 6, "dog"), matched(7, 10, "dog")],
                None,
                false,
                window,
                cx,
            );
            assert_eq!(panel.editor.read(cx).selected_range(), 3..6);
            panel.navigate_search(true, window, cx);
            assert_eq!(panel.editor.read(cx).selected_range(), 7..10);
            assert!(panel.search_can_replace(cx));
            panel.set_editor_text_and_selection("new typing", None, window, cx);
            panel.replace_search(true, window, cx);
            assert_eq!(panel.current_text(cx), "new typing");
            assert!(commands.try_recv().is_err());
        });
    }

    #[gpui::test]
    fn query_review_commits_accepted_deletions_in_one_range_transaction(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (workspace, test_cx) = cx.add_window_view(|window, cx| {
            Workspace::new(
                crate::gui::connect::parse_connect_target("ws://", None),
                window,
                cx,
            )
        });
        let (ade, commands) = AdeHandle::test_channel();
        let panel = test_cx.update(|window, cx| {
            cx.new(|cx| {
                EditorPanel::new(
                    "buffer".into(),
                    "test.txt".into(),
                    None,
                    None,
                    None,
                    true,
                    None,
                    ade,
                    workspace.downgrade(),
                    TabStripMetrics::default(),
                    window,
                    cx,
                )
            })
        });
        panel.update_in(test_cx, |panel, window, cx| {
            panel.set_editor_text_and_selection("cat é cat cat", None, window, cx);
            panel.range_edits = true;
            panel.edit_sync = Some(EditSync::new("cat é cat cat".into(), 4));
            panel.search.pending = Some(("review".into(), "cat é cat cat".into()));
            panel.apply_search_result(
                "review",
                &[matched(0, 3, ""), matched(7, 10, ""), matched(11, 14, "")],
                None,
                false,
                window,
                cx,
            );
            panel.start_search_review(window, cx);
            panel.review_decision('y', window, cx);
            panel.review_decision('n', window, cx);
            assert_eq!(panel.current_text(cx), "cat é cat cat");
            assert!(commands.try_recv().is_err());
            panel.review_decision('q', window, cx);
            assert_eq!(panel.current_text(cx), " é cat cat");
            assert!(panel.search.review.is_none());
            let AdeCmd::RangeEdit {
                base_rev, edits, ..
            } = commands.try_recv().expect("one replacement transaction")
            else {
                panic!("expected range edits")
            };
            assert_eq!(base_rev, 4);
            assert_eq!(edits.len(), 1);
            assert_eq!(
                (edits[0].start, edits[0].end, edits[0].text.as_str()),
                (0, 3, "")
            );
            assert!(commands.try_recv().is_err());
        });
    }

    #[gpui::test]
    fn selection_scope_survives_search_selection_and_resets_on_page_change(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (workspace, test_cx) = cx.add_window_view(|window, cx| {
            Workspace::new(
                crate::gui::connect::parse_connect_target("ws://", None),
                window,
                cx,
            )
        });
        let (ade, _) = AdeHandle::test_channel();
        let panel = test_cx.update(|window, cx| {
            cx.new(|cx| {
                EditorPanel::new(
                    "buffer".into(),
                    "test.txt".into(),
                    None,
                    None,
                    None,
                    true,
                    None,
                    ade,
                    workspace.downgrade(),
                    TabStripMetrics::default(),
                    window,
                    cx,
                )
            })
        });
        panel.update_in(test_cx, |panel, window, cx| {
            panel.set_editor_text_and_selection(
                "cat é cat",
                Some(ByteSelection { anchor: 0, head: 6 }),
                window,
                cx,
            );
            panel.open_search(false, window, cx);
            assert_eq!(
                panel.search.selection_candidate,
                Some(ByteRange { start: 0, len: 6 })
            );
            panel.search.scope = panel.search.selection_candidate;
            panel.search.scope_text = panel.current_text(cx);
            panel.search.pending = Some(("selection".into(), panel.current_text(cx)));
            panel.apply_search_result(
                "selection",
                &[matched(0, 3, "dog")],
                None,
                false,
                window,
                cx,
            );
            assert_eq!(panel.editor.read(cx).selected_range(), 0..3);
            assert_eq!(panel.search.scope, Some(ByteRange { start: 0, len: 6 }));
            panel.reset_search_scope(cx);
            assert_eq!(panel.search.scope, None);
            assert_eq!(panel.search.selection_candidate, None);
            assert!(panel.search.matches.is_empty());
            panel.clear_search(window, cx);
            assert!(!panel.search.open);
            assert!(panel.search.query.read(cx).value().is_empty());
        });
    }
}

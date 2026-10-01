//! Workspace-wide search presentation. Search and edits are owned by ADE;
//! this view only gathers criteria and presents streamed match groups.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};

use fresh_gui_protocol::{
    Message, ProjectSearchFile, ProjectSearchRequest, ProjectSearchSelection, SearchOptions,
};
use gpui_kit::base::{Disableable as _, Selectable as _};
use gpui_kit::component::{
    ActiveTheme, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement as _,
    v_flex,
};
use gpui_kit::*;

static NEXT_SEARCH_ID: AtomicU64 = AtomicU64::new(1);
const PROJECT_SEARCH_LIMIT: usize = 2_000;

fn build_selections(selected: &HashMap<String, HashSet<usize>>) -> Vec<ProjectSearchSelection> {
    selected
        .iter()
        .filter(|(_, indices)| !indices.is_empty())
        .map(|(file_id, indices)| {
            let mut match_indices = indices.iter().copied().collect::<Vec<_>>();
            match_indices.sort_unstable();
            ProjectSearchSelection {
                file_id: file_id.clone(),
                match_indices,
            }
        })
        .collect()
}

#[derive(Debug, Clone)]
pub enum ProjectSearchEvent {
    Search {
        request_id: String,
        search: ProjectSearchRequest,
    },
    Cancel {
        request_id: String,
    },
    Apply {
        request_id: String,
        search_id: String,
        selections: Vec<ProjectSearchSelection>,
    },
    OpenMatch {
        path: Option<String>,
        buffer_id: Option<String>,
        draft_id: Option<String>,
        start: usize,
        end: usize,
        line: u32,
        column: u32,
    },
    Close,
}

#[derive(Clone)]
struct ResultGroup {
    file: ProjectSearchFile,
}

pub struct ProjectSearchPanel {
    root: String,
    supported: bool,
    query: Entity<InputState>,
    replacement: Entity<InputState>,
    globs: Entity<InputState>,
    options: SearchOptions,
    include_ignored: bool,
    running: bool,
    search_complete: bool,
    request_id: Option<String>,
    replace_request_id: Option<String>,
    results: Vec<ResultGroup>,
    selected: HashMap<String, HashSet<usize>>,
    status: Option<String>,
    truncated: bool,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<ProjectSearchEvent> for ProjectSearchPanel {}

impl ProjectSearchPanel {
    pub fn new(root: String, supported: bool, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search in workspace"));
        let replacement =
            cx.new(|cx| InputState::new(window, cx).placeholder("Replace with (empty deletes)"));
        let globs = cx
            .new(|cx| InputState::new(window, cx).placeholder("Files to include (glob patterns)"));
        let subscriptions = [&query, &replacement, &globs]
            .into_iter()
            .map(|input| {
                cx.subscribe(input, |this, _, event: &InputEvent, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.invalidate_preview(cx);
                    }
                })
            })
            .collect();
        Self {
            root,
            supported,
            query,
            replacement,
            globs,
            options: SearchOptions::default(),
            include_ignored: false,
            running: false,
            search_complete: false,
            request_id: None,
            replace_request_id: None,
            results: Vec::new(),
            selected: HashMap::new(),
            status: (!supported)
                .then(|| "Project search requires a daemon with project.search.v1".into()),
            truncated: false,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_error(&mut self, error: impl Into<String>, cx: &mut Context<Self>) {
        self.status = Some(error.into());
        self.running = false;
        self.search_complete = false;
        self.request_id = None;
        self.replace_request_id = None;
        cx.notify();
    }

    fn invalidate_preview(&mut self, cx: &mut Context<Self>) {
        if let Some(request_id) = self.request_id.take() {
            cx.emit(ProjectSearchEvent::Cancel { request_id });
            self.results.clear();
            self.selected.clear();
            self.running = false;
            self.search_complete = false;
            self.status = Some("Search settings changed; run the search again".into());
        }
        cx.notify();
    }

    fn can_apply(&self) -> bool {
        self.supported
            && self.search_complete
            && self.replace_request_id.is_none()
            && self.selected.values().any(|indices| !indices.is_empty())
    }

    /// Consume daemon stream messages. Call only for messages routed to this panel's
    /// currently active request.
    pub fn handle_message(&mut self, message: &Message, cx: &mut Context<Self>) {
        match message {
            Message::ProjectSearchFile { request_id, file }
                if self.request_id.as_deref() == Some(request_id.as_str()) =>
            {
                if let Some(existing) = self
                    .results
                    .iter_mut()
                    .find(|group| group.file.id == file.id)
                {
                    existing.file = file.clone();
                } else {
                    self.results.push(ResultGroup { file: file.clone() });
                }
                cx.notify();
            }
            Message::ProjectSearchDone {
                request_id,
                truncated,
                cancelled,
                warnings,
                error,
            } if self.request_id.as_deref() == Some(request_id.as_str()) => {
                self.running = false;
                self.truncated = *truncated;
                self.search_complete = !*cancelled && error.is_none();
                self.status = error.clone().or_else(|| {
                    let mut messages = warnings.clone();
                    if *cancelled {
                        messages.push("Search cancelled".into());
                    }
                    if *truncated {
                        messages.push("Result limit reached".into());
                    }
                    (!messages.is_empty()).then(|| messages.join(" · "))
                });
                cx.notify();
            }
            Message::ProjectReplaceResult { request_id, files }
                if self.replace_request_id.as_deref() == Some(request_id.as_str()) =>
            {
                self.replace_request_id = None;
                self.search_complete = false;
                let failures = files
                    .iter()
                    .filter_map(|file| {
                        file.error
                            .as_ref()
                            .map(|error| format!("{}: {error}", file.file_id))
                    })
                    .collect::<Vec<_>>();
                self.status = Some(if failures.is_empty() {
                    format!("Applied replacements in {} file(s)", files.len())
                } else {
                    failures.join(" · ")
                });
                self.selected.clear();
                cx.notify();
            }
            _ => {}
        }
    }

    fn begin_search(&mut self, cx: &mut Context<Self>) {
        if !self.supported {
            return;
        }
        let query = self.query.read(cx).value().to_string();
        if query.is_empty() {
            self.status = Some("Enter a search query".into());
            cx.notify();
            return;
        }
        let request_id = format!(
            "project-search-{}",
            NEXT_SEARCH_ID.fetch_add(1, Ordering::Relaxed)
        );
        let globs = self
            .globs
            .read(cx)
            .value()
            .split_whitespace()
            .map(str::to_owned)
            .collect();
        let search = ProjectSearchRequest {
            query,
            replacement: self.replacement.read(cx).value().to_string(),
            options: self.options.clone(),
            globs,
            include_ignored: self.include_ignored,
            max_matches: PROJECT_SEARCH_LIMIT,
        };
        self.request_id = Some(request_id.clone());
        self.results.clear();
        self.selected.clear();
        self.status = None;
        self.truncated = false;
        self.running = true;
        self.search_complete = false;
        cx.emit(ProjectSearchEvent::Search { request_id, search });
        cx.notify();
    }

    pub fn cancel(&mut self, cx: &mut Context<Self>) {
        if let Some(request_id) = self.request_id.clone() {
            cx.emit(ProjectSearchEvent::Cancel { request_id });
            self.running = false;
            self.search_complete = false;
            self.status = Some("Cancelling search…".into());
            cx.notify();
        }
    }

    fn toggle_match(&mut self, file_id: &str, index: usize, cx: &mut Context<Self>) {
        let selected = self.selected.entry(file_id.to_owned()).or_default();
        if !selected.insert(index) {
            selected.remove(&index);
        }
        if selected.is_empty() {
            self.selected.remove(file_id);
        }
        cx.notify();
    }

    fn toggle_file(&mut self, file: &ProjectSearchFile, cx: &mut Context<Self>) {
        let selected_count = self.selected.get(&file.id).map_or(0, HashSet::len);
        if selected_count == file.matches.len() {
            self.selected.remove(&file.id);
        } else {
            self.selected
                .insert(file.id.clone(), (0..file.matches.len()).collect());
        }
        cx.notify();
    }

    fn apply(&mut self, cx: &mut Context<Self>) {
        if !self.can_apply() {
            return;
        }
        let Some(search_id) = self.request_id.clone() else {
            return;
        };
        let selections = build_selections(&self.selected);
        if selections.is_empty() {
            return;
        }
        let request_id = format!(
            "project-replace-{}",
            NEXT_SEARCH_ID.fetch_add(1, Ordering::Relaxed)
        );
        self.replace_request_id = Some(request_id.clone());
        cx.emit(ProjectSearchEvent::Apply {
            request_id,
            search_id,
            selections,
        });
        self.status = Some("Applying selected replacements…".into());
        cx.notify();
    }
}

impl Render for ProjectSearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selected_count = self.selected.values().map(HashSet::len).sum::<usize>();
        let mut view = v_flex()
            .w_full()
            .h_full()
            .gap_2()
            .p_3()
            .bg(cx.theme().background)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_1().text_sm().child("Search in workspace"))
                    .child(
                        Button::new("project-search-run")
                            .label("Search")
                            .disabled(!self.supported || self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.begin_search(cx))),
                    )
                    .child(
                        Button::new("project-search-cancel")
                            .label("Cancel")
                            .disabled(!self.running)
                            .on_click(cx.listener(|this, _, _, cx| this.cancel(cx))),
                    )
                    .child(
                        Button::new("project-search-apply")
                            .label(format!("Replace selected ({selected_count})"))
                            .disabled(!self.can_apply())
                            .on_click(cx.listener(|this, _, _, cx| this.apply(cx))),
                    )
                    .child(Button::new("project-search-close").label("Close").on_click(
                        cx.listener(|this, _, _, cx| {
                            this.cancel(cx);
                            cx.emit(ProjectSearchEvent::Close);
                        }),
                    )),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("Workspace root: {}", self.root)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().child(Input::new(&self.query)))
                    .child(div().flex_1().child(Input::new(&self.replacement))),
            )
            .child(Input::new(&self.globs))
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        Button::new("project-search-case")
                            .ghost()
                            .xsmall()
                            .label("Aa")
                            .selected(self.options.case_sensitive)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.invalidate_preview(cx);
                                this.options.case_sensitive = !this.options.case_sensitive;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("project-search-word")
                            .ghost()
                            .xsmall()
                            .label("Word")
                            .selected(self.options.whole_word)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.invalidate_preview(cx);
                                this.options.whole_word = !this.options.whole_word;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("project-search-regex")
                            .ghost()
                            .xsmall()
                            .label(".*")
                            .selected(self.options.use_regex)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.invalidate_preview(cx);
                                this.options.use_regex = !this.options.use_regex;
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("project-search-ignored")
                            .ghost()
                            .xsmall()
                            .label("Include ignored")
                            .selected(self.include_ignored)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.invalidate_preview(cx);
                                this.include_ignored = !this.include_ignored;
                                cx.notify();
                            })),
                    ),
            )
            .child(div().text_xs().child(format!(
                "{} file(s) · {} selected match(es){}",
                self.results.len(),
                selected_count,
                if self.truncated { " · truncated" } else { "" }
            )));

        if let Some(status) = &self.status {
            view = view.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(status.clone()),
            );
        }
        let mut results = v_flex().w_full().gap_1();
        for group in &self.results {
            let file = group.file.clone();
            let file_id = file.id.clone();
            let all_selected = self
                .selected
                .get(&file_id)
                .is_some_and(|indices| indices.len() == file.matches.len());
            let path = file
                .path
                .clone()
                .or_else(|| file.buffer_id.clone())
                .unwrap_or_else(|| "(unknown buffer)".into());
            let file_for_toggle = file.clone();
            results =
                results.child(
                    v_flex()
                        .w_full()
                        .gap_1()
                        .p_2()
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .child(
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new(format!("project-search-select-file-{file_id}"))
                                        .ghost()
                                        .xsmall()
                                        .label(if all_selected { "✓" } else { "Select file" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_file(&file_for_toggle, cx)
                                        })),
                                )
                                .child(div().text_sm().child(path.clone())),
                        )
                        .children(file.matches.iter().enumerate().map(|(index, result)| {
                            let file_id = group.file.id.clone();
                            let toggle_id = file_id.clone();
                            let checked = self
                                .selected
                                .get(&file_id)
                                .is_some_and(|indices| indices.contains(&index));
                            let open_path = group.file.path.clone();
                            let buffer_id = group.file.buffer_id.clone();
                            let draft_id = group.file.draft_id.clone();
                            let start = result.start;
                            let end = result.end;
                            let line = result.line;
                            let column = result.column;
                            h_flex()
                                .gap_2()
                                .items_center()
                                .child(
                                    Button::new(format!("project-search-select-{file_id}-{index}"))
                                        .ghost()
                                        .xsmall()
                                        .label(if checked { "✓" } else { "○" })
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.toggle_match(&toggle_id, index, cx)
                                        })),
                                )
                                .child(
                                    Button::new(format!("project-search-open-{file_id}-{index}"))
                                        .ghost()
                                        .xsmall()
                                        .label(format!("{}:{}", line, column))
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.cancel(cx);
                                            cx.emit(ProjectSearchEvent::OpenMatch {
                                                path: open_path.clone(),
                                                buffer_id: buffer_id.clone(),
                                                draft_id: draft_id.clone(),
                                                start,
                                                end,
                                                line,
                                                column,
                                            })
                                        })),
                                )
                                .child(div().flex_1().text_xs().child(format!(
                                    "{}  →  {}",
                                    result.preview, result.replacement
                                )))
                        })),
                );
        }
        view.child(
            div()
                .flex_1()
                .w_full()
                .overflow_y_scrollbar()
                .child(results),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use gpui::TestAppContext;

    #[test]
    fn selections_are_sorted_and_empty_files_are_omitted() {
        let selected = HashMap::from([
            ("file-a".into(), HashSet::from([4, 1, 2])),
            ("file-empty".into(), HashSet::new()),
        ]);
        assert_eq!(
            build_selections(&selected),
            vec![ProjectSearchSelection {
                file_id: "file-a".into(),
                match_indices: vec![1, 2, 4],
            }]
        );
    }

    #[gpui::test]
    fn stale_streams_do_not_complete_current_search_and_cancel_disables_apply(
        cx: &mut TestAppContext,
    ) {
        cx.update(gpui_kit::init);
        let (panel, test_cx) = cx.add_window_view(|window, cx| {
            ProjectSearchPanel::new("/workspace".into(), true, window, cx)
        });
        panel.update_in(test_cx, |panel, _, cx| {
            panel.request_id = Some("new".into());
            panel.running = true;
            panel.handle_message(
                &Message::ProjectSearchDone {
                    request_id: "old".into(),
                    truncated: false,
                    cancelled: false,
                    warnings: vec![],
                    error: None,
                },
                cx,
            );
            assert!(panel.running);
            panel.search_complete = true;
            panel.selected.insert("file".into(), HashSet::from([0]));
            assert!(panel.can_apply());
            panel.cancel(cx);
            assert!(!panel.can_apply());
            panel.set_error("preflight failed", cx);
            assert!(!panel.running);
            assert!(!panel.can_apply());
        });
    }
}

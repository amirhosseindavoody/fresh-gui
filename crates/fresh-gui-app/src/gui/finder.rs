//! Native finder presentation. Enumeration/ranking are daemon services.
use super::*;
use fresh_gui_client::navigation::EditorLocation;
use fresh_gui_protocol::{CAP_FILE_FINDER, Message};

#[derive(Default)]
pub(super) struct Finder {
    pub(super) paths: Vec<String>,
    request: Option<String>,
    generation: u64,
    selected: usize,
    origin: Option<EditorLocation>,
    preview: bool,
    preview_valid: bool,
    status: String,
}

impl Workspace {
    fn stop_finder_request(&mut self) {
        self.finder.generation = self.finder.generation.wrapping_add(1);
        if let Some(request_id) = self.finder.request.take() {
            self.ade
                .send(AdeCmd::Project(Message::FileFinderCancel { request_id }));
        }
    }

    pub(super) fn abandon_finder(&mut self) {
        self.stop_finder_request();
        self.finder = Finder {
            generation: self.finder.generation,
            ..Finder::default()
        };
        self.goto_open = false;
    }

    pub(super) fn reset_finder(&mut self, cx: &mut Context<Self>) {
        if self.finder.preview
            && let Some(origin) = &self.finder.origin
            && let Some(panel) = self
                .editors
                .values()
                .find(|panel| panel.read(cx).navigation_location(cx).view_id == origin.view_id)
                .cloned()
        {
            panel.update(cx, |panel, cx| {
                panel.restore_preview_position(origin.offset, cx)
            });
        }
        self.abandon_finder();
    }

    pub(super) fn cancel_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.finder.preview {
            if let Some(origin) = &self.finder.origin
                && let Some(panel) = self
                    .editors
                    .values()
                    .find(|panel| panel.read(cx).navigation_location(cx).view_id == origin.view_id)
                    .cloned()
            {
                panel.update(cx, |panel, cx| panel.reveal_byte(origin.offset, window, cx));
            }
        } else if let Some(location) = self.current_location(cx)
            && let Some(panel) = self.editors.get(&location.path)
        {
            panel.update(cx, |panel, cx| panel.focus_navigation_source(window, cx));
        }
        self.abandon_finder();
        cx.notify();
    }

    pub(super) fn start_finder(
        &mut self,
        prefix: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.cancel_finder(window, cx);
        self.dismiss_navigation(cx);
        self.palette_open = false;
        self.rename_pty = None;
        self.finder.origin = self.current_location(cx);
        self.goto_open = true;
        self.goto_input.update(cx, |state, cx| {
            state.set_value(prefix, window, cx);
            state.focus(window, cx);
        });
        self.finder_changed(window, cx);
        cx.notify();
    }

    pub(super) fn finder_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.stop_finder_request();
        self.finder.selected = 0;
        self.finder.preview_valid = false;
        self.finder.paths.clear();
        let query = self.goto_input.read(cx).value().to_string();
        // Leaving line mode restores its origin before another mode starts.
        if self.finder.preview && !query.starts_with(':') {
            if let Some(origin) = &self.finder.origin
                && let Some(panel) = self.editors.get(&origin.path)
            {
                panel.update(cx, |panel, cx| panel.reveal_byte(origin.offset, window, cx));
                self.goto_input
                    .update(cx, |state, cx| state.focus(window, cx));
            }
            self.finder.preview = false;
        }
        if let Some(line) = query.strip_prefix(':') {
            let panel = self.finder.origin.as_ref().and_then(|origin| {
                self.editors
                    .values()
                    .find(|panel| panel.read(cx).navigation_location(cx).view_id == origin.view_id)
                    .cloned()
            });
            self.finder.status = if let Some(panel) = panel {
                if let Some(preview) = panel.update(cx, |panel, cx| panel.preview_line(line, cx)) {
                    self.finder.preview = true;
                    self.finder.preview_valid = true;
                    preview
                } else {
                    "Enter line[:column]. Line preview requires a fully loaded buffer.".into()
                }
            } else {
                "Select an editor to preview a line".into()
            };
            return;
        }
        if query.starts_with('@') {
            self.finder.status = "Open buffers".into();
            return;
        }
        if let Some(command) = query.strip_prefix('>') {
            self.goto_open = false;
            self.palette_open = true;
            self.command_state.update(cx, |state, cx| {
                state.set_query(command, window, cx);
                state.focus(window, cx);
            });
            return;
        }
        self.request_goto_listing(cx);
        if !self.capabilities.iter().any(|cap| cap == CAP_FILE_FINDER) {
            self.finder.status = "Recursive search requires an upgraded daemon (project.file-finder). Enter an exact path to open it.".into();
            return;
        }
        self.finder.status = "Searching workspace… Exact paths can be opened immediately.".into();
        let generation = self.finder.generation;
        let workspace = self.workspace_id_or_empty();
        let query = goto_completion_path(&query);
        let timer = cx.background_executor().timer(Duration::from_millis(120));
        cx.spawn(async move |this, cx| {
            timer.await;
            let _ = this.update(cx, |this, cx| {
                if !this.goto_open
                    || generation != this.finder.generation
                    || workspace != this.workspace_id_or_empty()
                {
                    return;
                }
                let request_id = next_id("finder");
                this.finder.request = Some(request_id.clone());
                this.ade
                    .send(AdeCmd::Project(Message::FileFinder { request_id, query }));
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn handle_finder_message(&mut self, message: &Message, cx: &mut Context<Self>) {
        if let Message::FileFinderResults {
            request_id,
            paths,
            truncated,
            cancelled,
            error,
        } = message
            && self.goto_open
            && self.finder.request.as_deref() == Some(request_id)
        {
            self.finder.paths = paths.clone();
            self.finder.selected = self.finder.selected.min(paths.len().saturating_sub(1));
            self.finder.status = error.clone().unwrap_or_else(|| {
                if *cancelled {
                    "Search cancelled".into()
                } else {
                    format!(
                        "{} matches{}",
                        paths.len(),
                        if *truncated { " (limited)" } else { "" }
                    )
                }
            });
            cx.notify();
        }
    }

    fn finder_items(&self, cx: &App) -> Vec<String> {
        let query = self.goto_input.read(cx).value().to_string();
        if let Some(query) = query.strip_prefix('@') {
            fresh_gui_client::finder::ranked(query, self.editors.keys().cloned(), |path| {
                path.as_str()
            })
        } else if query.starts_with(':') {
            Vec::new()
        } else {
            self.goto_matches(&query)
        }
    }

    pub(super) fn confirm_finder(
        &mut self,
        chosen: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = self.goto_input.read(cx).value().to_string();
        if query.starts_with(':') {
            if self.finder.preview_valid {
                if let (Some(origin), Some(destination)) =
                    (self.finder.origin.clone(), self.current_location(cx))
                {
                    self.record_location_jump(origin, destination);
                }
                self.finder.preview = false;
                self.cancel_finder(window, cx);
            }
            return;
        }
        if query.starts_with('@') {
            if let Some(path) =
                chosen.or_else(|| self.finder_items(cx).get(self.finder.selected).cloned())
                && let Some(panel) = self.editors.get(&path).cloned()
            {
                let origin = self.finder.origin.clone();
                self.select_entity(&panel, window, cx);
                let destination = panel.read(cx).navigation_location(cx);
                if let Some(origin) = origin {
                    self.record_location_jump(origin, destination);
                }
                self.abandon_finder();
                panel.update(cx, |panel, cx| panel.focus_navigation_source(window, cx));
                cx.notify();
            }
            return;
        }
        let (file, line, column) = parse_goto_spec(&query);
        if file.is_empty() && chosen.is_none() {
            // Empty query selects the first ranked file.
            if self.finder.paths.is_empty() {
                return;
            }
        }
        // Path-shaped input is resolved by Fresh immediately, even while the
        // recursive scan is pending. Bare names select ranked results.
        let explicit =
            goto_is_absolute(&file) || file.starts_with(['.', '~']) || file.contains(['/', '\\']);
        let target = chosen.unwrap_or_else(|| {
            if explicit {
                file.clone()
            } else {
                let items = self.finder_items(cx);
                if file.is_empty() || self.finder.selected > 0 {
                    items
                        .get(self.finder.selected)
                        .cloned()
                        .unwrap_or(file.clone())
                } else {
                    pick_goto_target(&file, &items)
                }
            }
        });
        if self.goto_path_is_dir(&target) {
            self.complete_goto_path(target, window, cx);
            return;
        }
        let path = self.resolve_goto_path(&target);
        self.open_file_location(path, line, column, None, cx);
        self.abandon_finder();
        cx.notify();
    }

    pub(super) fn render_goto(
        &self,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let items = self.finder_items(cx);
        div().id("goto-overlay").absolute().inset_0().flex().justify_center().items_start().pt(px(80.))
            .bg(cx.theme().background.opacity(0.45))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, _, window, cx| this.cancel_finder(window, cx)))
            .child(v_flex().id("goto-dialog").w(self.ui_px(640.)).gap_2().p_3().rounded(cx.theme().radius)
                .bg(cx.theme().background).border_1().border_color(cx.theme().border)
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                    match event.keystroke.key.to_lowercase().as_str() {
                        "escape" => this.cancel_finder(window, cx),
                        "enter" => this.confirm_goto(window, cx),
                        "tab" => {
                            if !this.goto_input.read(cx).value().starts_with(['@', ':']) { this.complete_goto(window, cx); }
                        }
                        "right" | "arrowright" => {
                            let query = this.goto_input.read(cx).value().to_string();
                            if this.goto_input.read(cx).cursor() != query.len()
                                || this.goto_matches(&query).first().is_none_or(|path| goto_ghost_suffix(&query, path).is_none()) { return; }
                            this.complete_goto(window, cx);
                        }
                        "up" | "arrowup" => this.finder.selected = this.finder.selected.saturating_sub(1),
                        "down" | "arrowdown" => this.finder.selected = (this.finder.selected + 1).min(this.finder_items(cx).len().saturating_sub(1)),
                        _ => return,
                    }
                    cx.stop_propagation(); cx.notify();
                }))
                .child(div().text_sm().font_bold().child("Quick Finder"))
                .child(Input::new(&self.goto_input).font_family(cx.theme().mono_font_family.clone()))
                .child(div().text_xs().text_color(cx.theme().muted_foreground).child("Files · > commands · @ buffers · :line[:column] · path:line:column"))
                .child(div().text_sm().child(self.finder.status.clone()))
                .child(v_flex().id("goto-results").max_h(self.ui_px((GOTO_MAX_VISIBLE as f32) * 32.)).overflow_y_scroll()
                    .children(items.into_iter().enumerate().map(|(index, path)| {
                        let label = display_path(&path);
                        div().id(SharedString::from(format!("goto-{path}"))).w_full().h(self.ui_px(32.)).flex_shrink_0().flex().items_center().px_2().cursor_pointer()
                            .when(index == self.finder.selected, |view| view.bg(cx.theme().accent.opacity(0.15)))
                            .hover(|style| style.bg(cx.theme().accent.opacity(0.15)))
                            .child(div().text_sm().text_ellipsis().child(label))
                            .on_click(cx.listener(move |this, _, window, cx| this.confirm_finder(Some(path.clone()), window, cx)))
                    })))
                .child(h_flex().justify_end().gap_2()
                    .child(Button::new("goto-cancel").ghost().label("Cancel").on_click(cx.listener(|this, _, window, cx| this.cancel_finder(window, cx))))
                    .child(Button::new("goto-open").primary().label("Open").on_click(cx.listener(|this, _, window, cx| this.confirm_goto(window, cx)))))
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::prelude::v1::test;
    use gpui::TestAppContext;

    fn workspace(window: &mut Window, cx: &mut Context<Workspace>) -> Workspace {
        Workspace::new_for_test(
            crate::gui::connect::parse_connect_target("ws://", None),
            window,
            cx,
        )
    }

    #[gpui::test]
    fn line_preview_cancel_restores_cursor_and_confirm_records_history(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (view, test_cx) = cx.add_window_view(workspace);
        view.update_in(test_cx, |this, window, cx| {
            this.begin_editor_tab(
                "buffer".into(),
                None,
                "a.txt".into(),
                None,
                None,
                None,
                true,
                window,
                cx,
            );
            this.apply_snapshot(
                "buffer".into(),
                1,
                "first\né🐈last\n".into(),
                "a.txt".into(),
                window,
                cx,
            );
            let panel = this.editors["a.txt"].clone();
            panel.update(cx, |panel, cx| panel.reveal_byte(1, window, cx));
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |this, window, cx| {
            let panel = this.editors["a.txt"].clone();
            this.start_finder(":2:3", window, cx);
            assert_eq!(panel.read(cx).navigation_location(cx).offset, 12);
            this.cancel_finder(window, cx);
            assert_eq!(panel.read(cx).navigation_location(cx).offset, 1);
            assert!(!this.goto_open);
            this.start_finder(":2:3", window, cx);
            this.confirm_goto(window, cx);
            assert_eq!(panel.read(cx).navigation_location(cx).offset, 12);
            this.on_navigate_back(&NavigateBack, window, cx);
            assert_eq!(panel.read(cx).navigation_location(cx).offset, 1);
        });
    }

    #[gpui::test]
    fn buffer_mode_switches_existing_views_and_records_history(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (view, test_cx) = cx.add_window_view(workspace);
        view.update_in(test_cx, |this, window, cx| {
            for name in ["b.txt", "a.txt"] {
                this.begin_editor_tab(
                    name.into(),
                    None,
                    name.into(),
                    None,
                    None,
                    None,
                    true,
                    window,
                    cx,
                );
                this.apply_snapshot(name.into(), 1, "contents".into(), name.into(), window, cx);
            }
            let source = this.editors["a.txt"].clone();
            source.update(cx, |panel, cx| panel.reveal_byte(3, window, cx));
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |this, window, cx| {
            this.start_finder("@b", window, cx);
            this.confirm_goto(window, cx);
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |this, window, cx| {
            assert!(matches!(&this.active, Some(ActiveSurface::Editor(path)) if path == "b.txt"));
            assert_eq!(this.editors.len(), 2);
            this.on_navigate_back(&NavigateBack, window, cx);
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |this, _, cx| {
            assert!(matches!(&this.active, Some(ActiveSurface::Editor(path)) if path == "a.txt"));
            assert_eq!(
                this.editors["a.txt"]
                    .read(cx)
                    .navigation_location(cx)
                    .offset,
                3
            );
        });
    }

    #[gpui::test]
    fn file_jump_records_loaded_destination_and_project_byte_offset(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (view, test_cx) = cx.add_window_view(workspace);
        view.update_in(test_cx, |this, window, cx| {
            this.begin_editor_tab(
                "source".into(),
                None,
                "a.txt".into(),
                None,
                None,
                None,
                true,
                window,
                cx,
            );
            this.apply_snapshot(
                "source".into(),
                1,
                "original".into(),
                "a.txt".into(),
                window,
                cx,
            );
            this.editors["a.txt"].update(cx, |panel, cx| panel.reveal_byte(3, window, cx));
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |this, window, cx| {
            let (ade, commands) = AdeHandle::test_channel();
            this.ade = ade;
            this.open_file_location("b.txt".into(), Some(2), Some(1), Some(7), cx);
            let AdeCmd::OpenEditor { request_id, .. } = commands.try_recv().unwrap() else {
                panic!("expected open");
            };
            this.handle_event(
                AdeEvent::EditorOpened {
                    request_id,
                    buffer_id: "target".into(),
                    draft_id: None,
                    path: "b.txt".into(),
                    language: None,
                    line: Some(2),
                    column: Some(1),
                },
                window,
                cx,
            );
            this.handle_event(
                AdeEvent::BufferSnapshot {
                    buffer_id: "target".into(),
                    rev: 1,
                    text: "first\nsecond".into(),
                    path: "b.txt".into(),
                },
                window,
                cx,
            );
            assert_eq!(
                this.editors["b.txt"]
                    .read(cx)
                    .navigation_location(cx)
                    .offset,
                7
            );
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |this, window, cx| {
            this.on_navigate_back(&NavigateBack, window, cx)
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |this, window, cx| {
            assert_eq!(this.current_location(cx).unwrap().path, "a.txt");
            assert_eq!(this.current_location(cx).unwrap().offset, 3);
            this.on_navigate_forward(&NavigateForward, window, cx);
        });
        test_cx.run_until_parked();
        view.update_in(test_cx, |this, _, cx| {
            assert_eq!(this.current_location(cx).unwrap().path, "b.txt");
            assert_eq!(this.current_location(cx).unwrap().offset, 7);
        });
    }

    #[gpui::test]
    fn exact_path_opens_before_enumeration_and_old_results_are_ignored(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
        let (view, test_cx) = cx.add_window_view(workspace);
        view.update_in(test_cx, |this, window, cx| {
            let (ade, commands) = AdeHandle::test_channel();
            this.ade = ade;
            this.session_root = "/remote/project".into();
            this.capabilities.push(CAP_FILE_FINDER.into());
            this.start_finder("src/deep/file.rs:7:2", window, cx);
            this.finder.request = Some("new-request".into());
            this.handle_finder_message(
                &Message::FileFinderResults {
                    request_id: "old-request".into(),
                    paths: vec!["wrong.rs".into()],
                    truncated: false,
                    cancelled: false,
                    error: None,
                },
                cx,
            );
            assert!(this.finder.paths.is_empty());
            this.confirm_goto(window, cx);
            let mut opened = None;
            while let Ok(command) = commands.try_recv() {
                if let AdeCmd::OpenEditor {
                    path, line, column, ..
                } = command
                {
                    opened = Some((path, line, column));
                }
            }
            assert_eq!(
                opened,
                Some(("/remote/project/src/deep/file.rs".into(), Some(7), Some(2)))
            );
            let generation = this.finder.generation;
            this.start_finder("", window, cx);
            assert!(
                this.finder.generation > generation,
                "generations must survive dismissal"
            );
            this.handle_finder_message(
                &Message::FileFinderResults {
                    request_id: "new-request".into(),
                    paths: vec!["wrong.rs".into()],
                    truncated: false,
                    cancelled: false,
                    error: None,
                },
                cx,
            );
            assert!(this.finder.paths.is_empty());
        });
    }
}

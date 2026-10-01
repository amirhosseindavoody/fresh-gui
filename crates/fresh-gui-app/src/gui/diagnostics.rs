//! Workspace diagnostics chrome; Fresh owns diagnostic production and server processes.
use super::*;
use fresh_gui_protocol::{BufferDiagnostic, LanguageServerAction, LanguageServerState, CAP_LSP_CONTROLS};

#[derive(Default)]
pub(super) struct Diagnostics {
    pub open: bool,
    pub servers_open: bool,
    severity: usize,
    source: Option<String>,
    selected: Option<(String, u32, u32)>,
    server_request: Option<(String, String, String)>,
    servers: Vec<LanguageServerState>,
}

fn matches_filter(d: &BufferDiagnostic, severity: usize, source: Option<&str>) -> bool {
    (severity == 0 || d.severity == ["all", "error", "warning", "info", "hint"][severity])
        && source.is_none_or(|source| d.source.as_deref().unwrap_or("LSP") == source)
}

impl Workspace {
    fn problem_rows(&self, cx: &App, errors_only: bool) -> Vec<(String, BufferDiagnostic)> {
        let mut rows = self.editors.iter().flat_map(|(path, panel)| {
            panel.read(cx).problems().iter().filter(|d| {
                if errors_only { d.severity == "error" } else { matches_filter(d, self.diagnostics.severity, self.diagnostics.source.as_deref()) }
            }).cloned().map(|d| (path.clone(), d)).collect::<Vec<_>>()
        }).collect::<Vec<_>>();
        rows.sort_by(|a,b| (&a.0, a.1.start_line, a.1.start_character, &a.1.message).cmp(&(&b.0, b.1.start_line, b.1.start_character, &b.1.message)));
        rows
    }

    fn select_problem(&mut self, path: String, d: BufferDiagnostic, quick_fix: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.editors.get(&path).cloned() {
            let origin = self.current_location(cx);
            self.select_entity(&panel, window, cx);
            panel.update(cx, |panel, cx| panel.reveal_problem(d.start_line, d.start_character, window, cx));
            if let Some(origin) = origin { self.record_location_jump(origin, panel.read(cx).navigation_location(cx)); }
            self.diagnostics.selected = Some((path, d.start_line, d.start_character));
            if quick_fix { workspace_edits::begin_code_actions(self, window, cx); }
        }
    }

    pub(super) fn on_show_problems(&mut self, _: &ShowProblems, _: &mut Window, cx: &mut Context<Self>) {
        self.diagnostics.open = !self.diagnostics.open;
        cx.notify();
    }
    pub(super) fn on_format_selection(&mut self, _: &FormatSelection, window: &mut Window, cx: &mut Context<Self>) {
        if !self.capabilities.iter().any(|cap| cap == CAP_LSP_CONTROLS) { self.status = "Format Selection requires lsp.controls; upgrade and restart the daemon".into(); return; }
        if let Some(ActiveSurface::Editor(path)) = &self.active {
            if let Some(panel) = self.editors.get(path) { panel.update(cx, |panel, cx| panel.request_format_selection(window, cx)); }
        }
    }
    fn step_error(&mut self, previous: bool, window: &mut Window, cx: &mut Context<Self>) {
        let rows = self.problem_rows(cx, true);
        if rows.is_empty() { self.status = "No errors in open workspace buffers".into(); cx.notify(); return; }
        let selected = self.diagnostics.selected.as_ref().and_then(|key| rows.iter().position(|(path, d)| key == &(path.clone(), d.start_line, d.start_character)));
        let index = match (selected, previous) { (Some(i), true) => (i + rows.len() - 1) % rows.len(), (Some(i), false) => (i + 1) % rows.len(), (None, true) => rows.len()-1, (None, false) => 0 };
        let (path, d) = rows[index].clone();
        self.select_problem(path, d, false, window, cx);
        cx.notify();
    }
    pub(super) fn on_next_error(&mut self, _: &NextError, window: &mut Window, cx: &mut Context<Self>) { self.step_error(false, window, cx); }
    pub(super) fn on_previous_error(&mut self, _: &PreviousError, window: &mut Window, cx: &mut Context<Self>) { self.step_error(true, window, cx); }

    pub(super) fn render_problems(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut sources = self.editors.values().flat_map(|panel| panel.read(cx).problems().iter().map(|d| d.source.clone().unwrap_or_else(|| "LSP".into())).collect::<Vec<_>>()).collect::<Vec<_>>();
        sources.sort(); sources.dedup();
        let mut root = v_flex().id("workspace-problems").w_full().h(px(240.)).bg(cx.theme().background).border_t_1().border_color(cx.theme().border)
            .child(h_flex().gap_2().px_2().py_1()
                .child(div().text_sm().child("Problems · open workspace buffers"))
                .child(Button::new("problem-severity").small().label(format!("Severity: {}", ["all", "error", "warning", "info", "hint"][self.diagnostics.severity])).on_click(cx.listener(|this, _, _, cx| { this.diagnostics.severity = (this.diagnostics.severity + 1) % 5; cx.notify(); })))
                .child(Button::new("problem-source").small().label(format!("Source: {}", self.diagnostics.source.as_deref().unwrap_or("all"))).on_click(cx.listener(move |this, _, _, cx| {
                    this.diagnostics.source = match this.diagnostics.source.as_ref().and_then(|s| sources.iter().position(|v| v == s)) { Some(i) => sources.get(i+1).cloned(), None => sources.first().cloned() }; cx.notify();
                })))
                .child(Button::new("problem-close").small().label("Close").on_click(cx.listener(|this, _, _, cx| { this.diagnostics.open = false; cx.notify(); }))));
        let mut list = v_flex().id("workspace-problem-list").flex_1().overflow_y_scroll();
        let rows = self.problem_rows(cx, false);
        if rows.is_empty() { list = list.child(div().px_2().text_xs().child("No matching problems")); }
        for (i, (path, d)) in rows.into_iter().enumerate() {
            let target = path.clone(); let diagnostic = d.clone();
            let fix_path = path.clone(); let fix_diagnostic = d.clone();
            list = list.child(h_flex().gap_2().px_2().py_1()
                .child(Button::new(format!("workspace-problem-{i}")).small().label(format!("{}:{}:{} · {} · {} · {}", path_basename(&path).unwrap_or(&path), d.start_line+1, d.start_character+1, d.severity, d.source.as_deref().unwrap_or("LSP"), d.message.replace('\n', " "))).on_click(cx.listener(move |this, _, window, cx| this.select_problem(target.clone(), diagnostic.clone(), false, window, cx))))
                .child(Button::new(format!("workspace-problem-fix-{i}")).small().label("Quick fix…").on_click(cx.listener(move |this, _, window, cx| this.select_problem(fix_path.clone(), fix_diagnostic.clone(), true, window, cx)))));
            for (j, info) in d.related_information.into_iter().enumerate() {
                list = list.child(Button::new(format!("problem-related-{i}-{j}")).small().label(format!("↳ {} · {}:{}", info.message, info.uri, info.line+1)).on_click(cx.listener(move |this, _, _, cx| {
                    if !this.capabilities.iter().any(|cap| cap == fresh_gui_protocol::CAP_LSP_NAVIGATION) { this.status = "Upgrade daemon for related-location navigation".into(); return; }
                    this.open_problem_location(info.uri.clone(), info.line, info.character, cx);
                })));
            }
        }
        root = root.child(list); root
    }

    pub(super) fn on_language_servers(&mut self, _: &ShowLanguageServers, _: &mut Window, cx: &mut Context<Self>) {
        self.diagnostics.servers_open = !self.diagnostics.servers_open;
        if self.diagnostics.servers_open { self.request_language_servers(LanguageServerAction::Status, cx); }
        cx.notify();
    }
    pub(super) fn on_start_language_servers(&mut self, _: &StartLanguageServers, _: &mut Window, cx: &mut Context<Self>) { self.diagnostics.servers_open = true; self.request_language_servers(LanguageServerAction::Start, cx); }
    pub(super) fn on_stop_language_servers(&mut self, _: &StopLanguageServers, _: &mut Window, cx: &mut Context<Self>) { self.diagnostics.servers_open = true; self.request_language_servers(LanguageServerAction::Stop, cx); }
    pub(super) fn on_restart_language_servers(&mut self, _: &RestartLanguageServers, _: &mut Window, cx: &mut Context<Self>) { self.diagnostics.servers_open = true; self.request_language_servers(LanguageServerAction::Restart, cx); }
    fn request_language_servers(&mut self, action: LanguageServerAction, cx: &mut Context<Self>) {
        if !self.capabilities.iter().any(|cap| cap == CAP_LSP_CONTROLS) { self.status = "Language-server controls require lsp.controls; upgrade and restart the daemon".into(); cx.notify(); return; }
        let Some(ActiveSurface::Editor(path)) = &self.active else { self.status = "Select an editor to inspect its configured servers".into(); cx.notify(); return; };
        let Some(panel) = self.editors.get(path) else { return; };
        let buffer_id = panel.read(cx).buffer_id().to_owned();
        let request_id = next_id("language-servers");
        self.diagnostics.server_request = Some((request_id.clone(), buffer_id.clone(), self.workspace_id_or_empty()));
        self.ade.send(AdeCmd::LanguageServers { request_id, buffer_id, action });
        cx.notify();
    }
    pub(super) fn receive_language_servers(&mut self, request_id: String, buffer_id: String, servers: Vec<LanguageServerState>, cx: &mut Context<Self>) {
        if self.diagnostics.server_request.as_ref() == Some(&(request_id, buffer_id, self.workspace_id_or_empty())) { self.diagnostics.servers = servers; cx.notify(); }
    }
    pub(super) fn render_language_servers(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let mut root = v_flex().id("language-servers").w_full().h(px(220.)).bg(cx.theme().background).border_t_1().border_color(cx.theme().border).gap_2().p_2();
        let mut buttons = h_flex().gap_2().child(div().text_sm().child("Language servers · active editor language"));
        for (label, action) in [("Refresh", LanguageServerAction::Status), ("Start", LanguageServerAction::Start), ("Stop", LanguageServerAction::Stop), ("Restart", LanguageServerAction::Restart)] {
            buttons = buttons.child(Button::new(format!("servers-{label}")).small().label(label).on_click(cx.listener(move |this, _, _, cx| this.request_language_servers(action, cx))));
        }
        buttons = buttons.child(Button::new("servers-close").small().label("Close").on_click(cx.listener(|this, _, _, cx| { this.diagnostics.servers_open = false; cx.notify(); })));
        root = root.child(buttons);
        let mut list = v_flex().id("server-list").flex_1().overflow_y_scroll();
        if self.diagnostics.servers.is_empty() { list = list.child(div().text_xs().child("No server state received. Configure servers and per-language enabled flags in Settings → lsp.")); }
        for server in &self.diagnostics.servers {
            list = list.child(div().text_xs().child(format!("{} ({}) · {} · {}", server.name, server.language, server.status, server.command)));
            for line in &server.logs { list = list.child(div().text_xs().child(line.clone())); }
        }
        root.child(list)
    }
}

#[cfg(test)]
mod tests {
    use super::{matches_filter, BufferDiagnostic};
    #[test]
    fn severity_and_source_filters_are_independent() {
        let d = BufferDiagnostic { start_line: 0, start_character: 0, end_line: 0, end_character: 1, severity: "error".into(), message: "bad".into(), source: Some("TY".into()), related_information: Vec::new() };
        assert!(matches_filter(&d, 0, None)); assert!(matches_filter(&d, 1, Some("TY")));
        assert!(!matches_filter(&d, 2, Some("TY"))); assert!(!matches_filter(&d, 1, Some("Ruff")));
    }
}

//! Workspace search preferences use the existing daemon layout persistence.
use super::*;

impl Workspace {
    pub(super) fn set_search_options(
        &mut self,
        options: Option<fresh_gui_protocol::SearchOptions>,
        cx: &mut Context<Self>,
    ) {
        self.search_options = options.clone();
        for panel in self.editors.values() {
            panel.update(cx, |panel, cx| panel.configure_search(options.clone(), cx));
        }
        self.publish_layout(cx);
    }

    fn on_find_in_buffer(&mut self, _: &FindInBuffer, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActiveSurface::Editor(path)) = &self.active
            && let Some(panel) = self.editors.get(path)
        {
            panel.update(cx, |panel, cx| panel.open_search(false, window, cx));
        }
    }
    fn on_replace_in_buffer(
        &mut self,
        _: &ReplaceInBuffer,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActiveSurface::Editor(path)) = &self.active
            && let Some(panel) = self.editors.get(path)
        {
            panel.update(cx, |panel, cx| panel.open_search(true, window, cx));
        }
    }
    fn on_query_replace(&mut self, _: &QueryReplace, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActiveSurface::Editor(path)) = &self.active
            && let Some(panel) = self.editors.get(path)
        {
            panel.update(cx, |panel, cx| panel.open_search(true, window, cx));
        }
    }
    fn on_clear_search(
        &mut self,
        _: &ClearSearchHighlights,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActiveSurface::Editor(path)) = &self.active
            && let Some(panel) = self.editors.get(path)
        {
            panel.update(cx, |panel, cx| panel.clear_search(window, cx));
        }
    }
    fn on_next_search(&mut self, _: &NextSearchMatch, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ActiveSurface::Editor(path)) = &self.active
            && let Some(panel) = self.editors.get(path)
        {
            panel.update(cx, |panel, cx| panel.navigate_search(false, window, cx));
        }
    }
    fn on_previous_search(
        &mut self,
        _: &PreviousSearchMatch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ActiveSurface::Editor(path)) = &self.active
            && let Some(panel) = self.editors.get(path)
        {
            panel.update(cx, |panel, cx| panel.navigate_search(true, window, cx));
        }
    }
}

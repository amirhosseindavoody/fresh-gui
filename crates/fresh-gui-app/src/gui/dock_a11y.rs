//! Roles for dock chrome that tracks focus.
//!
//! GPUI allows one accessibility focus node per frame. The tab group frame
//! (`id` `tab-panel`) tracks the *active panel's* focus handle, and the panel
//! (terminal, editor, diff) tracks that same handle and has its own id and
//! role. A role on the tab frame makes both nodes call `set_focus`, which
//! GPUI logs at warn:
//! `a11y: set_focus called more than once in a single frame`.
//! Clicking the title bar redraws while the panel stays focused, so the
//! warning repeats. The tab frame stays role-less on purpose: the panel is
//! the single focus owner. GPUI still notes the role-less frame at info,
//! which the host filter (`gpui=warn`) hides.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, App, AppContext as _, Axis, Div, Entity, Stateful,
    StatefulInteractiveElement as _, Window,
};
use gpui_kit::Role;
use gpui_kit::component::dock::{
    BasePanelView, DockArea, DockAreaRenderer, DockContext, DockSkin, DropIndicator, NodeId,
    PanelState, PanelStyle, TabGroupContext, TabGroupRenderer,
};

/// Workspace dock: tab bar, no dock-collapse button, accessibility roles on
/// the frames that track focus.
///
/// Skin setters notify the area. That update has to wait until `cx.new` has
/// inserted the entity. Calling them inside the constructor leases an id that
/// is not in the map yet, and GPUI panics with `cannot update DockArea while
/// it is already being updated` before the window is shown.
pub(crate) fn install_workspace_dock(
    window: &mut Window,
    cx: &mut App,
) -> (Entity<DockArea>, Rc<DockSkin>) {
    let mut installed = None;
    let dock = cx.new(|cx| {
        let skin = DockSkin::new(cx);
        installed = Some(skin.clone());
        DockArea::new("workspace", Some(1), window, cx).with_renderer(A11yDockSkin::wrap(skin))
    });
    let skin = installed.expect("DockSkin::new ran inside the constructor");
    skin.set_panel_style(PanelStyle::TabBar, cx);
    skin.set_toggle_button_visible(false, cx);
    (dock, skin)
}

/// [`DockSkin`] with a group role on every frame that carries an element id.
pub struct A11yDockSkin {
    inner: Rc<DockSkin>,
}

impl A11yDockSkin {
    pub fn wrap(inner: Rc<DockSkin>) -> Rc<dyn DockAreaRenderer> {
        Rc::new(Self { inner })
    }
}

impl DockAreaRenderer for A11yDockSkin {
    fn frame(&self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        DockAreaRenderer::frame(self.inner.as_ref(), window, cx)
            .role(Role::Group)
            .aria_label("Workspace")
    }

    fn split_frame(
        &self,
        node: NodeId,
        axis: Axis,
        window: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        DockAreaRenderer::split_frame(self.inner.as_ref(), node, axis, window, cx).role(Role::Group)
    }

    fn center_frame(&self, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        DockAreaRenderer::center_frame(self.inner.as_ref(), window, cx).role(Role::Group)
    }

    fn render_dock(
        &self,
        dock: &DockContext,
        content: AnyElement,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        DockAreaRenderer::render_dock(self.inner.as_ref(), dock, content, window, cx)
    }

    fn build_placeholder(
        &self,
        state: &PanelState,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Arc<dyn BasePanelView>> {
        DockAreaRenderer::build_placeholder(self.inner.as_ref(), state, window, cx)
    }

    fn tab_group_renderer(&self) -> Rc<dyn TabGroupRenderer> {
        Rc::new(RoleTabGroup {
            inner: DockAreaRenderer::tab_group_renderer(self.inner.as_ref()),
        })
    }
}

struct RoleTabGroup {
    inner: Rc<dyn TabGroupRenderer>,
}

impl TabGroupRenderer for RoleTabGroup {
    fn frame(&self, group: &TabGroupContext, window: &mut Window, cx: &mut App) -> Stateful<Div> {
        // No role. This frame and the focused panel share one focus handle;
        // a role here is a second `set_focus` in the same frame.
        self.inner.frame(group, window, cx)
    }

    fn content_frame(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> Stateful<Div> {
        self.inner
            .content_frame(group, window, cx)
            .role(Role::Group)
    }

    fn render_tab_bar(
        &self,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.inner.render_tab_bar(group, window, cx)
    }

    fn render_active_panel(
        &self,
        panel: AnyView,
        group: &TabGroupContext,
        window: &mut Window,
        cx: &mut App,
    ) -> AnyElement {
        self.inner.render_active_panel(panel, group, window, cx)
    }

    fn render_drop_indicator(
        &self,
        indicator: DropIndicator,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<AnyElement> {
        self.inner.render_drop_indicator(indicator, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Empty, Render, TestAppContext};

    struct DockHost {
        _dock: Entity<DockArea>,
    }

    impl Render for DockHost {
        fn render(
            &mut self,
            _: &mut Window,
            _: &mut gpui::Context<Self>,
        ) -> impl gpui::IntoElement {
            Empty
        }
    }

    /// The v2026.922.10 startup panic: skin setters inside `cx.new` update the
    /// area before it is inserted.
    #[gpui::test]
    #[should_panic(expected = "cannot update")]
    fn skin_settings_inside_the_constructor_panic(cx: &mut TestAppContext) {
        let _ = cx.add_window_view(|window, cx| {
            let dock = cx.new(|cx| {
                let skin = DockSkin::new(cx);
                skin.set_panel_style(PanelStyle::TabBar, cx);
                skin.set_toggle_button_visible(false, cx);
                DockArea::new("workspace", Some(1), window, cx)
                    .with_renderer(A11yDockSkin::wrap(skin))
            });
            DockHost { _dock: dock }
        });
    }

    #[gpui::test]
    fn workspace_dock_applies_the_tab_bar_after_insert(cx: &mut TestAppContext) {
        let _ = cx.add_window_view(|window, cx| {
            let (dock, skin) = install_workspace_dock(window, cx);
            assert_eq!(skin.panel_style(), PanelStyle::TabBar);
            assert!(!skin.is_toggle_button_visible());
            DockHost { _dock: dock }
        });
    }
}

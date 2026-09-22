//! Roles for dock chrome that tracks focus.
//!
//! GPUI logs when a focused element has an element id and no accessibility
//! role. `DockSkin`'s tab-panel frame tracks the active panel's focus handle,
//! and the dock area tracks its own, both without a role. A role on the panel
//! alone is not enough: prepaint visits the role-less frame first, a later
//! node clears the once-per-focus dedup, and the next frame logs again.

use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, AnyView, App, Axis, Div, Stateful, StatefulInteractiveElement as _, Window,
};
use gpui_kit::Role;
use gpui_kit::component::dock::{
    BasePanelView, DockAreaRenderer, DockContext, DockSkin, DropIndicator, NodeId, PanelState,
    TabGroupContext, TabGroupRenderer,
};

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
        self.inner
            .frame(group, window, cx)
            .role(Role::Group)
            .aria_label("Editor group")
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

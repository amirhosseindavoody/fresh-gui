//! Native GPUI + gpui-component host for the ADE protocol.
//!
//! Fresh remains buffer authority on the remote daemon. This crate is a
//! desktop renderer: it speaks `fresh-gui-protocol` over WebSocket via
//! `fresh-gui-client` and paints a Zed/VS Code-like shell.

mod actions;
mod ade;
mod connect;
mod osc7;
mod terminal;
mod workspace;

use anyhow::Result;
use gpui_kit::component::{Root, TitleBar};
use gpui_kit::*;

use crate::gui::workspace::Workspace;

pub use connect::parse_connect_target;

/// Launch the native ADE window. Blocks until the last window closes.
pub fn run(backend: String, token: Option<String>) -> Result<()> {
    let app = gpui_kit::application().with_assets(gpui_kit::assets::Assets);

    app.run(move |cx| {
        gpui_kit::init(cx);
        actions::init(cx);

        let target = parse_connect_target(&backend, token.clone());
        let mut window_size = size(px(1280.0), px(800.0));
        if let Some(display) = cx.primary_display() {
            let display_size = display.bounds().size;
            window_size.width = window_size.width.min(display_size.width * 0.9);
            window_size.height = window_size.height.min(display_size.height * 0.9);
        }

        let mut options = TitleBar::window_options();
        options.window_bounds = Some(WindowBounds::centered(window_size, cx));
        options.window_min_size = Some(size(px(720.0), px(480.0)));
        options.kind = WindowKind::Normal;
        #[cfg(target_os = "linux")]
        {
            options.window_background = WindowBackgroundAppearance::Transparent;
            options.window_decorations = Some(WindowDecorations::Client);
        }

        cx.spawn(async move |cx| {
            let window = cx
                .open_window(options, |window, cx| {
                    let view = cx.new(|cx| Workspace::new(target, window, cx));
                    cx.new(|cx| Root::new(view, window, cx))
                })
                .expect("open fresh-gui window");

            let _ = window.update(cx, |_, window, cx| {
                window.set_window_title("fresh-gui");
                window.activate_window();
                cx.on_release(|_, cx| cx.quit()).detach();
            });
        })
        .detach();
    });

    Ok(())
}

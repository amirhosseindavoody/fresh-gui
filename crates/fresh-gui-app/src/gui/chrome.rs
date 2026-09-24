//! Light-theme chrome contrast and the two zoom levels.
//!
//! gpui-component's default light border is about `#e5e5e5` on white, so a
//! one-pixel split handle disappears. Content zoom scales editor and terminal
//! text. UI zoom scales rem-based chrome text and workspace shell dimensions.

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Window, px};

/// Default UI font and rem, matching gpui-component's theme.
pub const UI_FONT_BASE: f32 = 16.0;

pub const CONTENT_ZOOM_MIN: f32 = 0.5;
pub const CONTENT_ZOOM_MAX: f32 = 3.0;
pub const UI_ZOOM_MIN: f32 = 0.8;
pub const UI_ZOOM_MAX: f32 = 2.0;
const ZOOM_STEP: f32 = 0.1;

/// Keep light-theme dividers visible without the heavy gray from #104.
const LIGHT_LINE_GAP: f32 = 0.20;

/// `steps` is positive to zoom in.
pub fn step_zoom(current: f32, steps: i32, min: f32, max: f32) -> f32 {
    let next = current + steps as f32 * ZOOM_STEP;
    let next = (next * 1000.0).round() / 1000.0;
    next.clamp(min, max)
}

pub fn ui_rem(ui_zoom: f32) -> f32 {
    UI_FONT_BASE * ui_zoom
}

/// Editor or terminal font in pixels. Both zooms apply so UI zoom still
/// enlarges panel text, and content zoom does not resize the rails.
pub fn content_font_px(base: f32, content_zoom: f32, ui_zoom: f32) -> f32 {
    (base * content_zoom * ui_zoom).clamp(8.0, 72.0)
}

/// Cell size for a monospace grid. At 14px this is the 8×18 grid the
/// terminal already used.
pub fn terminal_cell(font_px: f32) -> (f32, f32) {
    let width = (font_px * (8.0 / 14.0)).round().max(4.0);
    let height = (font_px * (18.0 / 14.0)).round().max(8.0);
    (width, height)
}

/// Darken `line_l` when it sits too close to a bright `surface_l`.
pub fn light_line_lightness(surface_l: f32, line_l: f32) -> f32 {
    if surface_l < 0.75 {
        return line_l;
    }
    if surface_l - line_l >= LIGHT_LINE_GAP {
        return line_l;
    }
    (surface_l - LIGHT_LINE_GAP).clamp(0.35, 0.82)
}

/// Follow the OS until a daemon config says light or dark.
pub fn install_initial_theme(window: Option<&mut Window>, cx: &mut App) {
    apply_configured_theme("system", window, cx);
}

pub fn apply_configured_theme(mode: &str, window: Option<&mut Window>, cx: &mut App) {
    match mode.trim().to_ascii_lowercase().as_str() {
        "dark" => Theme::change(ThemeMode::Dark, window, cx),
        "light" => Theme::change(ThemeMode::Light, window, cx),
        _ => Theme::sync_system_appearance(window, cx),
    }
    strengthen_light_chrome(cx);
}

/// Pull light borders and the pure-white canvas apart so split handles,
/// sidebars, and the git diff gutter read as separate surfaces.
pub fn strengthen_light_chrome(cx: &mut App) {
    if Theme::global(cx).is_dark() {
        return;
    }
    {
        let theme = Theme::global_mut(cx);
        let mut colors = theme.colors;
        if colors.background.l > 0.985 {
            colors.background.l = 0.97;
        }
        let surface = colors.background.l.max(colors.sidebar.l);
        colors.border.l = light_line_lightness(surface, colors.border.l);
        colors.drag_border.l = light_line_lightness(surface, colors.drag_border.l);
        colors.sidebar_border.l = light_line_lightness(surface, colors.sidebar_border.l);
        colors.title_bar_border.l = light_line_lightness(surface, colors.title_bar_border.l);
        colors.status_bar_border.l = light_line_lightness(surface, colors.status_bar_border.l);
        colors.window_border.l = light_line_lightness(surface, colors.window_border.l);
        theme.colors = colors;
    }
    Theme::sync_base(cx);
}

pub fn scale_ui_fonts(ui_zoom: f32, cx: &mut App) {
    Theme::global_mut(cx).font_size = px(ui_rem(ui_zoom));
    Theme::sync_base(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_near_white_border_is_pulled_away_from_the_surface() {
        // neutral-200 on white: the gap is about 0.10 and the line vanishes.
        let line = light_line_lightness(1.0, 0.898);
        assert!(1.0 - line >= LIGHT_LINE_GAP - 0.001);
        assert!(line > 0.75 && line <= 0.82);
    }

    #[test]
    fn an_already_dark_line_and_a_dark_surface_stay_put() {
        assert!((light_line_lightness(1.0, 0.4) - 0.4).abs() < f32::EPSILON);
        assert!((light_line_lightness(0.2, 0.9) - 0.9).abs() < f32::EPSILON);
    }

    #[test]
    fn zoom_steps_and_clamps() {
        assert!((step_zoom(1.0, 1, CONTENT_ZOOM_MIN, CONTENT_ZOOM_MAX) - 1.1).abs() < 0.001);
        assert!((step_zoom(1.0, -1, CONTENT_ZOOM_MIN, CONTENT_ZOOM_MAX) - 0.9).abs() < 0.001);
        assert!((step_zoom(0.5, -3, CONTENT_ZOOM_MIN, CONTENT_ZOOM_MAX) - 0.5).abs() < 0.001);
        assert!((step_zoom(2.0, 5, UI_ZOOM_MIN, UI_ZOOM_MAX) - 2.0).abs() < 0.001);
    }

    #[test]
    fn terminal_cells_match_the_14px_grid() {
        assert_eq!(terminal_cell(14.0), (8.0, 18.0));
        let (w, h) = terminal_cell(content_font_px(14.0, 2.0, 1.0));
        assert!(w > 8.0 && h > 18.0);
    }
}

//! Window icon for the GPUI host.
//!
//! Windows reads icon resource 1 from the executable (`build.rs` embeds
//! `assets/fresh-gui.ico`). X11 takes the PNG on the window. Wayland looks up
//! `APP_ID` in the desktop file this module writes under the user data dir.

#[cfg(target_os = "linux")]
use std::path::PathBuf;
use std::sync::Arc;

pub const APP_ID: &str = "fresh-gui";

const PNG: &[u8] = include_bytes!("../../assets/fresh-gui.png");

/// X11 `_NET_WM_ICON`. Wayland ignores this and uses [`install_user_desktop_entry`].
pub fn window_icon() -> Option<Arc<image::RgbaImage>> {
    let image = image::load_from_memory(PNG).ok()?;
    Some(Arc::new(image.to_rgba8()))
}

/// Best-effort launcher entry so a Wayland dock can show the icon.
/// Failures are ignored: the window still opens.
pub fn install_user_desktop_entry() {
    #[cfg(target_os = "linux")]
    install_linux_desktop_entry();
}

#[cfg(target_os = "linux")]
fn install_linux_desktop_entry() {
    let Some(data) = user_data_dir() else {
        return;
    };
    let icon_dir = data.join("icons/hicolor/256x256/apps");
    let apps = data.join("applications");
    if std::fs::create_dir_all(&icon_dir).is_err() || std::fs::create_dir_all(&apps).is_err() {
        return;
    }
    let icon_path = icon_dir.join("fresh-gui.png");
    if std::fs::read(&icon_path).ok().as_deref() != Some(PNG) {
        let _ = std::fs::write(&icon_path, PNG);
    }
    let exec = std::env::current_exe()
        .ok()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "fresh-gui".to_string());
    let desktop = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=fresh-gui\n\
         Comment=Files, terminals, and Git\n\
         Exec={exec}\n\
         Icon={icon}\n\
         Terminal=false\n\
         Categories=Development;\n\
         StartupWMClass={app_id}\n",
        exec = desktop_exec(&exec),
        icon = icon_path.display(),
        app_id = APP_ID,
    );
    let desktop_path = apps.join("fresh-gui.desktop");
    if std::fs::read_to_string(&desktop_path).ok().as_deref() != Some(desktop.as_str()) {
        let _ = std::fs::write(&desktop_path, desktop);
    }
}

#[cfg(target_os = "linux")]
fn user_data_dir() -> Option<PathBuf> {
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME")
        && !xdg.is_empty()
    {
        return Some(PathBuf::from(xdg));
    }
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".local/share"))
}

/// Quote a desktop `Exec` value when the path is not a bare token.
pub fn desktop_exec(path: &str) -> String {
    if path.contains([' ', '\t', '\n', '"', '\\', '\'']) {
        format!("\"{}\"", path.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_decodes_to_a_square_icon() {
        let image = window_icon().expect("png");
        assert_eq!(image.width(), 256);
        assert_eq!(image.height(), 256);
    }

    #[test]
    fn desktop_exec_quotes_paths_with_spaces() {
        assert_eq!(desktop_exec("/usr/bin/fresh-gui"), "/usr/bin/fresh-gui");
        assert_eq!(
            desktop_exec("/home/me/My Apps/fresh-gui"),
            "\"/home/me/My Apps/fresh-gui\""
        );
    }
}

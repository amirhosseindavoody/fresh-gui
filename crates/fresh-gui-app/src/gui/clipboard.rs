//! Text copied from remote paths belongs on the client's system clipboard.

use gpui_kit::{App, Window};
use gpui_kit::component::WindowExt;

#[cfg(not(windows))]
use gpui_kit::ClipboardItem;

/// Show the standard short-lived toast after a successful copy operation.
///
/// The app root already renders gpui-component's notification layer, so the
/// normal `WindowExt` notification path is available throughout the workspace.
pub fn notify_copied(window: &mut Window, cx: &mut App) {
    window.push_notification("Copied to clipboard", cx);
}

#[cfg(not(windows))]
pub fn write_text(_: &Window, cx: &mut App, text: &str) -> Result<(), String> {
    cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
    Ok(())
}

#[cfg(not(windows))]
pub fn read_text(_: &Window, cx: &App) -> Result<String, String> {
    let item = cx
        .read_from_clipboard()
        .ok_or_else(|| "Clipboard is empty".to_string())?;
    item.text()
        .filter(|text| !text.is_empty())
        .ok_or_else(|| "Clipboard has no text".to_string())
}

#[cfg(windows)]
pub fn write_text(window: &Window, _: &mut App, text: &str) -> Result<(), String> {
    use raw_window_handle::RawWindowHandle;

    // The GPUI Windows writer in 0.3.6 opens the clipboard with a null owner.
    // EmptyClipboard then leaves it ownerless and SetClipboardData fails.
    let handle = raw_window_handle::HasWindowHandle::window_handle(window)
        .map_err(|error| format!("Cannot get window handle: {error}"))?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err("Cannot get a Windows window handle".into());
    };
    let hwnd = handle.hwnd.get() as *mut std::ffi::c_void;
    clipboard_win::raw::open_for(hwnd)
        .map_err(|error| format!("Cannot open clipboard: {error}"))?;
    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            let _ = clipboard_win::raw::close();
        }
    }
    let _guard = ClipboardGuard;
    clipboard_win::raw::set_string(text).map_err(|error| format!("Cannot copy path: {error}"))
}

#[cfg(windows)]
pub fn read_text(window: &Window, _: &App) -> Result<String, String> {
    use raw_window_handle::RawWindowHandle;

    let handle = raw_window_handle::HasWindowHandle::window_handle(window)
        .map_err(|error| format!("Cannot get window handle: {error}"))?;
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return Err("Cannot get a Windows window handle".into());
    };
    let hwnd = handle.hwnd.get() as *mut std::ffi::c_void;
    clipboard_win::raw::open_for(hwnd)
        .map_err(|error| format!("Cannot open clipboard: {error}"))?;
    struct ClipboardGuard;
    impl Drop for ClipboardGuard {
        fn drop(&mut self) {
            let _ = clipboard_win::raw::close();
        }
    }
    let _guard = ClipboardGuard;
    // clipboard-win 5.4 writes UTF-8 into the buffer and returns the byte count.
    let mut bytes = Vec::new();
    clipboard_win::raw::get_string(&mut bytes)
        .map_err(|error| format!("Cannot read clipboard: {error}"))?;
    let text = String::from_utf8(bytes).map_err(|error| format!("Clipboard text is not UTF-8: {error}"))?;
    if text.is_empty() {
        Err("Clipboard has no text".into())
    } else {
        Ok(text)
    }
}

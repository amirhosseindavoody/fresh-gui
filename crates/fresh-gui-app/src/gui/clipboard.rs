//! Text copied from remote paths belongs on the client's system clipboard.

use gpui_kit::{App, Window};

#[cfg(not(windows))]
use gpui_kit::ClipboardItem;

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
    let text = clipboard_win::raw::get_string()
        .map_err(|error| format!("Cannot read clipboard: {error}"))?;
    if text.is_empty() {
        Err("Clipboard has no text".into())
    } else {
        Ok(text)
    }
}

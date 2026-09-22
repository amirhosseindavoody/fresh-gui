//! Window-level ADE actions (command palette + keybindings).

use gpui_kit::component::GlobalState;
use gpui_kit::{App, KeyBinding, Menu, MenuItem, actions};

/// File menu: stop the local daemon, or leave it running and close the window.
pub fn install_menus(cx: &mut App) {
    let menus = vec![file_menu().owned()];
    GlobalState::global_mut(cx).set_app_menus(menus);
    cx.set_menus(vec![file_menu()]);
}

fn file_menu() -> Menu {
    Menu::new("File").items([
        MenuItem::action("Stop Server", StopServer),
        MenuItem::separator(),
        MenuItem::action("Quit Client", QuitClient),
    ])
}

actions!(
    fresh_gui,
    [
        NewTerminal,
        CloseTab,
        SaveBuffer,
        ToggleSidebar,
        ToggleCommandPalette,
        GoToFile,
        OpenSettings,
        Reconnect,
        Disconnect,
        NextTab,
        PrevTab,
        CopyExplorer,
        PasteExplorer,
        NewWorkspace,
        RenameWorkspace,
        CloseWorkspace,
        StopServer,
        QuitClient,
    ]
);

pub fn init(cx: &mut App) {
    cx.bind_keys(vec![
        KeyBinding::new("ctrl-t", NewTerminal, None),
        KeyBinding::new("ctrl-w", CloseTab, None),
        KeyBinding::new("ctrl-s", SaveBuffer, None),
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("ctrl-shift-p", ToggleCommandPalette, None),
        KeyBinding::new("ctrl-p", GoToFile, None),
        KeyBinding::new("ctrl-,", OpenSettings, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        KeyBinding::new("ctrl-shift-r", Reconnect, None),
        KeyBinding::new("ctrl-c", CopyExplorer, Some("Explorer")),
        KeyBinding::new("cmd-c", CopyExplorer, Some("Explorer")),
        KeyBinding::new("ctrl-v", PasteExplorer, Some("Explorer")),
        KeyBinding::new("cmd-v", PasteExplorer, Some("Explorer")),
    ]);
}

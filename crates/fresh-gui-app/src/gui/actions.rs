//! Window-level ADE actions (command palette + keybindings).

use gpui_kit::component::GlobalState;
use gpui_kit::{App, KeyBinding, Menu, MenuItem, actions};
use serde::Deserialize;

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
        OpenDefaultSettings,
        Reconnect,
        Disconnect,
        NextTab,
        PrevTab,
        CopyExplorer,
        PasteExplorer,
        FilterExplorer,
        ClearExplorerInput,
        NewWorkspace,
        RenameWorkspace,
        CloseWorkspace,
        ZoomInContent,
        ZoomOutContent,
        ResetContentZoom,
        ZoomInUi,
        ZoomOutUi,
        ResetUiZoom,
        StopServer,
        QuitClient,
    ]
);

pub fn init(cx: &mut App) {
    #[derive(Deserialize)]
    struct Defaults {
        shortkeys: Vec<Shortkey>,
    }
    #[derive(Deserialize)]
    struct Shortkey {
        action: String,
        shortkey: String,
        when: Option<String>,
    }
    let defaults: Defaults = jsonc_parser::parse_to_serde_value(
        include_str!("../../../fresh-gui/defaults/config.default.jsonc"),
        &jsonc_parser::ParseOptions::default(),
    )
    .expect("embedded default shortkeys must parse");
    let bindings = defaults.shortkeys.iter().filter_map(|entry| {
        let key = entry.shortkey.as_str();
        let when = entry.when.as_deref();
        let binding = match entry.action.as_str() {
            "NewTerminal" => KeyBinding::new(key, NewTerminal, when),
            "CloseTab" => KeyBinding::new(key, CloseTab, when),
            "SaveBuffer" => KeyBinding::new(key, SaveBuffer, when),
            "ToggleSidebar" => KeyBinding::new(key, ToggleSidebar, when),
            "ToggleCommandPalette" => KeyBinding::new(key, ToggleCommandPalette, when),
            "GoToFile" => KeyBinding::new(key, GoToFile, when),
            "OpenSettings" => KeyBinding::new(key, OpenSettings, when),
            "OpenDefaultSettings" => KeyBinding::new(key, OpenDefaultSettings, when),
            "Reconnect" => KeyBinding::new(key, Reconnect, when),
            "Disconnect" => KeyBinding::new(key, Disconnect, when),
            "NextTab" => KeyBinding::new(key, NextTab, when),
            "PrevTab" => KeyBinding::new(key, PrevTab, when),
            "CopyExplorer" => KeyBinding::new(key, CopyExplorer, when),
            "PasteExplorer" => KeyBinding::new(key, PasteExplorer, when),
            "FilterExplorer" => KeyBinding::new(key, FilterExplorer, when),
            "ClearExplorerInput" => KeyBinding::new(key, ClearExplorerInput, when),
            "NewWorkspace" => KeyBinding::new(key, NewWorkspace, when),
            "RenameWorkspace" => KeyBinding::new(key, RenameWorkspace, when),
            "CloseWorkspace" => KeyBinding::new(key, CloseWorkspace, when),
            "ZoomInContent" => KeyBinding::new(key, ZoomInContent, when),
            "ZoomOutContent" => KeyBinding::new(key, ZoomOutContent, when),
            "ResetContentZoom" => KeyBinding::new(key, ResetContentZoom, when),
            "ZoomInUi" => KeyBinding::new(key, ZoomInUi, when),
            "ZoomOutUi" => KeyBinding::new(key, ZoomOutUi, when),
            "ResetUiZoom" => KeyBinding::new(key, ResetUiZoom, when),
            "StopServer" => KeyBinding::new(key, StopServer, when),
            "QuitClient" => KeyBinding::new(key, QuitClient, when),
            unknown => {
                tracing::warn!(action = unknown, "unknown default shortkey action");
                return None;
            }
        };
        Some(binding)
    });
    cx.bind_keys(bindings);
}

//! Window-level ADE actions (command palette + keybindings).

use gpui_kit::component::GlobalState;
use gpui_kit::{App, KeyBinding, Menu, MenuItem, actions};
use serde::Deserialize;
use fresh_gui_protocol::Shortkey;
use std::cell::RefCell;

thread_local! {
    /// GPUI Kit's editor/menu bindings, captured before ADE adds its bindings.
    static KIT_BINDINGS: RefCell<Vec<KeyBinding>> = const { RefCell::new(Vec::new()) };
}

/// Application menus for file and daemon operations.
pub fn install_menus(cx: &mut App) {
    let menus = vec![file_menu().owned(), server_menu().owned()];
    GlobalState::global_mut(cx).set_app_menus(menus);
    cx.set_menus(vec![file_menu(), server_menu()]);
}

fn file_menu() -> Menu {
    Menu::new("File").items([
        MenuItem::action("New Terminal", NewTerminal),
        MenuItem::action("New File", NewFile),
        MenuItem::separator(),
        MenuItem::action("Save", SaveBuffer),
        MenuItem::separator(),
        MenuItem::action("Quit Client", QuitClient),
    ])
}

fn server_menu() -> Menu {
    Menu::new("Server").items([
        MenuItem::action("Restart Server", RestartServer),
        MenuItem::action("Reload Config", ReloadConfig),
        MenuItem::separator(),
        MenuItem::action("Stop Server", StopServer),
    ])
}

actions!(
    fresh_gui,
    [
        NewTerminal,
        NewFile,
        SplitTerminal,
        FormatDocument,
        RenameSymbol,
        CodeActions,
        Complete,
        ShowHover,
        SignatureHelp,
        GoToDefinition,
        GoToDeclaration,
        GoToTypeDefinition,
        GoToImplementation,
        FindReferences,
        DocumentSymbols,
        WorkspaceSymbols,
        NavigateBack,
        NavigateForward,
        ToggleWordWrap,
        FindInBuffer,
        SearchProject,
        ReplaceInBuffer,
        QueryReplace,
        ClearSearchHighlights,
        NextSearchMatch,
        PreviousSearchMatch,
        TerminalInputTab,
        TerminalInputBacktab,
        CloseTab,
        CloseAllEditors,
        CloseAllTerminals,
        CloseAllOtherTerminals,
        CloseAllOtherTabs,
        SaveBuffer,
        ToggleSidebar,
        ToggleCommandPalette,
        SwitchBuffer,
        GoToLine,
        GoToFile,
        OpenSettings,
        OpenDefaultSettings,
        Reconnect,
        Disconnect,
        NextTab,
        PrevTab,
        CopyExplorer,
        PasteExplorer,
        DeleteExplorer,
        AskCopilot,
        TerminalCopyOrInterrupt,
        TogglePinTab,
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
        RestartServer,
        ReloadConfig,
        QuitClient,
    ]
);

pub fn init(cx: &mut App) {
    #[derive(Deserialize)]
    struct Defaults {
        shortkeys: Vec<Shortkey>,
    }
    KIT_BINDINGS.with(|saved| *saved.borrow_mut() = cx.key_bindings().borrow().bindings().cloned().collect());
    let defaults: Defaults = jsonc_parser::parse_to_serde_value(
        include_str!("../../../fresh-gui/defaults/config.default.jsonc"),
        &jsonc_parser::ParseOptions::default(),
    )
    .expect("embedded default shortkeys must parse");
    apply_shortkeys(cx, &defaults.shortkeys);
}

/// Replace ADE bindings after Hello or a settings save while retaining GPUI Kit's
/// built-in editor bindings. An empty list deliberately removes ADE shortcuts.
pub fn apply_shortkeys(cx: &mut App, shortkeys: &[Shortkey]) {
    let bindings = shortkeys.iter().filter_map(|entry| {
        let key = entry.shortkey.as_str();
        if !key.split_whitespace().all(|part| gpui_kit::Keystroke::parse(part).is_ok()) {
            tracing::warn!(key, "invalid shortkey");
            return None;
        }
        let when = match entry.when.as_deref() {
            None | Some("") => None,
            Some("Explorer") => Some("Explorer"),
            Some("Terminal") => Some("Terminal"),
            Some("Editor") => Some("Editor"),
            Some(other) => { tracing::warn!(when = other, "unsupported shortkey context"); return None; }
        };
        let command = super::commands::command_descriptor(&entry.action);
        match command {
            Some(command) => Some(command.key_binding(key, when)),
            None => { tracing::warn!(action = entry.action, "unknown shortkey action"); None }
        }
    });
    let bindings: Vec<_> = bindings.collect();
    cx.clear_key_bindings();
    KIT_BINDINGS.with(|saved| cx.bind_keys(saved.borrow().iter().cloned()));
    cx.bind_keys(bindings);
    // The window Root binds Tab to focus-next, which lands on the File menu.
    // A Terminal-context binding is more specific and wins while the shell is focused.
    cx.bind_keys([
        KeyBinding::new("tab", TerminalInputTab, Some("Terminal")),
        KeyBinding::new("shift-tab", TerminalInputBacktab, Some("Terminal")),
        KeyBinding::new("f2", RenameSymbol, Some("Editor")),
        KeyBinding::new("ctrl-.", CodeActions, Some("Editor")),
    ]);
}

/// Native actions understood by the shortkeys adapter.
pub fn known_action(action: &str) -> bool {
    super::commands::command_descriptor(action).is_some()
}

/// Command identifiers suitable for the keybinding editor's action picker.
pub fn command_ids() -> Vec<(String, String)> {
    super::commands::all_command_descriptors().into_iter().map(|command| (command.id, command.label)).collect()
}

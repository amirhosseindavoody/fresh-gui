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
        let binding = match entry.action.as_str() {
            "NewTerminal" => KeyBinding::new(key, NewTerminal, when),
            "NewFile" => KeyBinding::new(key, NewFile, when),
            "SplitTerminal" => KeyBinding::new(key, SplitTerminal, when),
            "Complete" => KeyBinding::new(key, Complete, when),
            "ShowHover" => KeyBinding::new(key, ShowHover, when),
            "SignatureHelp" => KeyBinding::new(key, SignatureHelp, when),
            "GoToDefinition" => KeyBinding::new(key, GoToDefinition, when),
            "GoToDeclaration" => KeyBinding::new(key, GoToDeclaration, when),
            "GoToTypeDefinition" => KeyBinding::new(key, GoToTypeDefinition, when),
            "GoToImplementation" => KeyBinding::new(key, GoToImplementation, when),
            "FindReferences" => KeyBinding::new(key, FindReferences, when),
            "DocumentSymbols" => KeyBinding::new(key, DocumentSymbols, when),
            "WorkspaceSymbols" => KeyBinding::new(key, WorkspaceSymbols, when),
            "NavigateBack" => KeyBinding::new(key, NavigateBack, when),
            "NavigateForward" => KeyBinding::new(key, NavigateForward, when),
            "FormatDocument" => KeyBinding::new(key, FormatDocument, when),
            "SearchProject" => KeyBinding::new(key, SearchProject, when),
            "FindInBuffer" => KeyBinding::new(key, FindInBuffer, when),
            "ReplaceInBuffer" => KeyBinding::new(key, ReplaceInBuffer, when),
            "QueryReplace" => KeyBinding::new(key, QueryReplace, when),
            "ClearSearchHighlights" => KeyBinding::new(key, ClearSearchHighlights, when),
            "NextSearchMatch" => KeyBinding::new(key, NextSearchMatch, when),
            "PreviousSearchMatch" => KeyBinding::new(key, PreviousSearchMatch, when),
            "ToggleWordWrap" => KeyBinding::new(key, ToggleWordWrap, when),
            "CloseTab" => KeyBinding::new(key, CloseTab, when),
            "CloseAllEditors" => KeyBinding::new(key, CloseAllEditors, when),
            "CloseAllTerminals" => KeyBinding::new(key, CloseAllTerminals, when),
            "CloseAllOtherTerminals" => KeyBinding::new(key, CloseAllOtherTerminals, when),
            "CloseAllOtherTabs" => KeyBinding::new(key, CloseAllOtherTabs, when),
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
            "DeleteExplorer" => KeyBinding::new(key, DeleteExplorer, when),
            "AskCopilot" => KeyBinding::new(key, AskCopilot, when),
            "TerminalCopyOrInterrupt" => KeyBinding::new(key, TerminalCopyOrInterrupt, when),
            "TogglePinTab" => KeyBinding::new(key, TogglePinTab, when),
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
            "RestartServer" => KeyBinding::new(key, RestartServer, when),
            "ReloadConfig" => KeyBinding::new(key, ReloadConfig, when),
            "QuitClient" => KeyBinding::new(key, QuitClient, when),
            unknown => {
                tracing::warn!(action = unknown, "unknown shortkey action");
                return None;
            }
        };
        Some(binding)
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
    ]);
}

/// Native actions understood by the shortkeys adapter.
pub fn known_action(action: &str) -> bool {
    matches!(
        action,
        "NewTerminal"
            | "NewFile"
            | "SplitTerminal"
            | "FormatDocument"
            | "Complete"
            | "ShowHover"
            | "SignatureHelp"
            | "SearchProject"
            | "FindInBuffer"
            | "ReplaceInBuffer"
            | "QueryReplace"
            | "ClearSearchHighlights"
            | "NextSearchMatch"
            | "PreviousSearchMatch"
            | "GoToDefinition"
            | "GoToDeclaration"
            | "GoToTypeDefinition"
            | "GoToImplementation"
            | "FindReferences"
            | "DocumentSymbols"
            | "WorkspaceSymbols"
            | "NavigateBack"
            | "NavigateForward"
            | "ToggleWordWrap"
            | "CloseTab"
            | "CloseAllEditors"
            | "CloseAllTerminals"
            | "CloseAllOtherTerminals"
            | "CloseAllOtherTabs"
            | "SaveBuffer"
            | "ToggleSidebar"
            | "ToggleCommandPalette"
            | "GoToFile"
            | "OpenSettings"
            | "OpenDefaultSettings"
            | "Reconnect"
            | "Disconnect"
            | "NextTab"
            | "PrevTab"
            | "CopyExplorer"
            | "PasteExplorer"
            | "DeleteExplorer"
            | "AskCopilot"
            | "TerminalCopyOrInterrupt"
            | "TogglePinTab"
            | "FilterExplorer"
            | "ClearExplorerInput"
            | "NewWorkspace"
            | "RenameWorkspace"
            | "CloseWorkspace"
            | "ZoomInContent"
            | "ZoomOutContent"
            | "ResetContentZoom"
            | "ZoomInUi"
            | "ZoomOutUi"
            | "ResetUiZoom"
            | "StopServer"
            | "RestartServer"
            | "ReloadConfig"
            | "QuitClient"
    )
}

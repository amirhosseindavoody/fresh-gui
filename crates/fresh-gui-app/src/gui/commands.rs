//! Shared command metadata for the palette and keybinding editor.
//!
//! Native descriptors own both the display metadata and the factories used by GPUI.
//! Future plugins can register namespaced descriptors here without adding a second
//! command lookup table; executing plugin commands still needs a protocol in #158.

use gpui::{Action, KeyBinding};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::actions::*;
use fresh_gui_protocol::{
    CAP_EDITOR_SEARCH, CAP_FILE_FINDER, CAP_LSP_NAVIGATION, CAP_LSP_REQUESTS,
    CAP_PROJECT_SEARCH,
};

pub type ActionFactory = Arc<dyn Fn() -> Box<dyn Action> + Send + Sync>;
pub type BindingFactory = Arc<dyn Fn(&str, Option<&'static str>) -> KeyBinding + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandContext {
    Editor,
    Explorer,
    Terminal,
}

#[derive(Clone)]
pub struct CommandDescriptor {
    pub id: String,
    pub label: String,
    pub context: Option<CommandContext>,
    pub capability: Option<String>,
    pub action: ActionFactory,
    pub binding: BindingFactory,
}

impl CommandDescriptor {
    pub fn action(&self) -> Box<dyn Action> { (self.action)() }
    pub fn key_binding(&self, key: &str, when: Option<&'static str>) -> KeyBinding {
        (self.binding)(key, when)
    }
}

/// Mutable registry. Plugin IDs must be namespaced (`plugin.command`) and unique.
#[derive(Default, Clone)]
pub struct CommandRegistry {
    commands: BTreeMap<String, CommandDescriptor>,
    builtin_ids: BTreeSet<String>,
}

thread_local! {
    static COMMANDS: RefCell<CommandRegistry> = RefCell::new(CommandRegistry::with_builtins());
}

/// Register a command in the process-wide GPUI command registry. This is the
/// extension point for plugin descriptors when #158 adds their execution path.
#[allow(dead_code)] // Public extension hook for the plugin command protocol in #158.
pub fn register_command(descriptor: CommandDescriptor) -> Result<(), String> {
    COMMANDS.with(|commands| commands.borrow_mut().register(descriptor))
}

#[allow(dead_code)] // Public extension hook for the plugin command protocol in #158.
pub fn unregister_command(id: &str) -> Option<CommandDescriptor> {
    COMMANDS.with(|commands| commands.borrow_mut().unregister(id))
}

pub fn command_descriptor(id: &str) -> Option<CommandDescriptor> {
    COMMANDS.with(|commands| commands.borrow().get(id).cloned())
}

pub fn all_command_descriptors() -> Vec<CommandDescriptor> {
    COMMANDS.with(|commands| commands.borrow().all().cloned().collect())
}

impl CommandRegistry {
    pub fn with_builtins() -> Self {
        let mut registry = Self::default();
        for descriptor in builtin_commands() {
            registry.builtin_ids.insert(descriptor.id.clone());
            registry.commands.insert(descriptor.id.clone(), descriptor);
        }
        registry
    }

    #[allow(dead_code)] // Used through the public process-wide registration hook in #158.
    pub fn register(&mut self, descriptor: CommandDescriptor) -> Result<(), String> {
        if !descriptor.id.contains('.') || descriptor.id.starts_with('.') || descriptor.id.ends_with('.') {
            return Err("command id must be namespaced".into());
        }
        if self.commands.contains_key(&descriptor.id) {
            return Err(format!("command already registered: {}", descriptor.id));
        }
        self.commands.insert(descriptor.id.clone(), descriptor);
        Ok(())
    }

    pub fn unregister(&mut self, id: &str) -> Option<CommandDescriptor> {
        if self.builtin_ids.contains(id) {
            return None;
        }
        self.commands.remove(id)
    }

    pub fn get(&self, id: &str) -> Option<&CommandDescriptor> { self.commands.get(id) }

    pub fn all(&self) -> impl Iterator<Item = &CommandDescriptor> { self.commands.values() }

    pub fn available(&self, context: Option<CommandContext>, capabilities: &[String]) -> Vec<CommandDescriptor> {
        self.commands.values().filter(|command| {
            command.context.is_none_or(|required| context == Some(required))
                && command.capability.as_ref().is_none_or(|required| capabilities.iter().any(|cap| cap == required))
        }).cloned().collect()
    }
}

/// Built-in descriptors, in display order. The palette can apply context and
/// capability filtering with [`CommandRegistry::available`].
pub fn builtin_commands() -> Vec<CommandDescriptor> {
    macro_rules! command {
        ($id:literal, $label:literal, $action:ident) => {
            descriptor($id, $label, None, None, || Box::new($action), |key, when| KeyBinding::new(key, $action, when))
        };
        ($id:literal, $label:literal, $action:ident, $context:expr, $cap:expr) => {
            descriptor($id, $label, $context, $cap, || Box::new($action), |key, when| KeyBinding::new(key, $action, when))
        };
    }
    vec![
        command!("NewTerminal", "New Terminal", NewTerminal), command!("NewFile", "New File", NewFile),
        command!("SplitTerminal", "Split Terminal Vertically", SplitTerminal, Some(CommandContext::Terminal), None), command!("FormatDocument", "Format Document", FormatDocument, Some(CommandContext::Editor), Some(CAP_LSP_REQUESTS)),
        command!("Complete", "Complete", Complete, Some(CommandContext::Editor), Some(CAP_LSP_REQUESTS)), command!("ShowHover", "Show Hover", ShowHover, Some(CommandContext::Editor), Some(CAP_LSP_REQUESTS)),
        command!("SignatureHelp", "Signature Help", SignatureHelp, Some(CommandContext::Editor), Some(CAP_LSP_REQUESTS)), command!("GoToDefinition", "Go to Definition", GoToDefinition, Some(CommandContext::Editor), Some(CAP_LSP_NAVIGATION)),
        command!("GoToDeclaration", "Go to Declaration", GoToDeclaration, Some(CommandContext::Editor), Some(CAP_LSP_NAVIGATION)), command!("GoToTypeDefinition", "Go to Type Definition", GoToTypeDefinition, Some(CommandContext::Editor), Some(CAP_LSP_NAVIGATION)),
        command!("GoToImplementation", "Go to Implementation", GoToImplementation, Some(CommandContext::Editor), Some(CAP_LSP_NAVIGATION)), command!("FindReferences", "Find References", FindReferences, Some(CommandContext::Editor), Some(CAP_LSP_NAVIGATION)),
        command!("DocumentSymbols", "Document Symbols", DocumentSymbols, Some(CommandContext::Editor), Some(CAP_LSP_NAVIGATION)), command!("WorkspaceSymbols", "Workspace Symbols", WorkspaceSymbols, Some(CommandContext::Editor), Some(CAP_LSP_NAVIGATION)),
        command!("NavigateBack", "Navigate Back", NavigateBack), command!("NavigateForward", "Navigate Forward", NavigateForward),
        command!("ToggleWordWrap", "Toggle Word Wrap", ToggleWordWrap), command!("FindInBuffer", "Find in Buffer", FindInBuffer, Some(CommandContext::Editor), Some(CAP_EDITOR_SEARCH)),
        command!("SearchProject", "Search in Workspace", SearchProject, None, Some(CAP_PROJECT_SEARCH)), command!("ReplaceInBuffer", "Replace in Buffer", ReplaceInBuffer, Some(CommandContext::Editor), Some(CAP_EDITOR_SEARCH)),
        command!("QueryReplace", "Query Replace", QueryReplace, Some(CommandContext::Editor), Some(CAP_EDITOR_SEARCH)), command!("ClearSearchHighlights", "Clear Search Highlights", ClearSearchHighlights, Some(CommandContext::Editor), Some(CAP_EDITOR_SEARCH)),
        command!("NextSearchMatch", "Next Search Match", NextSearchMatch, Some(CommandContext::Editor), Some(CAP_EDITOR_SEARCH)), command!("PreviousSearchMatch", "Previous Search Match", PreviousSearchMatch, Some(CommandContext::Editor), Some(CAP_EDITOR_SEARCH)),
        command!("TerminalInputTab", "Terminal Tab", TerminalInputTab, Some(CommandContext::Terminal), None), command!("TerminalInputBacktab", "Terminal Backtab", TerminalInputBacktab, Some(CommandContext::Terminal), None),
        command!("CloseTab", "Close Tab", CloseTab), command!("CloseAllEditors", "Close All Editors", CloseAllEditors), command!("CloseAllTerminals", "Close All Terminals", CloseAllTerminals),
        command!("CloseAllOtherTerminals", "Close All Other Terminals", CloseAllOtherTerminals), command!("CloseAllOtherTabs", "Close All Other Tabs", CloseAllOtherTabs),
        command!("SaveBuffer", "Save", SaveBuffer, Some(CommandContext::Editor), None), command!("ToggleSidebar", "Toggle Sidebar", ToggleSidebar),
        command!("ToggleCommandPalette", "Toggle Command Palette", ToggleCommandPalette), command!("GoToFile", "Go to File…", GoToFile, None, Some(CAP_FILE_FINDER)),
        command!("OpenSettings", "Open Settings", OpenSettings), command!("OpenDefaultSettings", "Open Default Settings", OpenDefaultSettings),
        command!("Reconnect", "Reconnect", Reconnect), command!("Disconnect", "Disconnect", Disconnect), command!("NextTab", "Next Tab", NextTab), command!("PrevTab", "Previous Tab", PrevTab),
        command!("CopyExplorer", "Copy Explorer", CopyExplorer, Some(CommandContext::Explorer), None), command!("PasteExplorer", "Paste Explorer", PasteExplorer, Some(CommandContext::Explorer), None),
        command!("DeleteExplorer", "Delete Explorer Item", DeleteExplorer, Some(CommandContext::Explorer), None), command!("AskCopilot", "Ask Copilot…", AskCopilot),
        command!("TerminalCopyOrInterrupt", "Copy or Interrupt Terminal", TerminalCopyOrInterrupt, Some(CommandContext::Terminal), None), command!("TogglePinTab", "Pin or Unpin Tab", TogglePinTab),
        command!("FilterExplorer", "Filter Explorer", FilterExplorer, Some(CommandContext::Explorer), None), command!("ClearExplorerInput", "Clear Explorer Filter", ClearExplorerInput, Some(CommandContext::Explorer), None),
        command!("NewWorkspace", "New Workspace", NewWorkspace), command!("RenameWorkspace", "Rename Workspace", RenameWorkspace), command!("CloseWorkspace", "Close Workspace", CloseWorkspace),
        command!("ZoomInContent", "Zoom In Panel", ZoomInContent), command!("ZoomOutContent", "Zoom Out Panel", ZoomOutContent), command!("ResetContentZoom", "Reset Panel Zoom", ResetContentZoom),
        command!("ZoomInUi", "Zoom In UI", ZoomInUi), command!("ZoomOutUi", "Zoom Out UI", ZoomOutUi), command!("ResetUiZoom", "Reset UI Zoom", ResetUiZoom),
        command!("StopServer", "Stop Server", StopServer), command!("RestartServer", "Restart Server", RestartServer), command!("ReloadConfig", "Reload Config", ReloadConfig), command!("QuitClient", "Quit Client", QuitClient),
        command!("SwitchBuffer", "Switch Buffer", SwitchBuffer, Some(CommandContext::Editor), None), command!("GoToLine", "Go to Line…", GoToLine, Some(CommandContext::Editor), None),
    ]
}

/// Query the built-in registry for palette-visible commands. `context: None`
/// includes context-independent commands only; callers should pass the active
/// focus context to include commands scoped to that surface.
pub fn command_descriptors(
    context: Option<CommandContext>,
    capabilities: &[String],
) -> Vec<CommandDescriptor> {
    COMMANDS.with(|commands| commands.borrow().available(context, capabilities))
}

fn descriptor(
    id: &str, label: &str, context: Option<CommandContext>, capability: Option<&str>,
    action: impl Fn() -> Box<dyn Action> + Send + Sync + 'static,
    binding: impl Fn(&str, Option<&'static str>) -> KeyBinding + Send + Sync + 'static,
) -> CommandDescriptor {
    CommandDescriptor { id: id.into(), label: label.into(), context, capability: capability.map(str::to_owned), action: Arc::new(action), binding: Arc::new(binding) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plugin(id: &str) -> CommandDescriptor {
        descriptor(id, "Plugin command", None, None, || Box::new(NewFile), |key, when| KeyBinding::new(key, NewFile, when))
    }

    #[test]
    fn plugin_commands_register_and_unregister_without_collisions() {
        let mut registry = CommandRegistry::with_builtins();
        assert!(registry.register(plugin("sample.run")).is_ok());
        assert!(registry.register(plugin("sample.run")).is_err());
        assert!(registry.register(plugin("unqualified")).is_err());
        assert!(registry.unregister("sample.run").is_some());
        assert!(registry.unregister("sample.run").is_none());
        assert!(registry.unregister("NewFile").is_none());
        assert!(registry.get("NewFile").is_some());
    }

    #[test]
    fn filters_by_context_and_capability() {
        let registry = CommandRegistry::with_builtins();
        let no_caps: Vec<String> = vec![];
        assert!(!registry.available(Some(CommandContext::Editor), &no_caps).iter().any(|c| c.id == "GoToDefinition"));
        let caps = vec!["lsp.requests".to_owned()];
        assert!(registry.available(Some(CommandContext::Editor), &caps).iter().any(|c| c.id == "GoToDefinition"));
        assert!(!registry.available(Some(CommandContext::Terminal), &caps).iter().any(|c| c.id == "GoToDefinition"));
    }

    #[test]
    fn defaults_include_shortkey_only_and_palette_commands() {
        let registry = CommandRegistry::with_builtins();
        for id in ["ToggleCommandPalette", "SwitchBuffer", "GoToLine", "QuitClient"] {
            assert!(registry.get(id).is_some(), "missing {id}");
        }
    }

    #[test]
    fn embedded_default_shortkeys_reference_unique_registered_commands() {
        let defaults: serde_json::Value = jsonc_parser::parse_to_serde_value(
            include_str!("../../../fresh-gui/defaults/config.default.jsonc"),
            &jsonc_parser::ParseOptions::default(),
        ).expect("embedded defaults parse");
        let registry = CommandRegistry::with_builtins();
        let mut seen_bindings = BTreeSet::new();
        for binding in defaults["shortkeys"].as_array().expect("shortkeys array") {
            let id = binding["action"].as_str().expect("action id");
            assert!(registry.get(id).is_some(), "unregistered default action: {id}");
            let shortkey = binding["shortkey"].as_str().expect("shortkey");
            let context = binding["when"].as_str().unwrap_or("");
            assert!(seen_bindings.insert((shortkey, context)), "duplicate default binding: {shortkey} in {context}");
        }
        assert_eq!(builtin_commands().len(), builtin_commands().iter().map(|command| &command.id).collect::<BTreeSet<_>>().len());
    }
}

//! Editing command descriptors share the palette and configurable shortcuts.

use fresh_gui_protocol::{CAP_EDITOR_SMART_EDITING, EditorAction};
use gpui::{Action, KeyBinding};
use serde::Deserialize;

use super::commands::{CommandContext, CommandDescriptor, descriptor};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
pub enum Operation {
    Fresh(EditorAction),
    AddCursorAbove,
    AddCursorBelow,
}

#[derive(Action, Clone, PartialEq, Eq, Deserialize)]
#[action(namespace = fresh_gui, no_json)]
pub struct EditingCommand {
    pub operation: Operation,
}

pub fn descriptors() -> Vec<CommandDescriptor> {
    use EditorAction::*;
    let fresh = [
        ("ExpandSelection", "Expand Selection", ExpandSelection),
        ("SelectWord", "Select Word", SelectWord),
        ("SelectLine", "Select Line", SelectLine),
        ("SmartHome", "Smart Home", SmartHome),
        ("SmartBackspace", "Smart Backspace", SmartBackspace),
        (
            "InsertNewline",
            "Insert Newline with Fresh Indentation",
            InsertNewline,
        ),
        ("InsertTab", "Indent with Fresh Settings", InsertTab),
        ("DedentSelection", "Dedent Selection", DedentSelection),
        ("DuplicateLine", "Duplicate Line", DuplicateLine),
        ("DeleteLine", "Delete Line", DeleteLine),
        ("MoveLineUp", "Move Line Up", MoveLineUp),
        ("MoveLineDown", "Move Line Down", MoveLineDown),
        ("ToggleComment", "Toggle Line Comment", ToggleComment),
        ("SortLines", "Sort Lines", SortLines),
        ("UniqueLines", "Remove Duplicate Lines", UniqueLines),
        ("ToUpperCase", "Convert to Uppercase", ToUpperCase),
        ("ToLowerCase", "Convert to Lowercase", ToLowerCase),
        ("ToggleCase", "Toggle Case", ToggleCase),
        (
            "GoToMatchingBracket",
            "Go to Matching Bracket",
            GoToMatchingBracket,
        ),
        (
            "SurroundParentheses",
            "Surround with Parentheses",
            SurroundParentheses,
        ),
        (
            "SurroundBrackets",
            "Surround with Brackets",
            SurroundBrackets,
        ),
        ("SurroundBraces", "Surround with Braces", SurroundBraces),
        (
            "SurroundDoubleQuotes",
            "Surround with Double Quotes",
            SurroundDoubleQuotes,
        ),
        (
            "SurroundSingleQuotes",
            "Surround with Single Quotes",
            SurroundSingleQuotes,
        ),
        (
            "SurroundBackticks",
            "Surround with Backticks",
            SurroundBackticks,
        ),
    ];
    let mut commands: Vec<_> = fresh
        .into_iter()
        .map(|(id, label, action)| {
            make_descriptor(
                id,
                label,
                Operation::Fresh(action),
                Some(CAP_EDITOR_SMART_EDITING),
            )
        })
        .collect();
    commands.extend([
        make_descriptor(
            "AddCursorAbove",
            "Add Cursor Above",
            Operation::AddCursorAbove,
            None,
        ),
        make_descriptor(
            "AddCursorBelow",
            "Add Cursor Below",
            Operation::AddCursorBelow,
            None,
        ),
    ]);
    commands
}

fn make_descriptor(
    id: &str,
    label: &str,
    operation: Operation,
    capability: Option<&str>,
) -> CommandDescriptor {
    descriptor(
        id,
        label,
        Some(CommandContext::Editor),
        capability,
        move || Box::new(EditingCommand { operation }),
        move |key, when| KeyBinding::new(key, EditingCommand { operation }, when),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gui::commands::CommandRegistry;

    #[test]
    fn editing_commands_share_factories_and_require_new_daemon_capability() {
        let registry = CommandRegistry::with_builtins();
        let old = registry.available(Some(CommandContext::Editor), &[]);
        let supported = registry.available(
            Some(CommandContext::Editor),
            &[CAP_EDITOR_SMART_EDITING.into()],
        );
        for descriptor in descriptors() {
            let registered = registry
                .get(&descriptor.id)
                .expect("registered editing command");
            let command = registered.action();
            let command = command
                .as_any()
                .downcast_ref::<EditingCommand>()
                .expect("editing action");
            assert_eq!(
                command.operation,
                descriptor
                    .action()
                    .as_any()
                    .downcast_ref::<EditingCommand>()
                    .unwrap()
                    .operation
            );
            assert!(supported.iter().any(|entry| entry.id == descriptor.id));
            assert_eq!(
                old.iter().any(|entry| entry.id == descriptor.id),
                descriptor.capability.is_none()
            );
        }
        assert!(
            !registry
                .available(
                    Some(CommandContext::Terminal),
                    &[CAP_EDITOR_SMART_EDITING.into()]
                )
                .iter()
                .any(|entry| descriptors().iter().any(|editing| editing.id == entry.id))
        );
    }
}

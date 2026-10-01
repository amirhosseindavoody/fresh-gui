use anyhow::{Context, Result, bail};
use fresh::services::lsp::async_handler::LspClientState;
use fresh_gui_protocol::{LanguageServerAction, LanguageServerState};

use super::{Editor, TrackedBuffer};

pub(super) fn language_servers(
    editor: &mut Editor,
    tracked: &std::collections::HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    action: LanguageServerAction,
    logs: &[String],
) -> Result<Vec<LanguageServerState>> {
    super::activate_tracked(editor, tracked, buffer_id)?;
    let buffer = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown or inactive buffer {buffer_id}"))?;
    let language = buffer
        .language
        .as_deref()
        .filter(|language| !language.is_empty())
        .ok_or_else(|| anyhow::anyhow!("buffer has no language server language"))?;
    let path = buffer.path.as_deref();

    let configs = editor
        .active_window()
        .lsp
        .get_configs(language)
        .map(|configs| configs.to_vec())
        .unwrap_or_default();
    if configs.is_empty() {
        bail!("no language servers are configured for {language}");
    }

    // Fresh owns process lifecycle. Its manager methods also preserve its
    // cooldown, restart, authority-routing, and per-server configuration rules.
    let mut action_errors = std::collections::HashMap::new();
    if action != LanguageServerAction::Status {
        if action == LanguageServerAction::Stop {
            // Language-level shutdown marks this language disabled so a
            // subsequent edit or buffer activation cannot auto-respawn it.
            let _ = editor.active_window_mut().lsp.shutdown_server(language);
            // Fresh's stop action also disables auto-start for this runtime
            // configuration. This covers the case where no process handle was
            // alive when Stop was requested, so shutdown_server had nothing to
            // mark as explicitly disabled.
            let stopped_configs = configs
                .iter()
                .cloned()
                .map(|mut config| {
                    config.auto_start = false;
                    config
                })
                .collect();
            editor.set_lsp_config(language.to_string(), stopped_configs);
        } else {
            let globally_enabled = editor.config().lsp_enabled;
            for config in &configs {
                let name = config.display_name();
                if !config.enabled {
                    action_errors.insert(name, "disabled in language-server settings".to_string());
                    continue;
                }
                if !globally_enabled {
                    action_errors.insert(
                        name,
                        "language servers are disabled in settings".to_string(),
                    );
                    continue;
                }
                if action == LanguageServerAction::Start
                    && manager_has_language_server(editor, language, &name)
                {
                    continue;
                }
                let command = executable(&config.command);
                let missing_binary =
                    !command.is_empty() && !fresh::services::lsp::command_exists(command);
                if let Some(reason) =
                    start_block_reason(config.enabled, globally_enabled, command, missing_binary)
                {
                    action_errors.insert(name, reason);
                    continue;
                }
                let (started, message) = editor
                    .active_window_mut()
                    .lsp
                    .manual_restart_server(language, &name, path);
                if !started {
                    action_errors.insert(name, message);
                }
            }
        }
    }

    let globally_enabled = editor.config().lsp_enabled;
    let manager = &editor.active_window().lsp;
    let handles = manager.get_handles(language);
    let workspace_count = tracked
        .values()
        .map(|buffer| &buffer.workspace_id)
        .collect::<std::collections::HashSet<_>>()
        .len();
    let progress = editor
        .active_window()
        .get_lsp_progress()
        .into_iter()
        .filter(|_| workspace_count == 1)
        .take(8)
        .map(|(_, title, message)| {
            let progress = message
                .filter(|message| !message.is_empty())
                .map(|message| format!("{title}: {message}"))
                .unwrap_or(title);
            bounded_text(&progress, 512)
        })
        .collect::<Vec<_>>();
    let mut states = Vec::with_capacity(configs.len());
    for config in configs {
        let name = config.display_name();
        let command = config.command.clone();
        let found = handles.iter().find(|handle| handle.name == name);
        let mut status = if let Some(error) = action_errors.remove(&name) {
            error
        } else if !config.enabled {
            "disabled".to_string()
        } else if !globally_enabled {
            "language servers are disabled in settings".to_string()
        } else if let Some(handle) = found {
            client_status(handle.handle.state())
        } else if let Some(reason) = start_block_reason(
            config.enabled,
            globally_enabled,
            executable(&command),
            !executable(&command).is_empty()
                && !fresh::services::lsp::command_exists(executable(&command)),
        ) {
            reason
        } else {
            "stopped".to_string()
        };
        if !progress.is_empty() {
            status.push_str(" · window progress: ");
            status.push_str(&progress.join("; "));
        }
        states.push(LanguageServerState {
            name,
            language: language.to_string(),
            status,
            command,
            logs: logs.to_vec(),
        });
    }
    Ok(states)
}

fn manager_has_language_server(editor: &Editor, language: &str, name: &str) -> bool {
    editor
        .active_window()
        .lsp
        .get_handles(language)
        .iter()
        .any(|handle| handle.name == name)
}

fn client_status(state: LspClientState) -> String {
    match state {
        LspClientState::Initial | LspClientState::Starting => "starting",
        LspClientState::Initializing => "initializing",
        LspClientState::Running => "running",
        LspClientState::Stopping => "stopping",
        LspClientState::Stopped => "stopped",
        LspClientState::Error => "error",
    }
    .to_string()
}

fn executable(command: &str) -> &str {
    command
}

fn start_block_reason(
    language_enabled: bool,
    globally_enabled: bool,
    command: &str,
    missing_binary: bool,
) -> Option<String> {
    if !language_enabled {
        Some("disabled in language-server settings".to_string())
    } else if !globally_enabled {
        Some("language servers are disabled in settings".to_string())
    } else if command.is_empty() {
        Some("configure a server command in language-server settings".to_string())
    } else if missing_binary {
        Some(format!(
            "missing binary: install `{command}` or update its configured command"
        ))
    } else {
        None
    }
}

fn bounded_text(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_string();
    }
    let mut end = max_bytes.saturating_sub(3);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_lifecycle_states_are_reported() {
        assert_eq!(client_status(LspClientState::Initializing), "initializing");
        assert_eq!(client_status(LspClientState::Running), "running");
        assert_eq!(client_status(LspClientState::Error), "error");
    }

    #[test]
    fn configured_executable_is_extracted_for_actionable_status() {
        assert_eq!(
            executable(r"C:\Program Files\server.exe"),
            r"C:\Program Files\server.exe"
        );
        assert_eq!(executable(""), "");
    }

    #[test]
    fn disabled_servers_cannot_be_started_manually() {
        assert_eq!(
            start_block_reason(false, true, "rust-analyzer", false).as_deref(),
            Some("disabled in language-server settings")
        );
        assert_eq!(
            start_block_reason(true, false, "rust-analyzer", false).as_deref(),
            Some("language servers are disabled in settings")
        );
    }

    #[test]
    fn missing_server_commands_are_actionable() {
        assert!(
            start_block_reason(true, true, "", false)
                .as_deref()
                .unwrap()
                .contains("configure a server command")
        );
        assert!(
            start_block_reason(true, true, "rust-analyzer", true)
                .as_deref()
                .unwrap()
                .contains("install `rust-analyzer`")
        );
    }

    #[test]
    fn progress_text_is_bounded_on_utf8_boundaries() {
        let bounded = bounded_text(&format!("{}é", "x".repeat(512)), 512);
        assert!(bounded.len() <= 512);
        assert!(bounded.is_char_boundary(bounded.len()));
    }
}

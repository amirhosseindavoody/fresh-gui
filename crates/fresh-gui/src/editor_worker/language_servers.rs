use anyhow::{Context, Result, bail};
use fresh::services::lsp::async_handler::LspClientState;
use fresh_gui_protocol::{LanguageServerAction, LanguageServerState};

use super::{Editor, TrackedBuffer};

pub(super) fn language_servers(
    editor: &mut Editor,
    tracked: &std::collections::HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    action: LanguageServerAction,
) -> Result<Vec<LanguageServerState>> {
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
        .map(<[_]>::to_vec)
        .unwrap_or_default();
    if configs.is_empty() {
        bail!("no language servers are configured for {language}");
    }

    // Fresh owns process lifecycle. Its manager methods also preserve its
    // cooldown, restart, authority-routing, and per-server configuration rules.
    if action != LanguageServerAction::Status {
        for config in &configs {
            let name = config.display_name();
            match action {
                LanguageServerAction::Status => unreachable!(),
                LanguageServerAction::Start => {
                    if !manager_has_language_server(editor, language, &name) {
                        let command = config.command.split_whitespace().next().unwrap_or("");
                        if command.is_empty()
                            || !editor
                                .active_window()
                                .lsp
                                .command_exists_via_authority(command)
                        {
                            continue;
                        }
                        let _ = editor
                            .active_window_mut()
                            .lsp
                            .manual_restart_server(language, &name, path);
                    }
                }
                LanguageServerAction::Stop => {
                    let _ = editor
                        .active_window_mut()
                        .lsp
                        .shutdown_server_by_name(language, &name);
                }
                LanguageServerAction::Restart => {
                    let command = config.command.split_whitespace().next().unwrap_or("");
                    if command.is_empty()
                        || !editor
                            .active_window()
                            .lsp
                            .command_exists_via_authority(command)
                    {
                        continue;
                    }
                    let _ = editor
                        .active_window_mut()
                        .lsp
                        .manual_restart_server(language, &name, path);
                }
            }
        }
    }

    let manager = &editor.active_window().lsp;
    let handles = manager.get_handles(language);
    let mut states = Vec::with_capacity(configs.len());
    for config in configs {
        let name = config.display_name();
        let command = config.command.clone();
        let found = handles.iter().find(|handle| handle.name == name);
        let status = if !config.enabled {
            "disabled".to_string()
        } else if let Some(handle) = found {
            client_status(handle.handle.state())
        } else if command.is_empty() {
            "missing command: configure an executable in language-server settings".to_string()
        } else {
            let executable = command.split_whitespace().next().unwrap_or("");
            if !executable.is_empty() && !manager.command_exists_via_authority(executable) {
                format!("missing binary: install `{executable}` or update its configured command")
            } else {
                "stopped".to_string()
            }
        };
        states.push(LanguageServerState {
            name,
            language: language.to_string(),
            status,
            command,
            // Fresh exposes process diagnostics through its logging subsystem,
            // but does not provide a per-server log reader API here.
            logs: Vec::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn executable_name_is_taken_from_configured_command() {
        assert_eq!(executable("rust-analyzer --stdio"), "rust-analyzer");
        assert_eq!(executable(""), "");
    }

    #[test]
    fn client_lifecycle_states_are_reported() {
        assert_eq!(client_status(LspClientState::Initializing), "initializing");
        assert_eq!(client_status(LspClientState::Running), "running");
        assert_eq!(client_status(LspClientState::Error), "error");
    }

    fn executable(command: &str) -> &str {
        command.split_whitespace().next().unwrap_or("")
    }
}

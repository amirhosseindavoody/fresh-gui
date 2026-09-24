//! Pick an executable for a new PTY.
//!
//! Fresh `detect_shell` (`vendor/fresh/crates/fresh-editor/src/services/terminal/manager.rs`)
//! returns `$SHELL` or `/bin/sh` on Unix without checking that the file exists.
//! `select_windows_shell` only walks Windows candidates. fresh-gui PTYs do not
//! use Fresh's `TerminalManager` (see `docs/FRESH.md`), and the Unix default
//! here is `zsh`, so a missing shell is handled in this module.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::Result;

use crate::config::{Config, DEFAULT_SHELL_COMMAND};

/// Shell chosen for [`crate::pty::PtySession::spawn`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPtyShell {
    pub command: String,
    pub args: Vec<String>,
    /// When true, [`crate::pty`] applies interactive / OSC 7 setup.
    pub apply_osc7: bool,
    /// Why this command was chosen (for logs).
    pub reason: String,
    /// Earlier candidates that were missing or not executable.
    pub skipped: Vec<SkippedShell>,
}

/// A candidate that could not be launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedShell {
    pub command: String,
    pub origin: &'static str,
    pub problem: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Probe {
    Usable,
    NotFound,
    NotExecutable,
}

struct Candidate {
    command: String,
    args: Vec<String>,
    apply_osc7: bool,
    origin: &'static str,
    reason: &'static str,
}

/// Resolve the shell for a new PTY.
///
/// Unix order: client override (when set) → configured command (default `zsh`)
/// → `$SHELL` when it names a different usable binary → `bash` → `sh`.
/// Windows probes the configured command, then `pwsh`, `powershell`,
/// `%COMSPEC%`, and `cmd`.
pub fn resolve_for_spawn(client_shell: Option<&str>, config: &Config) -> Result<ResolvedPtyShell> {
    let (configured, args) = config.resolve_shell();

    #[cfg(windows)]
    {
        let comspec = std::env::var("COMSPEC").ok();
        return resolve_windows(client_shell, &configured, &args, comspec.as_deref(), |cmd| {
            probe_windows_command(cmd, std::env::var_os("PATH").as_deref())
        });
    }

    #[cfg(unix)]
    {
        let explicit = config
            .terminal
            .shell
            .as_ref()
            .is_some_and(|shell| !shell.command.is_empty());
        let env_shell = std::env::var("SHELL").ok();
        resolve_unix(
            client_shell,
            &configured,
            &args,
            explicit,
            env_shell.as_deref(),
            |cmd| probe_command(cmd, std::env::var_os("PATH").as_deref()),
        )
    }
}

pub fn log_resolved(resolved: &ResolvedPtyShell) {
    use tracing::{info, warn};

    if resolved.skipped.is_empty() {
        info!(
            shell = %resolved.command,
            reason = %resolved.reason,
            "pty shell selected"
        );
    } else {
        warn!(
            shell = %resolved.command,
            reason = %resolved.reason,
            skipped = %format_skipped(&resolved.skipped),
            "pty shell fell back"
        );
    }
}

pub fn spawn_failure_message(
    shell: &str,
    reason: &str,
    skipped: &[SkippedShell],
    err: &dyn std::fmt::Display,
) -> String {
    let skipped_note = if skipped.is_empty() {
        String::new()
    } else {
        format!(" Skipped: {}.", format_skipped(skipped))
    };
    format!(
        "failed to spawn shell `{shell}` ({reason}).{skipped_note} {err}. Set \"terminal.shell.command\" in config.json if this shell cannot start"
    )
}

#[cfg(windows)]
fn resolve_windows(
    client_shell: Option<&str>,
    configured: &str,
    args: &[String],
    comspec: Option<&str>,
    probe: impl FnMut(&str) -> Probe,
) -> Result<ResolvedPtyShell> {
    let mut candidates = Vec::new();
    if let Some(command) = trimmed(client_shell) {
        push_unique(&mut candidates, Candidate {
            command,
            args: Vec::new(),
            apply_osc7: true,
            origin: "client shell",
            reason: "client shell override",
        });
    }
    push_unique(&mut candidates, Candidate {
        command: configured.to_owned(),
        apply_osc7: args.is_empty(),
        args: args.to_vec(),
        origin: "configured shell",
        reason: "configured terminal.shell.command",
    });
    for (command, origin, reason) in [
        (Some("pwsh"), "fallback", "fallback pwsh"),
        (Some("powershell"), "fallback", "fallback powershell"),
        (comspec, "%COMSPEC%", "%COMSPEC%"),
        (Some("cmd"), "fallback", "fallback cmd"),
    ] {
        if let Some(command) = trimmed(command) {
            push_unique(&mut candidates, Candidate {
                command, args: Vec::new(), apply_osc7: true, origin, reason,
            });
        }
    }
    select_first_usable(candidates, probe)
}

#[cfg(unix)]
fn resolve_unix(
    client_shell: Option<&str>,
    configured: &str,
    configured_args: &[String],
    configured_is_explicit: bool,
    env_shell: Option<&str>,
    probe: impl FnMut(&str) -> Probe,
) -> Result<ResolvedPtyShell> {
    let candidates = unix_candidates(
        client_shell,
        configured,
        configured_args,
        configured_is_explicit,
        env_shell,
    );
    select_first_usable(candidates, probe)
}

fn select_first_usable(
    candidates: Vec<Candidate>,
    mut probe: impl FnMut(&str) -> Probe,
) -> Result<ResolvedPtyShell> {
    let mut skipped = Vec::new();
    for candidate in candidates {
        match probe(&candidate.command) {
            Probe::Usable => {
                return Ok(ResolvedPtyShell {
                    command: candidate.command,
                    args: candidate.args,
                    apply_osc7: candidate.apply_osc7,
                    reason: candidate.reason.to_owned(),
                    skipped,
                });
            }
            other => skipped.push(SkippedShell {
                problem: problem_label(&candidate.command, other),
                command: candidate.command,
                origin: candidate.origin,
            }),
        }
    }
    Err(anyhow::anyhow!("{}", no_usable_shell_message(&skipped)))
}

#[cfg(unix)]
fn unix_candidates(
    client_shell: Option<&str>,
    configured: &str,
    configured_args: &[String],
    configured_is_explicit: bool,
    env_shell: Option<&str>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    if let Some(command) = trimmed(client_shell) {
        push_unique(
            &mut out,
            Candidate {
                command,
                args: Vec::new(),
                apply_osc7: true,
                origin: "client shell",
                reason: "client shell override",
            },
        );
    }

    let configured_command = {
        let trimmed_cmd = configured.trim();
        if trimmed_cmd.is_empty() {
            DEFAULT_SHELL_COMMAND.to_owned()
        } else {
            trimmed_cmd.to_owned()
        }
    };
    let (origin, reason) = if configured_is_explicit {
        ("configured shell", "configured terminal.shell.command")
    } else {
        ("default shell", "default shell")
    };
    push_unique(
        &mut out,
        Candidate {
            apply_osc7: configured_args.is_empty(),
            command: configured_command,
            args: configured_args.to_vec(),
            origin,
            reason,
        },
    );

    if let Some(command) = trimmed(env_shell) {
        push_unique(
            &mut out,
            Candidate {
                command,
                args: Vec::new(),
                apply_osc7: true,
                origin: "$SHELL",
                reason: "$SHELL",
            },
        );
    }

    push_unique(
        &mut out,
        Candidate {
            command: "bash".to_owned(),
            args: Vec::new(),
            apply_osc7: true,
            origin: "fallback",
            reason: "fallback bash",
        },
    );
    // `configure_shell_cmd` treats `sh` like bash (`--rcfile`). Debian/Ubuntu
    // `/bin/sh` is dash, which rejects that flag, so the last resort starts
    // with no extra args. The PTY slave is a tty, so `sh` is still interactive.
    push_unique(
        &mut out,
        Candidate {
            command: "sh".to_owned(),
            args: Vec::new(),
            apply_osc7: false,
            origin: "fallback",
            reason: "fallback sh",
        },
    );
    out
}

fn push_unique(out: &mut Vec<Candidate>, candidate: Candidate) {
    if out
        .iter()
        .any(|existing| existing.command == candidate.command)
    {
        return;
    }
    out.push(candidate);
}

fn trimmed(value: Option<&str>) -> Option<String> {
    let value = value?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_owned())
    }
}

fn problem_label(command: &str, probe: Probe) -> &'static str {
    match probe {
        Probe::NotExecutable => "not executable",
        Probe::NotFound if has_path_separator(command) => "not found",
        Probe::NotFound => "not found on PATH",
        Probe::Usable => "usable",
    }
}

#[cfg(unix)]
fn probe_command(command: &str, path_env: Option<&OsStr>) -> Probe {
    let command = command.trim();
    if command.is_empty() {
        return Probe::NotFound;
    }
    if has_path_separator(command) {
        return probe_path(Path::new(command));
    }
    let Some(path_env) = path_env else {
        return Probe::NotFound;
    };
    let mut saw_unexecutable = false;
    for dir in std::env::split_paths(path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        match probe_path(&dir.join(command)) {
            Probe::Usable => return Probe::Usable,
            Probe::NotExecutable => saw_unexecutable = true,
            Probe::NotFound => {}
        }
    }
    if saw_unexecutable {
        Probe::NotExecutable
    } else {
        Probe::NotFound
    }
}

#[cfg(unix)]
fn probe_path(path: &Path) -> Probe {
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => {
            use std::os::unix::fs::PermissionsExt;
            if meta.permissions().mode() & 0o111 == 0 {
                Probe::NotExecutable
            } else {
                Probe::Usable
            }
        }
        Ok(_) => Probe::NotExecutable,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => Probe::NotExecutable,
        Err(_) => Probe::NotFound,
    }
}

#[cfg(windows)]
fn probe_windows_command(command: &str, path_env: Option<&OsStr>) -> Probe {
    let command = command.trim();
    if command.is_empty() {
        return Probe::NotFound;
    }
    let extensions = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    let extensions: Vec<_> = extensions.split(';').filter(|ext| !ext.is_empty()).collect();
    let candidates: Vec<_> = if has_path_separator(command) {
        vec![Path::new(command).to_path_buf()]
    } else {
        path_env
            .map(std::env::split_paths)
            .into_iter()
            .flatten()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| dir.join(command))
            .collect()
    };
    let mut blocked = false;
    for path in candidates {
        let mut paths = vec![path.clone()];
        if path.extension().is_none() {
            paths.extend(extensions.iter().map(|ext| {
                std::path::PathBuf::from(format!("{}{}", path.display(), ext))
            }));
        }
        for path in paths {
            match std::fs::metadata(&path) {
                Ok(meta) if meta.is_file() => return Probe::Usable,
                Ok(_) => blocked = true,
                Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => blocked = true,
                Err(_) => {}
            }
        }
    }
    if blocked { Probe::NotExecutable } else { Probe::NotFound }
}

fn has_path_separator(command: &str) -> bool {
    command.contains('/') || command.contains('\\')
}

fn format_skipped(skipped: &[SkippedShell]) -> String {
    skipped
        .iter()
        .map(|skipped| {
            format!(
                "{} ({}, {})",
                skipped.command, skipped.origin, skipped.problem
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn no_usable_shell_message(skipped: &[SkippedShell]) -> String {
    format!(
        "no usable shell for a new terminal. Tried: {}. Set \"terminal.shell.command\" in config.json to a shell that is installed and executable",
        format_skipped(skipped)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::path::{Path, PathBuf};

    #[cfg(unix)]
    fn map_probe<'a>(entries: &'a [(&'a str, Probe)]) -> impl FnMut(&str) -> Probe + 'a {
        move |command| {
            entries
                .iter()
                .find(|(name, _)| *name == command)
                .map(|(_, probe)| *probe)
                .unwrap_or(Probe::NotFound)
        }
    }

    #[cfg(unix)]
    fn unix(
        client: Option<&str>,
        configured: &str,
        args: &[&str],
        explicit: bool,
        env_shell: Option<&str>,
        entries: &[(&str, Probe)],
    ) -> Result<ResolvedPtyShell> {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        resolve_unix(
            client,
            configured,
            &args,
            explicit,
            env_shell,
            map_probe(entries),
        )
    }

    #[cfg(unix)]
    #[test]
    fn configured_shell_wins_when_it_is_executable() {
        let resolved = unix(
            None,
            "zsh",
            &["-l"],
            true,
            Some("/bin/bash"),
            &[("zsh", Probe::Usable), ("/bin/bash", Probe::Usable)],
        )
        .unwrap();
        assert_eq!(resolved.command, "zsh");
        assert_eq!(resolved.args, vec!["-l"]);
        assert!(!resolved.apply_osc7);
        assert_eq!(resolved.reason, "configured terminal.shell.command");
        assert!(resolved.skipped.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn configured_fish_bash_and_custom_path_are_honored() {
        for command in ["fish", "bash", "/opt/tools/my-shell"] {
            let resolved = unix(
                None, command, &[], true, Some("/bin/sh"),
                &[(command, Probe::Usable)],
            ).unwrap();
            assert_eq!(resolved.command, command);
            assert!(resolved.apply_osc7);
            assert!(resolved.skipped.is_empty());
        }
    }

    #[cfg(unix)]
    #[test]
    fn empty_configured_args_keep_osc7_setup() {
        let resolved = unix(None, "zsh", &[], true, None, &[("zsh", Probe::Usable)]).unwrap();
        assert!(resolved.apply_osc7);
        assert!(resolved.args.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn falls_back_to_env_shell_and_records_why() {
        let resolved = unix(
            None,
            "zsh",
            &["-l"],
            true,
            Some("/bin/bash"),
            &[
                ("zsh", Probe::NotFound),
                ("/bin/bash", Probe::Usable),
                ("bash", Probe::Usable),
            ],
        )
        .unwrap();
        assert_eq!(resolved.command, "/bin/bash");
        assert!(resolved.args.is_empty());
        assert!(resolved.apply_osc7);
        assert_eq!(resolved.reason, "$SHELL");
        assert_eq!(resolved.skipped.len(), 1);
        assert_eq!(resolved.skipped[0].command, "zsh");
        assert_eq!(resolved.skipped[0].origin, "configured shell");
        assert_eq!(resolved.skipped[0].problem, "not found on PATH");
    }

    #[cfg(unix)]
    #[test]
    fn skips_unusable_env_shell_then_uses_bash() {
        let resolved = unix(
            None,
            "zsh",
            &[],
            false,
            Some("/usr/local/bin/zsh"),
            &[
                ("zsh", Probe::NotFound),
                ("/usr/local/bin/zsh", Probe::NotExecutable),
                ("bash", Probe::Usable),
            ],
        )
        .unwrap();
        assert_eq!(resolved.command, "bash");
        assert_eq!(resolved.reason, "fallback bash");
        assert_eq!(resolved.skipped[0].origin, "default shell");
        assert_eq!(resolved.skipped[1].command, "/usr/local/bin/zsh");
        assert_eq!(resolved.skipped[1].origin, "$SHELL");
        assert_eq!(resolved.skipped[1].problem, "not executable");
    }

    #[cfg(unix)]
    #[test]
    fn falls_back_to_sh_without_bash_rcfile_args() {
        let resolved = unix(
            None,
            "zsh",
            &[],
            true,
            None,
            &[("zsh", Probe::NotFound), ("sh", Probe::Usable)],
        )
        .unwrap();
        assert_eq!(resolved.command, "sh");
        assert_eq!(resolved.reason, "fallback sh");
        assert!(!resolved.apply_osc7);
        assert!(resolved.args.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn client_override_is_tried_before_the_configured_shell() {
        let resolved = unix(
            Some(" fish "),
            "bash",
            &["-l"],
            true,
            Some("/bin/zsh"),
            &[("fish", Probe::NotFound), ("bash", Probe::Usable)],
        )
        .unwrap();
        assert_eq!(resolved.command, "bash");
        assert_eq!(resolved.args, vec!["-l"]);
        assert!(!resolved.apply_osc7);
        assert_eq!(resolved.skipped[0].command, "fish");
        assert_eq!(resolved.skipped[0].origin, "client shell");
    }

    #[cfg(unix)]
    #[test]
    fn client_override_is_used_when_it_exists() {
        let resolved = unix(
            Some("/bin/bash"),
            "zsh",
            &["-l"],
            true,
            None,
            &[("/bin/bash", Probe::Usable), ("zsh", Probe::Usable)],
        )
        .unwrap();
        assert_eq!(resolved.command, "/bin/bash");
        assert_eq!(resolved.reason, "client shell override");
        assert!(resolved.apply_osc7);
        assert!(resolved.skipped.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn identical_candidates_are_not_listed_twice() {
        let err = unix(
            None,
            "bash",
            &[],
            true,
            Some("bash"),
            &[("bash", Probe::NotFound), ("sh", Probe::NotFound)],
        )
        .unwrap_err();
        let message = err.to_string();
        assert_eq!(message.matches("bash (").count(), 1);
        assert!(message.contains("sh (fallback, not found on PATH)"));
        assert!(message.contains("terminal.shell.command"));
        assert!(!message.contains("spawn shell"));
    }

    #[cfg(unix)]
    #[test]
    fn every_candidate_fails_with_a_config_hint() {
        let err = unix(
            None,
            "/bin/zsh",
            &[],
            true,
            Some("/usr/bin/zsh"),
            &[
                ("/bin/zsh", Probe::NotFound),
                ("/usr/bin/zsh", Probe::NotExecutable),
                ("bash", Probe::NotFound),
                ("sh", Probe::NotFound),
            ],
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(message.starts_with("no usable shell for a new terminal. Tried: "));
        assert!(message.contains("/bin/zsh (configured shell, not found)"));
        assert!(message.contains("/usr/bin/zsh ($SHELL, not executable)"));
        assert!(message.contains("bash (fallback, not found on PATH)"));
        assert!(message.contains("sh (fallback, not found on PATH)"));
        assert!(message.contains("Set \"terminal.shell.command\" in config.json"));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_path_miss_is_not_described_as_a_path_search() {
        let resolved = unix(
            None,
            "/usr/bin/zsh",
            &[],
            true,
            None,
            &[("/usr/bin/zsh", Probe::NotFound), ("bash", Probe::Usable)],
        )
        .unwrap();
        assert_eq!(resolved.skipped[0].problem, "not found");
    }

    #[cfg(unix)]
    #[test]
    fn spawn_failure_names_the_shell_and_the_config_key() {
        let message = spawn_failure_message(
            "bash",
            "fallback bash",
            &[SkippedShell {
                command: "zsh".into(),
                origin: "configured shell",
                problem: "not found on PATH",
            }],
            &"No such file or directory (os error 2)",
        );
        assert!(message.contains("failed to spawn shell `bash` (fallback bash)"));
        assert!(message.contains("Skipped: zsh (configured shell, not found on PATH)"));
        assert!(message.contains("No such file or directory"));
        assert!(message.contains("terminal.shell.command"));
        assert!(!message.contains("spawn shell zsh"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_falls_back_from_missing_configured_shell() {
        let resolved = resolve_windows(
            None, "fish", &[], Some(r"C:\Windows\System32\cmd.exe"),
            |command| if command == "pwsh" { Probe::Usable } else { Probe::NotFound },
        ).unwrap();
        assert_eq!(resolved.command, "pwsh");
        assert_eq!(resolved.skipped[0].command, "fish");
        assert_eq!(resolved.skipped[0].problem, "not found on PATH");
        assert_eq!(resolved.reason, "fallback pwsh");
    }

    #[cfg(windows)]
    #[test]
    fn windows_keeps_installed_custom_path_and_args() {
        let resolved = resolve_windows(
            None, r"C:\Tools\fish.exe", &["--private".into()], None,
            |command| if command == r"C:\Tools\fish.exe" { Probe::Usable } else { Probe::NotFound },
        ).unwrap();
        assert_eq!(resolved.command, r"C:\Tools\fish.exe");
        assert_eq!(resolved.args, vec!["--private"]);
        assert!(!resolved.apply_osc7);
    }

    #[cfg(unix)]
    #[test]
    fn probe_sees_executable_bits_and_path_order() {
        let dir_a = temp_dir("a");
        let dir_b = temp_dir("b");
        let missing = dir_a.join("missing-shell");
        assert_eq!(
            probe_command(&missing.to_string_lossy(), None),
            Probe::NotFound
        );
        assert_eq!(
            probe_command(&dir_a.to_string_lossy(), None),
            Probe::NotExecutable
        );

        write_mode(&dir_a.join("plain-shell"), 0o644);
        write_mode(&dir_a.join("blocked-shell"), 0o644);
        write_mode(&dir_b.join("blocked-shell"), 0o755);
        write_mode(&dir_b.join("exec-shell"), 0o755);

        assert_eq!(
            probe_command(&dir_a.join("plain-shell").to_string_lossy(), None),
            Probe::NotExecutable
        );
        assert_eq!(
            probe_command(&dir_b.join("exec-shell").to_string_lossy(), None),
            Probe::Usable
        );

        let path = std::env::join_paths([&dir_a, &dir_b]).unwrap();
        assert_eq!(
            probe_command("exec-shell", Some(path.as_os_str())),
            Probe::Usable
        );
        assert_eq!(
            probe_command("plain-shell", Some(path.as_os_str())),
            Probe::NotExecutable
        );
        assert_eq!(
            probe_command("blocked-shell", Some(path.as_os_str())),
            Probe::Usable
        );
        assert_eq!(
            probe_command("nope", Some(path.as_os_str())),
            Probe::NotFound
        );
        assert_eq!(probe_command("exec-shell", None), Probe::NotFound);
    }

    #[cfg(unix)]
    #[test]
    fn resolve_for_spawn_uses_an_executable_configured_path() {
        let exec = temp_dir("cfg").join("my-shell");
        write_mode(&exec, 0o755);
        let mut cfg = Config::default();
        cfg.terminal.shell = Some(crate::config::TerminalShellConfig {
            command: exec.to_string_lossy().into_owned(),
            args: vec!["-l".into()],
        });
        let resolved = resolve_for_spawn(None, &cfg).unwrap();
        assert_eq!(resolved.command, exec.to_string_lossy());
        assert_eq!(resolved.args, vec!["-l".to_owned()]);
        assert!(!resolved.apply_osc7);
        assert!(resolved.skipped.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn resolve_for_spawn_falls_back_when_the_configured_binary_is_missing() {
        let mut cfg = Config::default();
        cfg.terminal.shell = Some(crate::config::TerminalShellConfig {
            command: "/no/such/fresh-gui-shell-zzz".into(),
            args: Vec::new(),
        });
        let resolved = resolve_for_spawn(None, &cfg).expect("a later candidate should exist");
        assert_ne!(resolved.command, "/no/such/fresh-gui-shell-zzz");
        assert_eq!(resolved.skipped[0].command, "/no/such/fresh-gui-shell-zzz");
        assert_eq!(resolved.skipped[0].problem, "not found");
        assert!(
            resolved.reason == "$SHELL" || resolved.reason.starts_with("fallback"),
            "reason was {}",
            resolved.reason
        );
        assert_eq!(
            probe_command(&resolved.command, std::env::var_os("PATH").as_deref()),
            Probe::Usable
        );
    }

    #[cfg(unix)]
    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-shell-{label}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[cfg(unix)]
    fn write_mode(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, b"#!/bin/sh\nexit 0\n").unwrap();
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_mode(mode);
        std::fs::set_permissions(path, perms).unwrap();
    }
}

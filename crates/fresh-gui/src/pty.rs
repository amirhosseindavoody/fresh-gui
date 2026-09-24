//! Local PTY session backed by `portable-pty`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::config::Config;

pub struct PtySession {
    id: String,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    /// Owned child so [`ChildKiller::kill`] can escalate SIGHUP → SIGKILL (Fresh shutdown).
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
}

impl PtySession {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Spawn a PTY. The shell is the client override when set, otherwise
    /// [`Config::resolve_shell`]. On Unix a command that is missing or not
    /// executable falls back to `$SHELL`, then `bash`, then `sh`; Windows
    /// tries `pwsh`, `powershell`, `%COMSPEC%`, then `cmd`
    /// ([`crate::shell_resolve`]). Empty `args` still get OSC 7 setup;
    /// non-empty `args` are passed through (Fresh-compatible).
    pub fn spawn(
        id: String,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
        shell: Option<String>,
        config: &Config,
        output_tx: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<Self> {
        let resolved = crate::shell_resolve::resolve_for_spawn(shell.as_deref(), config)?;
        crate::shell_resolve::log_resolved(&resolved);

        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let mut cmd = CommandBuilder::new(&resolved.command);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        // The daemon often inherits TERM=dumb (SSH, a detached Windows
        // process, a desktop launch). Fish and other TUIs refuse that.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        if resolved.apply_osc7 {
            configure_shell_cmd(&mut cmd, &resolved.command);
        } else {
            for arg in &resolved.args {
                cmd.arg(arg);
            }
        }

        let child = pair.slave.spawn_command(cmd).map_err(|err| {
            anyhow::anyhow!(
                "{}",
                crate::shell_resolve::spawn_failure_message(
                    &resolved.command,
                    &resolved.reason,
                    &resolved.skipped,
                    &err,
                )
            )
        })?;

        let mut reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let writer = pair.master.take_writer().context("take pty writer")?;
        let master = Arc::new(Mutex::new(pair.master));

        let id_for_thread = id.clone();
        thread::Builder::new()
            .name(format!("pty-read-{id_for_thread}"))
            .spawn(move || {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if output_tx.send(buf[..n].to_vec()).is_err() {
                                break;
                            }
                        }
                        Err(err) => {
                            warn!(pty = %id_for_thread, %err, "pty read error");
                            break;
                        }
                    }
                }
                debug!(pty = %id_for_thread, "pty reader exited");
            })
            .context("spawn pty reader thread")?;

        Ok(Self {
            id,
            master,
            writer: Arc::new(Mutex::new(writer)),
            child: Mutex::new(child),
        })
    }

    /// Terminate the shell process (Fresh `ChildKiller::kill` on shutdown).
    ///
    /// Uses the owned PTY child so unix kill can escalate from SIGHUP to SIGKILL
    /// (a cloned killer alone only sends SIGHUP).
    pub fn kill(&self) {
        match self.child.lock() {
            Ok(mut child) => {
                if let Err(err) = child.kill() {
                    warn!(pty = %self.id, %err, "pty child kill failed");
                } else {
                    debug!(pty = %self.id, "pty child killed");
                }
                let _ = child.try_wait();
            }
            Err(_) => warn!(pty = %self.id, "pty child lock poisoned"),
        }
    }

    pub fn write_all(&self, data: &[u8]) -> Result<()> {
        let mut w = self.writer.lock().expect("pty writer lock");
        w.write_all(data).context("pty write")?;
        w.flush().ok();
        Ok(())
    }

    pub fn resize(&self, cols: u16, rows: u16) -> Result<()> {
        let master = self.master.lock().expect("pty master lock");
        master
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("pty resize")
    }
}

fn shell_basename(shell: &str) -> &str {
    shell.rsplit(['/', '\\']).next().unwrap_or(shell)
}

fn shell_init_dir() -> PathBuf {
    std::env::temp_dir().join("fresh-gui-shell")
}

fn ensure_bash_rcfile() -> Option<PathBuf> {
    let dir = shell_init_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("bashrc");
    // Report cwd with an OSC 7 sequence at shell startup and after each prompt.
    let body = r#"# fresh-gui OSC 7 cwd reporting
[[ -f /etc/bash.bashrc ]] && . /etc/bash.bashrc
[[ -f ~/.bashrc ]] && . ~/.bashrc
_fresh_gui_urlencode() {
  local LC_ALL=C s="$1" i c
  for (( i=0; i<${#s}; i++ )); do
    c="${s:i:1}"
    case "$c" in
      [a-zA-Z0-9/._~-]) printf '%s' "$c" ;;
      *) printf '%%%02X' "'$c" ;;
    esac
  done
}
fresh_gui_osc7() {
  printf '\033]7;file://%s%s\033\\' "${HOSTNAME:-localhost}" "$(_fresh_gui_urlencode "$PWD")"
}
case ":${PROMPT_COMMAND:-}:" in
  *:fresh_gui_osc7:*) ;;
  *) PROMPT_COMMAND="fresh_gui_osc7${PROMPT_COMMAND:+;${PROMPT_COMMAND}}" ;;
esac
fresh_gui_osc7
"#;
    std::fs::write(&path, body).ok()?;
    Some(path)
}

fn ensure_zsh_zdotdir() -> Option<PathBuf> {
    let dir = shell_init_dir().join("zdot");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(".zshrc");
    let body = r#"# fresh-gui OSC 7 cwd reporting
[[ -f ${ZDOTDIR_USER:-$HOME}/.zshrc ]] && . ${ZDOTDIR_USER:-$HOME}/.zshrc
_fresh_gui_urlencode() {
  local LC_ALL=C s="$1" i c
  for (( i=1; i<=${#s}; i++ )); do
    c="$s[i]"
    case "$c" in
      [a-zA-Z0-9/._~-]) printf '%s' "$c" ;;
      *) printf '%%%02X' "'$c" ;;
    esac
  done
}
fresh_gui_osc7() {
  printf '\033]7;file://%s%s\033\\' "${HOST:-${HOSTNAME:-localhost}}" "$(_fresh_gui_urlencode "$PWD")"
}
autoload -Uz add-zsh-hook 2>/dev/null
if typeset -f add-zsh-hook >/dev/null 2>&1; then
  add-zsh-hook precmd fresh_gui_osc7
  add-zsh-hook chpwd fresh_gui_osc7
else
  precmd_functions=(${precmd_functions:#fresh_gui_osc7} fresh_gui_osc7)
  chpwd_functions=(${chpwd_functions:#fresh_gui_osc7} fresh_gui_osc7)
fi
fresh_gui_osc7
"#;
    std::fs::write(&path, body).ok()?;
    Some(dir)
}

/// Fish has no rcfile flag. An init command defines a prompt hook that
/// reports `$PWD` and does not change it, so the PTY cwd (workspace / remote
/// root) stays put.
fn ensure_fish_osc7() -> Option<PathBuf> {
    let dir = shell_init_dir();
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("osc7.fish");
    std::fs::write(&path, fish_osc7_script()).ok()?;
    Some(path)
}

fn fish_osc7_script() -> &'static str {
    r#"# fresh-gui OSC 7 cwd reporting
function _fresh_gui_osc7 --on-event fish_prompt --description 'fresh-gui OSC 7 cwd'
    set -l path (string escape --style=url -- $PWD 2>/dev/null)
    if test -z "$path"
        set path $PWD
    end
    printf '\033]7;file://%s%s\033\\' (prompt_hostname) $path
end
"#
}

fn fish_init_arg(script: &std::path::Path) -> String {
    let text = script
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    format!("source \"{text}\"")
}

/// Interactive shell args + OSC 7 hooks so the host can track cwd / tab titles.
fn configure_shell_cmd(cmd: &mut CommandBuilder, shell: &str) {
    let name = shell_basename(shell).to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    match name {
        "bash" => {
            if let Some(rc) = ensure_bash_rcfile() {
                cmd.arg("--rcfile");
                cmd.arg(rc);
                cmd.arg("-i");
            } else {
                cmd.arg("-l");
            }
        }
        "zsh" => {
            if let Some(zdot) = ensure_zsh_zdotdir() {
                if let Ok(home) = std::env::var("HOME") {
                    cmd.env("ZDOTDIR_USER", home);
                }
                cmd.env("ZDOTDIR", zdot);
                cmd.arg("-i");
            } else {
                cmd.arg("-l");
            }
        }
        // Fish is an interactive shell. `-l` is a login shell and can reset
        // the working directory from profile snippets; `-i` keeps the cwd
        // the PTY was given (the workspace / remote root). `-C` loads the
        // OSC 7 hook after the user's config, without replacing `fish_prompt`.
        "fish" => {
            cmd.arg("-i");
            if let Some(script) = ensure_fish_osc7() {
                cmd.arg("-C");
                cmd.arg(fish_init_arg(&script));
            }
        }
        // Windows console shells stay up when they are the ConPTY process.
        // A bare `-l` is not a login flag: `powershell.exe` runs it as the
        // command and exits, and `pwsh -l` binds to `-Login`.
        base if windows_console_shell(base) => {}
        // An arbitrary executable may not understand shell login flags.
        // The PTY itself makes a real shell interactive when appropriate.
        _ => {}
    }
}

/// `powershell` / `pwsh` / `cmd`, including a `.exe` suffix and any directory prefix.
fn windows_console_shell(shell: &str) -> bool {
    matches!(
        shell_basename(shell).to_ascii_lowercase().as_str(),
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" | "cmd" | "cmd.exe"
    )
}

#[cfg(test)]
mod shell_args_tests {
    use super::{configure_shell_cmd, windows_console_shell};
    use portable_pty::CommandBuilder;

    #[test]
    fn sh_and_custom_executables_do_not_get_guessed_login_flags() {
        for shell in ["sh", "/opt/bin/my-shell"] {
            let mut command = CommandBuilder::new(shell);
            configure_shell_cmd(&mut command, shell);
            assert_eq!(command.get_argv().len(), 1, "{shell}");
        }
    }

    #[test]
    fn powershell_cmd_and_pwsh_are_not_unix_login_shells() {
        assert!(windows_console_shell("powershell"));
        assert!(windows_console_shell("PowerShell.EXE"));
        assert!(windows_console_shell(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        ));
        assert!(windows_console_shell("pwsh"));
        assert!(windows_console_shell("cmd.exe"));
        assert!(!windows_console_shell("bash"));
        assert!(!windows_console_shell("zsh"));
        assert!(!windows_console_shell("fish"));
    }

    #[test]
    fn fish_osc7_hook_reports_pwd_without_changing_directory() {
        let script = super::fish_osc7_script();
        assert!(script.contains("--on-event fish_prompt"));
        assert!(script.contains("$PWD"));
        assert!(!script.contains("cd "));
        let arg = super::fish_init_arg(std::path::Path::new("/tmp/fresh-gui-shell/osc7.fish"));
        assert_eq!(arg, "source \"/tmp/fresh-gui-shell/osc7.fish\"");
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::time::{Duration, Instant};

    use tokio::sync::mpsc;

    use super::*;
    use crate::config::Config;

    /// Default config starts `powershell` with empty args. That must be an
    /// interactive console: ConPTY asks for the cursor (`CSI 6 n`) and, once
    /// answered, prints a `PS` prompt. A Unix `-l` makes PowerShell run `-l`
    /// as a command and exit before any prompt.
    #[test]
    fn default_powershell_prints_a_prompt() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = PtySession::spawn(
            "powershell-prompt".into(),
            100,
            30,
            None,
            None,
            &Config::default(),
            tx,
        )
        .expect("spawn powershell");

        let mut collected = Vec::new();
        let start = Instant::now();
        let mut replied = false;
        let mut saw_prompt = false;
        while start.elapsed() < Duration::from_secs(8) {
            match rx.try_recv() {
                Ok(bytes) => {
                    if !replied && bytes.windows(4).any(|w| w == *b"\x1b[6n") {
                        session.write_all(b"\x1b[1;1R").expect("cursor report");
                        replied = true;
                    }
                    collected.extend_from_slice(&bytes);
                    if collected.windows(3).any(|w| w == *b"PS ") {
                        saw_prompt = true;
                        break;
                    }
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
        session.kill();
        let text = String::from_utf8_lossy(&collected);
        assert!(saw_prompt, "powershell produced no PS prompt: {text:?}");
        assert!(
            !text.contains("The term '-l'"),
            "powershell was started with a Unix -l flag: {text:?}"
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::config::Config;

    #[test]
    fn configured_custom_shell_path_is_spawned_without_login_flags() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-custom-shell-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let shell = dir.join("custom-shell");
        std::fs::write(&shell, b"#!/bin/sh\nprintf 'custom-shell-ok\\n'\n").unwrap();
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut config = Config::default();
        config.terminal.shell = Some(crate::config::TerminalShellConfig {
            command: shell.display().to_string(),
            args: Vec::new(),
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = PtySession::spawn("custom".into(), 80, 24, None, None, &config, tx)
            .expect("spawn configured shell");
        let start = std::time::Instant::now();
        let mut output = String::new();
        while start.elapsed() < std::time::Duration::from_secs(3) {
            match rx.try_recv() {
                Ok(bytes) => output.push_str(&String::from_utf8_lossy(&bytes)),
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
            if output.contains("custom-shell-ok") {
                break;
            }
        }
        session.kill();
        let _ = std::fs::remove_dir_all(dir);
        assert!(output.contains("custom-shell-ok"), "{output:?}");
    }

    /// Default config asks for `zsh`. On a host where that binary is missing,
    /// spawn must still start a later candidate (`$SHELL`, `bash`, or `sh`).
    #[test]
    fn default_config_starts_a_shell_when_zsh_is_absent() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let config = Config::default();
        assert_eq!(config.resolve_shell().0, "zsh");

        let session = PtySession::spawn(
            "fallback".into(),
            80,
            24,
            None,
            Some("not-a-real-shell-fresh-gui".into()),
            &config,
            tx,
        )
        .expect("missing client shell and missing zsh should fall back");

        session
            .write_all(b"printf 'fresh-gui-fallback-ok\\n'\n")
            .expect("write");

        let mut collected = String::new();
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(5) {
            match rx.try_recv() {
                Ok(bytes) => {
                    collected.push_str(&String::from_utf8_lossy(&bytes));
                    if collected.contains("fresh-gui-fallback-ok") {
                        session.kill();
                        return;
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        session.kill();
        panic!("fallback shell did not print the marker: {collected:?}");
    }

    #[test]
    fn spawned_shell_starts_in_the_requested_directory_with_a_real_term() {
        let dir = std::env::temp_dir().join(format!(
            "fresh-gui-pty-cwd-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let session = PtySession::spawn(
            "cwd".into(),
            80,
            24,
            Some(dir.display().to_string()),
            Some("bash".into()),
            &Config::default(),
            tx,
        )
        .expect("spawn");
        session
            .write_all(b"printf 'cwd=%s term=%s\\n' \"$PWD\" \"$TERM\"\n")
            .expect("write");

        let mut collected = String::new();
        let start = std::time::Instant::now();
        let marker = format!("cwd={}", dir.display());
        while start.elapsed() < std::time::Duration::from_secs(5) {
            match rx.try_recv() {
                Ok(bytes) => {
                    collected.push_str(&String::from_utf8_lossy(&bytes));
                    if collected.contains(&marker) && collected.contains("term=xterm-256color") {
                        session.kill();
                        let _ = std::fs::remove_dir_all(&dir);
                        return;
                    }
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
            }
        }
        session.kill();
        let _ = std::fs::remove_dir_all(&dir);
        panic!("shell did not report cwd and TERM: {collected:?}");
    }
}

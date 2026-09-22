//! Local PTY session backed by `portable-pty`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
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
    /// executable falls back to `$SHELL`, then `bash`, then `sh`
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
    // Terax-style: ST terminator, urlencoded path, fire once at load, then PROMPT_COMMAND.
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

/// Interactive shell args + OSC 7 hooks so the host can track cwd / tab titles.
fn configure_shell_cmd(cmd: &mut CommandBuilder, shell: &str) {
    match shell_basename(shell) {
        "bash" | "sh" => {
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
        _ => {
            cmd.arg("-l");
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use tokio::sync::mpsc;

    use super::*;
    use crate::config::Config;

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
}

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
    _child_killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
}

impl PtySession {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Spawn a PTY. When `shell` is set (client override), that executable is
    /// used with interactive / OSC 7 setup. Otherwise [`Config::resolve_shell`]
    /// supplies the command; empty `args` still get OSC 7 setup, non-empty
    /// `args` are passed through as-is (Fresh-compatible).
    pub fn spawn(
        id: String,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
        shell: Option<String>,
        config: &Config,
        output_tx: mpsc::UnboundedSender<Vec<u8>>,
    ) -> Result<Self> {
        let pty_system = native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("openpty")?;

        let (shell, args, apply_osc7) = match shell {
            Some(s) => (s, Vec::new(), true),
            None => {
                let (cmd, args) = config.resolve_shell();
                let apply = args.is_empty();
                (cmd, args, apply)
            }
        };
        let mut cmd = CommandBuilder::new(&shell);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        if apply_osc7 {
            configure_shell_cmd(&mut cmd, &shell);
        } else {
            for arg in &args {
                cmd.arg(arg);
            }
        }

        let child = pair
            .slave
            .spawn_command(cmd)
            .with_context(|| format!("spawn shell {shell}"))?;
        let killer = child.clone_killer();

        let mut reader = pair
            .master
            .try_clone_reader()
            .context("clone pty reader")?;
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
            _child_killer: Arc::new(Mutex::new(killer)),
        })
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
    shell.rsplit('/').next().unwrap_or(shell)
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

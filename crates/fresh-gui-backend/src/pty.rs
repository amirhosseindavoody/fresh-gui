//! Local PTY session backed by `portable-pty`.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;
use tracing::{debug, warn};

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

    pub fn spawn(
        id: String,
        cols: u16,
        rows: u16,
        cwd: Option<String>,
        shell: Option<String>,
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

        let shell = shell.unwrap_or_else(default_shell);
        let mut cmd = CommandBuilder::new(&shell);
        if let Some(cwd) = cwd {
            cmd.cwd(cwd);
        }
        configure_shell_cmd(&mut cmd, &shell);

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

fn default_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_owned())
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
    let body = r#"# fresh-gui OSC 7 cwd reporting
[[ -f /etc/bash.bashrc ]] && . /etc/bash.bashrc
[[ -f ~/.bashrc ]] && . ~/.bashrc
fresh_gui_osc7() { printf '\033]7;file://localhost%s\007' "$PWD"; }
case ";${PROMPT_COMMAND:-};" in
  *fresh_gui_osc7*) ;;
  *) PROMPT_COMMAND="fresh_gui_osc7${PROMPT_COMMAND:+; ${PROMPT_COMMAND}}" ;;
esac
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
fresh_gui_osc7() { printf '\033]7;file://localhost%s\007' "$PWD"; }
if typeset -f add-zsh-hook >/dev/null 2>&1; then
  add-zsh-hook precmd fresh_gui_osc7
elif [[ -z "${precmd_functions[(r)fresh_gui_osc7]}" ]]; then
  precmd_functions+=(fresh_gui_osc7)
fi
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
                // Preserve user ZDOTDIR if set, else home is implied via ZDOTDIR_USER.
                cmd.env("ZDOTDIR", zdot);
                cmd.arg("-i");
            } else {
                cmd.arg("-l");
            }
        }
        _ => {
            // Best-effort interactive login for unknown shells.
            if shell_basename(shell) == "fish" {
                cmd.arg("-l");
            } else {
                cmd.arg("-l");
            }
        }
    }
}

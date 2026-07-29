//! Local PTY session backed by `portable-pty`.

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use portable_pty::{native_pty_system, CommandBuilder, MasterPty, PtySize};
use tokio::sync::mpsc;
use tracing::{debug, warn};

pub struct PtySession {
    _id: String,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    _child_killer: Arc<Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>>,
}

impl PtySession {
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
        // Login-ish interactive shell
        if shell.rsplit('/').next() == Some("bash") || shell.ends_with("bash") {
            cmd.arg("-l");
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
            _id: id,
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

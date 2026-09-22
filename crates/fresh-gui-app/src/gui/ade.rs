//! Tokio ADE worker thread. GPUI talks to it through channels.

use std::thread;
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{FsEntry, Hello, Message};

use super::connect::ConnectTarget;

#[derive(Debug, Clone)]
pub enum AdeCmd {
    OpenPty {
        cols: u16,
        rows: u16,
        cwd: Option<String>,
    },
    WritePty {
        id: String,
        data: Vec<u8>,
    },
    #[allow(dead_code)]
    ResizePty {
        id: String,
        cols: u16,
        rows: u16,
    },
    ClosePty {
        id: String,
    },
    ListDir {
        request_id: String,
        path: String,
    },
    OpenEditor {
        request_id: String,
        path: String,
        preview: bool,
        line: Option<u32>,
        column: Option<u32>,
    },
    EditBuffer {
        request_id: String,
        buffer_id: String,
        base_rev: u64,
        text: String,
    },
    SaveBuffer {
        request_id: String,
        buffer_id: String,
        base_rev: u64,
    },
    CloseEditor {
        buffer_id: String,
    },
    CopyPaths {
        request_id: String,
        sources: Vec<String>,
        destination: String,
    },
    MovePaths {
        request_id: String,
        sources: Vec<String>,
        destination: String,
    },
    Disconnect,
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum AdeEvent {
    Connecting,
    Connected {
        hello: Hello,
        session_id: String,
    },
    Disconnected {
        reason: String,
    },
    PtyOpened {
        id: String,
        cols: u16,
        rows: u16,
    },
    PtyData {
        id: String,
        bytes: Vec<u8>,
    },
    PtyClosed {
        id: String,
        reason: Option<String>,
    },
    FsListed {
        request_id: String,
        path: String,
        entries: Vec<FsEntry>,
    },
    EditorOpened {
        request_id: String,
        buffer_id: String,
        path: String,
        language: Option<String>,
        line: Option<u32>,
        column: Option<u32>,
    },
    BufferSnapshot {
        buffer_id: String,
        rev: u64,
        text: String,
        path: String,
    },
    BufferChanged {
        request_id: String,
        buffer_id: String,
        rev: u64,
    },
    BufferSaved {
        request_id: String,
        buffer_id: String,
        path: String,
        rev: u64,
    },
    FsCopied {
        request_id: String,
        entries: Vec<FsEntry>,
    },
    FsMoved {
        request_id: String,
        entries: Vec<FsEntry>,
    },
    Error {
        code: String,
        message: String,
    },
}

#[derive(Clone)]
pub struct AdeHandle {
    tx: async_channel::Sender<AdeCmd>,
}

impl AdeHandle {
    pub fn send(&self, cmd: AdeCmd) {
        let _ = self.tx.try_send(cmd);
    }
}

pub fn spawn(target: ConnectTarget) -> (AdeHandle, async_channel::Receiver<AdeEvent>) {
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<AdeCmd>();
    let (evt_tx, evt_rx) = async_channel::unbounded::<AdeEvent>();

    thread::Builder::new()
        .name("fresh-gui-ade".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(2)
                .build()
            {
                Ok(rt) => rt,
                Err(err) => {
                    let _ = evt_tx.send_blocking(AdeEvent::Disconnected {
                        reason: format!("tokio runtime: {err}"),
                    });
                    return;
                }
            };
            rt.block_on(ade_loop(target, cmd_rx, evt_tx));
        })
        .expect("spawn ADE thread");

    (AdeHandle { tx: cmd_tx }, evt_rx)
}

async fn ade_loop(
    target: ConnectTarget,
    cmd_rx: async_channel::Receiver<AdeCmd>,
    evt_tx: async_channel::Sender<AdeEvent>,
) {
    let _ = evt_tx.send(AdeEvent::Connecting).await;
    let mut opts = ConnectOptions::new(&target.ws_url);
    if let Some(token) = &target.token {
        opts = opts.with_token(token.clone());
    }

    let mut client = match Client::connect(opts).await {
        Ok(c) => c,
        Err(err) => {
            let _ = evt_tx
                .send(AdeEvent::Disconnected {
                    reason: format!("{err:#}"),
                })
                .await;
            return;
        }
    };

    let hello = client.backend_hello.clone();
    let session_id = match client.create_session(None).await {
        Ok(id) => id,
        Err(err) => {
            let _ = evt_tx
                .send(AdeEvent::Disconnected {
                    reason: format!("session: {err:#}"),
                })
                .await;
            return;
        }
    };

    let _ = evt_tx.send(AdeEvent::Connected { hello, session_id }).await;

    loop {
        tokio::select! {
            cmd = cmd_rx.recv() => {
                match cmd {
                    Ok(AdeCmd::Disconnect) | Err(_) => break,
                    Ok(cmd) => {
                        if let Err(err) = dispatch_cmd(&mut client, cmd).await {
                            let _ = evt_tx.send(AdeEvent::Error {
                                code: "ade".into(),
                                message: format!("{err:#}"),
                            }).await;
                        }
                    }
                }
            }
            msg = client.recv() => {
                match msg {
                    Ok(message) => {
                        if let Some(ev) = event_from_message(message) {
                            if evt_tx.send(ev).await.is_err() {
                                break;
                            }
                        }
                    }
                    Err(err) => {
                        let _ = evt_tx.send(AdeEvent::Disconnected {
                            reason: format!("{err:#}"),
                        }).await;
                        break;
                    }
                }
            }
        }
    }
}

async fn dispatch_cmd(client: &mut Client, cmd: AdeCmd) -> anyhow::Result<()> {
    match cmd {
        AdeCmd::OpenPty { cols, rows, cwd } => {
            client
                .send(Message::PtyOpen {
                    cols,
                    rows,
                    cwd,
                    shell: None,
                })
                .await?;
        }
        AdeCmd::WritePty { id, data } => {
            client.write_pty(&id, &data).await?;
        }
        AdeCmd::ResizePty { id, cols, rows } => {
            client.resize_pty(&id, cols, rows).await?;
        }
        AdeCmd::ClosePty { id } => {
            client.close_pty(&id).await?;
        }
        AdeCmd::ListDir { request_id, path } => {
            client.send(Message::FsList { request_id, path }).await?;
        }
        AdeCmd::OpenEditor {
            request_id,
            path,
            preview,
            line,
            column,
        } => {
            client
                .send(Message::EditorOpen {
                    request_id,
                    path,
                    preview,
                    cwd: None,
                    line,
                    column,
                })
                .await?;
        }
        AdeCmd::EditBuffer {
            request_id,
            buffer_id,
            base_rev,
            text,
        } => {
            client
                .send(Message::BufferEdit {
                    request_id,
                    buffer_id,
                    base_rev,
                    text,
                })
                .await?;
        }
        AdeCmd::SaveBuffer {
            request_id,
            buffer_id,
            base_rev,
        } => {
            client
                .send(Message::BufferSave {
                    request_id,
                    buffer_id,
                    base_rev,
                })
                .await?;
        }
        AdeCmd::CloseEditor { buffer_id } => {
            client.send(Message::EditorClose { buffer_id }).await?;
        }
        AdeCmd::CopyPaths {
            request_id,
            sources,
            destination,
        } => {
            client
                .send(Message::FsCopy {
                    request_id,
                    sources,
                    destination,
                })
                .await?;
        }
        AdeCmd::MovePaths {
            request_id,
            sources,
            destination,
        } => {
            client
                .send(Message::FsMove {
                    request_id,
                    sources,
                    destination,
                })
                .await?;
        }
        AdeCmd::Disconnect => {}
    }
    Ok(())
}

fn event_from_message(msg: Message) -> Option<AdeEvent> {
    match msg {
        Message::PtyOpened { id, cols, rows } => Some(AdeEvent::PtyOpened { id, cols, rows }),
        Message::PtyData { id, data } => {
            let bytes = Client::decode_pty_data(&data).ok()?;
            Some(AdeEvent::PtyData { id, bytes })
        }
        Message::PtyClosed { id, reason } => Some(AdeEvent::PtyClosed { id, reason }),
        Message::FsListed {
            request_id,
            path,
            entries,
        } => Some(AdeEvent::FsListed {
            request_id,
            path,
            entries,
        }),
        Message::EditorOpened {
            request_id,
            buffer_id,
            path,
            language,
            line,
            column,
        } => Some(AdeEvent::EditorOpened {
            request_id,
            buffer_id,
            path,
            language,
            line,
            column,
        }),
        Message::BufferSnapshot {
            buffer_id,
            rev,
            text,
            path,
        } => Some(AdeEvent::BufferSnapshot {
            buffer_id,
            rev,
            text,
            path,
        }),
        Message::BufferChanged {
            request_id,
            buffer_id,
            rev,
        } => Some(AdeEvent::BufferChanged {
            request_id,
            buffer_id,
            rev,
        }),
        Message::BufferSaved {
            request_id,
            buffer_id,
            path,
            rev,
        } => Some(AdeEvent::BufferSaved {
            request_id,
            buffer_id,
            path,
            rev,
        }),
        Message::FsCopied {
            request_id,
            entries,
        } => Some(AdeEvent::FsCopied {
            request_id,
            entries,
        }),
        Message::FsMoved {
            request_id,
            entries,
        } => Some(AdeEvent::FsMoved {
            request_id,
            entries,
        }),
        Message::Error { code, message } => Some(AdeEvent::Error { code, message }),
        Message::Pong { .. } | Message::Ping { .. } | Message::AuthOk => None,
        _ => None,
    }
}

/// Used by tests so the unused import of Duration stays meaningful if we add
/// reconnect backoff later.
#[allow(dead_code)]
pub fn reconnect_delay() -> Duration {
    Duration::from_secs(2)
}

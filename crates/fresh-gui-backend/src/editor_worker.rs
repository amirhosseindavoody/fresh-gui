//! In-process Fresh `Editor` on a dedicated `!Send` thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Context, Result};
use fresh::app::Editor;
use fresh::config::Config;
use fresh::config_io::DirectoryContext;
use fresh::model::filesystem::{FileSystem, StdFileSystem};
use fresh::view::color_support::ColorCapability;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug)]
pub struct OpenedBuffer {
    pub buffer_id: String,
    pub path: String,
    pub language: Option<String>,
    pub rev: u64,
    pub text: String,
}

enum Cmd {
    Open {
        path: PathBuf,
        preview: bool,
        reply: oneshot::Sender<Result<OpenedBuffer>>,
    },
    Close {
        buffer_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// Handle to the editor thread. Cloneable; commands are serialized on the worker.
#[derive(Clone)]
pub struct EditorHandle {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl EditorHandle {
    /// Spawn the Fresh editor on a dedicated OS thread. Returns `None` if init fails.
    pub fn spawn(working_dir: PathBuf) -> Option<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let (tx, rx) = mpsc::unbounded_channel::<Cmd>();
        let dir_for_log = working_dir.clone();

        thread::Builder::new()
            .name("fresh-editor".into())
            .spawn(move || {
                match build_editor(&working_dir) {
                    Ok(editor) => {
                        let _ = ready_tx.send(Ok(()));
                        run_loop(editor, rx);
                    }
                    Err(err) => {
                        let _ = ready_tx.send(Err(err));
                    }
                }
            })
            .ok()?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                info!(dir = %dir_for_log.display(), "Fresh editor worker ready");
                Some(Self { tx })
            }
            Ok(Err(err)) => {
                warn!(error = %err, "Fresh editor worker failed to start");
                None
            }
            Err(_) => {
                warn!("Fresh editor worker channel closed during startup");
                None
            }
        }
    }

    pub async fn open(&self, path: PathBuf, preview: bool) -> Result<OpenedBuffer> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Open {
                path,
                preview,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn close(&self, buffer_id: String) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Close {
                buffer_id,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }
}

fn build_editor(working_dir: &Path) -> Result<Editor> {
    // Keep Fresh config/state out of the project tree (and out of git).
    let state_dir = std::env::temp_dir().join(format!(
        "fresh-gui-editor-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("create editor state dir {}", state_dir.display()))?;
    let dir_context = DirectoryContext::for_testing(&state_dir);
    let mut cfg = Config::load_with_layers(&dir_context, working_dir);
    cfg.editor.animations = false;
    let fs: Arc<dyn FileSystem + Send + Sync> = Arc::new(StdFileSystem);
    Editor::with_working_dir(
        cfg,
        80,
        24,
        Some(working_dir.to_path_buf()),
        dir_context,
        false, // plugins off for ADE MVP — faster/stabler headless
        ColorCapability::TrueColor,
        fs,
    )
    .context("Editor::with_working_dir")
}

fn run_loop(mut editor: Editor, mut rx: mpsc::UnboundedReceiver<Cmd>) {
    // Drive the channel with a small Tokio runtime on this thread so we can
    // await without touching the axum runtime (Editor is !Send).
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            warn!(error = %err, "editor worker runtime failed");
            return;
        }
    };

    rt.block_on(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::Open {
                    path,
                    preview,
                    reply,
                } => {
                    let result = open_buffer(&mut editor, &path, preview);
                    let _ = reply.send(result);
                }
                Cmd::Close { buffer_id, reply } => {
                    let result = close_buffer(&mut editor, &buffer_id);
                    let _ = reply.send(result);
                }
            }
        }
    });
}

fn open_buffer(editor: &mut Editor, path: &Path, preview: bool) -> Result<OpenedBuffer> {
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() as usize > MAX_SNAPSHOT_BYTES {
        bail!(
            "file too large for snapshot ({} bytes; max {MAX_SNAPSHOT_BYTES})",
            meta.len()
        );
    }

    let buffer_id = if preview {
        editor
            .open_file_preview(path)
            .with_context(|| format!("open_file_preview {}", path.display()))?
    } else {
        editor
            .open_file(path)
            .with_context(|| format!("open_file {}", path.display()))?
    };

    // Ensure the opened buffer is active for text extraction.
    // open_file already activates; preview should too.
    let language = editor.active_buffer_mode().map(|s| s.to_owned());
    let text = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer has unloaded regions (large-file mode); cannot snapshot")?;

    if text.len() > MAX_SNAPSHOT_BYTES {
        bail!(
            "snapshot too large ({} bytes; max {MAX_SNAPSHOT_BYTES})",
            text.len()
        );
    }

    Ok(OpenedBuffer {
        buffer_id: buffer_id.0.to_string(),
        path: path.display().to_string(),
        language,
        rev: 0,
        text,
    })
}

fn close_buffer(_editor: &mut Editor, buffer_id: &str) -> Result<()> {
    // Phase 3a: validate id; actual Fresh buffer close deferred to 3b.
    let _: usize = buffer_id
        .parse()
        .with_context(|| format!("invalid buffer_id {buffer_id}"))?;
    Ok(())
}

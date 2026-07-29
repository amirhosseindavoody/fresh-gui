//! In-process Fresh `Editor` on a dedicated `!Send` thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{bail, Context, Result};
use fresh::app::Editor;
use fresh::config::Config;
use fresh::config_io::DirectoryContext;
use fresh::model::filesystem::{FileSystem, StdFileSystem};
use fresh::view::color_support::ColorCapability;
use fresh_gui_protocol::SceneBuffer;
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OpenedBuffer {
    pub buffer_id: String,
    pub path: String,
    pub language: Option<String>,
    pub rev: u64,
    pub text: String,
}

#[derive(Debug, Clone)]
struct TrackedBuffer {
    path: PathBuf,
    rev: u64,
    dirty: bool,
    language: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SceneState {
    pub buffers: Vec<SceneBuffer>,
    pub active_buffer_id: Option<String>,
}

enum Cmd {
    Open {
        path: PathBuf,
        preview: bool,
        reply: oneshot::Sender<Result<OpenedBuffer>>,
    },
    Edit {
        buffer_id: String,
        base_rev: u64,
        text: String,
        reply: oneshot::Sender<Result<u64>>,
    },
    Save {
        buffer_id: String,
        base_rev: u64,
        reply: oneshot::Sender<Result<(String, u64)>>,
    },
    Close {
        buffer_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Scene {
        reply: oneshot::Sender<Result<SceneState>>,
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
            .spawn(move || match build_editor(&working_dir) {
                Ok(editor) => {
                    let _ = ready_tx.send(Ok(()));
                    run_loop(editor, rx);
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
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

    pub async fn edit(&self, buffer_id: String, base_rev: u64, text: String) -> Result<u64> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Edit {
                buffer_id,
                base_rev,
                text,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn save(&self, buffer_id: String, base_rev: u64) -> Result<(String, u64)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Save {
                buffer_id,
                base_rev,
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

    pub async fn scene(&self) -> Result<SceneState> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Scene { reply: reply_tx })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }
}

fn build_editor(working_dir: &Path) -> Result<Editor> {
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
        false,
        ColorCapability::TrueColor,
        fs,
    )
    .context("Editor::with_working_dir")
}

fn run_loop(mut editor: Editor, mut rx: mpsc::UnboundedReceiver<Cmd>) {
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

    let mut tracked: HashMap<String, TrackedBuffer> = HashMap::new();

    rt.block_on(async move {
        while let Some(cmd) = rx.recv().await {
            match cmd {
                Cmd::Open {
                    path,
                    preview,
                    reply,
                } => {
                    let result = open_buffer(&mut editor, &mut tracked, &path, preview);
                    let _ = reply.send(result);
                }
                Cmd::Edit {
                    buffer_id,
                    base_rev,
                    text,
                    reply,
                } => {
                    let result = edit_buffer(&mut editor, &mut tracked, &buffer_id, base_rev, &text);
                    let _ = reply.send(result);
                }
                Cmd::Save {
                    buffer_id,
                    base_rev,
                    reply,
                } => {
                    let result = save_buffer(&mut editor, &mut tracked, &buffer_id, base_rev);
                    let _ = reply.send(result);
                }
                Cmd::Close { buffer_id, reply } => {
                    tracked.remove(&buffer_id);
                    let _ = reply.send(Ok(()));
                }
                Cmd::Scene { reply } => {
                    let active = editor.active_buffer().0.to_string();
                    let buffers = tracked
                        .iter()
                        .map(|(id, t)| SceneBuffer {
                            buffer_id: id.clone(),
                            path: t.path.display().to_string(),
                            rev: t.rev,
                            dirty: t.dirty,
                            language: t.language.clone(),
                        })
                        .collect();
                    let active_buffer_id = if tracked.contains_key(&active) {
                        Some(active)
                    } else {
                        tracked.keys().next().cloned()
                    };
                    let _ = reply.send(Ok(SceneState {
                        buffers,
                        active_buffer_id,
                    }));
                }
            }
        }
    });
}

fn open_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    path: &Path,
    preview: bool,
) -> Result<OpenedBuffer> {
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

    let id = buffer_id.0.to_string();
    let rev = tracked.get(&id).map(|t| t.rev).unwrap_or(0);
    tracked.insert(
        id.clone(),
        TrackedBuffer {
            path: path.to_path_buf(),
            rev,
            dirty: false,
            language: language.clone(),
        },
    );

    Ok(OpenedBuffer {
        buffer_id: id,
        path: path.display().to_string(),
        language,
        rev,
        text,
    })
}

fn activate_tracked(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    buffer_id: &str,
) -> Result<()> {
    let Some(entry) = tracked.get(buffer_id) else {
        bail!("unknown buffer_id {buffer_id}");
    };
    // open_file switches to an already-open buffer when the path matches.
    editor
        .open_file(&entry.path)
        .with_context(|| format!("activate {}", entry.path.display()))?;
    let active = editor.active_buffer().0.to_string();
    if active != buffer_id {
        bail!("failed to activate buffer {buffer_id} (active={active})");
    }
    Ok(())
}

fn edit_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    base_rev: u64,
    text: &str,
) -> Result<u64> {
    if text.len() > MAX_SNAPSHOT_BYTES {
        bail!(
            "edit too large ({} bytes; max {MAX_SNAPSHOT_BYTES})",
            text.len()
        );
    }
    let current = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?
        .rev;
    if current != base_rev {
        bail!("revision conflict: base_rev={base_rev} current={current}");
    }
    activate_tracked(editor, tracked, buffer_id)?;
    editor.active_state_mut().buffer.replace_content(text);
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    entry.rev += 1;
    entry.dirty = true;
    Ok(entry.rev)
}

fn save_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    base_rev: u64,
) -> Result<(String, u64)> {
    let current = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?
        .rev;
    if current != base_rev {
        bail!("revision conflict: base_rev={base_rev} current={current}");
    }
    activate_tracked(editor, tracked, buffer_id)?;
    editor.save().context("Editor::save")?;
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    entry.dirty = false;
    // Bump rev so peers know disk matches this generation.
    entry.rev += 1;
    Ok((entry.path.display().to_string(), entry.rev))
}

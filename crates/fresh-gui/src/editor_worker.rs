//! In-process Fresh `Editor` on a dedicated `!Send` thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use crate::drafts::{Draft, DraftStore};
use anyhow::{Context, Result, bail};
use fresh::app::Editor;
use fresh::config::Config;
use fresh::config_io::DirectoryContext;
use fresh::model::event::{BufferId, Event};
use fresh::model::filesystem::{FileSystem, StdFileSystem};
use fresh::types::LspFeature;
use fresh::view::color_support::ColorCapability;
use fresh_gui_protocol::{BufferDiagnostic, ByteSelection, EditorAction, RangeEdit, SceneBuffer};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OpenedBuffer {
    pub buffer_id: String,
    pub draft_id: String,
    pub path: String,
    pub language: Option<String>,
    pub rev: u64,
    pub text: String,
}

#[derive(Debug, Clone)]
struct TrackedBuffer {
    /// `None` until the buffer is saved to a path.
    path: Option<PathBuf>,
    text: String,
    rev: u64,
    dirty: bool,
    language: Option<String>,
    workspace_id: String,
    draft_id: String,
    base_text: Option<String>,
    recovery_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SceneState {
    pub buffers: Vec<SceneBuffer>,
    pub active_buffer_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct LspState {
    pub rev: u64,
    pub text: Option<String>,
    pub diagnostics: Vec<BufferDiagnostic>,
    pub status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct FormatState {
    pub rev: u64,
    pub text: Option<String>,
    pub status: Option<String>,
}

enum Cmd {
    Open {
        path: PathBuf,
        preview: bool,
        workspace_id: String,
        reply: oneshot::Sender<Result<OpenedBuffer>>,
    },
    New {
        workspace_id: String,
        reply: oneshot::Sender<Result<OpenedBuffer>>,
    },
    Edit {
        buffer_id: String,
        base_rev: u64,
        text: String,
        reply: oneshot::Sender<Result<u64>>,
    },
    RangeEdit {
        buffer_id: String,
        view_id: String,
        base_rev: u64,
        edits: Vec<RangeEdit>,
        selection: ByteSelection,
        reply: oneshot::Sender<Result<BufferTransactionResult>>,
    },
    Action {
        buffer_id: String,
        view_id: String,
        base_rev: u64,
        action: EditorAction,
        selection: ByteSelection,
        reply: oneshot::Sender<Result<BufferTransactionResult>>,
    },
    Sync {
        buffer_id: String,
        reply: oneshot::Sender<Result<BufferTransactionResult>>,
    },
    Save {
        buffer_id: String,
        base_rev: u64,
        /// Destination for an unsaved buffer. `None` saves the existing path.
        path: Option<PathBuf>,
        reply: oneshot::Sender<Result<(String, u64)>>,
    },
    Close {
        buffer_id: String,
        workspace_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    Scene {
        reply: oneshot::Sender<Result<SceneState>>,
    },
    LspGet {
        buffer_id: String,
        known_rev: u64,
        reply: oneshot::Sender<Result<LspState>>,
    },
    Format {
        buffer_id: String,
        base_rev: u64,
        reply: oneshot::Sender<Result<FormatState>>,
    },
    Reconfigure {
        config: crate::config::Config,
        reply: oneshot::Sender<Result<()>>,
    },
    DraftList {
        workspace_id: String,
        reply: oneshot::Sender<Result<Vec<Draft>>>,
    },
    DraftRestore {
        workspace_id: String,
        draft_id: String,
        reply: oneshot::Sender<Result<(OpenedBuffer, bool)>>,
    },
    DraftDiscard {
        buffer_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
    CheckWorkspace {
        buffer_id: String,
        workspace_id: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

#[derive(Debug, Clone)]
pub struct BufferTransactionResult {
    pub rev: u64,
    pub text: String,
    pub selection: ByteSelection,
    pub accepted: bool,
    pub dirty: bool,
}

/// Handle to the editor thread. Cloneable; commands are serialized on the worker.
#[derive(Clone)]
pub struct EditorHandle {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl EditorHandle {
    /// Spawn the Fresh editor on a dedicated OS thread. Returns `None` if init fails.
    #[cfg(test)]
    pub fn spawn(working_dir: PathBuf, gui_config: crate::config::Config) -> Option<Self> {
        Self::spawn_with_recovery_dir(
            working_dir,
            gui_config,
            std::env::temp_dir().join("fresh-gui-worker-test-drafts"),
        )
    }

    pub fn spawn_with_recovery_dir(
        working_dir: PathBuf,
        gui_config: crate::config::Config,
        recovery_dir: PathBuf,
    ) -> Option<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let (tx, rx) = mpsc::unbounded_channel::<Cmd>();
        let dir_for_log = working_dir.clone();

        thread::Builder::new()
            .name("fresh-editor".into())
            .spawn(move || match build_editor(&working_dir, &gui_config) {
                Ok(editor) => {
                    let _ = ready_tx.send(Ok(()));
                    run_loop(editor, rx, DraftStore::new(recovery_dir));
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

    #[cfg(test)]
    pub async fn open(&self, path: PathBuf, preview: bool) -> Result<OpenedBuffer> {
        self.open_in_workspace(path, preview, "default".into())
            .await
    }

    pub async fn open_in_workspace(
        &self,
        path: PathBuf,
        preview: bool,
        workspace_id: String,
    ) -> Result<OpenedBuffer> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Open {
                path,
                preview,
                workspace_id,
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

    pub async fn range_edit(
        &self,
        buffer_id: String,
        view_id: String,
        base_rev: u64,
        edits: Vec<RangeEdit>,
        selection: ByteSelection,
    ) -> Result<BufferTransactionResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::RangeEdit {
                buffer_id,
                view_id,
                base_rev,
                edits,
                selection,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn action(
        &self,
        buffer_id: String,
        view_id: String,
        base_rev: u64,
        action: EditorAction,
        selection: ByteSelection,
    ) -> Result<BufferTransactionResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Action {
                buffer_id,
                view_id,
                base_rev,
                action,
                selection,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn sync(&self, buffer_id: String) -> Result<BufferTransactionResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Sync { buffer_id, reply })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    #[cfg(test)]
    pub async fn new_buffer(&self) -> Result<OpenedBuffer> {
        self.new_buffer_in_workspace("default".into()).await
    }

    pub async fn new_buffer_in_workspace(&self, workspace_id: String) -> Result<OpenedBuffer> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::New {
                workspace_id,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn draft_list(&self, workspace_id: String) -> Result<Vec<Draft>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::DraftList {
                workspace_id,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn draft_restore(
        &self,
        workspace_id: String,
        draft_id: String,
    ) -> Result<(OpenedBuffer, bool)> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::DraftRestore {
                workspace_id,
                draft_id,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn draft_discard(&self, buffer_id: String) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::DraftDiscard { buffer_id, reply })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn check_workspace(&self, buffer_id: String, workspace_id: String) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::CheckWorkspace {
                buffer_id,
                workspace_id,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn save(
        &self,
        buffer_id: String,
        base_rev: u64,
        path: Option<PathBuf>,
    ) -> Result<(String, u64)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Save {
                buffer_id,
                base_rev,
                path,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    #[cfg(test)]
    pub async fn close(&self, buffer_id: String) -> Result<()> {
        self.close_in_workspace(buffer_id, "default".into()).await
    }

    pub async fn close_in_workspace(&self, buffer_id: String, workspace_id: String) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Close {
                buffer_id,
                workspace_id,
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

    pub async fn lsp_get(&self, buffer_id: String, known_rev: u64) -> Result<LspState> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::LspGet {
                buffer_id,
                known_rev,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn format(&self, buffer_id: String, base_rev: u64) -> Result<FormatState> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Format {
                buffer_id,
                base_rev,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn reconfigure(&self, config: crate::config::Config) -> Result<()> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Reconfigure { config, reply })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }
}

fn build_editor(working_dir: &Path, gui_config: &crate::config::Config) -> Result<Editor> {
    let state_dir = std::env::temp_dir().join(format!("fresh-gui-editor-{}", std::process::id()));
    std::fs::create_dir_all(&state_dir)
        .with_context(|| format!("create editor state dir {}", state_dir.display()))?;
    let dir_context = DirectoryContext::for_testing(&state_dir);
    let mut cfg = Config::load_with_layers(&dir_context, working_dir);
    cfg.editor.animations = false;
    // Fresh owns LSP lifecycle; only the servers explicitly configured for
    // fresh-gui are started on this daemon host.
    cfg.lsp = gui_config.lsp.clone();
    cfg.lsp_enabled = !cfg.lsp.is_empty();
    apply_language_associations(&mut cfg, &gui_config.languages);
    for language in cfg.lsp.keys() {
        if let Some(config) = cfg.languages.get_mut(language) {
            // The GUI's Format action should use the configured LSP server,
            // not Fresh's unrelated built-in external formatter command.
            config.formatter = None;
        }
    }
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

fn run_loop(mut editor: Editor, mut rx: mpsc::UnboundedReceiver<Cmd>, drafts: DraftStore) {
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

    // Borrow editor/tracked into the future (no `async move`) so `Editor` is
    // dropped *after* `block_on` returns — Fresh's Drop must not run while a
    // Tokio runtime is still in an async teardown path.
    rt.block_on(async {
        let mut ticks = tokio::time::interval(std::time::Duration::from_millis(50));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let cmd = tokio::select! {
                _ = ticks.tick() => {
                    if let Err(err) = fresh::app::editor_tick(&mut editor, || Ok(())) {
                        warn!(%err, "Fresh editor tick failed");
                    }
                    continue;
                }
                cmd = rx.recv() => match cmd { Some(cmd) => cmd, None => break },
            };
            match cmd {
                Cmd::Open {
                    path,
                    preview,
                    workspace_id,
                    reply,
                } => {
                    let result =
                        open_buffer(&mut editor, &mut tracked, &path, preview, &workspace_id);
                    let _ = reply.send(result);
                }
                Cmd::New {
                    workspace_id,
                    reply,
                } => {
                    let result = create_untitled(&mut editor, &mut tracked, &workspace_id);
                    let _ = reply.send(result);
                }
                Cmd::Edit {
                    buffer_id,
                    base_rev,
                    text,
                    reply,
                } => {
                    let result =
                        edit_buffer(&mut editor, &mut tracked, &buffer_id, base_rev, &text)
                            .and_then(|rev| {
                                checkpoint(&drafts, &tracked, &buffer_id)?;
                                Ok(rev)
                            });
                    let _ = reply.send(result);
                }
                Cmd::RangeEdit {
                    buffer_id,
                    view_id,
                    base_rev,
                    edits,
                    selection,
                    reply,
                } => {
                    let result = range_edit_buffer(
                        &mut editor,
                        &mut tracked,
                        &buffer_id,
                        &view_id,
                        base_rev,
                        edits,
                        selection,
                    )
                    .and_then(|result| {
                        if result.accepted {
                            if result.dirty {
                                checkpoint(&drafts, &tracked, &buffer_id)?;
                            }
                        }
                        Ok(result)
                    });
                    let _ = reply.send(result);
                }
                Cmd::Action {
                    buffer_id,
                    view_id,
                    base_rev,
                    action,
                    selection,
                    reply,
                } => {
                    let result = action_buffer(
                        &mut editor,
                        &mut tracked,
                        &buffer_id,
                        &view_id,
                        base_rev,
                        action,
                        selection,
                    )
                    .and_then(|result| {
                        if result.accepted {
                            if result.dirty {
                                checkpoint(&drafts, &tracked, &buffer_id)?;
                            } else if let Some(entry) = tracked.get(&buffer_id) {
                                drafts.discard(&entry.workspace_id, &entry.draft_id)?;
                            }
                        }
                        Ok(result)
                    });
                    let _ = reply.send(result);
                }
                Cmd::Sync { buffer_id, reply } => {
                    let result =
                        sync_buffer(&mut editor, &mut tracked, &buffer_id).and_then(|result| {
                            checkpoint(&drafts, &tracked, &buffer_id)?;
                            Ok(result)
                        });
                    let _ = reply.send(result);
                }
                Cmd::Save {
                    buffer_id,
                    base_rev,
                    path,
                    reply,
                } => {
                    let result = save_buffer(
                        &mut editor,
                        &mut tracked,
                        &buffer_id,
                        base_rev,
                        path.as_deref(),
                    )
                    .and_then(|saved| {
                        let entry = tracked.get(&buffer_id).expect("saved buffer");
                        drafts.discard(&entry.workspace_id, &entry.draft_id)?;
                        Ok(saved)
                    });
                    let _ = reply.send(result);
                }
                Cmd::Close {
                    buffer_id,
                    workspace_id,
                    reply,
                } => {
                    let result = match tracked.get(&buffer_id) {
                        Some(entry) if entry.workspace_id != workspace_id => {
                            Err(anyhow::anyhow!("buffer belongs to another workspace"))
                        }
                        Some(_) => close_buffer(&mut editor, &mut tracked, &buffer_id),
                        None => Ok(()),
                    };
                    let _ = reply.send(result);
                }
                Cmd::LspGet {
                    buffer_id,
                    known_rev,
                    reply,
                } => {
                    let result = lsp_state(&mut editor, &mut tracked, &buffer_id, known_rev)
                        .and_then(|result| {
                            if result.text.is_some() { checkpoint(&drafts, &tracked, &buffer_id)?; }
                            Ok(result)
                        });
                    let _ = reply.send(result);
                }
                Cmd::Format {
                    buffer_id,
                    base_rev,
                    reply,
                } => {
                    let result = format_buffer(&mut editor, &mut tracked, &buffer_id, base_rev)
                        .await
                        .and_then(|result| {
                            if result.text.is_some() {
                                checkpoint(&drafts, &tracked, &buffer_id)?;
                            }
                            Ok(result)
                        });
                    let _ = reply.send(result);
                }
                Cmd::Reconfigure { config, reply } => {
                    editor.active_window_mut().lsp.shutdown_all();
                    let languages: Vec<_> = editor.config().lsp.keys().cloned().collect();
                    for language in languages {
                        editor.set_lsp_config(language, Vec::new());
                    }
                    let mut fresh_config = editor.config().clone();
                    fresh_config.lsp = config.lsp.clone();
                    fresh_config.lsp_enabled = !fresh_config.lsp.is_empty();
                    apply_language_associations(&mut fresh_config, &config.languages);
                    for language in fresh_config.lsp.keys() {
                        if let Some(config) = fresh_config.languages.get_mut(language) {
                            config.formatter = None;
                        }
                    }
                    editor.set_config(fresh_config);
                    let lsp_enabled = editor.config().lsp_enabled;
                    editor
                        .active_window_mut()
                        .lsp
                        .set_globally_enabled(lsp_enabled);
                    let new_servers: Vec<_> = editor
                        .config()
                        .lsp
                        .iter()
                        .map(|(language, servers)| (language.clone(), servers.as_slice().to_vec()))
                        .collect();
                    for (language, servers) in new_servers {
                        editor.set_lsp_config(language, servers);
                    }
                    let _ = reply.send(Ok(()));
                }
                Cmd::DraftList {
                    workspace_id,
                    reply,
                } => {
                    let _ = reply.send(drafts.list(&workspace_id));
                }
                Cmd::DraftRestore {
                    workspace_id,
                    draft_id,
                    reply,
                } => {
                    let result =
                        restore_draft(&mut editor, &mut tracked, &drafts, &workspace_id, &draft_id);
                    let _ = reply.send(result);
                }
                Cmd::DraftDiscard { buffer_id, reply } => {
                    let result = tracked
                        .get_mut(&buffer_id)
                        .context("unknown buffer")
                        .and_then(|entry| {
                            drafts.discard(&entry.workspace_id, &entry.draft_id)?;
                            Ok(())
                        })
                        .and_then(|()| close_buffer(&mut editor, &mut tracked, &buffer_id));
                    let _ = reply.send(result);
                }
                Cmd::CheckWorkspace {
                    buffer_id,
                    workspace_id,
                    reply,
                } => {
                    let result =
                        tracked
                            .get(&buffer_id)
                            .context("unknown buffer")
                            .and_then(|entry| {
                                if entry.workspace_id == workspace_id {
                                    Ok(())
                                } else {
                                    bail!("buffer belongs to another workspace")
                                }
                            });
                    let _ = reply.send(result);
                }
                Cmd::Scene { reply } => {
                    let _ = sync_all_fresh_text(&editor, &mut tracked);
                    let active = editor.active_buffer().0.to_string();
                    let buffers = tracked
                        .iter()
                        .map(|(id, t)| SceneBuffer {
                            buffer_id: id.clone(),
                            path: t
                                .path
                                .as_ref()
                                .map(|path| path.display().to_string())
                                .unwrap_or_default(),
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
    drop(editor);
}

fn apply_language_associations(
    config: &mut Config,
    associations: &HashMap<String, fresh::config::LanguageConfig>,
) {
    for (name, override_config) in associations {
        if let Some(existing) = config.languages.get_mut(name) {
            if !override_config.extensions.is_empty() {
                existing.extensions = override_config.extensions.clone();
            }
            if !override_config.filenames.is_empty() {
                existing.filenames = override_config.filenames.clone();
            }
        } else {
            config
                .languages
                .insert(name.clone(), override_config.clone());
        }
    }
}

fn open_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    path: &Path,
    preview: bool,
    workspace_id: &str,
) -> Result<OpenedBuffer> {
    sync_all_fresh_text(editor, tracked)?;
    let same_path = |candidate: &Path| {
        candidate
            .canonicalize()
            .unwrap_or_else(|_| candidate.to_path_buf())
            == path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
    };
    if tracked.values().any(|entry| {
        entry.workspace_id != workspace_id && entry.path.as_deref().is_some_and(same_path)
    }) {
        bail!(
            "this file is already open in another workspace; close it there before opening it here"
        );
    }
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }
    if crate::binary::is_binary_file(path)? {
        return Err(anyhow::Error::new(crate::binary::BinaryFile {
            path: path.to_path_buf(),
        }));
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

    let language = Some(editor.active_state().language.clone());
    let text = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer has unloaded regions (large-file mode); cannot snapshot")?;
    let dirty = editor.active_state().buffer.is_modified();

    if text.len() > MAX_SNAPSHOT_BYTES {
        bail!(
            "snapshot too large ({} bytes; max {MAX_SNAPSHOT_BYTES})",
            text.len()
        );
    }

    let id = buffer_id.0.to_string();
    let rev = tracked.get(&id).map(|t| t.rev).unwrap_or(0);
    let base_text = tracked
        .get(&id)
        .filter(|entry| entry.workspace_id == workspace_id && entry.path.as_deref() == Some(path))
        .and_then(|entry| entry.base_text.clone())
        .unwrap_or_else(|| text.clone());
    tracked.insert(
        id.clone(),
        TrackedBuffer {
            path: Some(path.to_path_buf()),
            text: text.clone(),
            rev,
            dirty,
            language: language.clone(),
            workspace_id: workspace_id.to_owned(),
            draft_id: crate::drafts::named_draft_id(workspace_id, path),
            base_text: Some(base_text),
            recovery_path: Some(path.to_path_buf()),
        },
    );

    Ok(OpenedBuffer {
        draft_id: tracked[&id].draft_id.clone(),
        buffer_id: id,
        path: path.display().to_string(),
        language,
        rev,
        text,
    })
}

fn checkpoint(
    store: &DraftStore,
    tracked: &HashMap<String, TrackedBuffer>,
    buffer_id: &str,
) -> Result<()> {
    let entry = tracked.get(buffer_id).context("unknown buffer")?;
    if !entry.dirty {
        // A clean named open may be happening immediately before a recovery
        // restore. Explicit Save, Discard, or undo-to-savepoint removes copies.
        return Ok(());
    }
    store.checkpoint(
        &entry.workspace_id,
        Draft {
            draft_id: entry.draft_id.clone(),
            path: entry
                .recovery_path
                .as_ref()
                .map(|path| path.display().to_string()),
            text: entry.text.clone(),
            base_text: entry.base_text.clone(),
        },
    )
}

fn restore_draft(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    store: &DraftStore,
    workspace_id: &str,
    draft_id: &str,
) -> Result<(OpenedBuffer, bool)> {
    let draft = store
        .get(workspace_id, draft_id)?
        .context("draft not found")?;
    let source_changed = DraftStore::source_changed(&draft);
    let mut opened =
        if let Some(path) = draft.path.as_ref().filter(|path| Path::new(path).is_file()) {
            match open_buffer(editor, tracked, Path::new(path), false, workspace_id) {
                Ok(opened) => opened,
                Err(_) => create_untitled(editor, tracked, workspace_id)?,
            }
        } else {
            create_untitled(editor, tracked, workspace_id)?
        };
    let buffer_id = opened.buffer_id.clone();
    let current = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer unavailable")?;
    if current != draft.text {
        let rev = edit_buffer(editor, tracked, &buffer_id, opened.rev, &draft.text)?;
        opened.rev = rev;
    }
    // Restore the dirty state even when an external writer happens to have
    // installed text identical to the draft since its original checkpoint.
    editor.active_state_mut().buffer.set_modified(true);
    let entry = tracked.get_mut(&buffer_id).expect("restored buffer");
    entry.draft_id = draft.draft_id;
    entry.workspace_id = workspace_id.to_owned();
    entry.base_text = draft.base_text;
    entry.recovery_path = draft.path.as_ref().map(PathBuf::from);
    entry.dirty = true;
    entry.text = draft.text.clone();
    // Fresh may hold a missing source as an unnamed buffer; recovery_path
    // preserves its intended save target independently of Fresh's file path.
    opened.path = draft.path.unwrap_or_default();
    let restored_draft_id = entry.draft_id.clone();
    opened.draft_id = restored_draft_id;
    opened.text = draft.text;
    opened.language = entry.language.clone();
    checkpoint(store, tracked, &buffer_id)?;
    Ok((opened, source_changed))
}

fn activate_tracked(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    buffer_id: &str,
) -> Result<()> {
    let Some(entry) = tracked.get(buffer_id) else {
        bail!("unknown buffer_id {buffer_id}");
    };
    match &entry.path {
        Some(path) => {
            // open_file switches to an already-open buffer when the path matches.
            editor
                .open_file(path)
                .with_context(|| format!("activate {}", path.display()))?;
        }
        None => {
            let id = BufferId(
                buffer_id
                    .parse()
                    .with_context(|| format!("buffer id {buffer_id}"))?,
            );
            // `new_buffer` already activates the buffer. `switch_buffer` is a
            // no-op when that id is current, and the only public way back to
            // an unnamed buffer (`set_active_buffer` is crate-private).
            editor.switch_buffer(id);
        }
    }
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
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    let current = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?
        .rev;
    if current != base_rev {
        bail!("revision conflict: base_rev={base_rev} current={current}");
    }
    activate_tracked(editor, tracked, buffer_id)?;
    let old = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer has unloaded regions")?;
    if old != text {
        let cursor_id = editor.active_cursors().primary_id();
        let mut events = Vec::new();
        if !old.is_empty() {
            events.push(Event::Delete {
                range: 0..old.len(),
                deleted_text: old.clone(),
                cursor_id,
            });
        }
        if !text.is_empty() {
            events.push(Event::Insert {
                position: 0,
                text: text.to_owned(),
                cursor_id,
            });
        }
        editor.log_and_apply_event(&Event::Batch {
            events,
            description: "ADE full-text edit".into(),
        });
    }
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    entry.text = text.to_owned();
    entry.rev += 1;
    entry.dirty = true;
    Ok(entry.rev)
}

fn current_selection(editor: &Editor) -> ByteSelection {
    let cursor = editor.active_cursors().primary();
    ByteSelection {
        anchor: cursor.anchor.unwrap_or(cursor.position),
        head: cursor.position,
    }
}

fn transaction_result(
    tracked: &HashMap<String, TrackedBuffer>,
    editor: &Editor,
    buffer_id: &str,
    accepted: bool,
) -> Result<BufferTransactionResult> {
    let entry = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?;
    let id: usize = buffer_id.parse().context("invalid buffer_id")?;
    let text = editor
        .active_window()
        .buffers
        .get(&BufferId(id))
        .and_then(|state| state.buffer.to_string())
        .context("Fresh buffer unavailable")?;
    Ok(BufferTransactionResult {
        rev: entry.rev,
        text,
        selection: current_selection(editor),
        accepted,
        dirty: entry.dirty,
    })
}

fn set_selection(editor: &mut Editor, selection: ByteSelection) {
    let cursor = editor.active_cursors_mut().primary_mut();
    cursor.position = selection.head;
    cursor.anchor = (selection.anchor != selection.head).then_some(selection.anchor);
}

fn range_edit_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    view_id: &str,
    base_rev: u64,
    edits: Vec<RangeEdit>,
    selection: ByteSelection,
) -> Result<BufferTransactionResult> {
    if view_id.is_empty() {
        bail!("view_id cannot be empty");
    }
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    activate_tracked(editor, tracked, buffer_id)?;
    let initial = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer has unloaded regions")?;
    if initial.len() > MAX_SNAPSHOT_BYTES {
        bail!("buffer exceeds edit limit");
    }
    let current_rev = tracked.get(buffer_id).context("unknown buffer")?.rev;
    if current_rev != base_rev {
        return transaction_result(tracked, editor, buffer_id, false);
    }

    // Validate and construct every event before mutating Fresh. Ranges are
    // sequential: each one indexes the text produced by earlier edits.
    let mut working = initial.clone();
    let cursor_id = editor.active_cursors().primary_id();
    let before_cursor = editor.active_cursors().primary();
    let (old_position, old_anchor, old_sticky_column) = (
        before_cursor.position,
        before_cursor.anchor,
        before_cursor.sticky_column,
    );
    let mut events = Vec::with_capacity(3);
    for edit in edits {
        if edit.start > edit.end
            || edit.end > working.len()
            || !working.is_char_boundary(edit.start)
            || !working.is_char_boundary(edit.end)
        {
            bail!(
                "invalid UTF-8 byte range {}..{} for {} byte buffer",
                edit.start,
                edit.end,
                working.len()
            );
        }
        working.replace_range(edit.start..edit.end, &edit.text);
        if working.len() > MAX_SNAPSHOT_BYTES {
            bail!("edit too large (max {MAX_SNAPSHOT_BYTES} bytes)");
        }
    }
    if selection.anchor > working.len()
        || selection.head > working.len()
        || !working.is_char_boundary(selection.anchor)
        || !working.is_char_boundary(selection.head)
    {
        bail!("selection is outside the edited buffer or not on a UTF-8 boundary");
    }

    // Collapse the sequential protocol edits into one equivalent replacement.
    // Fresh computes all Batch LSP ranges against the pre-event buffer, while
    // LSP applies change arrays sequentially. A single net replacement keeps
    // both coordinate systems correct for arbitrary client edit sequences.
    let prefix = initial
        .bytes()
        .zip(working.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    let mut prefix = prefix;
    while !initial.is_char_boundary(prefix) || !working.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let suffix_max = initial
        .len()
        .saturating_sub(prefix)
        .min(working.len().saturating_sub(prefix));
    let mut suffix = initial.as_bytes()[initial.len() - suffix_max..]
        .iter()
        .rev()
        .zip(
            working.as_bytes()[working.len() - suffix_max..]
                .iter()
                .rev(),
        )
        .take_while(|(a, b)| a == b)
        .count();
    while suffix > 0
        && (!initial.is_char_boundary(initial.len() - suffix)
            || !working.is_char_boundary(working.len() - suffix))
    {
        suffix -= 1;
    }
    let old_end = initial.len() - suffix;
    let new_end = working.len() - suffix;
    if old_end > prefix {
        events.push(Event::Delete {
            range: prefix..old_end,
            deleted_text: initial[prefix..old_end].to_owned(),
            cursor_id,
        });
    }
    if new_end > prefix {
        events.push(Event::Insert {
            position: prefix,
            text: working[prefix..new_end].to_owned(),
            cursor_id,
        });
    }

    if working != initial
        || old_position != selection.head
        || old_anchor != (selection.anchor != selection.head).then_some(selection.anchor)
    {
        events.push(Event::MoveCursor {
            cursor_id,
            old_position,
            new_position: selection.head,
            old_anchor,
            new_anchor: (selection.anchor != selection.head).then_some(selection.anchor),
            old_sticky_column,
            new_sticky_column: None,
        });
        editor.log_and_apply_event(&Event::Batch {
            events,
            description: "ADE range edit".into(),
        });
    }
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    if working != initial {
        entry.rev += 1;
        entry.dirty = true;
        }
    entry.text = working;
    transaction_result(tracked, editor, buffer_id, true)
}

fn action_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    view_id: &str,
    base_rev: u64,
    action: EditorAction,
    selection: ByteSelection,
) -> Result<BufferTransactionResult> {
    if view_id.is_empty() {
        bail!("view_id cannot be empty");
    }
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    activate_tracked(editor, tracked, buffer_id)?;
    let current_rev = tracked.get(buffer_id).context("unknown buffer")?.rev;
    let text = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer has unloaded regions")?;
    if current_rev != base_rev {
        return transaction_result(tracked, editor, buffer_id, false);
    }
    if selection.anchor > text.len()
        || selection.head > text.len()
        || !text.is_char_boundary(selection.anchor)
        || !text.is_char_boundary(selection.head)
    {
        bail!("selection is outside the buffer or not on a UTF-8 boundary");
    }
    set_selection(editor, selection);
    match action {
        EditorAction::Undo => editor.handle_undo(),
        EditorAction::Redo => editor.handle_redo(),
    }
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    transaction_result(tracked, editor, buffer_id, true)
}

fn sync_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
) -> Result<BufferTransactionResult> {
    activate_tracked(editor, tracked, buffer_id)?;
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    transaction_result(tracked, editor, buffer_id, true)
}

fn close_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
) -> Result<()> {
    let id: usize = buffer_id.parse().context("invalid buffer_id")?;
    if !tracked.contains_key(buffer_id) {
        bail!("unknown buffer_id {buffer_id}");
    }
    editor
        .force_close_buffer(BufferId(id))
        .context("Fresh close_buffer")?;
    tracked.remove(buffer_id);
    if tracked.is_empty() {
        // `shutdown_server(language)` marks the language manually disabled;
        // reopening that language would never auto-start. `shutdown_all`
        // releases the processes without changing auto-start policy.
        editor.active_window_mut().lsp.shutdown_all();
    }
    Ok(())
}

fn sync_fresh_text(
    editor: &Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
) -> Result<Option<String>> {
    let id: usize = buffer_id.parse().context("invalid buffer_id")?;
    let state = editor
        .active_window()
        .buffers
        .get(&BufferId(id))
        .context("Fresh buffer unavailable")?;
    let text = state
        .buffer
        .to_string()
        .context("Fresh buffer text unavailable")?;
    let dirty = state.buffer.is_modified();
    let entry = tracked
        .get_mut(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?;
    // Fresh may apply asynchronous LSP formatting. Advance the ADE revision
    // when that happens so the host receives an authoritative new snapshot.
    let text_changed = entry.text != text;
    entry.dirty = dirty;
    if !text_changed {
        return Ok(None);
    }
    entry.text = text.clone();
    entry.rev += 1;
    Ok(Some(text))
}

fn sync_all_fresh_text(
    editor: &Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
) -> Result<()> {
    let ids: Vec<String> = tracked.keys().cloned().collect();
    for id in ids {
        let _ = sync_fresh_text(editor, tracked, &id)?;
    }
    Ok(())
}

fn lsp_state(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    known_rev: u64,
) -> Result<LspState> {
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    let entry = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?;
    let path = entry.path.clone();
    let language = entry.language.clone();
    let mut status = None;
    if let Some(language) = language {
        if let Some(servers) = editor.config().lsp.get(&language) {
            for server in servers.as_slice().iter().filter(|server| server.enabled) {
                if !fresh::services::lsp::command_exists(&server.command) {
                    status = Some(format!(
                        "LSP {}: command '{}' not found on daemon host",
                        server.display_name(),
                        server.command
                    ));
                    break;
                }
            }
        }
    }
    if status.is_none() {
        status = editor
            .get_status_message()
            .filter(|message| message.starts_with("LSP"))
            .cloned();
    }
    let diagnostics = path
        .as_ref()
        .and_then(|p| fresh::services::lsp::manager::path_to_uri(p))
        .and_then(|uri| editor.get_stored_diagnostics().get(uri.as_str()).cloned())
        .unwrap_or_default()
        .into_iter()
        .map(|d| BufferDiagnostic {
            start_line: d.range.start.line,
            start_character: d.range.start.character,
            end_line: d.range.end.line,
            end_character: d.range.end.character,
            severity: d
                .severity
                .map(|s| format!("{s:?}").to_ascii_lowercase())
                .unwrap_or_else(|| "info".into()),
            message: d.message,
            source: d.source,
        })
        .collect();
    let rev = tracked[buffer_id].rev;
    let text = if rev != known_rev {
        let id: usize = buffer_id.parse().context("invalid buffer_id")?;
        editor
            .active_window()
            .buffers
            .get(&BufferId(id))
            .and_then(|state| state.buffer.to_string())
    } else {
        None
    };
    Ok(LspState {
        rev,
        text,
        diagnostics,
        status,
    })
}

async fn format_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    base_rev: u64,
) -> Result<FormatState> {
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    let current = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?
        .rev;
    if current != base_rev {
        bail!("revision conflict: base_rev={base_rev} current={current}");
    }
    let language = tracked[buffer_id].language.as_deref().unwrap_or("");
    let servers = editor
        .config()
        .lsp
        .get(language)
        .context("no language server configured for this file")?;
    let formatter = servers
        .as_slice()
        .iter()
        .find(|server| server.enabled && server.feature_filter().allows(LspFeature::Format))
        .context("no formatting language server configured for this file")?;
    if !fresh::services::lsp::command_exists(&formatter.command) {
        bail!(
            "LSP {}: command '{}' not found on daemon host",
            formatter.display_name(),
            formatter.command
        );
    }
    activate_tracked(editor, tracked, buffer_id)?;
    let before = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer has unloaded regions")?;
    let prior_status = editor.get_status_message().cloned();
    editor.format_buffer().map_err(anyhow::Error::msg)?;
    if let Some(status) = editor.get_status_message()
        && prior_status.as_ref() != Some(status)
        && (status.contains("Formatting not supported") || status.contains("LSP not available"))
    {
        bail!("{status}");
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        fresh::app::editor_tick(editor, || Ok(()))?;
        let after = editor
            .active_state()
            .buffer
            .to_string()
            .context("buffer has unloaded regions")?;
        if after != before {
            let entry = tracked.get_mut(buffer_id).expect("tracked");
            entry.text = after.clone();
            entry.rev += 1;
            entry.dirty = true;
            return Ok(FormatState {
                rev: entry.rev,
                text: Some(after),
                status: None,
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(FormatState {
                rev: current,
                text: None,
                status: Some("No formatting changes returned within 5 seconds".into()),
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}

fn create_untitled(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    workspace_id: &str,
) -> Result<OpenedBuffer> {
    let buffer_id = editor.new_buffer();
    let id = buffer_id.0.to_string();
    let language = editor.active_buffer_mode().map(|mode| mode.to_owned());
    let text = editor.active_state().buffer.to_string().unwrap_or_default();
    tracked.insert(
        id.clone(),
        TrackedBuffer {
            path: None,
            text: text.clone(),
            rev: 0,
            dirty: false,
            language: language.clone(),
            workspace_id: workspace_id.to_owned(),
            draft_id: format!("untitled:{}", uuid::Uuid::new_v4()),
            base_text: None,
            recovery_path: None,
        },
    );
    Ok(OpenedBuffer {
        draft_id: tracked[&id].draft_id.clone(),
        buffer_id: id,
        path: String::new(),
        language,
        rev: 0,
        text,
    })
}

fn save_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    base_rev: u64,
    dest: Option<&Path>,
) -> Result<(String, u64)> {
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    let current = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?
        .rev;
    if current != base_rev {
        bail!("revision conflict: base_rev={base_rev} current={current}");
    }
    activate_tracked(editor, tracked, buffer_id)?;
    let explicit_destination = dest.is_some();
    let had_fresh_path = tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.path.is_some());
    let save_path = dest.map(Path::to_path_buf).or_else(|| {
        tracked
            .get(buffer_id)
            .and_then(|entry| entry.path.clone().or_else(|| entry.recovery_path.clone()))
    });
    if let Some(dest) = save_path.as_deref() {
        if explicit_destination || !had_fresh_path {
            editor
                .active_state_mut()
                .buffer
                .save_to_file(dest)
                .with_context(|| format!("save to {}", dest.display()))?;
        } else {
            editor.save().context("Editor::save")?;
        }
        let language = editor.active_buffer_mode().map(|mode| mode.to_owned());
        let entry = tracked.get_mut(buffer_id).expect("tracked");
        entry.path = Some(dest.to_path_buf());
        entry.recovery_path = Some(dest.to_path_buf());
        if language.is_some() {
            entry.language = language;
        }
    } else {
        bail!("unsaved buffer needs a path");
    }
    let text = editor.active_state().buffer.to_string().unwrap_or_default();
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    entry.text = text;
    entry.base_text = Some(entry.text.clone());
    entry.dirty = false;
    // Bump rev so peers know disk matches this generation.
    entry.rev += 1;
    Ok((
        entry
            .path
            .as_ref()
            .expect("saved buffer has a path")
            .display()
            .to_string(),
        entry.rev,
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn dirty_untitled_draft_survives_worker_restart_and_failed_save() {
        let root =
            std::env::temp_dir().join(format!("fresh-gui-draft-test-{}", uuid::Uuid::new_v4()));
        let recovery = root.join("recovery");
        std::fs::create_dir_all(&root).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery.clone(),
        )
        .expect("Fresh worker starts");
        let draft_id = rt.block_on(async {
            let opened = editor
                .new_buffer_in_workspace("workspace-one".into())
                .await
                .unwrap();
            let draft_id = opened.draft_id.clone();
            editor
                .edit(
                    opened.buffer_id.clone(),
                    opened.rev,
                    "unsaved scratch".into(),
                )
                .await
                .unwrap();
            let drafts = editor.draft_list("workspace-one".into()).await.unwrap();
            assert_eq!(drafts.len(), 1);
            assert_eq!(drafts[0].text, "unsaved scratch");
            assert!(
                editor
                    .draft_list("workspace-two".into())
                    .await
                    .unwrap()
                    .is_empty()
            );
            let missing = root.join("missing").join("file.txt");
            assert!(
                editor
                    .save(opened.buffer_id.clone(), 1, Some(missing))
                    .await
                    .is_err()
            );
            assert_eq!(
                editor
                    .draft_list("workspace-one".into())
                    .await
                    .unwrap()
                    .len(),
                1
            );
            draft_id
        });
        drop(editor);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let restarted = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery,
        )
        .expect("restarted Fresh worker");
        rt.block_on(async {
            let (restored, changed) = restarted
                .draft_restore("workspace-one".into(), draft_id.clone())
                .await
                .unwrap();
            assert!(!changed);
            assert_eq!(restored.text, "unsaved scratch");
            assert!(
                restarted.draft_list("workspace-one".into()).await.unwrap()[0].draft_id == draft_id
            );
            restarted.draft_discard(restored.buffer_id).await.unwrap();
            assert!(
                restarted
                    .draft_list("workspace-one".into())
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
        drop(restarted);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn named_draft_restarts_without_touching_source_until_save() {
        let root =
            std::env::temp_dir().join(format!("fresh-gui-named-draft-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("note.txt");
        std::fs::write(&source, "original").unwrap();
        let recovery = root.join("recovery");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery.clone(),
        )
        .unwrap();
        let draft_id = rt.block_on(async {
            let opened = editor
                .open_in_workspace(source.clone(), false, "workspace-named".into())
                .await
                .unwrap();
            editor
                .edit(opened.buffer_id, opened.rev, "unsaved change".into())
                .await
                .unwrap();
            assert_eq!(std::fs::read_to_string(&source).unwrap(), "original");
            opened.draft_id
        });
        drop(editor);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let restarted = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery,
        )
        .unwrap();
        rt.block_on(async {
            let (opened, changed) = restarted
                .draft_restore("workspace-named".into(), draft_id)
                .await
                .unwrap();
            assert!(!changed);
            assert_eq!(opened.text, "unsaved change");
            restarted
                .save(opened.buffer_id, opened.rev, None)
                .await
                .unwrap();
            assert_eq!(std::fs::read_to_string(&source).unwrap(), "unsaved change");
            assert!(
                restarted
                    .draft_list("workspace-named".into())
                    .await
                    .unwrap()
                    .is_empty()
            );
        });
        drop(restarted);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_named_source_restores_as_dirty_reviewable_buffer() {
        let root =
            std::env::temp_dir().join(format!("fresh-gui-missing-draft-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let recovery = root.join("recovery");
        let missing = root.join("deleted.txt");
        let store = DraftStore::new(recovery.clone());
        store
            .checkpoint(
                "workspace",
                Draft {
                    draft_id: "named-missing".into(),
                    path: Some(missing.display().to_string()),
                    text: "review this".into(),
                    base_text: Some("old source".into()),
                },
            )
            .unwrap();
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery,
        )
        .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (opened, changed) = editor
                .draft_restore("workspace".into(), "named-missing".into())
                .await
                .unwrap();
            assert!(changed);
            assert_eq!(opened.path, missing.display().to_string());
            assert_eq!(opened.text, "review this");
            let scene = editor.scene().await.unwrap();
            assert!(
                scene
                    .buffers
                    .iter()
                    .any(|buffer| buffer.buffer_id == opened.buffer_id && buffer.dirty)
            );
            editor.draft_discard(opened.buffer_id).await.unwrap();
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn range_transactions_validate_byte_offsets_stale_revisions_and_fresh_undo_redo() {
        let root =
            std::env::temp_dir().join(format!("fresh-gui-range-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let editor = EditorHandle::spawn(root.clone(), crate::config::Config::default())
            .expect("Fresh worker starts");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opened = editor.new_buffer().await.unwrap();
            // Deliberately leave another buffer active while targeting `opened`.
            let other = editor.new_buffer().await.unwrap();
            let initial = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "view-a".into(),
                    0,
                    vec![
                        RangeEdit {
                            start: 0,
                            end: 0,
                            text: "a🙂界z".into(),
                        },
                        // The second edit addresses the text after the first edit;
                        // the emoji occupies four UTF-8 bytes.
                        RangeEdit {
                            start: 1,
                            end: 5,
                            text: "🧪".into(),
                        },
                    ],
                    ByteSelection { anchor: 1, head: 5 },
                )
                .await
                .unwrap();
            assert!(initial.accepted);
            assert_eq!(initial.text, "a🧪界z");
            assert_eq!(initial.rev, 1);
            assert_eq!(initial.selection, ByteSelection { anchor: 1, head: 5 });
            assert_eq!(editor.sync(other.buffer_id.clone()).await.unwrap().text, "");

            // A stale transaction returns the current snapshot without changing it.
            let stale = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "view-a".into(),
                    0,
                    vec![RangeEdit {
                        start: 0,
                        end: 0,
                        text: "lost".into(),
                    }],
                    initial.selection,
                )
                .await
                .unwrap();
            assert!(!stale.accepted);
            assert_eq!(stale.rev, 1);
            assert_eq!(stale.text, "a🧪界z");

            let appended = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "view-a".into(),
                    1,
                    vec![RangeEdit {
                        start: 9,
                        end: 9,
                        text: "!".into(),
                    }],
                    ByteSelection {
                        anchor: 10,
                        head: 10,
                    },
                )
                .await
                .unwrap();
            assert!(appended.accepted);
            assert_eq!(appended.text, "a🧪界z!");
            assert_eq!(appended.rev, 2);

            let undo_append = editor
                .action(
                    opened.buffer_id.clone(),
                    "view-a".into(),
                    2,
                    EditorAction::Undo,
                    appended.selection,
                )
                .await
                .unwrap();
            assert_eq!(undo_append.text, "a🧪界z");
            assert_eq!(undo_append.rev, 3);
            assert_eq!(undo_append.selection, ByteSelection { anchor: 1, head: 5 });
            let undo_group = editor
                .action(
                    opened.buffer_id.clone(),
                    "view-a".into(),
                    3,
                    EditorAction::Undo,
                    undo_append.selection,
                )
                .await
                .unwrap();
            assert_eq!(undo_group.text, "");
            assert_eq!(undo_group.rev, 4);
            assert_eq!(undo_group.selection, ByteSelection { anchor: 0, head: 0 });
            let redo_group = editor
                .action(
                    opened.buffer_id.clone(),
                    "view-a".into(),
                    4,
                    EditorAction::Redo,
                    undo_group.selection,
                )
                .await
                .unwrap();
            assert_eq!(redo_group.text, "a🧪界z");
            assert_eq!(redo_group.rev, 5);
            assert_eq!(redo_group.selection, initial.selection);

            // A reconnect can request a fresh authoritative snapshot regardless
            // of the revision cached by the client.
            let synced = editor.sync(opened.buffer_id.clone()).await.unwrap();
            assert_eq!(synced.text, "a🧪界z");
            assert_eq!(synced.rev, 5);

            let invalid = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "view-a".into(),
                    5,
                    vec![
                        RangeEdit {
                            start: 9,
                            end: 9,
                            text: "partial".into(),
                        },
                        // A later invalid operation must reject the whole transaction.
                        RangeEdit {
                            start: 2,
                            end: 3,
                            text: String::new(),
                        },
                    ],
                    redo_group.selection,
                )
                .await;
            assert!(
                invalid.is_err(),
                "mid-character byte offsets must be rejected"
            );
            let after_invalid = editor.sync(opened.buffer_id.clone()).await.unwrap();
            assert_eq!(after_invalid.text, "a🧪界z");
            assert_eq!(after_invalid.rev, 5);
        });
        let _ = std::fs::remove_dir_all(root);
    }

    // A stdio LSP exercised through Fresh and the ADE worker, including
    // two Python servers and a TOML server. No developer-installed binary is needed.
    const FAKE_LSP: &str = r#"#!/usr/bin/env python3
import json, sys
source = sys.argv[1]
log_path = sys.argv[0] + '.' + source + '.log'
documents = {}
def send(obj):
    body = json.dumps(obj).encode()
    sys.stdout.buffer.write(b'Content-Length: %d\r\n\r\n' % len(body) + body)
    sys.stdout.buffer.flush()
def read():
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line: return None
        if line == b'\r\n': break
        key, value = line.decode().split(':', 1)
        headers[key.lower()] = value.strip()
    return json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
def byte_offset(text, position):
    lines = text.splitlines(keepends=True)
    line = position['line']
    character = position['character']
    prefix = ''.join(lines[:line])
    content = lines[line] if line < len(lines) else ''
    byte_count = 0
    utf16_count = 0
    for char in content:
        if utf16_count == character: return len(prefix.encode('utf-8')) + byte_count
        units = 2 if ord(char) > 0xffff else 1
        if utf16_count + units > character: raise ValueError('position splits UTF-16 surrogate pair')
        utf16_count += units
        byte_count += len(char.encode('utf-8'))
    if utf16_count != character: raise ValueError('position past line end')
    return len(prefix.encode('utf-8')) + byte_count
def record_document(uri):
    with open(log_path + '.text', 'a') as log:
        log.write(json.dumps(documents[uri], ensure_ascii=False) + '\n')
while True:
    msg = read()
    if msg is None: break
    method = msg.get('method')
    if method:
        with open(log_path, 'a') as log: log.write(method + '\n')
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':msg['id'],'result':{
            'capabilities':{'textDocumentSync':2,'documentFormattingProvider':True}}})
    elif method in ('textDocument/didOpen', 'textDocument/didChange'):
        uri = msg['params']['textDocument']['uri']
        if method == 'textDocument/didOpen':
            documents[uri] = msg['params']['textDocument']['text']
        else:
            with open(log_path + '.changes', 'a') as log:
                log.write(json.dumps(msg['params']['contentChanges'], ensure_ascii=False) + '\n')
            for change in msg['params']['contentChanges']:
                if 'range' not in change:
                    documents[uri] = change['text']
                else:
                    start = byte_offset(documents[uri], change['range']['start'])
                    end = byte_offset(documents[uri], change['range']['end'])
                    raw = documents[uri].encode('utf-8')
                    documents[uri] = (raw[:start] + change['text'].encode('utf-8') + raw[end:]).decode('utf-8')
        record_document(uri)
        bad = method == 'textDocument/didOpen'
        send({'jsonrpc':'2.0','method':'textDocument/publishDiagnostics','params':{
            'uri':uri,'diagnostics':[{'range':{'start':{'line':0,'character':0},
              'end':{'line':0,'character':3}},'severity':1,'source':source,
              'message':'bad value'}] if bad else []}})
    elif method == 'textDocument/formatting':
        send({'jsonrpc':'2.0','id':msg['id'],'result':[{'range':{
            'start':{'line':0,'character':0},'end':{'line':1,'character':0}},
            'newText':'formatted = true\n'}]})
    elif method == 'shutdown':
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
    elif 'id' in msg:
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
"#;

    #[test]
    fn incremental_lsp_applies_unicode_and_newline_range_batch_undo_redo() {
        if !fresh::services::lsp::command_exists("python3") {
            return;
        }
        let root =
            std::env::temp_dir().join(format!("fresh-gui-lsp-range-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let script_path = root.join("fake_lsp.py");
        std::fs::write(&script_path, FAKE_LSP).unwrap();
        let log_path = format!("{}.Incremental.log.text", script_path.display());
        let change_log_path = format!("{}.Incremental.log.changes", script_path.display());
        let source = root.join("range.py");
        let original = "a🙂界z\nold\n";
        std::fs::write(&source, original).unwrap();
        let cfg = crate::config::Config::parse(&serde_json::json!({
            "lsp": { "python": { "name":"Incremental", "command":"python3", "args":[script_path.display().to_string(), "Incremental"], "only_features":["diagnostics"] } }
        }).to_string()).unwrap();
        let editor = EditorHandle::spawn(root.clone(), cfg).expect("Fresh worker starts");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            async fn wait_documents(path: &str, wanted: usize) -> Vec<String> {
                for _ in 0..120 {
                    let contents = std::fs::read_to_string(path).unwrap_or_default();
                    let documents: Vec<String> = contents
                        .lines()
                        .filter_map(|line| serde_json::from_str(line).ok())
                        .collect();
                    if documents.len() >= wanted {
                        return documents;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                panic!(
                    "LSP did not apply {wanted} document updates; log: {}",
                    std::fs::read_to_string(path).unwrap_or_default()
                );
            }

            let opened = editor.open(source.clone(), false).await.unwrap();
            wait_documents(&log_path, 1).await;
            // Both ranges address sequential text: replace emoji + CJK on the
            // first line, then replace a line with text containing a newline.
            let edited = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "view-lsp".into(),
                    opened.rev,
                    vec![
                        RangeEdit {
                            start: 1,
                            end: 8,
                            text: "😃中".into(),
                        },
                        RangeEdit {
                            start: 10,
                            end: 13,
                            text: "新🙂\nnext".into(),
                        },
                    ],
                    ByteSelection { anchor: 1, head: 8 },
                )
                .await
                .unwrap();
            let edited_text = "a😃中z\n新🙂\nnext\n";
            assert_eq!(edited.text, edited_text);
            let reopened = editor.open(source, false).await.unwrap();
            assert_eq!(reopened.buffer_id, opened.buffer_id);
            assert!(
                editor
                    .scene()
                    .await
                    .unwrap()
                    .buffers
                    .iter()
                    .find(|buffer| buffer.buffer_id == opened.buffer_id)
                    .unwrap()
                    .dirty,
                "reopening a modified Fresh buffer must preserve its dirty state"
            );
            let docs = wait_documents(&log_path, 2).await;
            assert_eq!(docs[0], original);
            assert_eq!(
                docs[1], edited_text,
                "LSP applied the range changes using UTF-16 positions"
            );

            let undone = editor
                .action(
                    opened.buffer_id.clone(),
                    "view-lsp".into(),
                    edited.rev,
                    EditorAction::Undo,
                    edited.selection,
                )
                .await
                .unwrap();
            assert_eq!(undone.text, original);
            assert_eq!(undone.selection, ByteSelection { anchor: 0, head: 0 });
            assert!(
                !editor
                    .scene()
                    .await
                    .unwrap()
                    .buffers
                    .iter()
                    .find(|buffer| buffer.buffer_id == opened.buffer_id)
                    .unwrap()
                    .dirty,
                "undo to Fresh's save point must clear ADE dirty state"
            );
            let docs = wait_documents(&log_path, 3).await;
            assert_eq!(
                docs[2], original,
                "Fresh undo range changes reconstruct the document"
            );

            let redone = editor
                .action(
                    opened.buffer_id.clone(),
                    "view-lsp".into(),
                    undone.rev,
                    EditorAction::Redo,
                    undone.selection,
                )
                .await
                .unwrap();
            assert_eq!(redone.text, edited_text);
            assert_eq!(redone.selection, edited.selection);
            assert!(
                editor
                    .scene()
                    .await
                    .unwrap()
                    .buffers
                    .iter()
                    .find(|buffer| buffer.buffer_id == opened.buffer_id)
                    .unwrap()
                    .dirty
            );
            let docs = wait_documents(&log_path, 4).await;
            assert_eq!(
                docs[3], edited_text,
                "Fresh redo range changes reconstruct the document"
            );
            let change_groups: Vec<Vec<serde_json::Value>> =
                std::fs::read_to_string(&change_log_path)
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
            assert_eq!(
                change_groups.len(),
                3,
                "forward edit, undo, and redo each notify once"
            );
            assert!(
                change_groups
                    .iter()
                    .flatten()
                    .all(|change| change.get("range").is_some()),
                "incremental LSP sync must carry ranges, not whole-document replacements"
            );
            editor.close(opened.buffer_id).await.unwrap();
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn lsp_diagnostics_format_and_missing_binary() {
        if !fresh::services::lsp::command_exists("python3") {
            return;
        }
        let root =
            std::env::temp_dir().join(format!("fresh-gui-lsp-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let script = root.join("fake_lsp.py");
        std::fs::write(&script, FAKE_LSP).unwrap();
        let script = script.display().to_string();
        let missing = root.join("missing-lsp").display().to_string();
        let cfg = crate::config::Config::parse(&serde_json::json!({
            "lsp": {
                "python": [
                    {"name":"Ruff","command":"python3","args":[script,"Ruff"],"only_features":["diagnostics","format"]},
                    {"name":"TY","command":"python3","args":[script,"TY"],"only_features":["diagnostics"]}
                ],
                "toml": {"name":"Tombi","command":"python3","args":[script,"Tombi"]},
                "rust": {"name":"Missing","command":missing,"args":[]}
            }
        }).to_string()).unwrap();
        let python = root.join("sample.py");
        let toml = root.join("sample.toml");
        let rust = root.join("sample.rs");
        std::fs::write(&python, "bad = 1\n").unwrap();
        std::fs::write(&toml, "bad = 1\n").unwrap();
        std::fs::write(&rust, "fn main() {}\n").unwrap();
        let editor = EditorHandle::spawn(root.clone(), cfg).expect("Fresh worker starts");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opened = editor.open(python.clone(), false).await.unwrap();
            let mut diagnostics = Vec::new();
            for _ in 0..100 {
                diagnostics = editor
                    .lsp_get(opened.buffer_id.clone(), opened.rev)
                    .await
                    .unwrap()
                    .diagnostics;
                if diagnostics.len() == 2 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert_eq!(diagnostics.len(), 2, "Ruff and TY both publish diagnostics");
            assert!(
                diagnostics
                    .iter()
                    .all(|diagnostic| diagnostic.severity == "error")
            );
            let rev = editor
                .edit(opened.buffer_id.clone(), opened.rev, "good = 1\n".into())
                .await
                .unwrap();
            for _ in 0..100 {
                if editor
                    .lsp_get(opened.buffer_id.clone(), rev)
                    .await
                    .unwrap()
                    .diagnostics
                    .is_empty()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(
                editor
                    .lsp_get(opened.buffer_id.clone(), rev)
                    .await
                    .unwrap()
                    .diagnostics
                    .is_empty()
            );
            let formatted = editor.format(opened.buffer_id.clone(), rev).await.unwrap();
            assert_eq!(formatted.text.as_deref(), Some("formatted = true\n"));
            let stale = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "view-format".into(),
                    rev,
                    vec![RangeEdit {
                        start: 0,
                        end: 0,
                        text: "stale".into(),
                    }],
                    ByteSelection { anchor: 0, head: 0 },
                )
                .await
                .unwrap();
            assert!(
                !stale.accepted,
                "Fresh/LSP formatting advances the ADE revision"
            );
            assert_eq!(stale.rev, formatted.rev);
            assert_eq!(stale.text, "formatted = true\n");
            editor.close(opened.buffer_id).await.unwrap();

            for source in ["Ruff", "TY"] {
                let log = root.join(format!("fake_lsp.py.{source}.log"));
                let mut closed = false;
                for _ in 0..40 {
                    let events = std::fs::read_to_string(&log).unwrap_or_default();
                    closed = events.contains("textDocument/didClose");
                    if closed {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
                }
                assert!(closed, "{source} received didClose");
            }

            let reopened = editor.open(python, false).await.unwrap();
            let mut restarted = false;
            for _ in 0..100 {
                if editor
                    .lsp_get(reopened.buffer_id.clone(), reopened.rev)
                    .await
                    .unwrap()
                    .diagnostics
                    .len()
                    == 2
                {
                    restarted = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(
                restarted,
                "both Python servers restart after the last buffer closes"
            );
            editor.close(reopened.buffer_id).await.unwrap();

            let opened = editor.open(toml, false).await.unwrap();
            let mut found = false;
            for _ in 0..100 {
                if !editor
                    .lsp_get(opened.buffer_id.clone(), opened.rev)
                    .await
                    .unwrap()
                    .diagnostics
                    .is_empty()
                {
                    found = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(found, "Tombi publishes TOML diagnostics");
            assert!(
                editor
                    .format(opened.buffer_id.clone(), opened.rev)
                    .await
                    .unwrap()
                    .text
                    .is_some()
            );
            editor.close(opened.buffer_id).await.unwrap();

            let opened = editor.open(rust.clone(), false).await.unwrap();
            let state = editor
                .lsp_get(opened.buffer_id.clone(), opened.rev)
                .await
                .unwrap();
            assert!(
                state
                    .status
                    .unwrap_or_default()
                    .contains("not found on daemon host")
            );
            assert!(
                editor
                    .format(opened.buffer_id.clone(), opened.rev)
                    .await
                    .is_err()
            );
            editor.close(opened.buffer_id).await.unwrap();

            let changed_config = crate::config::Config::parse(
                &serde_json::json!({"lsp": {"rust": {
                    "name": "Fresh", "command": "python3", "args": [script, "Fresh"]
                }}})
                .to_string(),
            )
            .unwrap();
            editor.reconfigure(changed_config).await.unwrap();
            let reopened = editor.open(rust, false).await.unwrap();
            let mut configured = false;
            for _ in 0..100 {
                if !editor
                    .lsp_get(reopened.buffer_id.clone(), reopened.rev)
                    .await
                    .unwrap()
                    .diagnostics
                    .is_empty()
                {
                    configured = true;
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            }
            assert!(
                configured,
                "settings reload starts a newly configured server"
            );
            editor.close(reopened.buffer_id).await.unwrap();
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }
}

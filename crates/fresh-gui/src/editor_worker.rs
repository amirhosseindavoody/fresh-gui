//! In-process Fresh `Editor` on a dedicated `!Send` thread.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::Read;
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
use fresh_gui_protocol::{
    BufferDiagnostic, ByteRange, ByteSelection, EditorAction, LspRequest, LspRequestFeature,
    LspResult, LspServerResponse, RangeEdit, SceneBuffer,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;
const MAX_PAGE_BYTES: usize = 64 * 1024;
const MAX_PAGED_RECOVERY_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OpenedBuffer {
    pub buffer_id: String,
    pub draft_id: String,
    pub path: String,
    pub language: Option<String>,
    pub rev: u64,
    pub text: String,
    /// Total byte length when `text` is omitted for a lazily loaded buffer.
    pub total_bytes: Option<usize>,
    pub dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadPage {
    pub rev: u64,
    pub start: usize,
    pub total_bytes: usize,
    pub text: String,
    pub dirty: bool,
}

#[derive(Debug, Clone)]
struct TrackedBuffer {
    /// `None` until the buffer is saved to a path.
    path: Option<PathBuf>,
    text: String,
    total_bytes: Option<usize>,
    rev: u64,
    dirty: bool,
    language: Option<String>,
    workspace_id: String,
    draft_id: String,
    base_text: Option<String>,
    recovery_path: Option<PathBuf>,
    disk: Option<DiskGeneration>,
    external: Option<ExternalChange>,
    overwrite_generation: Option<String>,
    paged_generation: Option<String>,
    paged_journal: Vec<crate::drafts::PagedEditTransaction>,
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

#[derive(Debug, Clone)]
pub struct ExternalChange {
    pub buffer_id: String,
    pub path: String,
    pub rev: u64,
    pub generation: String,
    pub text: String,
    pub disk_text: Option<String>,
    pub dirty: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalResolution {
    Reload,
    Keep,
    Overwrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DiskGeneration {
    pub(crate) signature: String,
    pub(crate) text: Option<String>,
}

pub(crate) fn disk_generation(path: &Path) -> Result<DiskGeneration> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            None::<Vec<u8>>.hash(&mut hasher);
            return Ok(DiskGeneration {
                signature: format!("missing:{:016x}", hasher.finish()),
                text: None,
            });
        }
        Err(error) => return Err(error).with_context(|| format!("stat {}", path.display())),
    };
    if !metadata.is_file() {
        bail!("external path is not a regular file: {}", path.display());
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let text = if metadata.len() as usize <= MAX_SNAPSHOT_BYTES {
        let mut file = std::fs::File::open(path)
            .with_context(|| format!("read {}", path.display()))?
            .take((MAX_SNAPSHOT_BYTES + 1) as u64);
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            bail!("external file exceeds snapshot limit: {}", path.display());
        }
        let text = String::from_utf8(bytes.clone())
            .with_context(|| format!("external file is not UTF-8 text: {}", path.display()))?;
        Some(bytes).hash(&mut hasher);
        Some(text)
    } else {
        None
    };
    let len = metadata.len();
    let modified = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    #[cfg(unix)]
    let identity = {
        use std::os::unix::fs::MetadataExt;
        format!("{}:{}", metadata.dev(), metadata.ino())
    };
    #[cfg(not(unix))]
    let identity = String::new();
    Ok(DiskGeneration {
        signature: format!("{len}:{modified}:{identity}:{:016x}", hasher.finish()),
        text,
    })
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
        viewport: Option<ByteRange>,
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
    ReadPage {
        buffer_id: String,
        start: usize,
        len: usize,
        reply: oneshot::Sender<Result<ReadPage>>,
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
    CheckExternal {
        buffer_id: String,
        reply: oneshot::Sender<Result<Option<ExternalChange>>>,
    },
    ResolveExternal {
        buffer_id: String,
        base_rev: u64,
        generation: String,
        resolution: ExternalResolution,
        reply: oneshot::Sender<Result<BufferTransactionResult>>,
    },
    LspRequest { request: LspRequest },
    LspCancel { request_id: u64, buffer_id: String, view_id: String },
}

#[derive(Debug, Clone)]
pub struct BufferTransactionResult {
    pub rev: u64,
    pub text: String,
    pub selection: ByteSelection,
    pub accepted: bool,
    pub dirty: bool,
    pub page: Option<ReadPage>,
}

/// Handle to the editor thread. Cloneable; commands are serialized on the worker.
#[derive(Clone)]
pub struct EditorHandle {
    tx: mpsc::UnboundedSender<Cmd>,
    external_tx: tokio::sync::broadcast::Sender<ExternalChange>,
    lsp_tx: tokio::sync::broadcast::Sender<LspResult>,
}

impl EditorHandle {
    pub fn subscribe_lsp(&self) -> tokio::sync::broadcast::Receiver<LspResult> {
        self.lsp_tx.subscribe()
    }

    pub fn request_lsp(&self, request: LspRequest) -> Result<()> {
        self.tx
            .send(Cmd::LspRequest { request })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))
    }

    pub fn cancel_lsp(&self, request_id: u64, buffer_id: String, view_id: String) -> Result<()> {
        self.tx
            .send(Cmd::LspCancel { request_id, buffer_id, view_id })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))
    }

    pub fn subscribe_external(&self) -> tokio::sync::broadcast::Receiver<ExternalChange> {
        self.external_tx.subscribe()
    }

    pub async fn check_external(&self, buffer_id: String) -> Result<Option<ExternalChange>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::CheckExternal { buffer_id, reply })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn resolve_external(
        &self,
        buffer_id: String,
        base_rev: u64,
        generation: String,
        resolution: ExternalResolution,
    ) -> Result<BufferTransactionResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::ResolveExternal {
                buffer_id,
                base_rev,
                generation,
                resolution,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

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
        let (external_tx, _) = tokio::sync::broadcast::channel(128);
        let (lsp_tx, _) = tokio::sync::broadcast::channel(256);
        let worker_external = external_tx.clone();
        let worker_lsp = lsp_tx.clone();
        let dir_for_log = working_dir.clone();

        thread::Builder::new()
            .name("fresh-editor".into())
            .spawn(move || match build_editor(&working_dir, &gui_config) {
                Ok(editor) => {
                    let _ = ready_tx.send(Ok(()));
                    run_loop(editor, rx, DraftStore::new(recovery_dir), worker_external, worker_lsp);
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            })
            .ok()?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                info!(dir = %dir_for_log.display(), "Fresh editor worker ready");
                Some(Self { tx, external_tx, lsp_tx })
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
        viewport: Option<ByteRange>,
        selection: ByteSelection,
    ) -> Result<BufferTransactionResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::RangeEdit {
                buffer_id,
                view_id,
                base_rev,
                edits,
                viewport,
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

    pub async fn read_page(&self, buffer_id: String, start: usize, len: usize) -> Result<ReadPage> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::ReadPage {
                buffer_id,
                start,
                len,
                reply,
            })
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
    // ADE snapshots stop at 2 MiB; use Fresh's piece-tree lazy mode above the
    // same boundary so the daemon never builds a full String for those files.
    cfg.editor.large_file_threshold_bytes = MAX_SNAPSHOT_BYTES as u64 + 1;
    // Fresh owns LSP lifecycle; only the servers explicitly configured for
    // fresh-gui are started on this daemon host.
    cfg.lsp = gui_config.lsp.clone();
    cfg.lsp_enabled = !cfg.lsp.is_empty();
    gui_config.apply_fresh(&mut cfg);
    gui_config.apply_fresh_project(&mut cfg, working_dir)?;
    cfg.editor.animations = false;
    // Config layers cannot change the negotiated ADE lazy/snapshot boundary.
    cfg.editor.large_file_threshold_bytes = MAX_SNAPSHOT_BYTES as u64 + 1;
    cfg.lsp = gui_config.lsp.clone();
    cfg.lsp_enabled = !cfg.lsp.is_empty();
    for language in cfg.lsp.keys() {
        if let Some(config) = cfg.languages.get_mut(language) {
            // The GUI's Format action should use the configured LSP server,
            // not Fresh's unrelated built-in external formatter command.
            config.formatter = None;
        }
    }
    let fs: Arc<dyn FileSystem + Send + Sync> = Arc::new(crate::editor_fs::EditorFileSystem::new());
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

fn run_loop(
    mut editor: Editor,
    mut rx: mpsc::UnboundedReceiver<Cmd>,
    drafts: DraftStore,
    external_tx: tokio::sync::broadcast::Sender<ExternalChange>,
    lsp_tx: tokio::sync::broadcast::Sender<LspResult>,
) {
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

    // Give LspManager a daemon-owned inbox. Fresh drains its own editor/window
    // queues during editor_tick, so sharing one would lose ADE replies between
    // our drain and Fresh's drain. Its public runtime setter preserves the
    // existing Authority while routing all server messages through this inbox.
    let rt = fresh::services::runtime::LiveRuntime::new(rt);
    let lsp_inbox = fresh::services::async_bridge::AsyncBridge::new();
    editor.active_window_mut().lsp.set_runtime(rt.clone(), lsp_inbox.clone());
    let mut tracked: HashMap<String, TrackedBuffer> = HashMap::new();
    let mut lsp_bridge = LspBridgeState {
        inbox: lsp_inbox,
        pending: HashMap::new(),
        request_ids: HashMap::new(),
        aggregates: HashMap::new(),
        results: lsp_tx,
    };

    // Borrow editor/tracked into the future (no `async move`) so `Editor` is
    // dropped *after* `block_on` returns — Fresh's Drop must not run while a
    // Tokio runtime is still in an async teardown path.
    rt.block_on(async {
        let mut ticks = tokio::time::interval(std::time::Duration::from_millis(50));
        let mut external_ticks = tokio::time::interval(std::time::Duration::from_millis(350));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let cmd = tokio::select! {
                _ = ticks.tick() => {
                    poll_lsp_bridge(&mut editor, &tracked, &mut lsp_bridge);
                    cancel_stale_lsp_requests(&mut editor, &tracked, &mut lsp_bridge);
                    if let Err(err) = fresh::app::editor_tick(&mut editor, || Ok(())) {
                        warn!(%err, "Fresh editor tick failed");
                    }
                    continue;
                }
                _ = external_ticks.tick() => {
                    if let Err(err) = poll_external_changes(&mut editor, &mut tracked, &external_tx, &drafts) { warn!(%err, "external file reconciliation failed"); }
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
                    viewport,
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
                        viewport,
                        selection,
                    )
                    .and_then(|result| {
                        if result.accepted && result.dirty {
                            checkpoint(&drafts, &tracked, &buffer_id)?;
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
                Cmd::ReadPage { buffer_id, start, len, reply } => {
                    let result = read_page(&mut editor, &mut tracked, &buffer_id, start, len);
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
                        Some(_) => {
                            cancel_buffer_lsp_requests(&mut editor, &buffer_id, &mut lsp_bridge);
                            close_buffer(&mut editor, &mut tracked, &buffer_id)
                        },
                        None => Ok(()),
                    };
                    let _ = reply.send(result);
                }
                Cmd::LspRequest { request } => {
                    begin_lsp_request(&mut editor, &tracked, request, &mut lsp_bridge);
                }
                Cmd::LspCancel { request_id, buffer_id, view_id } => {
                    cancel_lsp_request(&mut editor, request_id, &buffer_id, &view_id, &mut lsp_bridge);
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
                    cancel_all_lsp_requests(&mut editor, &mut lsp_bridge);
                    let result = format_buffer(&mut editor, &mut tracked, &buffer_id, base_rev, &mut lsp_bridge)
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
                    // Editor preferences are applied at construction; existing buffers
                    // retain their settings until server restart.
                    config.apply_fresh_services(&mut fresh_config);
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
                        .and_then(|()| {
                            cancel_buffer_lsp_requests(&mut editor, &buffer_id, &mut lsp_bridge);
                            close_buffer(&mut editor, &mut tracked, &buffer_id)
                        });
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
                Cmd::CheckExternal { buffer_id, reply } => {
                    let result = poll_external_changes(&mut editor, &mut tracked, &external_tx, &drafts)
                        .and_then(|()| {
                            tracked
                                .get(&buffer_id)
                                .context("unknown buffer")
                                .map(|entry| entry.external.as_ref().map(|saved| ExternalChange {
                                    buffer_id: buffer_id.clone(),
                                    path: saved.path.clone(),
                                    rev: entry.rev,
                                    generation: saved.generation.clone(),
                                    text: entry.text.clone(),
                                    disk_text: saved.disk_text.clone(),
                                    dirty: entry.dirty || saved.dirty,
                                }))
                        });
                    let _ = reply.send(result);
                }
                Cmd::ResolveExternal { buffer_id, base_rev, generation, resolution, reply } => {
                    let result = resolve_external(&mut editor, &mut tracked, &buffer_id, base_rev, &generation, resolution, &drafts);
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

#[derive(Clone)]
struct PendingLspRequest {
    request: LspRequest,
    server: String,
    language: String,
    lsp_request_id: u64,
}

struct LspAggregate {
    request: LspRequest,
    remaining: usize,
    responses: Vec<LspServerResponse>,
    completion_triggers: Vec<String>,
    signature_triggers: Vec<String>,
    deadline: tokio::time::Instant,
    status: Option<String>,
}

struct LspBridgeState {
    inbox: fresh::services::async_bridge::AsyncBridge,
    pending: HashMap<u64, PendingLspRequest>,
    request_ids: HashMap<u64, Vec<u64>>,
    aggregates: HashMap<u64, LspAggregate>,
    results: tokio::sync::broadcast::Sender<LspResult>,
}

fn begin_lsp_request(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    request: LspRequest,
    bridge: &mut LspBridgeState,
) {
    if request.view_id.is_empty() {
        send_lsp_status(&bridge.results, &request, "view_id cannot be empty", false);
        return;
    }
    if let Some(superseded) = bridge.request_ids.keys().copied().find(|id| {
        bridge.pending.values().any(|entry| {
            entry.request.request_id == *id
                && entry.request.buffer_id == request.buffer_id
                && entry.request.view_id == request.view_id
                && entry.request.feature == request.feature
        })
    }) {
        if let Some(old) = bridge.aggregates.get(&superseded).map(|aggregate| aggregate.request.clone()) {
            send_lsp_status(&bridge.results, &old, "request superseded by a newer request", true);
        }
        cancel_lsp_request(
            editor,
            superseded,
            &request.buffer_id,
            &request.view_id,
            bridge,
        );
    }
    let LspBridgeState { pending, request_ids, aggregates, results, .. } = bridge;
    let Some(entry) = tracked.get(&request.buffer_id) else {
        send_lsp_status(results, &request, "buffer is closed", true);
        return;
    };
    if request.base_rev != entry.rev {
        send_lsp_status(results, &request, "buffer revision is stale", true);
        return;
    }
    if entry.total_bytes.is_some() {
        send_lsp_status(results, &request, "LSP requests are unavailable for paged buffers", false);
        return;
    }
    let language = entry.language.as_deref().unwrap_or("");
    let Ok(raw_id) = request.buffer_id.parse::<usize>() else {
        send_lsp_status(results, &request, "invalid buffer id", false);
        return;
    };
    let buffer_id = BufferId(raw_id);
    let window = editor.active_window();
    let Some(metadata) = window.buffer_metadata.get(&buffer_id) else {
        send_lsp_status(results, &request, "buffer metadata is unavailable", false);
        return;
    };
    let uri = metadata.file_uri().map(|uri| uri.as_uri().to_string());
    if request.offset > entry.text.len() || !entry.text.is_char_boundary(request.offset) {
        send_lsp_status(results, &request, "offset is outside the buffer or splits a UTF-8 character", false);
        return;
    }
    let Some(buffer_state) = editor.active_window().buffers.get(&buffer_id) else {
        send_lsp_status(results, &request, "Fresh buffer is unavailable", true);
        return;
    };
    let (line, character) = buffer_state.buffer.position_to_lsp_position(request.offset);
    let (line, character) = (line as u32, character as u32);
    let manager = &editor.active_window().lsp;
    let all_completion_triggers = manager.handles_for_feature(language, LspFeature::Completion)
        .into_iter().flat_map(|server| server.capabilities.completion_trigger_characters.clone())
        .collect::<std::collections::BTreeSet<_>>().into_iter().collect::<Vec<_>>();
    let all_signature_triggers = if manager.handles_for_feature(language, LspFeature::SignatureHelp).is_empty() {
        Vec::new()
    } else {
        vec!["(".into(), ",".into()]
    };

    if request.feature == LspRequestFeature::Capabilities {
        let _ = results.send(LspResult {
            request_id: request.request_id,
            buffer_id: request.buffer_id,
            view_id: request.view_id,
            rev: entry.rev,
            offset: request.offset,
            feature: request.feature,
            responses: Vec::new(),
            completion_triggers: all_completion_triggers,
            signature_triggers: all_signature_triggers,
            status: None,
            stale: false,
        });
        return;
    }

    let route_feature = match request.feature {
        LspRequestFeature::Capabilities | LspRequestFeature::Completion | LspRequestFeature::CompletionResolve => LspFeature::Completion,
        LspRequestFeature::Hover => LspFeature::Hover,
        LspRequestFeature::SignatureHelp => LspFeature::SignatureHelp,
    };
    let lsp = &editor.active_window().lsp;
    let mut eligible = lsp
        .handles_for_feature(language, route_feature)
        .into_iter()
        .filter(|server| {
            uri.is_some()
                && (request.feature != LspRequestFeature::CompletionResolve
                || (server.name == request.server.as_deref().unwrap_or("")
                    && server.capabilities.completion_resolve))
        })
        .map(|server| {
            let mut triggers = server.capabilities.completion_trigger_characters.clone();
            triggers.sort();
            triggers.dedup();
            (server.name.clone(), triggers)
        })
        .collect::<Vec<_>>();
    if request.feature == LspRequestFeature::SignatureHelp {
        eligible.truncate(1);
    }
    if eligible.is_empty() {
        let response = if request.feature == LspRequestFeature::Completion {
            buffer_word_completions(&entry.text, request.offset)
        } else {
            Vec::new()
        };
        let _ = results.send(LspResult {
            request_id: request.request_id,
            buffer_id: request.buffer_id,
            view_id: request.view_id,
            rev: entry.rev,
            offset: request.offset,
            feature: request.feature,
            responses: response,
            completion_triggers: all_completion_triggers,
            signature_triggers: all_signature_triggers,
            status: if language.is_empty() { Some("buffer has no language mode; using buffer words".into()) } else if uri.is_none() { Some("buffer has no file URI; using buffer words".into()) } else { Some("no eligible language server".into()) },
            stale: false,
        });
        return;
    }
    let completion_triggers = all_completion_triggers;
    let signature_triggers = all_signature_triggers;
    let request_ids_to_send = (0..eligible.len())
        .map(|_| editor.active_window_mut().alloc_lsp_request_id())
        .collect::<Vec<_>>();
    let mut sent = Vec::new();
    let manager = &mut editor.active_window_mut().lsp;
    for ((server_name, _), lsp_id) in eligible.into_iter().zip(request_ids_to_send) {
        let Some(server) = manager
            .handles_for_feature_mut(language, route_feature)
            .into_iter()
            .find(|server| server.name == server_name)
        else {
            continue;
        };
        let method = crate::lsp_bridge::method(request.feature).to_owned();
        let params = if request.feature == LspRequestFeature::CompletionResolve {
            request
                .item
                .clone()
                .map(crate::lsp_bridge::resolve_item_params)
        } else {
            let is_signature = request.feature == LspRequestFeature::SignatureHelp;
            let Some(uri) = uri.as_deref() else { continue };
            let allowed_triggers = if is_signature {
                vec!["(".to_owned(), ",".to_owned()]
            } else {
                server.capabilities.completion_trigger_characters.clone()
            };
            let trigger = request.trigger_character.as_deref().filter(|trigger| allowed_triggers.iter().any(|allowed| allowed == *trigger));
            Some(crate::lsp_bridge::position_params(
                uri,
                line,
                character,
                trigger,
                is_signature,
            ))
        };
        if server
            .handle
            .send_plugin_request(lsp_id, method, params)
            .is_ok()
        {
            pending.insert(
                lsp_id,
                PendingLspRequest {
                    request: request.clone(),
                    server: server_name,
                    language: language.to_owned(),
                    lsp_request_id: lsp_id,
                },
            );
            sent.push(lsp_id);
        }
    }
    if sent.is_empty() {
        send_lsp_status(results, &request, "language server request could not be queued", false);
    } else {
        request_ids.insert(request.request_id, sent.clone());
        aggregates.insert(request.request_id, LspAggregate {
            request,
            remaining: sent.len(),
            responses: Vec::new(),
            completion_triggers,
            signature_triggers,
            deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            status: None,
        });
    }
}

fn buffer_word_completions(text: &str, offset: usize) -> Vec<LspServerResponse> {
    use fresh::services::completion::{
        buffer_words::BufferWordProvider,
        provider::{CompletionContext, CompletionProvider, ProviderResult},
    };
    let word_start = text[..offset]
        .char_indices()
        .rev()
        .take_while(|(_, ch)| ch.is_alphanumeric() || *ch == '_')
        .last()
        .map_or(offset, |(index, _)| index);
    let prefix = &text[word_start..offset];
    // The GUI uses standard identifier boundaries. Fresh supplies smart-case
    // matching, frequency/proximity ranking, and a bounded scan window.
    let mut scan_range = CompletionContext::compute_scan_range(offset, text.len(), false);
    while !text.is_char_boundary(scan_range.start) { scan_range.start += 1; }
    while !text.is_char_boundary(scan_range.end) { scan_range.end -= 1; }
    let context = CompletionContext {
        prefix: prefix.into(),
        cursor_byte: offset,
        word_start_byte: word_start,
        buffer_len: text.len(),
        is_large_file: false,
        scan_range: scan_range.clone(),
        viewport_top_byte: offset,
        viewport_bottom_byte: offset,
        language_id: None,
        word_chars_extra: String::new(),
        prefix_has_uppercase: prefix.chars().any(char::is_uppercase),
        other_buffers: Vec::new(),
    };
    let provider = BufferWordProvider::new();
    if !provider.is_enabled(&context) { return Vec::new(); }
    let ProviderResult::Ready(candidates) = provider.provide(&context, &text.as_bytes()[scan_range]) else {
        return Vec::new();
    };
    let items = candidates.into_iter().enumerate().map(|(rank, candidate)| {
        serde_json::json!({
            "label": candidate.label,
            "insertText": candidate.insert_text,
            "sortText": format!("{rank:03}"),
        })
    }).collect::<Vec<_>>();
    if items.is_empty() { Vec::new() } else {
        vec![crate::lsp_bridge::one_response("buffer_words".into(), serde_json::json!(items))]
    }
}

fn send_lsp_status(
    sender: &tokio::sync::broadcast::Sender<LspResult>,
    request: &LspRequest,
    status: &str,
    stale: bool,
) {
    let _ = sender.send(LspResult {
        request_id: request.request_id,
        buffer_id: request.buffer_id.clone(),
        view_id: request.view_id.clone(),
        rev: request.base_rev,
        offset: request.offset,
        feature: request.feature,
        responses: Vec::new(),
        completion_triggers: Vec::new(),
        signature_triggers: Vec::new(),
        status: Some(status.into()),
        stale,
    });
}

fn cancel_lsp_request(
    editor: &mut Editor,
    request_id: u64,
    buffer_id: &str,
    view_id: &str,
    bridge: &mut LspBridgeState,
) {
    if !bridge.aggregates.get(&request_id).is_some_and(|aggregate| aggregate.request.buffer_id == buffer_id && aggregate.request.view_id == view_id) {
        return;
    }
    let Some(ids) = bridge.request_ids.remove(&request_id) else { bridge.aggregates.remove(&request_id); return };
    bridge.aggregates.remove(&request_id);
    for id in ids {
        let Some(entry) = bridge.pending.remove(&id) else { continue };
        if entry.request.buffer_id != buffer_id || entry.request.view_id != view_id {
            bridge.pending.insert(id, entry);
            bridge.request_ids.entry(request_id).or_default().push(id);
            continue;
        }
        let feature = match entry.request.feature {
            LspRequestFeature::Capabilities | LspRequestFeature::Completion | LspRequestFeature::CompletionResolve => LspFeature::Completion,
            LspRequestFeature::Hover => LspFeature::Hover,
            LspRequestFeature::SignatureHelp => LspFeature::SignatureHelp,
        };
        if let Some(server) = editor
            .active_window_mut()
            .lsp
            .handles_for_feature_mut(&entry.language, feature)
            .into_iter()
            .find(|server| server.name == entry.server)
        {
            let _ = server.handle.cancel_request(entry.lsp_request_id);
        }
    }
}

fn poll_lsp_bridge(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    bridge_state: &mut LspBridgeState,
) {
    let LspBridgeState { pending, request_ids, aggregates, results, .. } = bridge_state;
    use fresh::services::async_bridge::AsyncMessage;
    let Some(bridge) = editor.async_bridge() else { return };
    let sender = bridge.sender();
    for message in bridge_state.inbox.try_recv_all() {
        match message {
            AsyncMessage::PluginLspResponse { request_id, result, language } => {
                let Some(entry) = pending.remove(&request_id) else {
                    if sender.send(AsyncMessage::PluginLspResponse { request_id, result, language }).is_err() {
                        warn!("failed to return unmatched Fresh LSP response to its dispatcher");
                    }
                    continue;
                };
                let response = result.map(|value| {
                    if entry.request.feature == LspRequestFeature::Completion
                        || entry.request.feature == LspRequestFeature::CompletionResolve
                    {
                        crate::lsp_bridge::normalize_completion_result(value)
                    } else {
                        value
                    }
                });
                let current = tracked.get(&entry.request.buffer_id).map(|state| state.rev);
                let stale = current != Some(entry.request.base_rev);
                if let Some(aggregate) = aggregates.get_mut(&entry.request.request_id) {
                    if stale {
                        aggregate.status = Some("buffer revision changed while request was pending".into());
                        aggregate.responses.clear();
                        aggregate.remaining = 0;
                    } else {
                        match response {
                            Ok(value) => aggregate.responses.push(crate::lsp_bridge::one_response(entry.server, value)),
                            Err(error) => { aggregate.status.get_or_insert(error); }
                        };
                        aggregate.remaining = aggregate.remaining.saturating_sub(1);
                    }
                }
                if let Some(ids) = request_ids.get_mut(&entry.request.request_id) {
                    ids.retain(|id| *id != request_id);
                    if ids.is_empty() {
                        request_ids.remove(&entry.request.request_id);
                    }
                }
                finish_lsp_aggregate(entry.request.request_id, tracked, aggregates, request_ids, results);
            }
            message => {
                if sender.send(message).is_err() {
                    warn!("failed to return Fresh async message to its dispatcher");
                }
            }
        }
    }
    expire_lsp_aggregates(editor, tracked, bridge_state);
}

fn finish_lsp_aggregate(
    request_id: u64,
    tracked: &HashMap<String, TrackedBuffer>,
    aggregates: &mut HashMap<u64, LspAggregate>,
    request_ids: &mut HashMap<u64, Vec<u64>>,
    results: &tokio::sync::broadcast::Sender<LspResult>,
) {
    if !aggregates.get(&request_id).is_some_and(|aggregate| aggregate.remaining == 0) { return; }
    let Some(aggregate) = aggregates.remove(&request_id) else { return };
    request_ids.remove(&request_id);
    let current = tracked.get(&aggregate.request.buffer_id).map(|entry| entry.rev);
    let stale = current != Some(aggregate.request.base_rev);
    let _ = results.send(LspResult {
        request_id: aggregate.request.request_id,
        buffer_id: aggregate.request.buffer_id,
        view_id: aggregate.request.view_id,
        rev: current.unwrap_or(aggregate.request.base_rev),
        offset: aggregate.request.offset,
        feature: aggregate.request.feature,
        responses: if stale { Vec::new() } else { aggregate.responses },
        completion_triggers: aggregate.completion_triggers,
        signature_triggers: aggregate.signature_triggers,
        status: if stale { Some("buffer revision changed while request was pending".into()) } else { aggregate.status },
        stale,
    });
}

fn expire_lsp_aggregates(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    bridge: &mut LspBridgeState,
) {
    let now = tokio::time::Instant::now();
    let expired = bridge.aggregates.iter().filter_map(|(id, aggregate)| (aggregate.deadline <= now).then_some(*id)).collect::<Vec<_>>();
    for id in expired {
        if let Some(aggregate) = bridge.aggregates.get_mut(&id) {
            aggregate.remaining = 0;
            aggregate.status.get_or_insert_with(|| "language server request timed out".into());
        }
        let request = bridge.aggregates.get(&id).map(|aggregate| aggregate.request.clone());
        if let Some(request) = request {
            if let Some(ids) = bridge.request_ids.remove(&id) {
                for lsp_id in ids {
                    if let Some(entry) = bridge.pending.remove(&lsp_id) {
                        let route_feature = match entry.request.feature {
                            LspRequestFeature::Capabilities | LspRequestFeature::Completion | LspRequestFeature::CompletionResolve => LspFeature::Completion,
                            LspRequestFeature::Hover => LspFeature::Hover,
                            LspRequestFeature::SignatureHelp => LspFeature::SignatureHelp,
                        };
                        if let Some(server) = editor.active_window_mut().lsp.handles_for_feature_mut(&entry.language, route_feature).into_iter().find(|server| server.name == entry.server) {
                            let _ = server.handle.cancel_request(entry.lsp_request_id);
                        }
                    }
                }
            }
            let _ = request;
        }
        finish_lsp_aggregate(id, tracked, &mut bridge.aggregates, &mut bridge.request_ids, &bridge.results);
    }
}

fn cancel_buffer_lsp_requests(
    editor: &mut Editor,
    buffer_id: &str,
    bridge: &mut LspBridgeState,
) {
    let ids = bridge.aggregates.iter().filter_map(|(id, aggregate)| (aggregate.request.buffer_id == buffer_id).then_some(*id)).collect::<Vec<_>>();
    for id in ids {
        if let Some(request) = bridge.aggregates.get(&id).map(|aggregate| aggregate.request.clone()) {
            cancel_lsp_request(editor, id, &request.buffer_id, &request.view_id, bridge);
            let _ = bridge.results.send(LspResult {
                request_id: request.request_id,
                buffer_id: request.buffer_id,
                view_id: request.view_id,
                rev: request.base_rev,
                offset: request.offset,
                feature: request.feature,
                responses: Vec::new(),
                completion_triggers: Vec::new(),
                signature_triggers: Vec::new(),
                status: Some("buffer closed while request was pending".into()),
                stale: true,
            });
        }
    }
}

fn cancel_all_lsp_requests(
    editor: &mut Editor,
    bridge: &mut LspBridgeState,
) {
    let requests = bridge.aggregates.values().map(|aggregate| aggregate.request.clone()).collect::<Vec<_>>();
    for request in requests {
        let request_id = request.request_id;
        cancel_lsp_request(editor, request_id, &request.buffer_id, &request.view_id, bridge);
        send_lsp_status(&bridge.results, &request, "LSP request cancelled before formatting", false);
    }
}

fn cancel_stale_lsp_requests(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    bridge: &mut LspBridgeState,
) {
    let ids = bridge.aggregates.iter().filter_map(|(id, aggregate)| {
        (tracked.get(&aggregate.request.buffer_id).map(|entry| entry.rev) != Some(aggregate.request.base_rev)).then_some(*id)
    }).collect::<Vec<_>>();
    for id in ids {
        let request = bridge.aggregates.get(&id).map(|aggregate| aggregate.request.clone());
        if let Some(request) = request {
            cancel_lsp_request(editor, id, &request.buffer_id, &request.view_id, bridge);
            let current = tracked.get(&request.buffer_id).map(|entry| entry.rev).unwrap_or(request.base_rev);
            let _ = bridge.results.send(LspResult {
                request_id: request.request_id,
                buffer_id: request.buffer_id,
                view_id: request.view_id,
                rev: current,
                offset: request.offset,
                feature: request.feature,
                responses: Vec::new(),
                completion_triggers: Vec::new(),
                signature_triggers: Vec::new(),
                status: Some("buffer revision changed while request was pending".into()),
                stale: true,
            });
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
    let buffer_id = if preview {
        editor
            .open_file_preview(path)
            .with_context(|| format!("open_file_preview {}", path.display()))?
    } else {
        editor
            .open_file(path)
            .with_context(|| format!("open_file {}", path.display()))?
    };

    let id = buffer_id.0.to_string();
    let previous = tracked
        .get(&id)
        .filter(|entry| {
            entry.workspace_id == workspace_id && entry.path.as_deref().is_some_and(same_path)
        })
        .cloned();
    let language = Some(editor.active_state().language.clone());
    let total_bytes = editor.active_state().buffer.total_bytes();
    // Once a buffer enters Fresh's lazy piece-tree mode it stays paged for its
    // lifetime, even if edits later shrink it below the initial threshold.
    let paged = total_bytes > MAX_SNAPSHOT_BYTES
        || previous
            .as_ref()
            .is_some_and(|entry| entry.total_bytes.is_some());
    let text = if paged {
        String::new()
    } else {
        editor
            .active_state()
            .buffer
            .to_string()
            .context("buffer has unloaded regions")?
    };
    let dirty = editor.active_state().buffer.is_modified();

    if text.len() > MAX_SNAPSHOT_BYTES {
        bail!(
            "snapshot too large ({} bytes; max {MAX_SNAPSHOT_BYTES})",
            text.len()
        );
    }

    let rev = previous.as_ref().map(|t| t.rev).unwrap_or(0);
    let base_text = previous
        .as_ref()
        .and_then(|entry| entry.base_text.clone())
        .or_else(|| (!paged).then(|| text.clone()));
    let disk = match previous.as_ref().and_then(|entry| entry.disk.clone()) {
        Some(disk) => disk,
        None => disk_generation(path)?,
    };
    let external = previous.as_ref().and_then(|entry| entry.external.clone());
    let overwrite_generation = previous
        .as_ref()
        .and_then(|entry| entry.overwrite_generation.clone());
    let paged_generation = previous
        .as_ref()
        .and_then(|entry| entry.paged_generation.clone())
        .or_else(|| paged.then(|| disk.signature.clone()));
    let draft_id = previous
        .as_ref()
        .map(|entry| entry.draft_id.clone())
        .unwrap_or_else(|| crate::drafts::named_draft_id(workspace_id, path));
    tracked.insert(
        id.clone(),
        TrackedBuffer {
            path: Some(path.to_path_buf()),
            text: text.clone(),
            total_bytes: paged.then_some(total_bytes),
            rev,
            dirty,
            language: language.clone(),
            workspace_id: workspace_id.to_owned(),
            draft_id,
            base_text,
            recovery_path: Some(path.to_path_buf()),
            disk: Some(disk.clone()),
            external,
            overwrite_generation,
            paged_generation,
            paged_journal: previous
                .map(|entry| entry.paged_journal)
                .unwrap_or_default(),
        },
    );

    Ok(OpenedBuffer {
        draft_id: tracked[&id].draft_id.clone(),
        buffer_id: id,
        path: path.display().to_string(),
        language,
        rev,
        text,
        total_bytes: paged.then_some(total_bytes),
        dirty,
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
            paged: entry
                .paged_generation
                .clone()
                .filter(|_| !entry.paged_journal.is_empty())
                .map(|generation| crate::drafts::PagedDraft {
                    generation,
                    edits: entry.paged_journal.clone(),
                }),
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
    if let Some(paged) = draft.paged.as_ref() {
        if source_changed {
            bail!(
                "paged draft source changed; recovery copy is preserved and cannot be replayed safely"
            );
        }
        if let Some((buffer_id, entry)) = tracked.iter().find(|(_, entry)| {
            entry.draft_id == draft.draft_id
                && entry.total_bytes.is_some()
                && entry.paged_generation.as_deref() == Some(paged.generation.as_str())
        }) {
            let opened = OpenedBuffer {
                buffer_id: buffer_id.clone(),
                draft_id: entry.draft_id.clone(),
                path: entry
                    .recovery_path
                    .as_deref()
                    .or(entry.path.as_deref())
                    .map(|path| path.display().to_string())
                    .unwrap_or_default(),
                language: entry.language.clone(),
                rev: entry.rev,
                text: String::new(),
                total_bytes: entry.total_bytes,
                dirty: entry.dirty,
            };
            activate_tracked(editor, tracked, buffer_id)?;
            return Ok((opened, false));
        }
    }
    let mut opened =
        if let Some(path) = draft.path.as_ref().filter(|path| Path::new(path).is_file()) {
            match open_buffer(editor, tracked, Path::new(path), false, workspace_id) {
                Ok(opened) => opened,
                Err(_) => create_untitled(editor, tracked, workspace_id)?,
            }
        } else {
            create_untitled(editor, tracked, workspace_id)?
        };
    if let Some(paged) = draft.paged.as_ref() {
        let source = draft
            .path
            .as_deref()
            .context("paged draft has no source path")?;
        let generation = disk_generation(Path::new(source))?;
        if generation.signature != paged.generation {
            bail!(
                "paged draft source changed; recovery copy is preserved and cannot be replayed safely"
            );
        }
        if opened.total_bytes.is_none() {
            bail!("paged recovery source did not reopen lazily");
        }
        let mut rev = opened.rev;
        for transaction in &paged.edits {
            let batch = &transaction.edits;
            let page = read_page(
                editor,
                tracked,
                &opened.buffer_id,
                transaction.viewport.start,
                transaction.viewport.len,
            )?;
            let result = paged_range_edit(
                editor,
                tracked,
                &opened.buffer_id,
                "draft-recovery",
                rev,
                batch.clone(),
                Some(transaction.viewport),
                ByteSelection {
                    anchor: page.start,
                    head: page.start,
                },
            )?;
            rev = result.rev;
        }
        opened.rev = rev;
        opened.total_bytes = tracked
            .get(&opened.buffer_id)
            .and_then(|entry| entry.total_bytes);
        opened.dirty = true;
        if let Some(entry) = tracked.get_mut(&opened.buffer_id) {
            entry.draft_id = draft.draft_id.clone();
            entry.workspace_id = workspace_id.to_owned();
            entry.recovery_path = draft.path.as_deref().map(PathBuf::from);
            entry.dirty = true;
        }
        return Ok((opened, source_changed));
    }
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
    let recovery_path = draft.path.as_ref().map(PathBuf::from);
    let entry = tracked.get_mut(&buffer_id).expect("restored buffer");
    entry.draft_id = draft.draft_id;
    entry.workspace_id = workspace_id.to_owned();
    entry.base_text = draft.base_text;
    entry.recovery_path = recovery_path.clone();
    entry.dirty = true;
    entry.text = draft.text.clone();
    if source_changed {
        if let Some(path) = recovery_path.as_deref() {
            let disk = disk_generation(path)?;
            entry.external = Some(ExternalChange {
                buffer_id: buffer_id.clone(),
                path: path.display().to_string(),
                rev: entry.rev,
                generation: disk.signature.clone(),
                text: entry.text.clone(),
                disk_text: disk.text.clone(),
                dirty: true,
            });
            entry.disk = Some(disk);
        }
    }
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
    if tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some())
    {
        bail!("full-text replacement is unavailable for paged buffers; use viewport range edits");
    }
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
    let text = if entry.total_bytes.is_some() {
        String::new()
    } else {
        editor
            .active_window()
            .buffers
            .get(&BufferId(id))
            .and_then(|state| state.buffer.to_string())
            .context("Fresh buffer unavailable")?
    };
    Ok(BufferTransactionResult {
        rev: entry.rev,
        text,
        selection: current_selection(editor),
        accepted,
        dirty: entry.dirty,
        page: None,
    })
}

fn read_page(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    start: usize,
    len: usize,
) -> Result<ReadPage> {
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    if let Some(entry) = tracked.get(buffer_id) {
        let path = entry
            .path
            .as_deref()
            .context("paged buffer has no source path")?;
        if entry.total_bytes.is_some()
            && entry.paged_generation.as_deref() != Some(disk_generation(path)?.signature.as_str())
        {
            bail!(
                "file changed on disk; paged reads are blocked until external change is resolved"
            );
        }
    }
    activate_tracked(editor, tracked, buffer_id)?;
    let total = editor.active_state().buffer.total_bytes();
    if start > total {
        bail!("page start exceeds buffer length");
    }
    if len == 0 {
        if start > 0 && start < total {
            let mut probe_start = start.saturating_sub(4);
            let probe_end = start.saturating_add(4).min(total);
            let mut bytes = editor
                .active_state_mut()
                .buffer
                .get_text_range_mut(probe_start, probe_end - probe_start)
                .context("validate empty page boundary")?;
            while probe_start > 0 && bytes.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
                probe_start -= 1;
                bytes = editor
                    .active_state_mut()
                    .buffer
                    .get_text_range_mut(probe_start, probe_end - probe_start)
                    .context("align empty page boundary")?;
            }
            let valid_len = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(_) => bail!("Fresh page contains invalid UTF-8"),
            };
            let probe =
                std::str::from_utf8(&bytes[..valid_len]).context("invalid UTF-8 boundary probe")?;
            if start < probe_start
                || start - probe_start > probe.len()
                || !probe.is_char_boundary(start - probe_start)
            {
                bail!("empty viewport start is not a UTF-8 boundary");
            }
        }
        let entry = tracked.get(buffer_id).context("unknown buffer_id")?;
        return Ok(ReadPage {
            rev: entry.rev,
            start,
            total_bytes: total,
            text: String::new(),
            dirty: entry.dirty,
        });
    }
    if len > MAX_PAGE_BYTES {
        bail!("page length must be 1..={MAX_PAGE_BYTES} bytes");
    }
    let requested_end = start.saturating_add(len).min(total);
    let mut load_start = start.saturating_sub(4);
    let load_end = requested_end.saturating_add(4).min(total);
    let mut bytes = editor
        .active_state_mut()
        .buffer
        .get_text_range_mut(load_start, load_end - load_start)
        .context("load Fresh text page")?;
    // A byte range can begin within a UTF-8 code point. Back up to its lead
    // byte; at most three bytes are needed for valid UTF-8.
    while load_start > 0 && bytes.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
        load_start -= 1;
        bytes = editor
            .active_state_mut()
            .buffer
            .get_text_range_mut(load_start, load_end - load_start)
            .context("align Fresh page start")?;
    }
    let valid_len = match std::str::from_utf8(&bytes) {
        Ok(_) => bytes.len(),
        Err(error) if error.error_len().is_none() => error.valid_up_to(),
        Err(_) => bail!("Fresh page contains invalid UTF-8"),
    };
    let loaded = std::str::from_utf8(&bytes[..valid_len]).context("Fresh page is not UTF-8")?;
    let mut local_start = start - load_start;
    while local_start > 0 && !loaded.is_char_boundary(local_start) {
        local_start -= 1;
    }
    let mut local_end = requested_end - load_start;
    while local_end < loaded.len() && !loaded.is_char_boundary(local_end) {
        local_end += 1;
    }
    // Keep page transfer bounded even when a multibyte character straddles
    // the final byte. UTF-8 code points need at most three extra bytes.
    if local_end - local_start > MAX_PAGE_BYTES {
        local_end = local_start + MAX_PAGE_BYTES;
        while !loaded.is_char_boundary(local_end) {
            local_end -= 1;
        }
    }
    let entry = tracked.get(buffer_id).context("unknown buffer")?;
    Ok(ReadPage {
        rev: entry.rev,
        start: load_start + local_start,
        total_bytes: total,
        text: loaded[local_start..local_end].to_owned(),
        dirty: entry.dirty,
    })
}

fn set_selection(editor: &mut Editor, selection: ByteSelection) {
    let cursor = editor.active_cursors_mut().primary_mut();
    cursor.position = selection.head;
    cursor.anchor = (selection.anchor != selection.head).then_some(selection.anchor);
}

#[allow(clippy::too_many_arguments)]
fn range_edit_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    view_id: &str,
    base_rev: u64,
    edits: Vec<RangeEdit>,
    viewport: Option<ByteRange>,
    selection: ByteSelection,
) -> Result<BufferTransactionResult> {
    if view_id.is_empty() {
        bail!("view_id cannot be empty");
    }
    if tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some())
    {
        return paged_range_edit(
            editor, tracked, buffer_id, view_id, base_rev, edits, viewport, selection,
        );
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

#[allow(clippy::too_many_arguments)]
fn paged_range_edit(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    view_id: &str,
    base_rev: u64,
    edits: Vec<RangeEdit>,
    viewport: Option<ByteRange>,
    selection: ByteSelection,
) -> Result<BufferTransactionResult> {
    if view_id.is_empty() {
        bail!("view_id cannot be empty");
    }
    let viewport = viewport.context("paged edits require a viewport")?;
    let current = tracked.get(buffer_id).context("unknown buffer_id")?;
    if viewport.start > current.total_bytes.unwrap_or(0) || viewport.len > MAX_PAGE_BYTES {
        bail!("invalid paged edit viewport length");
    }
    if current.rev != base_rev {
        let mut result = transaction_result(tracked, editor, buffer_id, false)?;
        result.page = Some(read_page(
            editor,
            tracked,
            buffer_id,
            viewport.start,
            viewport.len,
        )?);
        return Ok(result);
    }
    if edits.is_empty() {
        let mut result = transaction_result(tracked, editor, buffer_id, true)?;
        result.page = Some(read_page(
            editor,
            tracked,
            buffer_id,
            viewport.start,
            viewport.len,
        )?);
        return Ok(result);
    }
    let path = current
        .path
        .as_deref()
        .context("paged buffer has no source path")?;
    let disk = disk_generation(path)?;
    if current.paged_generation.as_deref() != Some(disk.signature.as_str()) {
        bail!("file changed on disk; paged edits are blocked until external change is resolved");
    }
    if edits.len() > 128 {
        bail!("too many edits in one transaction");
    }
    let inserted: usize = edits.iter().map(|edit| edit.text.len()).sum();
    let deleted: usize = edits
        .iter()
        .map(|edit| edit.end.saturating_sub(edit.start))
        .sum();
    if inserted > MAX_PAGE_BYTES || deleted > MAX_PAGE_BYTES {
        bail!("paged edit payload exceeds 64 KiB");
    }
    let prior_recovery_bytes: usize = current
        .paged_journal
        .iter()
        .flat_map(|transaction| transaction.edits.iter())
        .map(|edit| 32usize.saturating_add(edit.text.len()))
        .sum();
    let incoming_recovery_bytes: usize = edits
        .iter()
        .map(|edit| 32usize.saturating_add(edit.text.len()))
        .sum();
    if prior_recovery_bytes.saturating_add(incoming_recovery_bytes) > MAX_PAGED_RECOVERY_BYTES {
        bail!(
            "paged recovery journal reached its 4 MiB limit; save the buffer before editing further"
        );
    }
    activate_tracked(editor, tracked, buffer_id)?;
    let initial = read_page(editor, tracked, buffer_id, viewport.start, viewport.len)?;
    if initial.start > viewport.start {
        bail!("Fresh page starts after requested viewport");
    }
    let mut working = initial.text.clone();
    let mut delta = 0isize;
    let viewport_end = viewport
        .start
        .checked_add(viewport.len)
        .context("viewport end overflow")?;
    for edit in &edits {
        let start = edit.start;
        let end = edit.end;
        let adjusted_viewport_end = if delta >= 0 {
            viewport_end.checked_add(delta as usize)
        } else {
            viewport_end.checked_sub(delta.unsigned_abs())
        }
        .context("adjusted viewport end overflow")?;
        if edit.start > edit.end
            || start < initial.start
            || end > initial.start + working.len()
            || start < viewport.start
            || end > adjusted_viewport_end
        {
            bail!("paged edit range must be inside the supplied viewport");
        }
        let local_start = start - initial.start;
        let local_end = end - initial.start;
        if !working.is_char_boundary(local_start) || !working.is_char_boundary(local_end) {
            bail!("paged edit range is not on a UTF-8 boundary");
        }
        working.replace_range(local_start..local_end, &edit.text);
        delta += edit.text.len() as isize - (end - start) as isize;
        if working.len() > MAX_PAGE_BYTES {
            bail!("edited viewport exceeds 64 KiB");
        }
    }
    let updated_view_end = initial.start + working.len();
    if selection.anchor < initial.start
        || selection.head < initial.start
        || selection.anchor > updated_view_end
        || selection.head > updated_view_end
        || !working.is_char_boundary(selection.anchor - initial.start)
        || !working.is_char_boundary(selection.head - initial.start)
    {
        bail!("paged selection must be inside the loaded viewport and on UTF-8 boundaries");
    }

    let mut prefix = initial
        .text
        .bytes()
        .zip(working.bytes())
        .take_while(|(a, b)| a == b)
        .count();
    while !initial.text.is_char_boundary(prefix) || !working.is_char_boundary(prefix) {
        prefix -= 1;
    }
    let suffix_max = initial
        .text
        .len()
        .saturating_sub(prefix)
        .min(working.len().saturating_sub(prefix));
    let mut suffix = initial.text.as_bytes()[initial.text.len() - suffix_max..]
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
        && (!initial.text.is_char_boundary(initial.text.len() - suffix)
            || !working.is_char_boundary(working.len() - suffix))
    {
        suffix -= 1;
    }
    let old_end = initial.text.len() - suffix;
    let new_end = working.len() - suffix;
    let cursor_id = editor.active_cursors().primary_id();
    let before = editor.active_cursors().primary();
    let mut events = Vec::new();
    if old_end > prefix {
        events.push(Event::Delete {
            range: initial.start + prefix..initial.start + old_end,
            deleted_text: initial.text[prefix..old_end].to_owned(),
            cursor_id,
        });
    }
    if new_end > prefix {
        events.push(Event::Insert {
            position: initial.start + prefix,
            text: working[prefix..new_end].to_owned(),
            cursor_id,
        });
    }
    if initial.text != working
        || before.position != selection.head
        || before.anchor != (selection.anchor != selection.head).then_some(selection.anchor)
    {
        events.push(Event::MoveCursor {
            cursor_id,
            old_position: before.position,
            new_position: selection.head,
            old_anchor: before.anchor,
            new_anchor: (selection.anchor != selection.head).then_some(selection.anchor),
            old_sticky_column: before.sticky_column,
            new_sticky_column: None,
        });
        editor.log_and_apply_event(&Event::Batch {
            events,
            description: "ADE paged range edit".into(),
        });
    }
    let changed = initial.text != working;
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    if changed {
        entry.rev += 1;
        entry.dirty = true;
        entry
            .paged_journal
            .push(crate::drafts::PagedEditTransaction { viewport, edits });
        entry.text.clear();
        entry.total_bytes = Some(editor.active_state().buffer.total_bytes());
    }
    let mut result = transaction_result(tracked, editor, buffer_id, true)?;
    let entry = tracked.get(buffer_id).context("unknown buffer_id")?;
    result.page = Some(ReadPage {
        rev: entry.rev,
        start: initial.start,
        total_bytes: editor.active_state().buffer.total_bytes(),
        text: working,
        dirty: entry.dirty,
    });
    Ok(result)
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
    if tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some())
    {
        bail!("undo and redo are unavailable for paged buffers in this version");
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
    if tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some())
    {
        bail!("paged buffers require a range read; full snapshots are unavailable");
    }
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
    let total_bytes = state.buffer.total_bytes();
    let was_paged = tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some());
    if total_bytes > MAX_SNAPSHOT_BYTES || was_paged {
        let entry = tracked
            .get_mut(buffer_id)
            .with_context(|| format!("unknown buffer_id {buffer_id}"))?;
        entry.total_bytes = Some(total_bytes);
        entry.dirty = state.buffer.is_modified();
        // Materializing to_string() here would defeat Fresh's lazy piece tree.
        // Large-buffer mutations are revisioned through the paged transaction.
        return Ok(None);
    }
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
        if tracked
            .get(&id)
            .is_some_and(|entry| entry.total_bytes.is_some())
        {
            continue;
        }
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
        if tracked
            .get(buffer_id)
            .is_some_and(|entry| entry.total_bytes.is_some())
        {
            None
        } else {
            let id: usize = buffer_id.parse().context("invalid buffer_id")?;
            editor
                .active_window()
                .buffers
                .get(&BufferId(id))
                .and_then(|state| state.buffer.to_string())
        }
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
    bridge: &mut LspBridgeState,
) -> Result<FormatState> {
    if tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some())
    {
        bail!("formatting is unavailable for paged buffers in this version");
    }
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
        poll_lsp_bridge(editor, tracked, bridge);
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
            total_bytes: None,
            rev: 0,
            dirty: false,
            language: language.clone(),
            workspace_id: workspace_id.to_owned(),
            draft_id: format!("untitled:{}", uuid::Uuid::new_v4()),
            base_text: None,
            recovery_path: None,
            disk: None,
            external: None,
            overwrite_generation: None,
            paged_generation: None,
            paged_journal: Vec::new(),
        },
    );
    Ok(OpenedBuffer {
        draft_id: tracked[&id].draft_id.clone(),
        buffer_id: id,
        path: String::new(),
        language,
        rev: 0,
        text,
        total_bytes: None,
        dirty: false,
    })
}

fn external_notice(
    buffer_id: &str,
    entry: &TrackedBuffer,
    generation: &DiskGeneration,
) -> ExternalChange {
    ExternalChange {
        buffer_id: buffer_id.to_owned(),
        path: entry
            .path
            .as_deref()
            .or(entry.recovery_path.as_deref())
            .unwrap_or(Path::new(""))
            .display()
            .to_string(),
        rev: entry.rev,
        generation: generation.signature.clone(),
        text: entry.text.clone(),
        disk_text: generation.text.clone(),
        dirty: entry.dirty,
    }
}

fn poll_external_changes(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    tx: &tokio::sync::broadcast::Sender<ExternalChange>,
    drafts: &DraftStore,
) -> Result<()> {
    let ids: Vec<String> = tracked
        .iter()
        .filter_map(|(id, e)| {
            e.path
                .as_ref()
                .or(e.recovery_path.as_ref())
                .map(|_| id.clone())
        })
        .collect();
    for id in ids {
        let Some(path) = tracked
            .get(&id)
            .and_then(|e| e.path.clone().or_else(|| e.recovery_path.clone()))
        else {
            continue;
        };
        let generation = match disk_generation(&path) {
            Ok(generation) => generation,
            Err(error) => {
                // One unavailable/oversized path must not stop reconciliation
                // for all other open files. Saving it still returns this error.
                warn!(path = %path.display(), %error, "cannot inspect external file");
                continue;
            }
        };
        let changed = tracked
            .get(&id)
            .and_then(|e| e.disk.as_ref())
            .is_some_and(|old| old.signature != generation.signature);
        if !changed {
            continue;
        }
        let same_pending = tracked
            .get(&id)
            .and_then(|e| e.external.as_ref())
            .is_some_and(|p| p.generation == generation.signature);
        if same_pending {
            continue;
        }
        if tracked
            .get(&id)
            .is_some_and(|entry| entry.total_bytes.is_some())
        {
            let notice = external_notice(&id, tracked.get(&id).expect("tracked"), &generation);
            let dirty = notice.dirty;
            tracked.get_mut(&id).expect("tracked").external = Some(notice.clone());
            // Fresh's lazy pieces still reference the original path. Once it
            // changes, neither reloading nor saving can safely reconstruct
            // the old backing. Preserve the journal and require manual review.
            if dirty {
                checkpoint(drafts, tracked, &id)?;
            }
            let _ = tx.send(notice);
            continue;
        }
        let _ = sync_fresh_text(editor, tracked, &id);
        let dirty = tracked.get(&id).is_some_and(|e| e.dirty);
        if generation.text.is_none() && !dirty {
            activate_tracked(editor, tracked, &id)?;
            editor.active_state_mut().buffer.set_modified(true);
            tracked.get_mut(&id).expect("tracked").dirty = true;
        }
        if dirty || generation.text.is_none() {
            let notice = external_notice(&id, tracked.get(&id).expect("tracked"), &generation);
            tracked.get_mut(&id).expect("tracked").external = Some(notice.clone());
            checkpoint(drafts, tracked, &id)?;
            let _ = tx.send(notice);
            continue;
        }
        let path_s = path.display().to_string();
        editor.handle_file_changed(&path_s);
        activate_tracked(editor, tracked, &id)?;
        let mut current = editor.active_state().buffer.to_string().unwrap_or_default();
        if Some(current.as_str()) != generation.text.as_deref() {
            // Fresh's watcher uses mtime ordering. For same-timestamp/content-hash
            // changes use the same event log path while keeping the buffer clean.
            let target = generation.text.as_deref().unwrap_or_default();
            let cursor_id = editor.active_cursors().primary_id();
            let mut events = Vec::new();
            if !current.is_empty() {
                events.push(Event::Delete {
                    range: 0..current.len(),
                    deleted_text: current.clone(),
                    cursor_id,
                });
            }
            if !target.is_empty() {
                events.push(Event::Insert {
                    position: 0,
                    text: target.to_owned(),
                    cursor_id,
                });
            }
            editor.log_and_apply_event(&Event::Batch {
                events,
                description: "External file reload".into(),
            });
            editor.active_event_log_mut().mark_saved();
            editor.active_state_mut().buffer.set_modified(false);
            current = target.to_owned();
        }
        let entry = tracked.get_mut(&id).expect("tracked");
        entry.text = current;
        entry.rev += 1;
        entry.dirty = false;
        entry.base_text = Some(entry.text.clone());
        entry.disk = Some(generation.clone());
        let notice = external_notice(&id, entry, &generation);
        entry.external = Some(notice.clone());
        entry.overwrite_generation = None;
        let _ = tx.send(notice);
    }
    Ok(())
}

fn resolve_external(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    base_rev: u64,
    generation: &str,
    resolution: ExternalResolution,
    drafts: &DraftStore,
) -> Result<BufferTransactionResult> {
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    let current = tracked.get(buffer_id).context("unknown buffer")?;
    if current.rev != base_rev {
        bail!(
            "revision conflict: base_rev={base_rev} current={}",
            current.rev
        );
    }
    let pending = if let Some(pending) = current.external.clone() {
        pending
    } else {
        let path = current
            .path
            .as_deref()
            .or(current.recovery_path.as_deref())
            .context("buffer has no external file")?;
        let observed = disk_generation(path)?;
        if observed.signature != generation {
            bail!("external generation changed; check again before resolving");
        }
        external_notice(buffer_id, current, &observed)
    };
    if pending.generation != generation {
        bail!("external generation changed; check again before resolving");
    }
    if tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some())
    {
        bail!(
            "Fresh lazy backing changed on disk; automatic reload, keep, and overwrite are unsafe. The paged recovery journal is preserved"
        );
    }
    let path = PathBuf::from(&pending.path);
    let now = disk_generation(&path)?;
    if now.signature != generation {
        bail!("file changed again; check the latest external generation");
    }
    match resolution {
        ExternalResolution::Keep => {
            let entry = tracked.get_mut(buffer_id).unwrap();
            entry.overwrite_generation = None;
            let mut pending = pending.clone();
            pending.dirty = true;
            entry.external = Some(pending);
        }
        ExternalResolution::Overwrite => {
            let entry = tracked.get_mut(buffer_id).unwrap();
            entry.overwrite_generation = Some(generation.to_owned());
            let mut pending = pending.clone();
            pending.dirty = true;
            entry.external = Some(pending);
        }
        ExternalResolution::Reload => {
            let target = pending
                .disk_text
                .as_deref()
                .context("file was deleted; cannot reload")?;
            activate_tracked(editor, tracked, buffer_id)?;
            let old = editor.active_state().buffer.to_string().unwrap_or_default();
            let cursor_id = editor.active_cursors().primary_id();
            let mut events = Vec::new();
            if !old.is_empty() {
                events.push(Event::Delete {
                    range: 0..old.len(),
                    deleted_text: old.clone(),
                    cursor_id,
                });
            }
            if !target.is_empty() {
                events.push(Event::Insert {
                    position: 0,
                    text: target.to_owned(),
                    cursor_id,
                });
            }
            editor.log_and_apply_event(&Event::Batch {
                events,
                description: "Reload external file".into(),
            });
            editor.active_event_log_mut().mark_saved();
            editor.active_state_mut().buffer.set_modified(false);
            editor.active_state_mut().buffer.set_file_path(path.clone());
            let entry = tracked.get_mut(buffer_id).unwrap();
            drafts.discard(&entry.workspace_id, &entry.draft_id)?;
            entry.text = target.to_owned();
            entry.rev += 1;
            entry.dirty = false;
            entry.base_text = Some(target.to_owned());
            entry.path = Some(path.clone());
            entry.recovery_path = Some(path.clone());
            entry.disk = Some(now.clone());
            entry.external = None;
            entry.overwrite_generation = None;
        }
    }
    let entry = tracked.get(buffer_id).unwrap();
    Ok(BufferTransactionResult {
        rev: entry.rev,
        text: entry.text.clone(),
        selection: ByteSelection { anchor: 0, head: 0 },
        accepted: true,
        dirty: entry.dirty,
        page: None,
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
    if let Some(entry) = tracked
        .get(buffer_id)
        .filter(|entry| entry.total_bytes.is_some())
    {
        let source = entry
            .path
            .as_deref()
            .or(entry.recovery_path.as_deref())
            .context("paged buffer has no original backing path")?;
        let generation = disk_generation(source)?;
        if entry.paged_generation.as_deref() != Some(generation.signature.as_str()) {
            bail!(
                "Fresh lazy backing changed on disk; paged save is unsafe and the recovery journal is preserved"
            );
        }
    }
    if let Some(dest) = save_path.as_deref() {
        if tracked.get(buffer_id).is_some_and(|entry| entry.total_bytes.is_some())
            && !StdFileSystem.is_owner(dest) {
            // Fresh's ownership-preserving path bypasses write_patched and
            // materializes Copy operations. Keep paged saves bounded.
            bail!("paged saves to files owned by another user are unavailable; Save As to a new file");
        }
        let current_disk = disk_generation(dest)?;
        let entry = tracked.get(buffer_id).expect("tracked");
        let same_destination = entry.path.as_deref().is_some_and(|source| {
            source
                .canonicalize()
                .unwrap_or_else(|_| source.to_path_buf())
                == dest.canonicalize().unwrap_or_else(|_| dest.to_path_buf())
        });
        let conflicts = if same_destination {
            entry
                .disk
                .as_ref()
                .is_some_and(|known| known.signature != current_disk.signature)
                || entry.external.as_ref().is_some_and(|external| {
                    external.dirty && external.path == dest.display().to_string()
                })
        } else {
            !current_disk.signature.starts_with("missing:")
        };
        let explicitly_authorized = entry.overwrite_generation.as_deref()
            == Some(current_disk.signature.as_str())
            && entry
                .external
                .as_ref()
                .is_some_and(|external| external.path == dest.display().to_string());
        if conflicts && !explicitly_authorized {
            let change = ExternalChange {
                buffer_id: buffer_id.to_owned(),
                path: dest.display().to_string(),
                rev: entry.rev,
                generation: current_disk.signature.clone(),
                text: entry.text.clone(),
                disk_text: current_disk.text.clone(),
                dirty: entry.dirty,
            };
            tracked.get_mut(buffer_id).expect("tracked").external = Some(change);
            bail!("file changed on disk; resolve the external change before saving");
        }
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
    let total_bytes = editor.active_state().buffer.total_bytes();
    let was_paged = tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some());
    let paged = total_bytes > MAX_SNAPSHOT_BYTES || was_paged;
    let text = if paged {
        String::new()
    } else {
        editor.active_state().buffer.to_string().unwrap_or_default()
    };
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    entry.text = text;
    entry.total_bytes = paged.then_some(total_bytes);
    entry.base_text = (entry.total_bytes.is_none()).then(|| entry.text.clone());
    entry.disk = Some(disk_generation(
        entry.path.as_deref().context("saved buffer path")?,
    )?);
    entry.paged_generation = entry
        .total_bytes
        .map(|_| entry.disk.as_ref().expect("just set").signature.clone());
    entry.paged_journal.clear();
    entry.external = None;
    entry.overwrite_generation = None;
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

#[cfg(test)]
mod external_generation_tests {
    use super::*;

    #[test]
    fn disk_generation_detects_atomic_replace_delete_and_recreate() {
        let root = std::env::temp_dir().join(format!(
            "fresh-external-generation-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("buffer.txt");
        let tmp = root.join("buffer.tmp");
        std::fs::write(&path, "old text").unwrap();
        let initial = disk_generation(&path).unwrap();
        let original_mtime = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&tmp, "new text").unwrap();
        #[cfg(windows)]
        {
            let old = root.join("buffer.old");
            std::fs::rename(&path, &old).unwrap();
            std::fs::rename(&tmp, &path).unwrap();
            std::fs::remove_file(old).unwrap();
        }
        #[cfg(not(windows))]
        std::fs::rename(&tmp, &path).unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(original_mtime))
            .unwrap();
        let replaced = disk_generation(&path).unwrap();
        assert_ne!(
            initial.signature, replaced.signature,
            "same-size atomic replacement is visible by content hash"
        );
        assert_eq!(replaced.text.as_deref(), Some("new text"));

        std::fs::remove_file(&path).unwrap();
        let missing = disk_generation(&path).unwrap();
        assert!(missing.text.is_none());
        assert_ne!(replaced.signature, missing.signature);
        std::fs::write(&path, "new text").unwrap();
        let recreated = disk_generation(&path).unwrap();
        assert!(recreated.text.is_some());
        assert_ne!(missing.signature, recreated.signature);
        assert_eq!(
            recreated.signature,
            disk_generation(&path).unwrap().signature,
            "duplicate notifications coalesce to one generation"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod paged_file_tests {
    use super::*;

    #[test]
    fn opens_reads_edits_and_stream_saves_a_file_over_snapshot_limit() {
        let root =
            std::env::temp_dir().join(format!("fresh-paged-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("large.txt");
        let recovery = root.join("recovery");
        let mut contents = vec![b'a'; 3 * 1024 * 1024];
        let marker = "🙂".as_bytes();
        let offset = 2 * 1024 * 1024 + 17;
        contents[offset..offset + marker.len()].copy_from_slice(marker);
        std::fs::write(&path, contents).unwrap();
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
            let opened = editor.open(path.clone(), false).await.unwrap();
            assert!(opened.text.is_empty());
            assert_eq!(opened.total_bytes, Some(3 * 1024 * 1024));
            let page = editor
                .read_page(opened.buffer_id.clone(), offset - 128, MAX_PAGE_BYTES)
                .await
                .unwrap();
            let marker_in_page = page.text.find("🙂").unwrap();
            assert!(page.text.is_char_boundary(marker_in_page + "🙂".len()));
            let result = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "paged-test".into(),
                    opened.rev,
                    vec![RangeEdit {
                        start: page.start + marker_in_page,
                        end: page.start + marker_in_page + 4,
                        text: "🧪".into(),
                    }],
                    Some(ByteRange {
                        start: page.start,
                        len: page.text.len(),
                    }),
                    ByteSelection {
                        anchor: page.start + marker_in_page + 4,
                        head: page.start + marker_in_page + 4,
                    },
                )
                .await
                .unwrap();
            assert!(result.accepted);
            assert_eq!(result.rev, opened.rev + 1);
            let updated = result.page.unwrap();
            assert!(updated.text.contains("🧪"));
            let eof = editor
                .read_page(opened.buffer_id.clone(), 3 * 1024 * 1024, 0)
                .await
                .unwrap();
            assert!(eof.text.is_empty());
            let appended = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "paged-test".into(),
                    result.rev,
                    vec![RangeEdit {
                        start: eof.total_bytes,
                        end: eof.total_bytes,
                        text: "end".into(),
                    }],
                    Some(ByteRange {
                        start: eof.start,
                        len: 0,
                    }),
                    ByteSelection {
                        anchor: eof.total_bytes + 3,
                        head: eof.total_bytes + 3,
                    },
                )
                .await
                .unwrap();
            assert!(appended.accepted);
            assert_eq!(appended.page.unwrap().text, "end");
            editor
                .save(opened.buffer_id.clone(), appended.rev, None)
                .await
                .unwrap();
            let saved = std::fs::read(&path).unwrap();
            assert_eq!(&saved[offset..offset + 4], "🧪".as_bytes());
            assert_eq!(&saved[saved.len() - 3..], b"end");
            assert_eq!(saved.len(), 3 * 1024 * 1024 + 3);
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn changed_lazy_source_blocks_reads_and_preserves_dirty_recovery_journal() {
        let root =
            std::env::temp_dir().join(format!("fresh-paged-external-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("large.txt");
        let recovery = root.join("recovery");
        std::fs::write(&path, vec![b'x'; 3 * 1024 * 1024]).unwrap();
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery.clone(),
        )
        .unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opened = editor.open(path.clone(), false).await.unwrap();
            let page = editor
                .read_page(opened.buffer_id.clone(), 1024, MAX_PAGE_BYTES)
                .await
                .unwrap();
            let edited = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "paged-test".into(),
                    opened.rev,
                    vec![RangeEdit {
                        start: page.start,
                        end: page.start + 1,
                        text: "y".into(),
                    }],
                    Some(ByteRange {
                        start: page.start,
                        len: page.text.len(),
                    }),
                    ByteSelection {
                        anchor: page.start + 1,
                        head: page.start + 1,
                    },
                )
                .await
                .unwrap();
            std::fs::write(&path, vec![b'z'; 3 * 1024 * 1024]).unwrap();
            assert!(
                editor
                    .read_page(opened.buffer_id.clone(), 0, 4096)
                    .await
                    .is_err()
            );
            let drafts = DraftStore::new(recovery);
            let draft = drafts.get("default", &opened.draft_id).unwrap().unwrap();
            assert!(draft.paged.is_some());
            assert_eq!(draft.paged.unwrap().edits.len(), 1);
            assert_eq!(edited.rev, opened.rev + 1);
            let copied = root.join("copy.txt");
            assert!(
                editor
                    .save(opened.buffer_id.clone(), edited.rev, Some(copied.clone()))
                    .await
                    .is_err()
            );
            assert!(!copied.exists());
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn deleting_a_full_page_returns_empty_without_exposing_following_text() {
        let root =
            std::env::temp_dir().join(format!("fresh-paged-empty-page-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("large.txt");
        let recovery = root.join("recovery");
        std::fs::write(&path, vec![b'x'; 3 * 1024 * 1024]).unwrap();
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
            let opened = editor.open(path.clone(), false).await.unwrap();
            let page = editor
                .read_page(opened.buffer_id.clone(), 0, MAX_PAGE_BYTES)
                .await
                .unwrap();
            let deleted = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "empty-page".into(),
                    opened.rev,
                    vec![RangeEdit {
                        start: page.start,
                        end: page.start + page.text.len(),
                        text: String::new(),
                    }],
                    Some(ByteRange {
                        start: page.start,
                        len: page.text.len(),
                    }),
                    ByteSelection {
                        anchor: page.start,
                        head: page.start,
                    },
                )
                .await
                .unwrap();
            let empty = deleted.page.unwrap();
            assert!(empty.text.is_empty());
            let following = editor
                .read_page(opened.buffer_id.clone(), page.start, 1)
                .await
                .unwrap();
            assert_eq!(following.text, "x");

            let inserted = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "empty-page".into(),
                    deleted.rev,
                    vec![RangeEdit {
                        start: page.start,
                        end: page.start,
                        text: "Q".into(),
                    }],
                    Some(ByteRange {
                        start: page.start,
                        len: 0,
                    }),
                    ByteSelection {
                        anchor: page.start + 1,
                        head: page.start + 1,
                    },
                )
                .await
                .unwrap();
            assert_eq!(inserted.page.unwrap().text, "Q");
            let following = editor
                .read_page(opened.buffer_id, page.start + 1, 1)
                .await
                .unwrap();
            assert_eq!(following.text, "x");
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn shrinking_a_lazy_buffer_does_not_switch_back_to_snapshot_edits() {
        let root =
            std::env::temp_dir().join(format!("fresh-paged-sticky-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("large.txt");
        let recovery = root.join("recovery");
        let original_len = MAX_SNAPSHOT_BYTES + 32 * 1024;
        std::fs::write(&path, vec![b'x'; original_len]).unwrap();
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
            let opened = editor.open(path.clone(), false).await.unwrap();
            assert_eq!(opened.total_bytes, Some(original_len));
            let tail = editor
                .read_page(opened.buffer_id.clone(), MAX_SNAPSHOT_BYTES, 32 * 1024)
                .await
                .unwrap();
            let deleted = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "sticky-page".into(),
                    opened.rev,
                    vec![RangeEdit {
                        start: MAX_SNAPSHOT_BYTES,
                        end: original_len,
                        text: String::new(),
                    }],
                    Some(ByteRange {
                        start: tail.start,
                        len: tail.text.len(),
                    }),
                    ByteSelection {
                        anchor: MAX_SNAPSHOT_BYTES,
                        head: MAX_SNAPSHOT_BYTES,
                    },
                )
                .await
                .unwrap();
            editor
                .save(opened.buffer_id.clone(), deleted.rev, None)
                .await
                .unwrap();
            let page = editor
                .read_page(opened.buffer_id.clone(), 0, 16)
                .await
                .unwrap();
            assert_eq!(page.total_bytes, MAX_SNAPSHOT_BYTES);
            let edited = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "sticky-page".into(),
                    page.rev,
                    vec![RangeEdit {
                        start: page.start,
                        end: page.start + 1,
                        text: "y".into(),
                    }],
                    Some(ByteRange {
                        start: page.start,
                        len: page.text.len(),
                    }),
                    ByteSelection {
                        anchor: page.start + 1,
                        head: page.start + 1,
                    },
                )
                .await
                .unwrap();
            assert_eq!(edited.page.unwrap().text, "yxxxxxxxxxxxxxxx");
            assert_eq!(
                editor.read_page(opened.buffer_id, 0, 1).await.unwrap().text,
                "y"
            );
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn paged_recovery_replays_incremental_edits_after_reopen() {
        let root =
            std::env::temp_dir().join(format!("fresh-paged-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("large.txt");
        let recovery = root.join("recovery");
        let mut bytes = vec![b'x'; 3 * 1024 * 1024];
        let at = 1024 * 1024 + 11;
        bytes[at] = b'a';
        std::fs::write(&path, bytes).unwrap();
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
            let opened = editor.open(path.clone(), false).await.unwrap();
            let page = editor
                .read_page(opened.buffer_id.clone(), at, MAX_PAGE_BYTES / 2)
                .await
                .unwrap();
            let target = at;
            let edited = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "paged-recovery".into(),
                    opened.rev,
                    vec![RangeEdit {
                        start: target,
                        end: target + 1,
                        text: "🧪".into(),
                    }],
                    Some(ByteRange {
                        start: page.start,
                        len: page.text.len(),
                    }),
                    ByteSelection {
                        anchor: target + 4,
                        head: target + 4,
                    },
                )
                .await
                .unwrap();
            editor.close(opened.buffer_id.clone()).await.unwrap();
            let (restored, changed) = editor
                .draft_restore("default".into(), opened.draft_id)
                .await
                .unwrap();
            assert!(!changed);
            assert_eq!(restored.rev, edited.rev);
            let restored_page = editor
                .read_page(restored.buffer_id.clone(), at, MAX_PAGE_BYTES)
                .await
                .unwrap();
            assert!(restored_page.text.contains("🧪"));
            assert!(restored.dirty);
            let (again, _) = editor
                .draft_restore("default".into(), restored.draft_id.clone())
                .await
                .unwrap();
            assert_eq!(again.rev, restored.rev);
            let page_again = editor
                .read_page(again.buffer_id, at, MAX_PAGE_BYTES)
                .await
                .unwrap();
            assert!(page_again.text.contains("🧪"));
        });
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod external_worker_behavior_tests {
    use super::*;

    async fn next_for(
        events: &mut tokio::sync::broadcast::Receiver<ExternalChange>,
        buffer_id: &str,
    ) -> ExternalChange {
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                if let Ok(event) = events.recv().await {
                    if event.buffer_id == buffer_id {
                        break event;
                    }
                }
            }
        })
        .await
        .expect("daemon reports external file change")
    }

    #[test]
    fn worker_reconciles_external_changes_without_losing_drafts() {
        let root =
            std::env::temp_dir().join(format!("fresh-external-worker-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("source.txt");
        let recovery = root.join("recovery");
        std::fs::write(&path, "initial").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery,
        )
        .expect("worker starts");

        runtime.block_on(async {
            let opened = editor
                .open_in_workspace(path.clone(), false, "external-test".into())
                .await
                .unwrap();
            let mut events = editor.subscribe_external();

            // A clean buffer follows disk changes automatically and reports its new revision.
            let replacement = root.join("source.tmp");
            std::fs::write(&replacement, "clean reload").unwrap();
            #[cfg(windows)]
            std::fs::remove_file(&path).unwrap();
            std::fs::rename(&replacement, &path).unwrap();
            let clean = next_for(&mut events, &opened.buffer_id).await;
            assert!(!clean.dirty);
            assert_eq!(clean.text, "clean reload");
            assert_eq!(clean.disk_text.as_deref(), Some("clean reload"));
            let mut rev = clean.rev;

            // A dirty buffer retains its draft and rejects an implicit save over disk.
            rev = editor
                .edit(opened.buffer_id.clone(), rev, "local draft".into())
                .await
                .unwrap();
            std::fs::write(&path, "external two").unwrap();
            let changed = next_for(&mut events, &opened.buffer_id).await;
            assert!(changed.dirty);
            assert_eq!(changed.text, "local draft");
            assert_eq!(
                editor
                    .save(opened.buffer_id.clone(), rev, None)
                    .await
                    .unwrap_err()
                    .to_string(),
                "file changed on disk; resolve the external change before saving"
            );
            let pending = editor
                .check_external(opened.buffer_id.clone())
                .await
                .unwrap()
                .unwrap();
            let reopened = editor
                .open_in_workspace(path.clone(), false, "external-test".into())
                .await
                .unwrap();
            assert_eq!(reopened.buffer_id, opened.buffer_id);
            assert_eq!(
                editor
                    .check_external(opened.buffer_id.clone())
                    .await
                    .unwrap()
                    .unwrap()
                    .generation,
                pending.generation,
                "reopening a tracked path must preserve its unresolved generation"
            );
            editor
                .resolve_external(
                    opened.buffer_id.clone(),
                    rev,
                    pending.generation,
                    ExternalResolution::Keep,
                )
                .await
                .unwrap();
            assert!(
                editor
                    .save(opened.buffer_id.clone(), rev, None)
                    .await
                    .is_err(),
                "Keep must not authorize an overwrite"
            );

            // Overwrite authorizes one exact generation. A later disk write invalidates it.
            editor
                .resolve_external(
                    opened.buffer_id.clone(),
                    rev,
                    changed.generation,
                    ExternalResolution::Overwrite,
                )
                .await
                .unwrap();
            std::fs::write(&path, "external three").unwrap();
            assert!(
                editor
                    .save(opened.buffer_id.clone(), rev, None)
                    .await
                    .is_err()
            );
            let latest = editor
                .check_external(opened.buffer_id.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(latest.disk_text.as_deref(), Some("external three"));
            editor
                .resolve_external(
                    opened.buffer_id.clone(),
                    rev,
                    latest.generation,
                    ExternalResolution::Overwrite,
                )
                .await
                .unwrap();
            let (saved_path, saved_rev) = editor
                .save(opened.buffer_id.clone(), rev, None)
                .await
                .unwrap();
            assert_eq!(saved_path, path.display().to_string());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "local draft");

            // Reload accepts the current disk generation and removes the recovered draft.
            rev = editor
                .edit(opened.buffer_id.clone(), saved_rev, "second draft".into())
                .await
                .unwrap();
            std::fs::write(&path, "reload target").unwrap();
            let reload_notice = next_for(&mut events, &opened.buffer_id).await;
            let reloaded = editor
                .resolve_external(
                    opened.buffer_id.clone(),
                    rev,
                    reload_notice.generation,
                    ExternalResolution::Reload,
                )
                .await
                .unwrap();
            assert_eq!(reloaded.text, "reload target");
            assert!(!reloaded.dirty);
            assert!(
                editor
                    .draft_list("external-test".into())
                    .await
                    .unwrap()
                    .iter()
                    .all(|draft| draft.draft_id != opened.draft_id)
            );

            // A dirty draft survives deletion and recreation, and duplicate checks coalesce.
            rev = editor
                .edit(
                    opened.buffer_id.clone(),
                    reloaded.rev,
                    "survives delete".into(),
                )
                .await
                .unwrap();
            std::fs::remove_file(&path).unwrap();
            let deleted = next_for(&mut events, &opened.buffer_id).await;
            assert!(deleted.dirty);
            assert!(deleted.disk_text.is_none());
            std::fs::write(&path, "recreated").unwrap();
            let recreated = next_for(&mut events, &opened.buffer_id).await;
            assert!(recreated.dirty);
            assert_eq!(recreated.disk_text.as_deref(), Some("recreated"));
            assert_eq!(recreated.text, "survives delete");
            let current = editor
                .check_external(opened.buffer_id.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(current.generation, recreated.generation);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(800), events.recv())
                    .await
                    .is_err(),
                "unchanged generation should not be broadcast repeatedly"
            );
            let kept = editor.sync(opened.buffer_id.clone()).await.unwrap();
            assert_eq!(kept.rev, rev);
            assert_eq!(kept.text, "survives delete");
        });

        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod external_recovery_tests {
    use super::*;

    #[test]
    fn restored_named_drafts_report_changed_and_missing_sources() {
        for missing in [false, true] {
            let root = std::env::temp_dir()
                .join(format!("fresh-external-restore-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("source.txt");
            let recovery = root.join("recovery");
            std::fs::write(&path, "original").unwrap();
            let drafts = DraftStore::new(recovery.clone());
            let id = crate::drafts::named_draft_id("restore-test", &path);
            drafts
                .checkpoint(
                    "restore-test",
                    Draft {
                        draft_id: id.clone(),
                        path: Some(path.display().to_string()),
                        text: "recovered draft".into(),
                        base_text: Some("original".into()),
                        paged: None,
                    },
                )
                .unwrap();
            if missing {
                std::fs::remove_file(&path).unwrap();
            } else {
                std::fs::write(&path, "changed on disk").unwrap();
            }
            let editor = EditorHandle::spawn_with_recovery_dir(
                root.clone(),
                crate::config::Config::default(),
                recovery,
            )
            .expect("worker starts");
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async {
                let (opened, source_changed) = editor
                    .draft_restore("restore-test".into(), id.clone())
                    .await
                    .unwrap();
                assert!(source_changed);
                let external = editor
                    .check_external(opened.buffer_id.clone())
                    .await
                    .unwrap()
                    .expect("reconnect must expose changed recovery source");
                assert!(external.dirty);
                assert_eq!(external.text, "recovered draft");
                if missing {
                    assert!(external.disk_text.is_none());
                    std::fs::write(&path, "recreated source").unwrap();
                    let recreated = editor
                        .check_external(opened.buffer_id.clone())
                        .await
                        .unwrap()
                        .unwrap();
                    assert_eq!(recreated.disk_text.as_deref(), Some("recreated source"));
                    let reloaded = editor
                        .resolve_external(
                            opened.buffer_id.clone(),
                            external.rev,
                            recreated.generation,
                            ExternalResolution::Reload,
                        )
                        .await
                        .unwrap();
                    assert_eq!(reloaded.text, "recreated source");
                    let rev = editor
                        .edit(
                            opened.buffer_id.clone(),
                            reloaded.rev,
                            "after reload".into(),
                        )
                        .await
                        .unwrap();
                    editor
                        .save(opened.buffer_id.clone(), rev, None)
                        .await
                        .unwrap();
                    assert_eq!(std::fs::read_to_string(&path).unwrap(), "after reload");
                } else {
                    assert_eq!(external.disk_text.as_deref(), Some("changed on disk"));
                }
            });
            drop(editor);
            let _ = std::fs::remove_dir_all(root);
        }
    }

    #[test]
    fn save_as_checks_existing_destinations_and_allows_new_ones() {
        let root =
            std::env::temp_dir().join(format!("fresh-save-as-conflict-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let recovery = root.join("recovery");
        let new_path = root.join("new.txt");
        let existing_path = root.join("existing.txt");
        std::fs::write(&existing_path, "existing source").unwrap();
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery,
        )
        .expect("worker starts");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let fresh = editor
                .new_buffer_in_workspace("save-as-test".into())
                .await
                .unwrap();
            let rev = editor
                .edit(fresh.buffer_id.clone(), fresh.rev, "new text".into())
                .await
                .unwrap();
            let (_, saved_rev) = editor
                .save(fresh.buffer_id.clone(), rev, Some(new_path.clone()))
                .await
                .unwrap();
            assert_eq!(std::fs::read_to_string(&new_path).unwrap(), "new text");

            let other = editor
                .new_buffer_in_workspace("save-as-test".into())
                .await
                .unwrap();
            let rev = editor
                .edit(
                    other.buffer_id.clone(),
                    other.rev,
                    "overwrite consciously".into(),
                )
                .await
                .unwrap();
            assert!(
                editor
                    .save(other.buffer_id.clone(), rev, Some(existing_path.clone()))
                    .await
                    .is_err()
            );
            let pending = editor
                .check_external(other.buffer_id.clone())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(pending.disk_text.as_deref(), Some("existing source"));
            editor
                .resolve_external(
                    other.buffer_id.clone(),
                    rev,
                    pending.generation,
                    ExternalResolution::Overwrite,
                )
                .await
                .unwrap();
            editor
                .save(other.buffer_id, rev, Some(existing_path.clone()))
                .await
                .unwrap();
            assert_eq!(
                std::fs::read_to_string(&existing_path).unwrap(),
                "overwrite consciously"
            );
            editor
                .close_in_workspace(fresh.buffer_id, "save-as-test".into())
                .await
                .unwrap();
            let _ = saved_rev;
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn settings_layers_preserve_ade_lazy_file_boundary() {
        let root = std::env::temp_dir().join(format!(
            "fresh-gui-config-boundary-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(root.join(".fresh")).unwrap();
        std::fs::write(
            root.join(".fresh/config.json"),
            r#"{"editor":{"large_file_threshold_bytes":99999999}}"#,
        )
        .unwrap();
        let config = crate::config::Config::parse(
            r#"{"editor":{"large_file_threshold_bytes":999999999,"tab_size":9}}"#,
        )
        .unwrap();
        let editor = super::build_editor(&root, &config).unwrap();
        assert_eq!(
            editor.config().editor.large_file_threshold_bytes,
            super::MAX_SNAPSHOT_BYTES as u64 + 1
        );
        assert_eq!(editor.config().editor.tab_size, 9);
        drop(editor);
        std::fs::remove_dir_all(root).unwrap();
    }

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
                    paged: None,
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
                    None,
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
                    None,
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
                    None,
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
                    None,
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
                    None,
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
                    None,
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

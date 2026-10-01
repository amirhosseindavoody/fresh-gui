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
use fresh::model::buffer::{Encoding, LineEnding};
use fresh::model::event::{BufferId, Event};
use fresh::model::filesystem::{FileSystem, StdFileSystem};
use fresh::input::keybindings::Action as FreshAction;
use fresh::types::LspFeature;
use fresh::view::color_support::ColorCapability;
use fresh_gui_protocol::{
    BufferDiagnostic, ByteRange, ByteSelection, EditorAction, LspRequest, LspRequestFeature,
    LspResult, LspServerResponse, MAX_SNAPSHOT_BYTES, RangeEdit, SceneBuffer,
};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

mod workspace_edits;
mod language_servers;
#[cfg(test)]
mod workspace_edit_tests;
#[cfg(test)]
#[path = "editor_worker/formatting_tests.rs"]
mod formatting_tests;
#[cfg(test)]
#[path = "editor_worker/file_format_tests.rs"]
mod file_format_tests;
pub(crate) use workspace_edits::WorkspaceNotice;
use workspace_edits::WorkspaceEdits;

const MAX_PAGE_BYTES: usize = 64 * 1024;
const MAX_PAGED_RECOVERY_BYTES: usize = 4 * 1024 * 1024;
const MAX_PROJECT_BUFFER_SNAPSHOT_BYTES: usize = 32 * 1024 * 1024;

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

/// Authoritative worker-thread state used by project search. `text` is omitted
/// for Fresh's lazy/paged buffers, which callers must report as skipped.
#[derive(Debug, Clone)]
pub struct ProjectBufferSnapshot {
    pub buffer_id: String,
    pub draft_id: String,
    pub path: Option<String>,
    pub rev: u64,
    pub text: Option<String>,
    pub total_bytes: Option<usize>,
    pub dirty: bool,
    pub skipped_reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProjectReplaceResult {
    pub buffer_id: String,
    pub path: String,
    pub base_rev: u64,
    pub rev: u64,
    pub text: String,
    pub dirty: bool,
    pub saved: bool,
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
    encoding: String,
    line_ending: String,
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
pub struct SavedBuffer {
    pub path: String,
    pub rev: u64,
    pub outcome: fresh_gui_protocol::SaveOutcome,
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
    disk_generation_with_encoding(path, None)
}

fn disk_generation_with_encoding(
    path: &Path,
    override_encoding: Option<Encoding>,
) -> Result<DiskGeneration> {
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
        let (encoding, binary) = override_encoding
            .map(|encoding| (encoding, false))
            .unwrap_or_else(|| fresh::model::encoding::detect_encoding_or_binary(&bytes, false));
        let text = if binary {
            None
        } else {
            let decoded = fresh::model::encoding::convert_to_utf8(&bytes, encoding);
            let normalized = fresh::model::buffer::format::normalize_line_endings(decoded);
            String::from_utf8(normalized).ok()
        };
        Some(bytes).hash(&mut hasher);
        text
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

fn normalize_text_line_endings(text: &str) -> String {
    String::from_utf8(fresh::model::buffer::format::normalize_line_endings(
        text.as_bytes().to_vec(),
    ))
    .expect("normalizing UTF-8 preserves UTF-8")
}

enum Cmd {
    Open {
        path: PathBuf,
        preview: bool,
        workspace_id: String,
        reply: oneshot::Sender<Result<OpenedBuffer>>,
    },
    OpenLocation {
        path: PathBuf,
        workspace_id: String,
        line: u32,
        character: u32,
        reply: oneshot::Sender<Result<(OpenedBuffer, usize)>>,
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
    ProjectSnapshots {
        workspace_id: String,
        root: PathBuf,
        reply: oneshot::Sender<Result<Vec<ProjectBufferSnapshot>>>,
    },
    ProjectReplace {
        workspace_id: String,
        root: PathBuf,
        path: Option<String>,
        expected_buffer_id: Option<String>,
        expected_rev: Option<u64>,
        expected_text: String,
        edits: Vec<RangeEdit>,
        view_id: String,
        reply: oneshot::Sender<Result<ProjectReplaceResult>>,
    },
    Save {
        buffer_id: String,
        base_rev: u64,
        /// Destination for an unsaved buffer. `None` saves the existing path.
        path: Option<PathBuf>,
        on_save_actions: bool,
        reply: oneshot::Sender<Result<SavedBuffer>>,
    },
    FileControl {
        request: fresh_gui_protocol::BufferFileControl,
        reply: oneshot::Sender<Result<fresh_gui_protocol::BufferFileState>>,
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
    LanguageServers {
        buffer_id: String,
        action: fresh_gui_protocol::LanguageServerAction,
        reply: oneshot::Sender<Result<Vec<fresh_gui_protocol::LanguageServerState>>>,
    },
    Format {
        buffer_id: String,
        base_rev: u64,
        range: Option<ByteRange>,
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
    WorkspaceAuthority(crate::fs::FsRoot),
    WorkspaceCancelOwner(String),
    WorkspacePrepare {
        buffer_id: String, base_rev: u64, owner: String, edit: serde_json::Value,
        reply: oneshot::Sender<Result<fresh_gui_protocol::WorkspaceEditPreview>>,
    },
    WorkspaceApply {
        buffer_id: String, owner: String, token: String,
        reply: oneshot::Sender<Result<Vec<fresh_gui_protocol::WorkspaceBufferUpdate>>>,
    },
    WorkspaceCancel { buffer_id: String, owner: String, token: String },
    LspRequest {
        request: LspRequest,
    },
    LspCancel {
        request_id: u64,
        buffer_id: String,
        view_id: String,
    },
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
    workspace_tx: tokio::sync::broadcast::Sender<WorkspaceNotice>,
}

impl EditorHandle {
    pub(crate) fn set_workspace_authority(&self, authority: crate::fs::FsRoot) {
        let _ = self.tx.send(Cmd::WorkspaceAuthority(authority));
    }
    pub(crate) fn cancel_workspace_owner(&self, owner: &str) {
        let _ = self.tx.send(Cmd::WorkspaceCancelOwner(owner.to_owned()));
    }

    pub(crate) fn subscribe_workspace_edits(&self) -> tokio::sync::broadcast::Receiver<WorkspaceNotice> { self.workspace_tx.subscribe() }

    pub(crate) async fn prepare_workspace_edit(&self, buffer_id: String, base_rev: u64, owner: String, edit: serde_json::Value) -> Result<fresh_gui_protocol::WorkspaceEditPreview> {
        let (reply, receive) = oneshot::channel();
        self.tx.send(Cmd::WorkspacePrepare { buffer_id, base_rev, owner, edit, reply }).map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        receive.await.context("editor worker stopped")?
    }
    pub(crate) async fn apply_workspace_edit(&self, buffer_id: String, owner: String, token: String) -> Result<Vec<fresh_gui_protocol::WorkspaceBufferUpdate>> {
        let (reply, receive) = oneshot::channel();
        self.tx.send(Cmd::WorkspaceApply { buffer_id, owner, token, reply }).map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        receive.await.context("editor worker stopped")?
    }
    pub(crate) fn cancel_workspace_edit(&self, buffer_id: String, owner: String, token: String) -> Result<()> {
        self.tx.send(Cmd::WorkspaceCancel { buffer_id, owner, token }).map_err(|_| anyhow::anyhow!("editor worker stopped"))
    }

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
            .send(Cmd::LspCancel {
                request_id,
                buffer_id,
                view_id,
            })
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
        let (workspace_tx, _) = tokio::sync::broadcast::channel(128);
        let worker_workspace = workspace_tx.clone();
        let worker_external = external_tx.clone();
        let worker_lsp = lsp_tx.clone();
        let dir_for_log = working_dir.clone();

        thread::Builder::new()
            .name("fresh-editor".into())
            .spawn(move || match build_editor(&working_dir, &gui_config) {
                Ok(editor) => {
                    let _ = ready_tx.send(Ok(()));
                    run_loop(
                        editor,
                        rx,
                        DraftStore::new(recovery_dir),
                        worker_external,
                        worker_lsp,
                        worker_workspace,
                    );
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            })
            .ok()?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                info!(dir = %dir_for_log.display(), "Fresh editor worker ready");
                Some(Self {
                    tx,
                    external_tx,
                    lsp_tx,
                    workspace_tx,
                })
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

    pub async fn open_location_in_workspace(
        &self,
        path: PathBuf,
        workspace_id: String,
        line: u32,
        character: u32,
    ) -> Result<(OpenedBuffer, usize)> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::OpenLocation {
                path,
                workspace_id,
                line,
                character,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
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

    pub async fn project_snapshots(
        &self,
        workspace_id: String,
        root: PathBuf,
    ) -> Result<Vec<ProjectBufferSnapshot>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::ProjectSnapshots {
                workspace_id,
                root,
                reply,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    /// Apply replacements against the exact source captured by project search.
    /// For open buffers, identity and revision are checked and the buffer stays
    /// dirty/recoverable. For unopened files, disk text is checked before opening
    /// and Fresh's normal save conflict checks protect the write.
    #[allow(clippy::too_many_arguments)] // One revisioned, scoped worker transaction.
    pub async fn project_replace(
        &self,
        workspace_id: String,
        root: PathBuf,
        path: Option<String>,
        expected_buffer_id: Option<String>,
        expected_rev: Option<u64>,
        expected_text: String,
        edits: Vec<RangeEdit>,
    ) -> Result<ProjectReplaceResult> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::ProjectReplace {
                workspace_id,
                root,
                path,
                expected_buffer_id,
                expected_rev,
                expected_text,
                edits,
                view_id: "project-search".into(),
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

    #[cfg(test)]
    pub async fn save(
        &self,
        buffer_id: String,
        base_rev: u64,
        path: Option<PathBuf>,
    ) -> Result<(String, u64)> {
        let saved = self.save_with_actions(buffer_id, base_rev, path, true).await?;
        Ok((saved.path, saved.rev))
    }

    pub async fn save_with_actions(
        &self, buffer_id: String, base_rev: u64, path: Option<PathBuf>, on_save_actions: bool,
    ) -> Result<SavedBuffer> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Save {
                buffer_id,
                base_rev,
                path,
                on_save_actions,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn file_control(
        &self,
        request: fresh_gui_protocol::BufferFileControl,
    ) -> Result<fresh_gui_protocol::BufferFileState> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::FileControl {
                request,
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

    pub async fn language_servers(
        &self,
        buffer_id: String,
        action: fresh_gui_protocol::LanguageServerAction,
    ) -> Result<Vec<fresh_gui_protocol::LanguageServerState>> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::LanguageServers { buffer_id, action, reply })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        rx.await.map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn format(&self, buffer_id: String, base_rev: u64) -> Result<FormatState> {
        self.format_range(buffer_id, base_rev, None).await
    }

    pub async fn format_range(
        &self,
        buffer_id: String,
        base_rev: u64,
        range: Option<ByteRange>,
    ) -> Result<FormatState> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Cmd::Format {
                buffer_id,
                base_rev,
                range,
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
    workspace_tx: tokio::sync::broadcast::Sender<WorkspaceNotice>,
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

    // LspManager retains Fresh's original runtime and response inbox. Fresh's
    // dispatcher must drain a different queue: otherwise it can consume ADE
    // replies between our poll and editor_tick. Forward non-ADE messages into
    // the editor queue after filtering them, preserving Fresh's dispatch flow.
    let lsp_inbox = std::mem::replace(
        &mut editor.active_window_mut().bridge,
        fresh::services::async_bridge::AsyncBridge::new(),
    );
    let mut tracked: HashMap<String, TrackedBuffer> = HashMap::new();
    let mut lsp_bridge = LspBridgeState {
        inbox: lsp_inbox,
        pending: HashMap::new(),
        request_ids: HashMap::new(),
        aggregates: HashMap::new(),
        results: lsp_tx,
        edits: WorkspaceEdits::with_authority(crate::fs::FsRoot::new(editor.working_dir().to_path_buf()).ok()),
        drafts: drafts.clone(),
        workspace_tx,
        language_logs: HashMap::new(),
        formatting: None,
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
                    poll_lsp_bridge(&mut editor, &mut tracked, &mut lsp_bridge);
                    cancel_stale_lsp_requests(&mut editor, &tracked, &mut lsp_bridge);
                    // Fresh's regular GUI/TUI tick performs disk auto-save
                    // internally. ADE routes auto-save through its serialized,
                    // revision-checked save command so drafts and on-save actions
                    // share the same authoritative pipeline.
                    let auto_save_enabled = editor.config().editor.auto_save_enabled;
                    if auto_save_enabled {
                        editor.config_mut().editor.auto_save_enabled = false;
                    }
                    let tick_result = fresh::app::editor_tick(&mut editor, || Ok(()));
                    if auto_save_enabled {
                        editor.config_mut().editor.auto_save_enabled = true;
                    }
                    if let Err(err) = tick_result {
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
                Cmd::OpenLocation { path, workspace_id, line, character, reply } => {
                    let result = open_buffer(&mut editor, &mut tracked, &path, false, &workspace_id)
                        .and_then(|opened| {
                            let id = opened.buffer_id.parse::<usize>().expect("Fresh buffer id");
                            let buffer = &mut editor.active_window_mut().buffers.get_mut(&BufferId(id)).expect("Fresh buffer").buffer;
                            if opened.total_bytes.is_some()
                                && let Some(entry) = tracked.get(&opened.buffer_id)
                                && let Some(path) = entry.path.as_deref()
                                && entry.paged_generation.as_deref() != Some(disk_generation(path)?.signature.as_str())
                            {
                                bail!("file changed on disk; paged reads are blocked until external change is resolved");
                            }
                            let offset = if opened.total_bytes.is_some() {
                                lsp_position_to_byte_bounded(buffer, line as usize, character as usize)?
                            } else {
                                buffer.lsp_position_to_byte(line as usize, character as usize)
                            };
                            Ok((opened, offset))
                        });
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
                Cmd::ProjectSnapshots { workspace_id, root, reply } => {
                    let result = project_snapshots(
                        &mut editor,
                        &mut tracked,
                        &drafts,
                        &workspace_id,
                        &root,
                    );
                    let _ = reply.send(result);
                }
                Cmd::ProjectReplace {
                    workspace_id,
                    root,
                    path,
                    expected_buffer_id,
                    expected_rev,
                    expected_text,
                    edits,
                    view_id,
                    reply,
                } => {
                    let result = project_replace(
                        &mut editor,
                        &mut tracked,
                        &drafts,
                        &workspace_id,
                        &root,
                        path.as_deref().map(Path::new),
                        expected_buffer_id.as_deref(),
                        expected_rev,
                        &expected_text,
                        edits,
                        &view_id,
                    );
                    let _ = reply.send(result);
                }
                Cmd::Save {
                    buffer_id,
                    base_rev,
                    path,
                    on_save_actions,
                    reply,
                } => {
                    let result = save_buffer(
                        &mut editor,
                        &mut tracked,
                        &buffer_id,
                        base_rev,
                        path.as_deref(),
                        on_save_actions,
                    )
                    .and_then(|saved| {
                        let entry = tracked.get(&buffer_id).expect("saved buffer");
                        if entry.dirty { checkpoint(&drafts, &tracked, &buffer_id)?; } else { drafts.discard(&entry.workspace_id, &entry.draft_id)?; }
                        Ok(saved)
                    });
                    let _ = reply.send(result);
                }
                Cmd::FileControl { request, reply } => {
                    let buffer_id = request.buffer_id.clone();
                    let result = file_control(&mut editor, &mut tracked, request).and_then(|state| {
                        let entry = tracked.get(&buffer_id).context("controlled buffer disappeared")?;
                        if entry.dirty {
                            checkpoint(&drafts, &tracked, &buffer_id)?;
                        } else {
                            drafts.discard(&entry.workspace_id, &entry.draft_id)?;
                        }
                        Ok(state)
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
                Cmd::WorkspaceAuthority(authority) => { lsp_bridge.edits.authority = Some(authority); }
                Cmd::WorkspaceCancelOwner(owner) => { lsp_bridge.edits.cancel_owner(&owner); }
                Cmd::WorkspacePrepare { buffer_id, base_rev, owner, edit, reply } => {
                    let result = lsp_bridge.edits.claim(&buffer_id, base_rev, &owner, &edit);
                    let _ = reply.send(result);
                }
                Cmd::WorkspaceApply { buffer_id, owner, token, reply } => {
                    let result = lsp_bridge.edits.apply(&mut editor, &mut tracked, &drafts, &buffer_id, &owner, &token)
                        .map(|(workspace_id, updates)| {
                            let _ = lsp_bridge.workspace_tx.send(WorkspaceNotice::Applied { workspace_id, updates: updates.clone() });
                            updates
                        });
                    let _ = reply.send(result);
                }
                Cmd::WorkspaceCancel { buffer_id, owner, token } => { lsp_bridge.edits.cancel(&buffer_id, &owner, &token); }
                Cmd::LspRequest { request } => {
                    match sync_all_fresh_text(&editor, &mut tracked) {
                        Ok(_) => begin_lsp_request(&mut editor, &tracked, request, &mut lsp_bridge),
                        Err(error) => send_lsp_status(&lsp_bridge.results, &request, &error.to_string(), true),
                    }
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
                Cmd::LanguageServers { buffer_id, action, reply } => {
                    let logs: Vec<String> = tracked.get(&buffer_id).and_then(|entry| lsp_bridge.language_logs.get(&(entry.workspace_id.clone(), entry.language.clone().unwrap_or_default()))).map(|lines| lines.iter().cloned().collect()).unwrap_or_default();
                    let result = language_servers::language_servers(&mut editor, &tracked, &buffer_id, action, &logs);
                    let _ = reply.send(result);
                }
                Cmd::Format {
                    buffer_id,
                    base_rev,
                    range,
                    reply,
                } => {
                    cancel_all_lsp_requests(&mut editor, &mut lsp_bridge);
                    let result = format_buffer(&mut editor, &mut tracked, &buffer_id, base_rev, range, &mut lsp_bridge)
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
                    let mut fresh_config = editor.config().clone();
                    fresh_config.lsp = config.lsp.clone();
                    fresh_config.lsp_enabled = !fresh_config.lsp.is_empty();
                    // Editor preferences are applied at construction; existing buffers
                    // retain their settings until server restart.
                    config.apply_fresh_services(&mut fresh_config);
                    // Reload the startup-directory service layers too: a user
                    // config reload must not re-enable a project-disabled server.
                    // Keep unrelated editor preferences on their existing values.
                    let mut layered = fresh_config.clone();
                    if let Err(error) = config.apply_fresh_project(&mut layered, editor.working_dir()) {
                        let _ = reply.send(Err(error));
                        continue;
                    }
                    fresh_config.lsp = layered.lsp;
                    fresh_config.lsp_enabled = layered.lsp_enabled;
                    editor.active_window_mut().lsp.shutdown_all();
                    let languages: Vec<_> = editor.config().lsp.keys().cloned().collect();
                    for language in languages {
                        editor.set_lsp_config(language, Vec::new());
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

/// Convert a daemon-owned LSP UTF-16 position without materializing a long
/// lazy line. Fresh's pinned converter reads an entire line at once, so page
/// through the piece tree in bounded chunks for location opens.
fn lsp_position_to_byte_bounded(
    buffer: &mut fresh::model::buffer::TextBuffer,
    wanted_line: usize,
    wanted_character: usize,
) -> Result<usize> {
    use fresh::model::piece_tree::BufferData;

    fn scan(
        bytes: &[u8],
        base: usize,
        wanted_line: usize,
        wanted_character: usize,
        line: &mut usize,
        utf16: &mut usize,
    ) -> Result<Option<usize>> {
        let valid_len = match std::str::from_utf8(bytes) {
            Ok(_) => bytes.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => anyhow::bail!(
                "LSP target buffer contains invalid UTF-8 at byte {}",
                base + error.valid_up_to()
            ),
        };
        let text = std::str::from_utf8(&bytes[..valid_len]).expect("validated UTF-8 prefix");
        for (relative, character) in text.char_indices() {
            if *line == wanted_line && *utf16 >= wanted_character {
                return Ok(Some(base + relative));
            }
            if character == '\n' {
                if *line == wanted_line {
                    return Ok(Some(base + relative));
                }
                *line += 1;
                *utf16 = 0;
            } else if *line == wanted_line {
                *utf16 += character.len_utf16();
            }
        }
        Ok(None)
    }

    let total = buffer.total_bytes();
    let leaves = buffer.piece_tree_leaves();
    let mut document_offset = 0usize;
    let mut line = 0usize;
    let mut utf16 = 0usize;
    let mut carry = Vec::new();
    const CHUNK: usize = 32 * 1024;

    for leaf in leaves {
        let leaf_io = buffer.leaf_io_params(&leaf);
        let mut within_leaf = 0usize;
        while within_leaf < leaf.bytes {
            let length = (leaf.bytes - within_leaf).min(CHUNK);
            let loaded_bytes;
            let bytes = if let Some((path, file_offset, _)) = &leaf_io {
                loaded_bytes = buffer.filesystem().read_range(
                    path,
                    *file_offset + within_leaf as u64,
                    length,
                )?;
                loaded_bytes.as_slice()
            } else {
                let data = buffer
                    .buffer_slice()
                    .get(leaf.location.buffer_id())
                    .context("Fresh piece tree references a missing string buffer")?;
                let BufferData::Loaded { data, .. } = &data.data else {
                    anyhow::bail!("Fresh piece tree leaf has unloaded data without I/O metadata");
                };
                let start = leaf.offset + within_leaf;
                let end = start.saturating_add(length).min(data.len());
                anyhow::ensure!(
                    end - start == length,
                    "Fresh piece tree data range is truncated"
                );
                &data[start..end]
            };
            anyhow::ensure!(
                !bytes.is_empty(),
                "Fresh filesystem returned an empty range during LSP location scan"
            );
            let prefix = std::mem::take(&mut carry);
            let base = document_offset.saturating_sub(prefix.len());
            let mut combined = prefix;
            combined.extend_from_slice(bytes);
            let valid_len = match std::str::from_utf8(&combined) {
                Ok(_) => combined.len(),
                Err(error) if error.error_len().is_none() => error.valid_up_to(),
                Err(error) => anyhow::bail!(
                    "LSP target buffer contains invalid UTF-8 at byte {}",
                    base + error.valid_up_to()
                ),
            };
            if let Some(found) = scan(
                &combined[..valid_len],
                base,
                wanted_line,
                wanted_character,
                &mut line,
                &mut utf16,
            )? {
                return Ok(found);
            }
            carry.extend_from_slice(&combined[valid_len..]);
            document_offset += bytes.len();
            within_leaf += bytes.len();
            anyhow::ensure!(
                within_leaf <= leaf.bytes,
                "Fresh filesystem returned more data than requested"
            );
        }
    }
    anyhow::ensure!(carry.is_empty(), "LSP target ends inside a UTF-8 character");
    Ok(total)
}

#[cfg(test)]
mod lsp_location_tests {
    use super::*;

    #[test]
    fn distant_paged_utf16_position_reads_without_materializing_backing_chunks() {
        let root =
            std::env::temp_dir().join(format!("fresh-lsp-location-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("paged.rs");
        std::fs::write(&path, "a😀x\n".repeat(400_000)).unwrap();
        let mut buffer = fresh::model::buffer::TextBuffer::load_from_file_force_text(
            &path,
            MAX_SNAPSHOT_BYTES,
            Arc::new(StdFileSystem),
        )
        .unwrap();
        let resident_before = buffer.resident_bytes();
        let offset = lsp_position_to_byte_bounded(&mut buffer, 350_000, 3).unwrap();
        assert_eq!(offset, 350_000 * 7 + 5);
        assert_eq!(buffer.resident_bytes(), resident_before);
        let _ = std::fs::remove_dir_all(root);
    }
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
    revisions: HashMap<String, u64>,
}

struct PendingFormatting {
    request_id: u64,
    uri: String,
    buffer_id: String,
    rev: u64,
    text: String,
}

struct LspBridgeState {
    inbox: fresh::services::async_bridge::AsyncBridge,
    pending: HashMap<u64, PendingLspRequest>,
    request_ids: HashMap<u64, Vec<u64>>,
    aggregates: HashMap<u64, LspAggregate>,
    results: tokio::sync::broadcast::Sender<LspResult>,
    edits: WorkspaceEdits,
    drafts: DraftStore,
    workspace_tx: tokio::sync::broadcast::Sender<WorkspaceNotice>,
    language_logs: HashMap<(String, String), std::collections::VecDeque<String>>,
    formatting: Option<PendingFormatting>,
}

fn begin_lsp_request(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    request: LspRequest,
    bridge: &mut LspBridgeState,
) {
    let mut request = request;
    if let Some(item) = request.item.as_mut().and_then(serde_json::Value::as_object_mut) {
        if request.server.is_none() { request.server = item.remove("_fresh_gui_server").and_then(|v| v.as_str().map(str::to_owned)); }
        else { item.remove("_fresh_gui_server"); }
    }
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
        if let Some(old) = bridge
            .aggregates
            .get(&superseded)
            .map(|aggregate| aggregate.request.clone())
        {
            send_lsp_status(
                &bridge.results,
                &old,
                "request superseded by a newer request",
                true,
            );
        }
        cancel_lsp_request(
            editor,
            superseded,
            &request.buffer_id,
            &request.view_id,
            bridge,
        );
    }
    if matches!(request.feature, LspRequestFeature::Rename | LspRequestFeature::CodeActions | LspRequestFeature::CodeActionResolve)
        && let Some((owner, _)) = request.view_id.split_once(':') {
        bridge.edits.cancel_source_owner(&request.buffer_id, owner);
    }
    let LspBridgeState {
        pending,
        request_ids,
        aggregates,
        results,
        ..
    } = bridge;
    let Some(entry) = tracked.get(&request.buffer_id) else {
        send_lsp_status(results, &request, "buffer is closed", true);
        return;
    };
    if request.base_rev != entry.rev {
        send_lsp_status(results, &request, "buffer revision is stale", true);
        return;
    }
    if entry.total_bytes.is_some() {
        send_lsp_status(
            results,
            &request,
            "LSP requests are unavailable for paged buffers",
            false,
        );
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
        send_lsp_status(
            results,
            &request,
            "offset is outside the buffer or splits a UTF-8 character",
            false,
        );
        return;
    }
    let Some(buffer_state) = editor.active_window().buffers.get(&buffer_id) else {
        send_lsp_status(results, &request, "Fresh buffer is unavailable", true);
        return;
    };
    let (line, character) = buffer_state.buffer.position_to_lsp_position(request.offset);
    let (line, character) = (line as u32, character as u32);
    let manager = &editor.active_window().lsp;
    let all_completion_triggers = manager
        .handles_for_feature(language, LspFeature::Completion)
        .into_iter()
        .flat_map(|server| server.capabilities.completion_trigger_characters.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let all_signature_triggers = if manager
        .handles_for_feature(language, LspFeature::SignatureHelp)
        .is_empty()
    {
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
            navigation_targets: Vec::new(),
            completion_triggers: all_completion_triggers,
            signature_triggers: all_signature_triggers,
            status: None,
            stale: false,
        });
        return;
    }

    let route_feature = lsp_route_feature(request.feature);
    let lsp = &editor.active_window().lsp;
    let mut eligible = lsp
        .handles_for_feature(language, route_feature)
        .into_iter()
        .filter(|server| {
            (uri.is_some() || request.feature == LspRequestFeature::WorkspaceSymbols)
                && (!matches!(request.feature, LspRequestFeature::Rename | LspRequestFeature::CodeActionResolve | LspRequestFeature::ExecuteCommand)
                    || request.server.as_deref() == Some(server.name.as_str()))
                && (request.feature != LspRequestFeature::CodeActionResolve || server.capabilities.code_action_resolve)
                && (request.feature != LspRequestFeature::PrepareRename || server.capabilities.rename)
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
    if matches!(request.feature, LspRequestFeature::SignatureHelp | LspRequestFeature::PrepareRename | LspRequestFeature::Rename) { eligible.truncate(1); }
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
            navigation_targets: Vec::new(),
            completion_triggers: all_completion_triggers,
            signature_triggers: all_signature_triggers,
            status: if language.is_empty() && request.feature != LspRequestFeature::Completion {
                Some("buffer has no language mode".into())
            } else if language.is_empty() {
                Some("buffer has no language mode; using buffer words".into())
            } else if uri.is_none() {
                Some(
                    if request.feature != LspRequestFeature::Completion {
                        "buffer has no file URI"
                    } else {
                        "buffer has no file URI; using buffer words"
                    }
                    .into(),
                )
            } else {
                Some("no eligible language server".into())
            },
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
    let diagnostics = uri.as_ref().and_then(|uri| editor.get_stored_diagnostics().get(uri)).cloned().unwrap_or_default();
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
        let params = if matches!(request.feature, LspRequestFeature::PrepareRename | LspRequestFeature::Rename | LspRequestFeature::CodeActions | LspRequestFeature::CodeActionResolve | LspRequestFeature::ExecuteCommand) {
            match refactoring_params(&entry.text, uri.as_deref().unwrap_or(""), line, character, &request, &diagnostics) {
                Ok(params) => Some(params),
                Err(error) => { send_lsp_status(results, &request, &error.to_string(), false); continue; }
            }
        } else if request.feature == LspRequestFeature::CompletionResolve {
            request
                .item
                .clone()
                .map(crate::lsp_bridge::resolve_item_params)
        } else if is_navigation_feature(request.feature) {
            let uri = uri.as_deref().unwrap_or("");
            Some(crate::lsp_bridge::navigation_params(
                uri,
                line,
                character,
                request.feature,
                request.item.as_ref(),
            ))
        } else {
            let is_signature = request.feature == LspRequestFeature::SignatureHelp;
            let Some(uri) = uri.as_deref() else { continue };
            let allowed_triggers = if is_signature {
                vec!["(".to_owned(), ",".to_owned()]
            } else {
                server.capabilities.completion_trigger_characters.clone()
            };
            let trigger = request
                .trigger_character
                .as_deref()
                .filter(|trigger| allowed_triggers.iter().any(|allowed| allowed == *trigger));
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
        send_lsp_status(
            results,
            &request,
            "language server request could not be queued",
            false,
        );
    } else {
        request_ids.insert(request.request_id, sent.clone());
        aggregates.insert(
            request.request_id,
            LspAggregate {
                request,
                remaining: sent.len(),
                responses: Vec::new(),
                completion_triggers,
                signature_triggers,
                deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(5),
                status: None,
                revisions: tracked.iter().map(|(id, entry)| (id.clone(), entry.rev)).collect(),
            },
        );
    }
}

fn refactoring_params(text: &str, uri: &str, line: u32, character: u32, request: &LspRequest, diagnostics: &[lsp_types::Diagnostic]) -> Result<serde_json::Value> {
    use serde_json::json;
    let mut params = json!({"textDocument":{"uri":uri},"position":{"line":line,"character":character}});
    match request.feature {
        LspRequestFeature::PrepareRename => {}
        LspRequestFeature::Rename => {
            let name = request.item.as_ref().and_then(|v| v.get("newName")).and_then(serde_json::Value::as_str).context("rename requires newName")?;
            anyhow::ensure!(!name.is_empty() && name.len() <= 1024 && !name.contains(['\n', '\r']), "invalid rename name");
            params["newName"] = json!(name);
        }
        LspRequestFeature::CodeActions => {
            let start = request.item.as_ref().and_then(|v| v.get("startOffset")).and_then(serde_json::Value::as_u64).unwrap_or(request.offset as u64);
            let end = request.item.as_ref().and_then(|v| v.get("endOffset")).and_then(serde_json::Value::as_u64).unwrap_or(request.offset as u64);
            let (start, end) = (usize::try_from(start)?, usize::try_from(end)?);
            anyhow::ensure!(start <= end && end <= text.len() && text.is_char_boundary(start) && text.is_char_boundary(end), "code action range is invalid");
            let at = |offset| {
                let prefix = &text[..offset];
                json!({"line":prefix.bytes().filter(|b| *b == b'\n').count() as u32,
                    "character":prefix.rsplit('\n').next().unwrap_or("").encode_utf16().count() as u32})
            };
            params = json!({"textDocument":{"uri":uri},"range":{"start":at(start),"end":at(end)},"context":{"diagnostics":diagnostics}});
        }
        LspRequestFeature::CodeActionResolve => {
            params = request.item.clone().context("code action resolve requires an action")?;
            let _: lsp_types::CodeAction = serde_json::from_value(params.clone()).context("invalid code action")?;
        }
        LspRequestFeature::ExecuteCommand => {
            let item = request.item.as_ref().context("executeCommand requires a command")?;
            let command = item.get("command").and_then(serde_json::Value::as_str).context("invalid command identifier")?;
            let arguments = item.get("arguments").cloned().unwrap_or_else(|| json!([]));
            anyhow::ensure!(arguments.is_array(), "command arguments must be an array");
            params = json!({"command":command,"arguments":arguments});
        }
        _ => bail!("not a refactoring feature"),
    }
    Ok(params)
}

fn is_navigation_feature(feature: LspRequestFeature) -> bool {
    matches!(
        feature,
        LspRequestFeature::Definition
            | LspRequestFeature::Declaration
            | LspRequestFeature::TypeDefinition
            | LspRequestFeature::Implementation
            | LspRequestFeature::References
            | LspRequestFeature::DocumentSymbols
            | LspRequestFeature::WorkspaceSymbols
    )
}

fn lsp_route_feature(feature: LspRequestFeature) -> LspFeature {
    match feature {
        LspRequestFeature::Capabilities
        | LspRequestFeature::Completion
        | LspRequestFeature::CompletionResolve => LspFeature::Completion,
        LspRequestFeature::Hover => LspFeature::Hover,
        LspRequestFeature::SignatureHelp => LspFeature::SignatureHelp,
        LspRequestFeature::Definition
        | LspRequestFeature::Declaration
        | LspRequestFeature::TypeDefinition => LspFeature::Definition,
        LspRequestFeature::Implementation => LspFeature::Implementation,
        LspRequestFeature::References => LspFeature::References,
        LspRequestFeature::DocumentSymbols => LspFeature::DocumentSymbols,
        LspRequestFeature::WorkspaceSymbols => LspFeature::WorkspaceSymbols,
        LspRequestFeature::PrepareRename | LspRequestFeature::Rename => LspFeature::Rename,
        LspRequestFeature::CodeActions | LspRequestFeature::CodeActionResolve | LspRequestFeature::ExecuteCommand => LspFeature::CodeAction,
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
    while !text.is_char_boundary(scan_range.start) {
        scan_range.start += 1;
    }
    while !text.is_char_boundary(scan_range.end) {
        scan_range.end -= 1;
    }
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
    if !provider.is_enabled(&context) {
        return Vec::new();
    }
    let ProviderResult::Ready(candidates) =
        provider.provide(&context, &text.as_bytes()[scan_range])
    else {
        return Vec::new();
    };
    let items = candidates
        .into_iter()
        .enumerate()
        .map(|(rank, candidate)| {
            serde_json::json!({
                "label": candidate.label,
                "insertText": candidate.insert_text,
                "sortText": format!("{rank:03}"),
            })
        })
        .collect::<Vec<_>>();
    if items.is_empty() {
        Vec::new()
    } else {
        vec![crate::lsp_bridge::one_response(
            "buffer_words".into(),
            serde_json::json!(items),
        )]
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
        navigation_targets: Vec::new(),
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
    if !bridge.aggregates.get(&request_id).is_some_and(|aggregate| {
        aggregate.request.buffer_id == buffer_id && aggregate.request.view_id == view_id
    }) {
        return;
    }
    let Some(ids) = bridge.request_ids.remove(&request_id) else {
        bridge.aggregates.remove(&request_id);
        return;
    };
    bridge.aggregates.remove(&request_id);
    for id in ids {
        let Some(entry) = bridge.pending.remove(&id) else {
            continue;
        };
        if entry.request.buffer_id != buffer_id || entry.request.view_id != view_id {
            bridge.pending.insert(id, entry);
            bridge.request_ids.entry(request_id).or_default().push(id);
            continue;
        }
        let feature = lsp_route_feature(entry.request.feature);
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
    tracked: &mut HashMap<String, TrackedBuffer>,
    bridge_state: &mut LspBridgeState,
) {
    let LspBridgeState {
        pending,
        request_ids,
        aggregates,
        results,
        ..
    } = bridge_state;
    use fresh::services::async_bridge::AsyncMessage;
    let Some(bridge) = editor.async_bridge() else {
        return;
    };
    let sender = bridge.sender();
    for message in bridge_state.inbox.try_recv_all() {
        match message {
            message @ AsyncMessage::LspFormatting { .. } => {
                let (request_id, uri) = match &message {
                    AsyncMessage::LspFormatting { request_id, uri, .. } => (*request_id, uri),
                    _ => unreachable!(),
                };
                // Fresh's dispatcher applies formatting without a revision guard.
                // Only the currently awaited ADE request can mutate the buffer;
                // timed-out or superseded responses must never reach it.
                let valid = bridge_state.formatting.as_ref().is_some_and(|request| {
                    request.request_id == request_id && request.uri == *uri
                        && tracked.get(&request.buffer_id).is_some_and(|buffer| buffer.rev == request.rev)
                        && request.buffer_id.parse::<usize>().ok().and_then(|id| editor.active_window().buffers.get(&BufferId(id))).and_then(|state| state.buffer.to_string()).as_deref() == Some(request.text.as_str())
                });
                if valid {
                    bridge_state.formatting = None;
                    let _ = sender.send(message);
                }
            }
            message @ (AsyncMessage::LspLogMessage { .. } | AsyncMessage::LspWindowMessage { .. }) => {
                let (language, message_type, body) = match &message {
                    AsyncMessage::LspLogMessage { language, message_type, message } | AsyncMessage::LspWindowMessage { language, message_type, message } => (language, message_type, message),
                    _ => unreachable!(),
                };
                let workspaces = tracked.values().map(|entry| &entry.workspace_id).collect::<std::collections::HashSet<_>>();
                if workspaces.len() == 1 {
                    let workspace = (*workspaces.iter().next().expect("one workspace")).clone();
                    let ring = bridge_state.language_logs.entry((workspace, language.clone())).or_default();
                    let body = body.chars().take(2048).collect::<String>();
                    ring.push_back(format!("Language log (server identity unavailable) {message_type:?}: {body}"));
                    while ring.len() > 100 { ring.pop_front(); }
                }
                let _ = sender.send(message);
            }
            AsyncMessage::PluginLspResponse {
                request_id,
                result,
                language,
            } => {
                let Some(entry) = pending.remove(&request_id) else {
                    if sender
                        .send(AsyncMessage::PluginLspResponse {
                            request_id,
                            result,
                            language,
                        })
                        .is_err()
                    {
                        warn!("failed to return unmatched Fresh LSP response to its dispatcher");
                    }
                    continue;
                };
                // Fresh can also mutate the document asynchronously. Refresh
                // the authoritative revision before deciding to present a reply.
                let _ = sync_fresh_text(editor, tracked, &entry.request.buffer_id);
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
                        aggregate.status =
                            Some("buffer revision changed while request was pending".into());
                        aggregate.responses.clear();
                        aggregate.remaining = 0;
                    } else {
                        match response {
                            Ok(mut value) => {
                                if entry.request.feature == LspRequestFeature::CodeActions {
                                    for action in value.as_array_mut().into_iter().flatten() {
                                        if action.get("disabled").is_some() { continue; }
                                        let Some(edit) = action.get("edit").filter(|edit| edit.is_object()).cloned() else { continue; };
                                        if let Err(error) = bridge_state.edits.offer(editor, tracked, &bridge_state.drafts,
                                            &entry.request.buffer_id, entry.request.base_rev,
                                            entry.request.view_id.split_once(':').map(|(owner, _)| owner.to_owned()), edit,
                                            Some(&entry.server), &aggregate.revisions) {
                                            action["disabled"] = serde_json::json!({"reason":format!("Workspace edit rejected: {error:#}")});
                                        }
                                    }
                                }
                                for edit in
                                    workspace_edits::response_edits(entry.request.feature, &value)
                                        .into_iter()
                                        .filter(|_| {
                                            entry.request.feature != LspRequestFeature::CodeActions
                                        })
                                {
                                    if let Err(error) = bridge_state.edits.offer(
                                        editor,
                                        tracked,
                                        &bridge_state.drafts,
                                        &entry.request.buffer_id,
                                        entry.request.base_rev,
                                        entry
                                            .request
                                            .view_id
                                            .split_once(':')
                                            .map(|(owner, _)| owner.to_owned()),
                                        edit,
                                        Some(&entry.server),
                                        &aggregate.revisions,
                                    ) {
                                        aggregate.status.get_or_insert(format!(
                                            "workspace edit rejected: {error:#}"
                                        ));
                                    }
                                }
                                aggregate
                                    .responses
                                    .push(crate::lsp_bridge::one_response(entry.server, value));
                            }
                            Err(error)
                                if entry.request.feature == LspRequestFeature::PrepareRename
                                    && (error
                                        .to_ascii_lowercase()
                                        .contains("method not found")
                                        || error.contains("-32601")) =>
                            {
                                // prepareRename is optional even when renameProvider is true.
                                aggregate.responses.push(crate::lsp_bridge::one_response(entry.server, serde_json::json!({"defaultBehavior":true})));
                            }
                            Err(error) => {
                                aggregate.status.get_or_insert(error);
                            }
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
                finish_lsp_aggregate(
                    entry.request.request_id,
                    tracked,
                    aggregates,
                    request_ids,
                    results,
                );
            }
            AsyncMessage::LspApplyEdit { edit, label: _ } => {
                let commands = pending.values().filter(|p| p.request.feature == LspRequestFeature::ExecuteCommand).cloned().collect::<Vec<_>>();
                let command = (commands.len() == 1).then(|| &commands[0]);
                let touched = workspace_edits::touched_open_buffers(&edit, tracked);
                let touched_workspaces = touched.as_ref().ok().into_iter().flatten().filter_map(|id| tracked.get(id)).map(|e| &e.workspace_id).collect::<std::collections::HashSet<_>>();
                let all_workspaces = tracked.values().map(|e| &e.workspace_id).collect::<std::collections::HashSet<_>>();
                let source = if touched.is_err() || touched_workspaces.len() > 1 {
                    None
                } else if let Some(command) = command {
                    Some((command.request.buffer_id.clone(), command.request.base_rev))
                } else if let Some(id) = touched.as_ref().ok().and_then(|ids| ids.first()) {
                    tracked.get(id).map(|entry| (id.clone(), entry.rev))
                } else if all_workspaces.len() == 1 {
                    let active = editor.active_buffer().0.to_string();
                    tracked.get_key_value(&active).or_else(|| tracked.iter().min_by_key(|(id, _)| *id)).map(|(id, e)| (id.clone(), e.rev))
                } else { None };
                if let Some((source, rev)) = source {
                    if let Some(entry) = tracked.get(&source) {
                        let workspace_id = entry.workspace_id.clone();
                        let revisions = command
                            .and_then(|c| aggregates.get(&c.request.request_id))
                            .map(|a| a.revisions.clone())
                            .unwrap_or_else(|| {
                                tracked.iter().map(|(id, e)| (id.clone(), e.rev)).collect()
                            });
                        let result = serde_json::to_value(edit)
                            .context("encode server workspace edit")
                            .and_then(|edit| {
                                bridge_state.edits.offer(
                                    editor,
                                    tracked,
                                    &bridge_state.drafts,
                                    &source,
                                    rev,
                                    None,
                                    edit,
                                    command.map(|c| c.server.as_str()),
                                    &revisions,
                                )
                            });
                        let notice = match result {
                            Ok(preview) => WorkspaceNotice::Preview { workspace_id, preview },
                            Err(error) => WorkspaceNotice::Rejected { workspace_id, message: format!("Server workspace edit rejected: {error:#}") },
                        };
                        let _ = bridge_state.workspace_tx.send(notice);
                    }
                } else {
                    // Pinned Fresh doesn't attach a server identity to this callback.
                    // Refuse ambiguous scope; never forward to its sequential applier.
                    for workspace_id in all_workspaces {
                        let _ = bridge_state.workspace_tx.send(WorkspaceNotice::Rejected {
                            workspace_id: workspace_id.clone(), message: "Server workspace edit rejected: target is invalid or workspace ownership is ambiguous".into(),
                        });
                    }
                }
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
    if !aggregates
        .get(&request_id)
        .is_some_and(|aggregate| aggregate.remaining == 0)
    {
        return;
    }
    let Some(aggregate) = aggregates.remove(&request_id) else {
        return;
    };
    request_ids.remove(&request_id);
    let current = tracked
        .get(&aggregate.request.buffer_id)
        .map(|entry| entry.rev);
    let stale = current != Some(aggregate.request.base_rev);
    let source_uri = tracked
        .get(&aggregate.request.buffer_id)
        .and_then(|entry| entry.path.as_deref())
        .and_then(fresh::app::types::file_path_to_lsp_uri)
        .map(|uri| uri.to_string());
    let _ = results.send(LspResult {
        request_id: aggregate.request.request_id,
        buffer_id: aggregate.request.buffer_id,
        view_id: aggregate.request.view_id,
        rev: current.unwrap_or(aggregate.request.base_rev),
        offset: aggregate.request.offset,
        feature: aggregate.request.feature,
        navigation_targets: if stale || !is_navigation_feature(aggregate.request.feature) {
            Vec::new()
        } else {
            crate::lsp_bridge::navigation_targets(&aggregate.responses, source_uri.as_deref())
        },
        responses: if stale {
            Vec::new()
        } else {
            aggregate.responses
        },
        completion_triggers: aggregate.completion_triggers,
        signature_triggers: aggregate.signature_triggers,
        status: if stale {
            Some("buffer revision changed while request was pending".into())
        } else {
            aggregate.status
        },
        stale,
    });
}

fn expire_lsp_aggregates(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    bridge: &mut LspBridgeState,
) {
    let now = tokio::time::Instant::now();
    let expired = bridge
        .aggregates
        .iter()
        .filter_map(|(id, aggregate)| (aggregate.deadline <= now).then_some(*id))
        .collect::<Vec<_>>();
    for id in expired {
        if let Some(aggregate) = bridge.aggregates.get_mut(&id) {
            aggregate.remaining = 0;
            aggregate
                .status
                .get_or_insert_with(|| "language server request timed out".into());
        }
        if let Some(ids) = bridge.request_ids.remove(&id) {
            for lsp_id in ids {
                if let Some(entry) = bridge.pending.remove(&lsp_id) {
                    let route_feature = lsp_route_feature(entry.request.feature);
                    if let Some(server) = editor
                        .active_window_mut()
                        .lsp
                        .handles_for_feature_mut(&entry.language, route_feature)
                        .into_iter()
                        .find(|server| server.name == entry.server)
                    {
                        let _ = server.handle.cancel_request(entry.lsp_request_id);
                    }
                }
            }
        }
        finish_lsp_aggregate(
            id,
            tracked,
            &mut bridge.aggregates,
            &mut bridge.request_ids,
            &bridge.results,
        );
    }
}

fn cancel_buffer_lsp_requests(editor: &mut Editor, buffer_id: &str, bridge: &mut LspBridgeState) {
    let ids = bridge
        .aggregates
        .iter()
        .filter_map(|(id, aggregate)| (aggregate.request.buffer_id == buffer_id).then_some(*id))
        .collect::<Vec<_>>();
    for id in ids {
        if let Some(request) = bridge
            .aggregates
            .get(&id)
            .map(|aggregate| aggregate.request.clone())
        {
            cancel_lsp_request(editor, id, &request.buffer_id, &request.view_id, bridge);
            let _ = bridge.results.send(LspResult {
                request_id: request.request_id,
                buffer_id: request.buffer_id,
                view_id: request.view_id,
                rev: request.base_rev,
                offset: request.offset,
                feature: request.feature,
                responses: Vec::new(),
                navigation_targets: Vec::new(),
                completion_triggers: Vec::new(),
                signature_triggers: Vec::new(),
                status: Some("buffer closed while request was pending".into()),
                stale: true,
            });
        }
    }
}

fn cancel_all_lsp_requests(editor: &mut Editor, bridge: &mut LspBridgeState) {
    let requests = bridge
        .aggregates
        .values()
        .map(|aggregate| aggregate.request.clone())
        .collect::<Vec<_>>();
    for request in requests {
        let request_id = request.request_id;
        cancel_lsp_request(
            editor,
            request_id,
            &request.buffer_id,
            &request.view_id,
            bridge,
        );
        send_lsp_status(
            &bridge.results,
            &request,
            "LSP request cancelled before formatting",
            false,
        );
    }
}

fn cancel_stale_lsp_requests(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    bridge: &mut LspBridgeState,
) {
    let ids = bridge
        .aggregates
        .iter()
        .filter_map(|(id, aggregate)| {
            (tracked
                .get(&aggregate.request.buffer_id)
                .map(|entry| entry.rev)
                != Some(aggregate.request.base_rev))
            .then_some(*id)
        })
        .collect::<Vec<_>>();
    for id in ids {
        let request = bridge
            .aggregates
            .get(&id)
            .map(|aggregate| aggregate.request.clone());
        if let Some(request) = request {
            cancel_lsp_request(editor, id, &request.buffer_id, &request.view_id, bridge);
            let current = tracked
                .get(&request.buffer_id)
                .map(|entry| entry.rev)
                .unwrap_or(request.base_rev);
            let _ = bridge.results.send(LspResult {
                request_id: request.request_id,
                buffer_id: request.buffer_id,
                view_id: request.view_id,
                rev: current,
                offset: request.offset,
                feature: request.feature,
                responses: Vec::new(),
                navigation_targets: Vec::new(),
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
            encoding: editor
                .active_state()
                .buffer
                .encoding()
                .display_name()
                .to_owned(),
            line_ending: editor
                .active_state()
                .buffer
                .line_ending()
                .display_name()
                .to_owned(),
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
            encoding: Some(entry.encoding.clone()),
            line_ending: Some(entry.line_ending.clone()),
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
    activate_tracked(editor, tracked, &opened.buffer_id)?;
    if let Some(encoding) = draft.encoding.as_deref() {
        let encoding = parse_encoding(encoding)?;
        let current = editor.active_state().buffer.encoding();
        if current != encoding {
            editor.active_state_mut().buffer.set_encoding(encoding);
        }
    }
    if let Some(line_ending) = draft.line_ending.as_deref() {
        let line_ending = parse_line_ending(line_ending)?;
        if editor.active_state().buffer.line_ending() != line_ending {
            editor
                .active_state_mut()
                .buffer
                .set_line_ending(line_ending);
        }
    }
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
    if let Some(encoding) = draft.encoding.as_deref() {
        entry.encoding = parse_encoding(encoding)?.display_name().to_owned();
    }
    if let Some(line_ending) = draft.line_ending.as_deref() {
        entry.line_ending = parse_line_ending(line_ending)?.display_name().to_owned();
    }
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
    if !matches!(action, EditorAction::Undo | EditorAction::Redo) {
        clear_secondary_cursors(editor);
    }
    set_selection(editor, selection);
    if action == EditorAction::UniqueLines {
        let (edits, next_selection) = unique_selected_lines(&text, selection)?;
        if !edits.is_empty() {
            return range_edit_buffer(
                editor,
                tracked,
                buffer_id,
                view_id,
                base_rev,
                edits,
                None,
                next_selection,
            );
        }
        return transaction_result(tracked, editor, buffer_id, true);
    }
    if action == EditorAction::ToggleComment {
        let affected_lines = text.lines().count().max(1);
        let language = &editor.active_state().language;
        let prefix_len = editor
            .config()
            .languages
            .get(language)
            .and_then(|language| language.comment_prefix.as_ref())
            .map(|prefix| prefix.len() + usize::from(!prefix.ends_with(' ')))
            .unwrap_or(0);
        if text
            .len()
            .saturating_add(affected_lines.saturating_mul(prefix_len))
            > MAX_SNAPSHOT_BYTES
        {
            bail!("smart edit exceeds the full-buffer editing limit");
        }
    }
    match action {
        EditorAction::Undo => editor.handle_undo(),
        EditorAction::Redo => editor.handle_redo(),
        action => apply_smart_edit_action(editor, action)?,
    }
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    transaction_result(tracked, editor, buffer_id, true)
}

/// Run an allowlisted Fresh editing action through Fresh's own action/event
/// pipeline. `dispatch_action_for_tests` is the only public complete dispatcher
/// at this Fresh pin; actions with public event conversion use that narrower
/// path and are recorded as one Fresh undo event.
fn apply_smart_edit_action(editor: &mut Editor, action: EditorAction) -> Result<()> {
    let fresh_action = match action {
        EditorAction::ExpandSelection => FreshAction::ExpandSelection,
        EditorAction::SelectWord => FreshAction::SelectWord,
        EditorAction::SelectLine => FreshAction::SelectLine,
        EditorAction::SmartHome => FreshAction::SmartHome,
        // Fresh's DeleteBackward already implements smart indentation and
        // paired-delimiter deletion from the current buffer settings.
        EditorAction::SmartBackspace => FreshAction::DeleteBackward,
        EditorAction::InsertNewline => FreshAction::InsertNewline,
        EditorAction::InsertTab => FreshAction::InsertTab,
        EditorAction::DedentSelection => FreshAction::DedentSelection,
        EditorAction::DuplicateLine => FreshAction::DuplicateLine,
        EditorAction::DeleteLine => FreshAction::DeleteLine,
        EditorAction::MoveLineUp => FreshAction::MoveLineUp,
        EditorAction::MoveLineDown => FreshAction::MoveLineDown,
        EditorAction::UniqueLines => {
            unreachable!("custom text transforms are handled by action_buffer")
        }
        EditorAction::ToggleComment => FreshAction::ToggleComment,
        EditorAction::SortLines => FreshAction::SortLines,
        EditorAction::ToUpperCase => FreshAction::ToUpperCase,
        EditorAction::ToLowerCase => FreshAction::ToLowerCase,
        EditorAction::ToggleCase => FreshAction::ToggleCase,
        EditorAction::GoToMatchingBracket => FreshAction::GoToMatchingBracket,
        EditorAction::SurroundParentheses => FreshAction::InsertChar('('),
        EditorAction::SurroundBrackets => FreshAction::InsertChar('['),
        EditorAction::SurroundBraces => FreshAction::InsertChar('{'),
        EditorAction::SurroundDoubleQuotes => FreshAction::InsertChar('"'),
        EditorAction::SurroundSingleQuotes => FreshAction::InsertChar('\''),
        EditorAction::SurroundBackticks => FreshAction::InsertChar('`'),
        EditorAction::Undo | EditorAction::Redo => unreachable!("handled by caller"),
    };

    if let FreshAction::InsertChar(delimiter) = fresh_action {
        let state = editor.active_state();
        if editor.active_cursors().primary().anchor.is_some()
            && fresh::input::actions::get_auto_close_char(
                delimiter,
                state.buffer_settings.auto_surround,
                &state.language,
            )
            .is_none()
        {
            bail!("surround is disabled or this delimiter is unsupported for the buffer language");
        }
    }

    match action {
        EditorAction::SmartHome
        | EditorAction::ToggleComment
        | EditorAction::GoToMatchingBracket => {
            // The pinned Fresh release keeps these cross-cutting handlers
            // behind its internal action dispatcher; this public adapter is
            // the only route that preserves their Fresh-native behavior.
            editor.dispatch_action_for_tests(fresh_action);
        }
        _ => {
            if let Some(events) = editor.active_window_mut().action_to_events(fresh_action) {
                let added_bytes = events.iter().fold(0usize, |total, event| {
                    total.saturating_add(match event {
                        Event::Insert { text, .. } => text.len(),
                        _ => 0,
                    })
                });
                let removed_bytes = events.iter().fold(0usize, |total, event| {
                    total.saturating_add(match event {
                        Event::Delete { range, .. } => range.len(),
                        _ => 0,
                    })
                });
                let resulting_len = editor
                    .active_state()
                    .buffer
                    .len()
                    .saturating_sub(removed_bytes)
                    .saturating_add(added_bytes);
                if resulting_len > MAX_SNAPSHOT_BYTES {
                    bail!("smart edit exceeds the full-buffer editing limit");
                }
                if let Some(event) =
                    editor.apply_events_as_bulk_edit(events, format!("Fresh action: {action:?}"))
                {
                    editor.active_event_log_mut().append(event);
                }
            }
        }
    }
    Ok(())
}

fn clear_secondary_cursors(editor: &mut Editor) {
    // ADE currently carries a single selection. Drop any cursors left behind
    // by another Fresh action without logging a standalone undo transaction.
    editor.active_cursors_mut().remove_secondary();
}

fn unique_selected_lines(
    text: &str,
    selection: ByteSelection,
) -> Result<(Vec<RangeEdit>, ByteSelection)> {
    if text.is_empty() {
        return Ok((Vec::new(), selection));
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut starts = Vec::with_capacity(lines.len());
    let mut offset = 0usize;
    for line in &lines {
        starts.push(offset);
        offset += line.len();
    }
    let line_at = |offset: usize| -> usize {
        starts
            .iter()
            .rposition(|start| *start <= offset.min(text.len().saturating_sub(1)))
            .unwrap_or(0)
    };
    let low = selection.anchor.min(selection.head);
    let high = selection.anchor.max(selection.head);
    let first = line_at(low);
    let last = if high > low { line_at(high - 1) } else { first };
    use std::collections::HashSet;
    let mut seen = HashSet::<&str>::new();
    let mut keep = Vec::new();
    for (index, line) in lines.iter().enumerate().take(last + 1).skip(first) {
        let mut key = *line;
        if let Some(stripped) = key.strip_suffix('\n') {
            key = stripped.strip_suffix('\r').unwrap_or(stripped);
        }
        if seen.insert(key) {
            keep.push(index);
        }
    }
    if keep.len() == last - first + 1 {
        return Ok((Vec::new(), selection));
    }
    let selected_start = starts[first];
    let selected_end = starts[last] + lines[last].len();
    let mut transformed = String::new();
    for index in &keep {
        transformed.push_str(lines[*index]);
    }
    if !text.ends_with('\n')
        && last + 1 == lines.len()
        && let Some(without_lf) = transformed.strip_suffix('\n')
    {
        transformed.truncate(without_lf.len());
        if transformed.ends_with('\r') {
            transformed.pop();
        }
    }
    let transformed_end = selected_start + transformed.len();
    let next_selection = if selection.anchor <= selection.head {
        ByteSelection {
            anchor: selected_start,
            head: transformed_end,
        }
    } else {
        ByteSelection {
            anchor: transformed_end,
            head: selected_start,
        }
    };
    Ok((
        vec![RangeEdit {
            start: selected_start,
            end: selected_end,
            text: transformed,
        }],
        next_selection,
    ))
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

fn same_path(left: &Path, right: &Path) -> bool {
    left.canonicalize().unwrap_or_else(|_| left.to_path_buf())
        == right.canonicalize().unwrap_or_else(|_| right.to_path_buf())
}

fn project_snapshots(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    drafts: &DraftStore,
    workspace_id: &str,
    root: &Path,
) -> Result<Vec<ProjectBufferSnapshot>> {
    let recovered = drafts.list(workspace_id)?;
    let mut skipped_recovery = Vec::new();
    for draft in recovered {
        if draft.path.as_deref().is_some_and(|path| {
            crate::project_search::confined_path(root, Path::new(path)).is_none()
        }) {
            continue;
        }
        if tracked.values().any(|entry| {
            entry.workspace_id == workspace_id && entry.draft_id == draft.draft_id && entry.dirty
        }) {
            continue;
        }
        // Searching must not replay an older recovery copy over an already
        // opened source, even when it belongs to another workspace.
        let already_open = draft.path.as_deref().is_some_and(|path| {
            tracked.values().any(|entry| {
                entry.draft_id != draft.draft_id
                    && entry
                        .path
                        .as_deref()
                        .is_some_and(|open| same_path(open, Path::new(path)))
            })
        });
        if already_open {
            skipped_recovery.push(ProjectBufferSnapshot {
                buffer_id: format!("recovery:{}", draft.draft_id),
                draft_id: draft.draft_id,
                path: None,
                rev: 0,
                total_bytes: Some(draft.text.len()),
                dirty: true,
                text: None,
                skipped_reason: Some(
                    "older recovery copy preserved; its source is already open".into(),
                ),
            });
            continue;
        }
        if let Err(error) = restore_draft(editor, tracked, drafts, workspace_id, &draft.draft_id) {
            warn!(draft_id = %draft.draft_id, %error, "project search could not restore recovery draft");
            skipped_recovery.push(ProjectBufferSnapshot {
                buffer_id: format!("recovery:{}", draft.draft_id),
                draft_id: draft.draft_id,
                path: draft.path,
                rev: 0,
                total_bytes: Some(draft.text.len()),
                dirty: true,
                text: None,
                skipped_reason: Some(format!("recovery draft could not be restored: {error}")),
            });
        }
    }
    sync_all_fresh_text(editor, tracked)?;
    let mut snapshot_bytes = 0usize;
    let mut snapshots = tracked
        .iter()
        .filter(|(_, entry)| {
            entry.workspace_id == workspace_id
                && entry
                    .path
                    .as_deref()
                    .is_none_or(|path| crate::project_search::confined_path(root, path).is_some())
        })
        .map(|(buffer_id, entry)| {
            let path = entry.path.as_ref().map(|path| path.display().to_string());
            let text_bytes = entry.text.len();
            let paged = entry.total_bytes.is_some();
            let over_budget = !paged
                && snapshot_bytes.saturating_add(text_bytes) > MAX_PROJECT_BUFFER_SNAPSHOT_BYTES;
            let skipped_reason = if paged {
                Some("open buffer is paged".to_owned())
            } else if over_budget {
                Some("open buffer snapshot budget reached".to_owned())
            } else {
                snapshot_bytes = snapshot_bytes.saturating_add(text_bytes);
                None
            };
            ProjectBufferSnapshot {
                buffer_id: buffer_id.clone(),
                draft_id: entry.draft_id.clone(),
                path,
                rev: entry.rev,
                text: skipped_reason.is_none().then(|| entry.text.clone()),
                total_bytes: entry
                    .total_bytes
                    .or(skipped_reason.as_ref().map(|_| text_bytes)),
                dirty: entry.dirty,
                skipped_reason,
            }
        })
        .collect::<Vec<_>>();
    snapshots.extend(skipped_recovery);
    Ok(snapshots)
}

#[allow(clippy::too_many_arguments)]
fn project_replace(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    drafts: &DraftStore,
    workspace_id: &str,
    root: &Path,
    path: Option<&Path>,
    expected_buffer_id: Option<&str>,
    expected_rev: Option<u64>,
    expected_text: &str,
    edits: Vec<RangeEdit>,
    view_id: &str,
) -> Result<ProjectReplaceResult> {
    if expected_text.len() > MAX_SNAPSHOT_BYTES {
        bail!("project replacement source exceeds snapshot limit");
    }
    let check_scope = || -> Result<()> {
        if path.is_some_and(|path| crate::project_search::confined_path(root, path).is_none()) {
            bail!("project replacement path moved outside workspace");
        }
        Ok(())
    };
    check_scope()?;
    let open = tracked
        .iter()
        .find(|(buffer_id, entry)| match (entry.path.as_deref(), path) {
            (Some(open_path), Some(path)) => same_path(open_path, path),
            (None, None) => expected_buffer_id == Some(buffer_id.as_str()),
            _ => false,
        });
    let (buffer_id, save_unopened) = match (expected_buffer_id, open) {
        (Some(expected_id), Some((buffer_id, entry)))
            if expected_id == buffer_id
                && entry.workspace_id == workspace_id
                && Some(entry.rev) == expected_rev =>
        {
            if entry.total_bytes.is_some() {
                bail!("project replacement skipped: open buffer is paged");
            }
            if entry.text != expected_text {
                bail!("project replacement source changed since search");
            }
            (buffer_id.clone(), false)
        }
        (Some(_), _) => bail!("open project buffer changed or closed since search"),
        (None, Some(_)) => {
            bail!("file was opened after project search; rerun search before replacing")
        }
        (None, None) if path.is_none() => bail!("pathless draft is no longer open"),
        (None, None) => {
            if expected_rev.is_some() {
                bail!("invalid project replacement source identity");
            }
            let path = path.context("unopened project file has no path")?;
            if !path.is_file() {
                bail!("project search result is no longer a regular file");
            }
            let generation = disk_generation(path)?;
            if generation.text.as_deref() != Some(expected_text) {
                bail!("file changed on disk since project search");
            }
            check_scope()?;
            let opened = open_buffer(editor, tracked, path, false, workspace_id)?;
            let entry = tracked
                .get(&opened.buffer_id)
                .context("opened project file is untracked")?;
            if entry.total_bytes.is_some() || entry.text != expected_text {
                bail!("file changed or became paged while opening project search result");
            }
            let current_disk = disk_generation(path)?;
            if current_disk.signature != generation.signature
                || entry
                    .disk
                    .as_ref()
                    .is_none_or(|disk| disk.signature != generation.signature)
            {
                bail!("file changed on disk while opening project search result");
            }
            (opened.buffer_id, true)
        }
    };

    let entry = tracked
        .get(&buffer_id)
        .context("project buffer closed during replacement")?;
    if entry.workspace_id != workspace_id
        || match (entry.path.as_deref(), path) {
            (Some(open_path), Some(path)) => !same_path(open_path, path),
            (None, None) => false,
            _ => true,
        }
    {
        bail!("project replacement path or workspace changed");
    }
    let base_rev = entry.rev;
    check_scope()?;
    let mut outcome = range_edit_buffer(
        editor,
        tracked,
        &buffer_id,
        view_id,
        base_rev,
        edits,
        None,
        ByteSelection { anchor: 0, head: 0 },
    )?;
    if !outcome.accepted {
        bail!("project replacement revision conflict");
    }
    if outcome.dirty {
        checkpoint(drafts, tracked, &buffer_id)?;
    }
    if save_unopened {
        check_scope()?;
        save_buffer(editor, tracked, &buffer_id, outcome.rev, None, false)?;
        let entry = tracked
            .get(&buffer_id)
            .context("saved project buffer vanished")?;
        drafts.discard(&entry.workspace_id, &entry.draft_id)?;
        outcome = transaction_result(tracked, editor, &buffer_id, true)?;
        close_buffer(editor, tracked, &buffer_id)?;
    }
    Ok(ProjectReplaceResult {
        buffer_id,
        path: path
            .map(|path| path.display().to_string())
            .unwrap_or_default(),
        base_rev,
        rev: outcome.rev,
        text: outcome.text,
        dirty: outcome.dirty,
        saved: save_unopened,
    })
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
            severity: match d.severity {
                Some(lsp_types::DiagnosticSeverity::ERROR) => "error",
                Some(lsp_types::DiagnosticSeverity::WARNING) => "warning",
                Some(lsp_types::DiagnosticSeverity::HINT) => "hint",
                _ => "info",
            }.into(),
            message: d.message,
            source: d.source,
            related_information: d.related_information.unwrap_or_default().into_iter().map(|info| fresh_gui_protocol::DiagnosticRelatedInformation {
                uri: info.location.uri.to_string(), line: info.location.range.start.line, character: info.location.range.start.character, message: info.message,
            }).collect(),
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
    range: Option<ByteRange>,
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
    activate_tracked(editor, tracked, buffer_id)?;
    let language = tracked[buffer_id].language.as_deref().unwrap_or("");
    let has_external = editor.config().languages.get(language).is_some_and(|language| language.formatter.is_some());
    if !has_external {
        let formatter = editor
            .config()
            .lsp
            .get(language)
            .and_then(|servers| {
                servers.as_slice().iter().find(|server| {
                    server.enabled && server.feature_filter().allows(LspFeature::Format)
                })
            })
            .context("no formatting server or external formatter configured for this file")?;
        if !editor.config().lsp_enabled { bail!("Language servers are disabled in settings"); }
        if !fresh::services::lsp::command_exists(&formatter.command) { bail!("LSP {}: command '{}' not found on daemon host; install it or change lsp settings", formatter.display_name(), formatter.command); }
    }
    let before = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer has unloaded regions")?;
    let prior_status = editor.get_status_message().cloned();
    let prior_selection = current_selection(editor);
    if let Some(range) = range {
        let end = range.start.checked_add(range.len).context("format range overflow")?;
        if end > before.len()
            || !before.is_char_boundary(range.start)
            || !before.is_char_boundary(end)
        {
            bail!("format range is outside the buffer or not on UTF-8 boundaries");
        }
        set_selection(editor, ByteSelection { anchor: range.start, head: end });
    }
    if range.is_none() { set_selection(editor, ByteSelection { anchor: prior_selection.head, head: prior_selection.head }); }
    let before_request = editor.active_window().next_lsp_request_id;
    let format_result = editor.format_buffer();
    set_selection(editor, prior_selection);
    format_result.map_err(anyhow::Error::msg)?;
    let requested_lsp = editor.active_window().next_lsp_request_id != before_request;
    if requested_lsp {
        bridge.formatting = Some(PendingFormatting {
            request_id: editor.active_window().next_lsp_request_id,
            uri: fresh::services::lsp::manager::path_to_uri(tracked[buffer_id].path.as_deref().context("formatting needs a saved path")?).context("invalid formatting file URI")?.to_string(),
            buffer_id: buffer_id.to_owned(), rev: current, text: before.clone(),
        });
    }
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
            bridge.formatting = None;
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
        if !requested_lsp || bridge.formatting.is_none() {
            return Ok(FormatState { rev: current, text: None, status: Some("No formatting changes".into()) });
        }
        if tokio::time::Instant::now() >= deadline {
            bridge.formatting = None;
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
            encoding: editor
                .active_state()
                .buffer
                .encoding()
                .display_name()
                .to_owned(),
            line_ending: editor
                .active_state()
                .buffer
                .line_ending()
                .display_name()
                .to_owned(),
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
    on_save_actions: bool,
) -> Result<SavedBuffer> {
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    let current = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?
        .rev;
    if current != base_rev {
        bail!("revision conflict: base_rev={base_rev} current={current}");
    }
    activate_tracked(editor, tracked, buffer_id)?;
    preflight_active_encoding(
        editor,
        tracked
            .get(buffer_id)
            .is_some_and(|entry| entry.total_bytes.is_some()),
    )?;
    let explicit_destination = dest.is_some();
    let had_fresh_path = tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.path.is_some());
    let save_path = dest.map(Path::to_path_buf).or_else(|| {
        tracked
            .get(buffer_id)
            .and_then(|entry| entry.path.clone().or_else(|| entry.recovery_path.clone()))
    });
    if let Some(target) = save_path.as_deref() {
        if path_is_read_only(target) {
            bail!("destination is read-only; choose a writable Save As path");
        }
        let buffer_key = BufferId(buffer_id.parse().context("invalid buffer_id")?);
        let fresh_read_only = editor
            .active_window()
            .buffer_metadata
            .get(&buffer_key)
            .is_some_and(|metadata| metadata.read_only)
            || editor.active_window().is_editing_disabled();
        let same_source = tracked
            .get(buffer_id)
            .and_then(|entry| entry.path.as_deref())
            .is_some_and(|source| {
                source
                    .canonicalize()
                    .unwrap_or_else(|_| source.to_path_buf())
                    == target
                        .canonicalize()
                        .unwrap_or_else(|_| target.to_path_buf())
            });
        if fresh_read_only && same_source {
            bail!("buffer is read-only; choose a writable Save As path");
        }
    }
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
        if tracked
            .get(buffer_id)
            .is_some_and(|entry| entry.total_bytes.is_some())
            && !StdFileSystem.is_owner(dest)
        {
            // Fresh's ownership-preserving path bypasses write_patched and
            // materializes Copy operations. Keep paged saves bounded.
            bail!(
                "paged saves to files owned by another user are unavailable; Save As to a new file"
            );
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
            // Editor::save also runs actions inside its private finalizer.
            // Defer them to ADE's explicit, capability-gated pass below so
            // they run once and legacy encoding output can be protected.
            let config = editor.config().clone();
            editor.config_mut().editor.trim_trailing_whitespace_on_save = false;
            editor.config_mut().editor.ensure_final_newline_on_save = false;
            for language in editor.config_mut().languages.values_mut() {
                language.format_on_save = false;
                language.on_save.clear();
            }
            let result = editor.save();
            *editor.config_mut() = config;
            result.context("Editor::save")?;
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
    // Reuse Fresh's configured format-on-save and on-save actions after its
    // normal save has passed ADE's external-change checks. Fresh persists any
    // formatter output itself and refreshes its watched-file metadata.
    let status = if on_save_actions
        && tracked
            .get(buffer_id)
            .is_some_and(|entry| entry.total_bytes.is_none())
    {
        let encoding = editor.active_state().buffer.encoding();
        let language = editor.active_state().language.clone();
        let skip_formatter = !encoding_supports_all_unicode(encoding)
            && editor
                .config()
                .languages
                .get(&language)
                .is_some_and(|config| config.format_on_save);
        if skip_formatter {
            let language_name = language.as_str();
            editor
                .config_mut()
                .languages
                .get_mut(language_name)
                .expect("checked formatter config")
                .format_on_save = false;
        }
        let action_result = editor.run_on_save_actions();
        if skip_formatter {
            let language_name = language.as_str();
            editor
                .config_mut()
                .languages
                .get_mut(language_name)
                .expect("checked formatter config")
                .format_on_save = true;
            Some(match action_result {
                Ok(_) => format!(
                    "Fresh formatter was skipped to preserve lossless {} output; other on-save actions ran",
                    encoding.display_name()
                ),
                Err(error) => format!(
                    "Fresh formatter was skipped to preserve lossless {} output; other on-save actions failed: {error}",
                    encoding.display_name()
                ),
            })
        } else {
            match action_result {
                Ok(_) => editor
                    .get_status_message()
                    .filter(|message| message.starts_with("Formatter "))
                    .cloned(),
                Err(error) => Some(format!(
                    "File written, but Fresh on-save actions failed: {error}"
                )),
            }
        }
    } else {
        None
    };
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
    entry.disk = Some(disk_generation_with_encoding(
        entry.path.as_deref().context("saved buffer path")?,
        Some(editor.active_state().buffer.encoding()),
    )?);
    entry.paged_generation = entry
        .total_bytes
        .map(|_| entry.disk.as_ref().expect("just set").signature.clone());
    entry.paged_journal.clear();
    entry.external = None;
    entry.overwrite_generation = None;
    entry.base_text = entry.disk.as_ref().and_then(|disk| disk.text.clone());
    entry.dirty = editor.active_state().buffer.is_modified()
        || (!paged
            && entry.base_text.as_deref().is_some_and(|base| {
                normalize_text_line_endings(base) != normalize_text_line_endings(&entry.text)
            }));
    // Acknowledge the actual generation even if a later on-save action failed.
    entry.rev += 1;
    Ok(SavedBuffer {
        path: entry.path.as_ref().expect("saved buffer has a path").display().to_string(),
        rev: entry.rev,
        outcome: fresh_gui_protocol::SaveOutcome {
            text: (!paged && on_save_actions).then(|| entry.text.clone()),
            status,
            dirty: entry.dirty,
        },
    })
}

fn parse_encoding(value: &str) -> Result<Encoding> {
    Encoding::all()
        .iter()
        .copied()
        .find(|encoding| encoding.display_name().eq_ignore_ascii_case(value.trim()))
        .ok_or_else(|| anyhow::anyhow!("unsupported encoding: {value}"))
}

fn parse_line_ending(value: &str) -> Result<LineEnding> {
    match value.trim().to_ascii_uppercase().as_str() {
        "LF" => Ok(LineEnding::LF),
        "CRLF" => Ok(LineEnding::CRLF),
        "CR" => Ok(LineEnding::CR),
        _ => bail!("unsupported line ending: {value}"),
    }
}

fn path_is_read_only(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|metadata| metadata.permissions().readonly())
}

fn strict_encode_check(text: &str, encoding: Encoding) -> Result<()> {
    if encoding == Encoding::Ascii && !text.is_ascii() {
        bail!(
            "text contains characters that cannot be represented as ASCII; choose UTF-8 or another encoding"
        );
    }
    let encoded = fresh::model::encoding::convert_from_utf8(text.as_bytes(), encoding);
    let decoded = fresh::model::encoding::convert_to_utf8(&encoded, encoding);
    if decoded != text.as_bytes() {
        bail!(
            "text cannot be represented losslessly as {}; choose another encoding",
            encoding.display_name()
        );
    }
    Ok(())
}

fn encoding_supports_all_unicode(encoding: Encoding) -> bool {
    matches!(
        encoding,
        Encoding::Utf8 | Encoding::Utf8Bom | Encoding::Utf16Le | Encoding::Utf16Be
    )
}

fn preflight_active_encoding(editor: &Editor, paged: bool) -> Result<()> {
    let buffer = &editor.active_state().buffer;
    let encoding = buffer.encoding();
    if paged {
        if encoding_supports_all_unicode(encoding) || encoding == Encoding::Ascii {
            // Fresh's only genuinely lazy input encodings are UTF-8 and
            // ASCII. Their unchanged backing pieces are copied verbatim, and
            // those encodings do not replace Unicode on write.
            return Ok(());
        }
        bail!(
            "lossless encoding validation is unavailable for paged buffers; reopen as UTF-8 or save a copy"
        );
    }
    let text = buffer
        .to_string()
        .context("buffer text unavailable for encoding validation")?;
    strict_encode_check(&text, encoding)
}

fn file_control(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    request: fresh_gui_protocol::BufferFileControl,
) -> Result<fresh_gui_protocol::BufferFileState> {
    use fresh_gui_protocol::{BufferFileMetadata, BufferFileState, FileControlOperation};
    let buffer_id = request.buffer_id.as_str();
    let _ = sync_fresh_text(editor, tracked, buffer_id)?;
    let entry = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?;
    if entry.rev != request.base_rev {
        bail!(
            "revision conflict: base_rev={} current={}",
            request.base_rev,
            entry.rev
        );
    }
    activate_tracked(editor, tracked, buffer_id)?;
    let buffer_key = BufferId(buffer_id.parse().context("invalid buffer_id")?);
    let source_path = tracked
        .get(buffer_id)
        .and_then(|entry| entry.path.as_deref());
    let metadata_read_only = editor
        .active_window()
        .buffer_metadata
        .get(&buffer_key)
        .is_some_and(|metadata| metadata.read_only)
        || editor.active_window().is_editing_disabled()
        || source_path.is_some_and(path_is_read_only);
    let paged = tracked
        .get(buffer_id)
        .is_some_and(|entry| entry.total_bytes.is_some());
    let mut text = None;
    let is_inspect = matches!(&request.operation, FileControlOperation::Inspect);
    let is_reopen = matches!(&request.operation, FileControlOperation::Reopen { .. });
    match request.operation {
        FileControlOperation::Inspect => {}
        FileControlOperation::SetEncoding { encoding } => {
            if paged {
                bail!("encoding changes are unavailable for paged buffers");
            }
            if metadata_read_only {
                bail!("buffer is read-only; encoding cannot be changed");
            }
            let encoding = parse_encoding(&encoding)?;
            let current_text = editor
                .active_state()
                .buffer
                .to_string()
                .context("buffer text unavailable")?;
            strict_encode_check(&current_text, encoding)?;
            editor.active_state_mut().buffer.set_encoding(encoding);
            tracked.get_mut(buffer_id).expect("tracked").encoding =
                encoding.display_name().to_owned();
        }
        FileControlOperation::SetLineEnding { line_ending } => {
            if paged {
                bail!("line-ending changes are unavailable for paged buffers");
            }
            if metadata_read_only {
                bail!("buffer is read-only; line endings cannot be changed");
            }
            let line_ending = parse_line_ending(&line_ending)?;
            preflight_active_encoding(editor, false)?;
            editor
                .active_state_mut()
                .buffer
                .set_line_ending(line_ending);
            tracked.get_mut(buffer_id).expect("tracked").line_ending =
                line_ending.display_name().to_owned();
        }
        FileControlOperation::Reopen { encoding } => {
            if paged {
                bail!("reopen with encoding is unavailable for paged buffers");
            }
            if editor.active_state().buffer.is_modified()
                || tracked.get(buffer_id).is_some_and(|entry| entry.dirty)
            {
                bail!("cannot reopen with encoding while the buffer has unsaved changes");
            }
            let path = tracked
                .get(buffer_id)
                .and_then(|entry| entry.path.clone())
                .context("scratch buffers cannot be reopened with an encoding")?;
            let known = tracked
                .get(buffer_id)
                .and_then(|entry| entry.disk.as_ref())
                .map(|disk| disk.signature.clone());
            let disk = disk_generation(&path)?;
            if known.as_deref() != Some(disk.signature.as_str()) {
                bail!("file changed on disk; resolve the external change before reopening");
            }
            let encoding = parse_encoding(&encoding)?;
            editor.reload_with_encoding(encoding)?;
            let current = editor
                .active_state()
                .buffer
                .to_string()
                .context("reopened buffer text unavailable")?;
            let line_ending = editor
                .active_state()
                .buffer
                .line_ending()
                .display_name()
                .to_owned();
            let entry = tracked.get_mut(buffer_id).expect("validated tracked entry");
            entry.text = current;
            entry.base_text = Some(entry.text.clone());
            entry.disk = Some(disk);
            entry.dirty = false;
            entry.encoding = encoding.display_name().to_owned();
            entry.line_ending = line_ending;
            entry.rev = entry.rev.wrapping_add(1);
            text = Some(entry.text.clone());
        }
    }
    let buffer = &editor.active_state().buffer;
    let entry = tracked.get_mut(buffer_id).expect("tracked buffer");
    entry.dirty = buffer.is_modified();
    if !paged && !is_inspect && text.is_none() {
        entry.text = buffer.to_string().context("buffer text unavailable")?;
        text = Some(entry.text.clone());
    }
    if !is_inspect && !is_reopen {
        entry.rev = entry.rev.wrapping_add(1);
    }
    let encoding = buffer.encoding();
    let line_ending = buffer.line_ending();
    let autosave = editor
        .config()
        .editor
        .auto_save_enabled
        .then_some(editor.config().editor.auto_save_interval_secs as u64);
    Ok(BufferFileState {
        request_id: request.request_id,
        buffer_id: buffer_id.to_owned(),
        rev: entry.rev,
        metadata: BufferFileMetadata {
            encoding: encoding.display_name().to_owned(),
            bom: encoding.has_bom(),
            line_ending: line_ending.display_name().to_owned(),
            read_only: metadata_read_only,
            paged,
            auto_save_interval_secs: autosave,
        },
        text,
        dirty: entry.dirty,
    })
}

#[cfg(test)]
mod external_generation_tests {
    use super::*;

    #[test]
    fn disk_generation_decodes_bom_encodings_and_normalizes_endings() {
        let root =
            std::env::temp_dir().join(format!("fresh-encoded-generation-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("utf16.txt");
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "first\r\nsecond\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        std::fs::write(&path, bytes).unwrap();
        let generation = disk_generation(&path).unwrap();
        assert_eq!(generation.text.as_deref(), Some("first\nsecond\n"));
        let be = root.join("utf16be.txt");
        let mut bytes = vec![0xFE, 0xFF];
        for unit in "first\r\nsecond\r\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_be_bytes());
        }
        std::fs::write(&be, bytes).unwrap();
        assert_eq!(
            disk_generation(&be).unwrap().text.as_deref(),
            Some("first\nsecond\n")
        );
        let cp1251 = root.join("cp1251.txt");
        let encoded = fresh::model::encoding::convert_from_utf8(
            "Привет\r\n".as_bytes(),
            Encoding::Windows1251,
        );
        std::fs::write(&cp1251, encoded).unwrap();
        assert_eq!(
            disk_generation(&cp1251).unwrap().text.as_deref(),
            Some("Привет\n")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn encoding_preflight_rejects_lossy_and_ascii_saves() {
        assert!(strict_encode_check("café", Encoding::Latin1).is_ok());
        assert!(strict_encode_check("λ", Encoding::Latin1).is_err());
        assert!(strict_encode_check("é", Encoding::Ascii).is_err());
        assert!(encoding_supports_all_unicode(Encoding::Utf16Le));
        assert!(!encoding_supports_all_unicode(Encoding::Windows1252));
    }

    #[cfg(unix)]
    #[test]
    fn no_write_permission_bits_are_reported_read_only_even_for_root() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("fresh-readonly-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("locked.txt");
        std::fs::write(&path, "locked").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
        assert!(path_is_read_only(&path));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn reopen_encoding_rejects_dirty_and_stale_sources() {
        let root =
            std::env::temp_dir().join(format!("fresh-reopen-encoding-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("source.txt");
        std::fs::write(&path, "source").unwrap();
        let editor = EditorHandle::spawn(root.clone(), crate::config::Config::default()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opened = editor.open(path.clone(), false).await.unwrap();
            let changed = editor
                .file_control(fresh_gui_protocol::BufferFileControl {
                    request_id: "encoding-dirty".into(),
                    buffer_id: opened.buffer_id.clone(),
                    base_rev: opened.rev,
                    operation: fresh_gui_protocol::FileControlOperation::SetEncoding {
                        encoding: "Latin-1".into(),
                    },
                })
                .await
                .unwrap();
            let dirty_reopen = editor
                .file_control(fresh_gui_protocol::BufferFileControl {
                    request_id: "dirty-reopen".into(),
                    buffer_id: opened.buffer_id.clone(),
                    base_rev: changed.rev,
                    operation: fresh_gui_protocol::FileControlOperation::Reopen {
                        encoding: "UTF-8".into(),
                    },
                })
                .await
                .unwrap_err();
            assert!(dirty_reopen.to_string().contains("unsaved changes"));
            let saved = editor
                .save(opened.buffer_id.clone(), changed.rev, None)
                .await
                .unwrap();
            std::fs::write(&path, "external").unwrap();
            let stale_reopen = editor
                .file_control(fresh_gui_protocol::BufferFileControl {
                    request_id: "stale-reopen".into(),
                    buffer_id: opened.buffer_id.clone(),
                    base_rev: saved.1,
                    operation: fresh_gui_protocol::FileControlOperation::Reopen {
                        encoding: "UTF-8".into(),
                    },
                })
                .await
                .unwrap_err();
            assert!(stale_reopen.to_string().contains("changed on disk"));
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn worker_ticks_do_not_bypass_revisioned_autosave() {
        let root =
            std::env::temp_dir().join(format!("fresh-autosave-policy-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("source.txt");
        std::fs::write(&path, "disk").unwrap();
        let config = crate::config::Config::parse(
            r#"{"editor":{"auto_save_enabled":true,"auto_save_interval_secs":1}}"#,
        )
        .unwrap();
        let editor = EditorHandle::spawn(root.clone(), config).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opened = editor.open(path.clone(), false).await.unwrap();
            editor
                .edit(opened.buffer_id, opened.rev, "draft".into())
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(1250)).await;
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk");
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }

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
            let state = editor
                .file_control(fresh_gui_protocol::BufferFileControl {
                    request_id: "inspect-paged".into(),
                    buffer_id: opened.buffer_id.clone(),
                    base_rev: opened.rev,
                    operation: fresh_gui_protocol::FileControlOperation::Inspect,
                })
                .await
                .unwrap();
            assert!(state.metadata.paged);
            assert!(
                editor
                    .file_control(fresh_gui_protocol::BufferFileControl {
                        request_id: "reject-paged-eol".into(),
                        buffer_id: opened.buffer_id.clone(),
                        base_rev: opened.rev,
                        operation: fresh_gui_protocol::FileControlOperation::SetLineEnding {
                            line_ending: "CRLF".into()
                        },
                    })
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains("unavailable for paged buffers")
            );
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
                        encoding: None,
                        line_ending: None,
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
    fn project_replace_checks_open_drafts_and_unopened_disk_snapshots() {
        let root = std::env::temp_dir().join(format!(
            "fresh-gui-project-replace-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("source.txt");
        std::fs::write(&source, "from disk").unwrap();
        let recovery = root.join("recovery");
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery.clone(),
        )
        .expect("Fresh worker starts");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let draft_id = rt.block_on(async {
            let draft = editor
                .new_buffer_in_workspace("project-test".into())
                .await
                .unwrap();
            let rev = editor
                .edit(draft.buffer_id.clone(), draft.rev, "draft match".into())
                .await
                .unwrap();
            let snapshot = editor
                .project_snapshots("project-test".into(), root.clone())
                .await
                .unwrap();
            let draft_snapshot = snapshot
                .iter()
                .find(|buffer| buffer.buffer_id == draft.buffer_id)
                .unwrap();
            assert!(draft_snapshot.path.is_none());
            assert!(draft_snapshot.dirty);
            let applied = editor
                .project_replace(
                    "project-test".into(),
                    root.clone(),
                    None,
                    Some(draft.buffer_id.clone()),
                    Some(rev),
                    "draft match".into(),
                    vec![RangeEdit {
                        start: 6,
                        end: 11,
                        text: "updated".into(),
                    }],
                )
                .await
                .unwrap();
            assert!(applied.dirty);
            assert!(!applied.saved);
            assert_eq!(applied.text, "draft updated");
            assert!(
                editor
                    .project_replace(
                        "project-test".into(),
                        root.clone(),
                        None,
                        Some(draft.buffer_id.clone()),
                        Some(rev),
                        "draft match".into(),
                        vec![RangeEdit {
                            start: 6,
                            end: 11,
                            text: "stale".into()
                        }],
                    )
                    .await
                    .is_err()
            );

            // Recheck the explicit scope inside the serialized worker transaction.
            assert!(
                editor
                    .project_replace(
                        "project-test".into(),
                        recovery.clone(),
                        Some(source.display().to_string()),
                        None,
                        None,
                        "from disk".into(),
                        vec![RangeEdit {
                            start: 0,
                            end: 4,
                            text: "lost".into()
                        }],
                    )
                    .await
                    .is_err()
            );
            assert_eq!(std::fs::read_to_string(&source).unwrap(), "from disk");

            let disk_replaced = editor
                .project_replace(
                    "project-test".into(),
                    root.clone(),
                    Some(source.display().to_string()),
                    None,
                    None,
                    "from disk".into(),
                    vec![RangeEdit {
                        start: 5,
                        end: 9,
                        text: "Fresh".into(),
                    }],
                )
                .await
                .unwrap();
            assert!(disk_replaced.saved);
            assert_eq!(std::fs::read_to_string(&source).unwrap(), "from Fresh");
            draft.draft_id
        });
        drop(editor);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery,
        )
        .expect("Fresh worker restarts");
        rt.block_on(async {
            let recovered = editor
                .project_snapshots("project-test".into(), root.clone())
                .await
                .unwrap();
            let snapshot = recovered
                .iter()
                .find(|buffer| buffer.draft_id == draft_id)
                .unwrap();
            let draft = snapshot.clone();
            assert!(snapshot.path.is_none());
            assert_eq!(snapshot.text.as_deref(), Some("draft updated"));
            assert!(snapshot.dirty);
            let externally_changed = root.join("external.txt");
            std::fs::write(&externally_changed, "before").unwrap();
            std::fs::write(&externally_changed, "after").unwrap();
            assert!(
                editor
                    .project_replace(
                        "project-test".into(),
                        root.clone(),
                        Some(externally_changed.display().to_string()),
                        None,
                        None,
                        "before".into(),
                        vec![RangeEdit {
                            start: 0,
                            end: 6,
                            text: "lost".into()
                        }],
                    )
                    .await
                    .is_err()
            );

            let newly_opened = root.join("opened-after-search.txt");
            std::fs::write(&newly_opened, "search text").unwrap();
            editor
                .open_in_workspace(newly_opened.clone(), false, "project-test".into())
                .await
                .unwrap();
            assert!(
                editor
                    .project_replace(
                        "project-test".into(),
                        root.clone(),
                        Some(newly_opened.display().to_string()),
                        None,
                        None,
                        "search text".into(),
                        vec![RangeEdit {
                            start: 0,
                            end: 6,
                            text: "changed".into()
                        }],
                    )
                    .await
                    .is_err()
            );
            editor.draft_discard(draft.buffer_id).await.unwrap();
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn project_search_never_replays_older_recovery_over_an_open_draft() {
        let root =
            std::env::temp_dir().join(format!("fresh-project-recovery-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("source.txt");
        std::fs::write(&path, "disk").unwrap();
        let recovery = root.join("recovery");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let old = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery.clone(),
        )
        .unwrap();
        let old_id = rt.block_on(async {
            let opened = old
                .open_in_workspace(path.clone(), false, "project".into())
                .await
                .unwrap();
            old.edit(opened.buffer_id, opened.rev, "older recovery draft".into())
                .await
                .unwrap();
            opened.draft_id
        });
        drop(old);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let editor = EditorHandle::spawn_with_recovery_dir(
            root.clone(),
            crate::config::Config::default(),
            recovery,
        )
        .unwrap();
        rt.block_on(async {
            let opened = editor
                .open_in_workspace(path.clone(), false, "project".into())
                .await
                .unwrap();
            assert_eq!(opened.draft_id, old_id);
            let recovered = editor
                .project_snapshots("project".into(), root.clone())
                .await
                .unwrap();
            assert_eq!(
                recovered
                    .iter()
                    .find(|s| s.buffer_id == opened.buffer_id)
                    .unwrap()
                    .text
                    .as_deref(),
                Some("older recovery draft")
            );
            let restored_rev = recovered
                .iter()
                .find(|s| s.buffer_id == opened.buffer_id)
                .unwrap()
                .rev;
            editor
                .edit(
                    opened.buffer_id.clone(),
                    restored_rev,
                    "newer visible draft".into(),
                )
                .await
                .unwrap();
            let snapshots = editor
                .project_snapshots("project".into(), root.clone())
                .await
                .unwrap();
            assert_eq!(
                snapshots
                    .iter()
                    .find(|s| s.buffer_id == opened.buffer_id)
                    .unwrap()
                    .text
                    .as_deref(),
                Some("newer visible draft")
            );
            assert!(!snapshots.iter().any(|s| s.skipped_reason.is_some()));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "disk");
        });
        drop(editor);
        let _ = std::fs::remove_dir_all(root);
    }

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
            let encoding = editor
                .file_control(fresh_gui_protocol::BufferFileControl {
                    request_id: "set-encoding".into(),
                    buffer_id: opened.buffer_id.clone(),
                    base_rev: 1,
                    operation: fresh_gui_protocol::FileControlOperation::SetEncoding {
                        encoding: "Latin-1".into(),
                    },
                })
                .await
                .unwrap();
            let format = editor
                .file_control(fresh_gui_protocol::BufferFileControl {
                    request_id: "set-eol".into(),
                    buffer_id: opened.buffer_id.clone(),
                    base_rev: encoding.rev,
                    operation: fresh_gui_protocol::FileControlOperation::SetLineEnding {
                        line_ending: "CRLF".into(),
                    },
                })
                .await
                .unwrap();
            let dirty_rev = editor
                .edit(opened.buffer_id.clone(), format.rev, "unsaved λ".into())
                .await
                .unwrap();
            let drafts = editor.draft_list("workspace-one".into()).await.unwrap();
            assert_eq!(drafts.len(), 1);
            assert_eq!(drafts[0].text, "unsaved λ");
            assert!(
                editor
                    .draft_list("workspace-two".into())
                    .await
                    .unwrap()
                    .is_empty()
            );
            let missing = root.join("missing").join("file.txt");
            let save_error = editor
                .save(opened.buffer_id.clone(), dirty_rev, Some(missing))
                .await
                .unwrap_err();
            assert!(format!("{save_error:#}").contains("cannot be represented"));
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
            assert_eq!(restored.text, "unsaved λ");
            let file_state = restarted
                .file_control(fresh_gui_protocol::BufferFileControl {
                    request_id: "inspect-restored".into(),
                    buffer_id: restored.buffer_id.clone(),
                    base_rev: restored.rev,
                    operation: fresh_gui_protocol::FileControlOperation::Inspect,
                })
                .await
                .unwrap();
            assert_eq!(file_state.metadata.encoding, "Latin-1");
            assert_eq!(file_state.metadata.line_ending, "CRLF");
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
                    encoding: None,
                    line_ending: None,
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

    #[test]
    fn smart_edit_actions_use_fresh_semantics_and_one_undo_transaction() {
        let root = std::env::temp_dir().join(format!(
            "fresh-gui-smart-edit-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let markdown = root.join("notes.md");
        std::fs::write(&markdown, "hello λ🙂\n").unwrap();
        let rust = root.join("indent.rs");
        std::fs::write(&rust, "    λ🙂\n").unwrap();
        let mut config = crate::config::Config::default();
        config.languages.entry("markdown".into()).or_default().auto_surround = Some(true);
        let editor = EditorHandle::spawn(root.clone(), config).expect("Fresh worker starts");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opened = editor.open(markdown.clone(), false).await.unwrap();
            // UTF-8 byte offsets select the Greek letter and emoji together.
            let selected = editor
                .range_edit(
                    opened.buffer_id.clone(),
                    "view-smart".into(),
                    opened.rev,
                    Vec::new(),
                    None,
                    ByteSelection { anchor: 6, head: 12 },
                )
                .await
                .unwrap();
            assert!(selected.accepted);
            let surrounded = editor
                .action(
                    opened.buffer_id.clone(),
                    "view-smart".into(),
                    selected.rev,
                    EditorAction::SurroundDoubleQuotes,
                    selected.selection,
                )
                .await
                .unwrap();
            assert_eq!(surrounded.text, "hello \"λ🙂\"\n");
            let saved = editor
                .save(opened.buffer_id.clone(), surrounded.rev, None)
                .await
                .unwrap();
            assert_eq!(std::fs::read_to_string(&markdown).unwrap(), surrounded.text);

            let undo = editor
                .action(
                    opened.buffer_id.clone(),
                    "view-smart".into(),
                    saved.1,
                    EditorAction::Undo,
                    surrounded.selection,
                )
                .await
                .unwrap();
            assert_eq!(undo.text, "hello λ🙂\n");
            assert!(undo.dirty, "undoing a post-save edit diverges from disk");

            let indented = editor.open(rust.clone(), false).await.unwrap();
            let backspaced = editor
                .action(
                    indented.buffer_id.clone(),
                    "view-smart".into(),
                    indented.rev,
                    EditorAction::SmartBackspace,
                    ByteSelection { anchor: 4, head: 4 },
                )
                .await
                .unwrap();
            assert_eq!(backspaced.text, "λ🙂\n");
            let restored = editor
                .action(
                    indented.buffer_id,
                    "view-smart".into(),
                    backspaced.rev,
                    EditorAction::Undo,
                    backspaced.selection,
                )
                .await
                .unwrap();
            assert_eq!(restored.text, "    λ🙂\n");
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn explicit_surround_preserves_text_when_language_rules_disable_pairing() {
        let root = std::env::temp_dir().join(format!("fresh-gui-surround-disabled-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("disabled.md");
        std::fs::write(&path, "λ🙂").unwrap();
        let editor = EditorHandle::spawn(root.clone(), crate::config::Config::default()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let opened = editor.open(path, false).await.unwrap();
            let result = editor.action(opened.buffer_id.clone(), "view-disabled".into(), opened.rev,
                EditorAction::SurroundBackticks, ByteSelection { anchor: 0, head: 6 }).await;
            assert!(result.is_err());
            let retained = editor.sync(opened.buffer_id).await.unwrap();
            assert_eq!(retained.text, "λ🙂");
            assert_eq!(retained.rev, opened.rev);
            assert!(!retained.dirty);
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn oversized_smart_action_rejects_without_mutating_the_buffer() {
        let root = std::env::temp_dir().join(format!("fresh-gui-smart-limit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("limit.txt");
        let text = "x".repeat(MAX_SNAPSHOT_BYTES - 1) + "\n";
        std::fs::write(&path, &text).unwrap();
        let editor = EditorHandle::spawn(root.clone(), crate::config::Config::default()).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let opened = editor.open(path, false).await.unwrap();
            let result = editor.action(opened.buffer_id.clone(), "view-limit".into(), opened.rev,
                EditorAction::DuplicateLine, ByteSelection { anchor: 0, head: 0 }).await;
            assert!(result.is_err());
            let retained = editor.sync(opened.buffer_id).await.unwrap();
            assert_eq!(retained.text, text);
            assert_eq!(retained.rev, opened.rev);
            assert!(!retained.dirty);
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn editor_action_bridge_matches_direct_fresh_dispatch_for_shared_fixtures() {
        let root = std::env::temp_dir().join(format!(
            "fresh-gui-action-parity-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let fixtures = vec![
            (
                "newline.rs",
                "if ready {\n    value\n}\n",
                ByteSelection { anchor: 14, head: 14 },
                EditorAction::InsertNewline,
                FreshAction::InsertNewline,
            ),
            (
                "tab.txt",
                "\tvalue\n",
                ByteSelection { anchor: 1, head: 1 },
                EditorAction::InsertTab,
                FreshAction::InsertTab,
            ),
            (
                "word.txt",
                "hello λ🙂\n",
                ByteSelection { anchor: 1, head: 1 },
                EditorAction::SelectWord,
                FreshAction::SelectWord,
            ),
            (
                "comment.rs",
                "let value = 1;\n",
                ByteSelection { anchor: 0, head: 14 },
                EditorAction::ToggleComment,
                FreshAction::ToggleComment,
            ),
            (
                "sort.txt",
                "z\na\n",
                ByteSelection { anchor: 0, head: 4 },
                EditorAction::SortLines,
                FreshAction::SortLines,
            ),
            (
                "case.txt",
                "straße\n",
                ByteSelection { anchor: 0, head: 7 },
                EditorAction::ToUpperCase,
                FreshAction::ToUpperCase,
            ),
            (
                "bracket.rs",
                "fn f() {}\n",
                ByteSelection { anchor: 4, head: 4 },
                EditorAction::GoToMatchingBracket,
                FreshAction::GoToMatchingBracket,
            ),
            (
                "surround.md",
                "λ🙂\n",
                ByteSelection { anchor: 0, head: 6 },
                EditorAction::SurroundBackticks,
                FreshAction::InsertChar('`'),
            ),
            (
                "home.txt",
                "\t    λ\n",
                ByteSelection { anchor: 7, head: 7 },
                EditorAction::SmartHome,
                FreshAction::SmartHome,
            ),
            (
                "expand.txt",
                "first second third\n",
                ByteSelection { anchor: 0, head: 5 },
                EditorAction::ExpandSelection,
                FreshAction::ExpandSelection,
            ),
            (
                "move.txt",
                "first\nsecond\n",
                ByteSelection { anchor: 6, head: 6 },
                EditorAction::MoveLineUp,
                FreshAction::MoveLineUp,
            ),
        ];
        let mut config = crate::config::Config::default();
        config.languages.entry("markdown".into()).or_default().auto_surround = Some(true);
        let mut expected = Vec::new();
        {
            let mut fresh = build_editor(&root, &config).unwrap();
            for (name, initial, selection, _, action) in &fixtures {
                let path = root.join(name);
                std::fs::write(&path, initial).unwrap();
                fresh.open_file(&path).unwrap();
                set_selection(&mut fresh, *selection);
                fresh.dispatch_action_for_tests(action.clone());
                expected.push((
                    fresh.active_state().buffer.to_string().unwrap(),
                    current_selection(&fresh),
                ));
            }
        }
        let editor = EditorHandle::spawn(root.clone(), config).expect("Fresh worker starts");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            for ((name, _, selection, action, _), (expected_text, expected_selection)) in
                fixtures.into_iter().zip(expected)
            {
                let opened = editor.open(root.join(name), false).await.unwrap();
                let result = editor
                    .action(
                        opened.buffer_id,
                        "view-parity".into(),
                        opened.rev,
                        action,
                        selection,
                    )
                    .await
                    .unwrap();
                assert_eq!(result.text, expected_text, "action fixture {name}");
                assert_eq!(
                    result.selection, expected_selection,
                    "selection fixture {name}"
                );
            }
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn daemon_line_move_is_single_fresh_undoable_edit() {
        let root = std::env::temp_dir().join(format!(
            "fresh-gui-line-move-test-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let source = root.join("lines.txt");
        std::fs::write(&source, "one\ntwo\nthree\n").unwrap();
        let editor = EditorHandle::spawn(root.clone(), crate::config::Config::default())
            .expect("Fresh worker starts");
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let opened = editor.open(source, false).await.unwrap();
            let moved = editor
                .action(
                    opened.buffer_id.clone(),
                    "view-lines".into(),
                    opened.rev,
                    EditorAction::MoveLineDown,
                    ByteSelection { anchor: 0, head: 3 },
                )
                .await
                .unwrap();
            assert_eq!(moved.text, "two\none\nthree\n");
            let undone = editor
                .action(
                    opened.buffer_id,
                    "view-lines".into(),
                    moved.rev,
                    EditorAction::Undo,
                    moved.selection,
                )
                .await
                .unwrap();
            assert_eq!(undone.text, "one\ntwo\nthree\n");
        });
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn unique_lines_preserves_terminators_and_selection_direction() {
        fn apply(text: &str, selection: ByteSelection) -> (String, ByteSelection) {
            let (edits, selection) = unique_selected_lines(text, selection).unwrap();
            let Some(edit) = edits.first() else {
                return (text.to_owned(), selection);
            };
            let mut result = text.to_owned();
            result.replace_range(edit.start..edit.end, &edit.text);
            (result, selection)
        }

        let (unique, unique_selection) = apply(
            "α\nβ\nα",
            ByteSelection { anchor: 0, head: 8 },
        );
        assert_eq!(unique, "α\nβ");
        assert_eq!(unique_selection, ByteSelection { anchor: 0, head: 5 });

        let (reverse, reverse_selection) = apply(
            "α\nβ\nα",
            ByteSelection { anchor: 8, head: 0 },
        );
        assert_eq!(reverse, "α\nβ");
        assert_eq!(reverse_selection, ByteSelection { anchor: 5, head: 0 });

        let (crlf, crlf_selection) = apply(
            "keep\r\nx\r\ny\r\nx\r\nend",
            ByteSelection { anchor: 6, head: 15 },
        );
        assert_eq!(crlf, "keep\r\nx\r\ny\r\nend");
        assert_eq!(crlf_selection, ByteSelection { anchor: 6, head: 12 });

        let (mixed, mixed_selection) = apply(
            "keep\nx\r\nx\nend",
            ByteSelection { anchor: 5, head: 10 },
        );
        assert_eq!(mixed, "keep\nx\r\nend");
        assert_eq!(mixed_selection, ByteSelection { anchor: 5, head: 8 });
        let (stray_cr, _) = apply("x\r\nx\r", ByteSelection { anchor: 0, head: 5 });
        assert_eq!(stray_cr, "x\r\nx\r");
    }

    // A stdio LSP exercised through Fresh and the ADE worker, including
    // two Python servers and a TOML server. No developer-installed binary is needed.
    pub(super) const FAKE_LSP: &str = r#"#!/usr/bin/env python3
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
            "languages": {"fixture_python": {"extensions":["fixturepy"], "grammar":"python"}, "fixture_rust":{"extensions":["fixturers"], "grammar":"rust"}},
            "lsp": {
                "fixture_python": [
                    {"name":"Ruff","command":"python3","args":[script,"Ruff"],"only_features":["diagnostics","format"]},
                    {"name":"TY","command":"python3","args":[script,"TY"],"only_features":["diagnostics"]}
                ],
                "toml": {"name":"Tombi","command":"python3","args":[script,"Tombi"]},
                "fixture_rust": {"name":"Missing","command":missing,"args":[]}
            }
        }).to_string()).unwrap();
        let python = root.join("sample.fixturepy");
        let toml = root.join("sample.toml");
        let rust = root.join("sample.fixturers");
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
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
                while tokio::time::Instant::now() < deadline {
                    let events = std::fs::read_to_string(&log).unwrap_or_default();
                    closed = events.contains("textDocument/didClose");
                    if closed {
                        break;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
                &serde_json::json!({"lsp": {"fixture_rust": {
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

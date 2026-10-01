//! ADE policy around Fresh workspace edits. Fresh's private applier is sequential
//! and TUI-coupled; proposals here share the daemon's revision and draft authority.
use super::*;
use crate::workspace_transaction::{self, FileChange};
use fresh_gui_protocol::{WorkspaceBufferUpdate, WorkspaceEditFile, WorkspaceEditPreview};
use lsp_types::{DocumentChangeOperation, DocumentChanges, OneOf, ResourceOp, WorkspaceEdit};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const MAX_FILES: usize = 64;
const MAX_TOTAL_BYTES: usize = 8 * 1024 * 1024;
const TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub(crate) enum WorkspaceNotice {
    Preview {
        workspace_id: String,
        preview: WorkspaceEditPreview,
    },
    Applied {
        workspace_id: String,
        updates: Vec<WorkspaceBufferUpdate>,
    },
    Rejected {
        workspace_id: String,
        message: String,
    },
}

struct Target {
    path: PathBuf,
    disk: DiskGeneration,
    before: Option<String>,
    after: Option<String>,
    buffer_id: Option<String>,
    rev: Option<u64>,
    permissions: Option<std::fs::Permissions>,
    version_check: Option<(String, i64)>,
}

struct Proposal {
    workspace: String,
    owner: Option<String>,
    source: String,
    source_rev: u64,
    expires: Instant,
    edit: Value,
    targets: Vec<Target>,
}

#[derive(Default)]
pub(super) struct WorkspaceEdits {
    proposals: HashMap<String, Proposal>,
    pub(super) authority: Option<crate::fs::FsRoot>,
}

fn uri_path(uri: &lsp_types::Uri) -> Result<PathBuf> {
    let path = fresh::app::types::LspUri::from_wire(uri.clone())
        .to_host_path(None)
        .context("workspace edit target is not a file URI")?;
    anyhow::ensure!(
        path.is_absolute(),
        "workspace edit file URI must be absolute"
    );
    let metadata = std::fs::symlink_metadata(&path);
    if let Ok(metadata) = &metadata {
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            bail!(
                "workspace edit target must be a regular non-symlink file: {}",
                path.display()
            );
        }
    } else if let Err(error) = metadata
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    // Normalize parents on the authority, making aliases and '..' share one key.
    let parent = path
        .parent()
        .context("workspace edit path has no parent")?
        .canonicalize()?;
    Ok(parent.join(
        path.file_name()
            .context("workspace edit has no file name")?,
    ))
}

fn canonical_named(path: &Path) -> Option<PathBuf> {
    path.parent()?
        .canonicalize()
        .ok()
        .map(|parent| parent.join(path.file_name().unwrap_or_default()))
}

/// Strict UTF-16 conversion: malformed positions cannot be silently clamped.
fn position(text: &str, pos: lsp_types::Position) -> Result<usize> {
    let mut line = 0;
    let mut start = 0;
    while line < pos.line {
        let end = text[start..]
            .find('\n')
            .context("LSP edit line is outside the document")?;
        start += end + 1;
        line += 1;
    }
    let end = text[start..]
        .find('\n')
        .map(|n| start + n)
        .unwrap_or(text.len());
    let end = if end > start && text.as_bytes()[end - 1] == b'\r' {
        end - 1
    } else {
        end
    };
    let mut units = 0;
    for (offset, ch) in text[start..end].char_indices() {
        if units == pos.character {
            return Ok(start + offset);
        }
        units += ch.len_utf16() as u32;
        if units > pos.character {
            bail!("LSP edit position splits a UTF-16 surrogate pair");
        }
    }
    if units == pos.character {
        Ok(end)
    } else {
        bail!("LSP edit character is outside the line")
    }
}

fn apply_text(text: &str, edits: Vec<lsp_types::TextEdit>) -> Result<String> {
    let mut ranges = edits
        .into_iter()
        .map(|edit| {
            let start = position(text, edit.range.start)?;
            let end = position(text, edit.range.end)?;
            anyhow::ensure!(start <= end, "LSP edit range is reversed");
            Ok((start, end, edit.new_text))
        })
        .collect::<Result<Vec<_>>>()?;
    ranges.sort_by_key(|(start, end, _)| (*start, *end));
    for adjacent in ranges.windows(2) {
        anyhow::ensure!(
            adjacent[0].1 <= adjacent[1].0 && adjacent[0].0 != adjacent[1].0,
            "overlapping or ambiguous LSP edits"
        );
    }
    let mut result = text.to_owned();
    for (start, end, replacement) in ranges.into_iter().rev() {
        result.replace_range(start..end, &replacement);
        anyhow::ensure!(
            result.len() <= MAX_SNAPSHOT_BYTES,
            "workspace edit exceeds buffer snapshot limit"
        );
    }
    Ok(result)
}

impl WorkspaceEdits {
    pub(super) fn with_authority(authority: Option<crate::fs::FsRoot>) -> Self {
        Self {
            authority,
            ..Self::default()
        }
    }

    fn expire(&mut self) {
        self.proposals
            .retain(|_, proposal| proposal.expires > Instant::now());
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn offer(
        &mut self,
        editor: &Editor,
        tracked: &HashMap<String, TrackedBuffer>,
        drafts: &DraftStore,
        source: &str,
        source_rev: u64,
        owner: Option<String>,
        edit_value: Value,
        server: Option<&str>,
        revisions: &HashMap<String, u64>,
    ) -> Result<WorkspaceEditPreview> {
        self.expire();
        anyhow::ensure!(
            self.proposals.len() < 32,
            "too many pending workspace edit previews; dismiss one first"
        );
        let source_entry = tracked
            .get(source)
            .context("workspace edit source is closed")?;
        anyhow::ensure!(
            source_entry.rev == source_rev,
            "workspace edit source revision is stale"
        );
        let workspace = source_entry.workspace_id.clone();
        let edit: WorkspaceEdit =
            serde_json::from_value(edit_value.clone()).context("invalid LSP workspace edit")?;
        anyhow::ensure!(
            edit.changes.is_none() || edit.document_changes.is_none(),
            "workspace edit has both changes and documentChanges"
        );
        if edit.change_annotations.as_ref().is_some_and(|annotations| {
            annotations
                .values()
                .any(|a| a.needs_confirmation == Some(true))
        }) {
            bail!("workspace edit requires change-annotation confirmation, which is unsupported");
        }
        let authority = self
            .authority
            .as_ref()
            .context("workspace edit filesystem authority unavailable")?;
        let recovery = drafts.protected_paths()?;
        let mut targets = BTreeMap::<PathBuf, Target>::new();
        let mut load = |uri: &lsp_types::Uri| -> Result<PathBuf> {
            let path = uri_path(uri)?;
            authority.validate_workspace_edit_path(&path)?;
            if targets.contains_key(&path) {
                return Ok(path);
            }
            anyhow::ensure!(
                targets.len() < MAX_FILES,
                "workspace edit exceeds {MAX_FILES} files"
            );
            let buffers = tracked
                .iter()
                .filter(|(_, entry)| {
                    entry
                        .path
                        .as_deref()
                        .or(entry.recovery_path.as_deref())
                        .and_then(canonical_named)
                        .as_ref()
                        == Some(&path)
                })
                .collect::<Vec<_>>();
            anyhow::ensure!(
                buffers.len() <= 1,
                "workspace edit target is open in multiple workspaces: {}",
                path.display()
            );
            let disk = disk_generation(&path)?;
            let (before, buffer_id, rev) = if let Some((id, entry)) = buffers.first() {
                anyhow::ensure!(
                    entry.workspace_id == workspace,
                    "workspace edit target belongs to another workspace: {}",
                    path.display()
                );
                anyhow::ensure!(
                    entry.total_bytes.is_none(),
                    "workspace edits do not support paged buffers: {}",
                    path.display()
                );
                anyhow::ensure!(
                    revisions.get(*id) == Some(&entry.rev),
                    "workspace edit target revision is stale: {}",
                    path.display()
                );
                anyhow::ensure!(
                    entry.external.is_none() && entry.disk.as_ref() == Some(&disk),
                    "resolve external changes before workspace edits: {}",
                    path.display()
                );
                (
                    Some(entry.text.clone()),
                    Some((*id).clone()),
                    Some(entry.rev),
                )
            } else {
                anyhow::ensure!(
                    !recovery
                        .iter()
                        .any(|draft| canonical_named(draft).as_ref() == Some(&path)),
                    "workspace edit target has a recovery draft; restore or discard it first: {}",
                    path.display()
                );
                anyhow::ensure!(
                    disk.text.is_some() || disk.signature.starts_with("missing:"),
                    "workspace edit target exceeds snapshot limit: {}",
                    path.display()
                );
                (disk.text.clone(), None, None)
            };
            targets.insert(
                path.clone(),
                Target {
                    path: path.clone(),
                    disk,
                    after: before.clone(),
                    before,
                    buffer_id,
                    rev,
                    permissions: None,
                    version_check: None,
                },
            );
            Ok(path)
        };
        // Parse ordered operations first, loading every target before simulating any changes.
        enum Operation {
            Text(PathBuf, Option<i32>, Vec<lsp_types::TextEdit>),
            Create(PathBuf),
            Rename(PathBuf, PathBuf),
            Delete(PathBuf),
        }
        let mut operations = Vec::new();
        if let Some(changes) = edit.changes {
            for (uri, edits) in changes {
                operations.push(Operation::Text(load(&uri)?, None, edits));
            }
        }
        if let Some(changes) = edit.document_changes {
            let operations_in = match changes {
                DocumentChanges::Edits(edits) => edits
                    .into_iter()
                    .map(DocumentChangeOperation::Edit)
                    .collect(),
                DocumentChanges::Operations(operations) => operations,
            };
            for operation in operations_in {
                match operation {
                    DocumentChangeOperation::Edit(edit) => {
                        let edits = edit
                            .edits
                            .into_iter()
                            .map(|edit| match edit {
                                OneOf::Left(edit) => Ok(edit),
                                OneOf::Right(_) => {
                                    Err(anyhow::anyhow!("annotated text edits are unsupported"))
                                }
                            })
                            .collect::<Result<Vec<_>>>()?;
                        operations.push(Operation::Text(
                            load(&edit.text_document.uri)?,
                            edit.text_document.version,
                            edits,
                        ));
                    }
                    DocumentChangeOperation::Op(ResourceOp::Create(op)) => {
                        anyhow::ensure!(
                            !op.options.is_some_and(|o| o.overwrite == Some(true)),
                            "workspace edit create overwrite is unsupported"
                        );
                        operations.push(Operation::Create(load(&op.uri)?));
                    }
                    DocumentChangeOperation::Op(ResourceOp::Rename(op)) => {
                        anyhow::ensure!(
                            !op.options.is_some_and(|o| o.overwrite == Some(true)),
                            "workspace edit rename overwrite is unsupported"
                        );
                        operations.push(Operation::Rename(load(&op.old_uri)?, load(&op.new_uri)?));
                    }
                    DocumentChangeOperation::Op(ResourceOp::Delete(op)) => {
                        anyhow::ensure!(
                            !op.options.is_some_and(|o| o.recursive == Some(true)),
                            "workspace edit recursive delete is unsupported"
                        );
                        operations.push(Operation::Delete(load(&op.uri)?));
                    }
                }
            }
        }
        for operation in operations {
            match operation {
                Operation::Text(path, version, edits) => {
                    let target = targets.get_mut(&path).expect("loaded");
                    if let Some(version) = version {
                        anyhow::ensure!(
                            target.buffer_id.is_some(),
                            "versioned edits of closed files are unsupported: {}",
                            path.display()
                        );
                        let server = server
                            .context("versioned server-initiated edit lacks server identity")?;
                        let language = target
                            .buffer_id
                            .as_ref()
                            .and_then(|id| tracked.get(id))
                            .and_then(|e| e.language.as_deref())
                            .unwrap_or("");
                        let actual = editor
                            .active_window()
                            .lsp
                            .get_handles(language)
                            .into_iter()
                            .find(|s| s.name == server)
                            .and_then(|s| s.handle.document_version(&path));
                        anyhow::ensure!(
                            actual == Some(i64::from(version)),
                            "stale LSP document version for {}",
                            path.display()
                        );
                        target.version_check = Some((server.to_owned(), i64::from(version)));
                    }
                    let before = target
                        .after
                        .as_ref()
                        .context("workspace text edit target does not exist")?;
                    target.after = Some(apply_text(before, edits)?);
                }
                Operation::Create(path) => {
                    let target = targets.get_mut(&path).expect("loaded");
                    anyhow::ensure!(
                        target.buffer_id.is_none(),
                        "resource operations on open buffers are unsupported: {}",
                        path.display()
                    );
                    anyhow::ensure!(
                        target.after.is_none(),
                        "workspace create would overwrite {}",
                        path.display()
                    );
                    target.after = Some(String::new());
                }
                Operation::Delete(path) => {
                    let target = targets.get_mut(&path).expect("loaded");
                    anyhow::ensure!(
                        target.buffer_id.is_none(),
                        "resource operations on open buffers are unsupported: {}",
                        path.display()
                    );
                    anyhow::ensure!(
                        target.after.is_some(),
                        "workspace delete target does not exist: {}",
                        path.display()
                    );
                    target.after = None;
                }
                Operation::Rename(from, to) => {
                    anyhow::ensure!(from != to, "workspace rename source equals destination");
                    let source = targets.get(&from).expect("loaded");
                    let destination = targets.get(&to).expect("loaded");
                    anyhow::ensure!(
                        source.buffer_id.is_none() && destination.buffer_id.is_none(),
                        "resource operations on open buffers are unsupported"
                    );
                    anyhow::ensure!(
                        destination.after.is_none(),
                        "workspace rename would overwrite {}",
                        to.display()
                    );
                    let text = source
                        .after
                        .clone()
                        .context("workspace rename source does not exist")?;
                    let permissions = source
                        .permissions
                        .clone()
                        .or_else(|| std::fs::metadata(&from).ok().map(|m| m.permissions()));
                    let source = targets.get_mut(&from).expect("loaded");
                    source.after = None;
                    let destination = targets.get_mut(&to).expect("loaded");
                    destination.after = Some(text);
                    destination.permissions = permissions;
                }
            }
        }
        let targets = targets
            .into_values()
            .filter(|target| target.before != target.after)
            .collect::<Vec<_>>();
        anyhow::ensure!(!targets.is_empty(), "workspace edit contains no changes");
        let total = targets
            .iter()
            .map(|t| {
                t.before.as_ref().map_or(0, String::len) + t.after.as_ref().map_or(0, String::len)
            })
            .sum::<usize>();
        anyhow::ensure!(
            total <= MAX_TOTAL_BYTES,
            "workspace edit preview exceeds {MAX_TOTAL_BYTES} bytes"
        );
        let token = uuid::Uuid::new_v4().to_string();
        let preview = WorkspaceEditPreview {
            token: token.clone(),
            buffer_id: source.to_owned(),
            files: targets
                .iter()
                .map(|t| WorkspaceEditFile {
                    path: t.path.display().to_string(),
                    operation: match (&t.before, &t.after) {
                        (None, _) => "create",
                        (_, None) => "delete",
                        _ => "edit",
                    }
                    .into(),
                    before: t.before.clone().unwrap_or_default(),
                    after: t.after.clone().unwrap_or_default(),
                    buffer_id: t.buffer_id.clone(),
                    base_rev: t.rev,
                })
                .collect(),
        };
        self.proposals.insert(
            token,
            Proposal {
                workspace,
                owner,
                source: source.to_owned(),
                source_rev,
                expires: Instant::now() + TTL,
                edit: edit_value,
                targets,
            },
        );
        Ok(preview)
    }

    pub(super) fn claim(
        &mut self,
        source: &str,
        rev: u64,
        owner: &str,
        edit: &Value,
    ) -> Result<WorkspaceEditPreview> {
        self.expire();
        let (token, proposal) = self
            .proposals
            .iter()
            .find(|(_, p)| {
                p.source == source
                    && p.source_rev == rev
                    && p.owner.as_deref() == Some(owner)
                    && &p.edit == edit
            })
            .context(
                "workspace edit was not offered by a current LSP response; request it again",
            )?;
        Ok(WorkspaceEditPreview {
            token: token.clone(),
            buffer_id: source.to_owned(),
            files: proposal
                .targets
                .iter()
                .map(|t| WorkspaceEditFile {
                    path: t.path.display().to_string(),
                    operation: match (&t.before, &t.after) {
                        (None, _) => "create",
                        (_, None) => "delete",
                        _ => "edit",
                    }
                    .into(),
                    before: t.before.clone().unwrap_or_default(),
                    after: t.after.clone().unwrap_or_default(),
                    buffer_id: t.buffer_id.clone(),
                    base_rev: t.rev,
                })
                .collect(),
        })
    }

    pub(super) fn cancel_source_owner(&mut self, source: &str, owner: &str) {
        self.proposals
            .retain(|_, p| p.source != source || p.owner.as_deref() != Some(owner));
    }

    pub(super) fn cancel_owner(&mut self, owner: &str) {
        self.proposals
            .retain(|_, p| p.owner.as_deref() != Some(owner));
    }

    pub(super) fn cancel(&mut self, source: &str, owner: &str, token: &str) {
        if self
            .proposals
            .get(token)
            .is_some_and(|p| p.source == source && p.owner.as_deref().is_none_or(|o| o == owner))
        {
            self.proposals.remove(token);
        }
    }

    pub(super) fn apply(
        &mut self,
        editor: &mut Editor,
        tracked: &mut HashMap<String, TrackedBuffer>,
        drafts: &DraftStore,
        source: &str,
        owner: &str,
        token: &str,
    ) -> Result<(String, Vec<WorkspaceBufferUpdate>)> {
        self.expire();
        let proposal = self
            .proposals
            .get(token)
            .context("workspace edit preview expired or was already used")?;
        anyhow::ensure!(
            proposal.source == source && proposal.owner.as_deref().is_none_or(|o| o == owner),
            "workspace edit token belongs to another connection or buffer"
        );
        let proposal = self.proposals.remove(token).expect("checked");
        sync_all_fresh_text(editor, tracked)?;
        anyhow::ensure!(
            tracked.get(source).is_some_and(
                |e| e.rev == proposal.source_rev && e.workspace_id == proposal.workspace
            ),
            "workspace edit source changed since preview"
        );
        let recovery = drafts.protected_paths()?;
        let authority = self
            .authority
            .as_ref()
            .context("workspace edit filesystem authority unavailable")?;
        let prior_active = editor.active_buffer();
        for target in &proposal.targets {
            authority.validate_workspace_edit_path(&target.path)?;
            anyhow::ensure!(
                disk_generation(&target.path)? == target.disk,
                "workspace edit file changed since preview: {}",
                target.path.display()
            );
            if let Some(id) = &target.buffer_id {
                let entry = tracked
                    .get(id)
                    .context("workspace edit buffer closed since preview")?;
                anyhow::ensure!(
                    Some(entry.rev) == target.rev
                        && entry.text == target.before.as_deref().unwrap_or_default()
                        && entry.external.is_none(),
                    "workspace edit buffer changed since preview: {}",
                    target.path.display()
                );
                let fresh_id = BufferId(id.parse()?);
                anyhow::ensure!(
                    editor.active_window().buffers.contains_key(&fresh_id),
                    "Fresh workspace edit buffer is unavailable"
                );
                if let Some((server, version)) = &target.version_check {
                    let actual = editor
                        .active_window()
                        .lsp
                        .get_handles(entry.language.as_deref().unwrap_or(""))
                        .into_iter()
                        .find(|s| &s.name == server)
                        .and_then(|s| s.handle.document_version(&target.path));
                    anyhow::ensure!(
                        actual == Some(*version),
                        "LSP document version changed since preview: {}",
                        target.path.display()
                    );
                }
            } else {
                anyhow::ensure!(
                    !tracked.values().any(|e| e
                        .path
                        .as_deref()
                        .or(e.recovery_path.as_deref())
                        .and_then(canonical_named)
                        .as_ref()
                        == Some(&target.path)),
                    "workspace edit closed file was opened since preview: {}",
                    target.path.display()
                );
                anyhow::ensure!(
                    !recovery
                        .iter()
                        .any(|d| canonical_named(d).as_ref() == Some(&target.path)),
                    "workspace edit target now has a recovery draft: {}",
                    target.path.display()
                );
            }
        }
        // Persist prospective open-buffer drafts first, keeping prior records for rollback.
        let mut prospective = proposal
            .targets
            .iter()
            .filter_map(|t| t.buffer_id.as_ref())
            .map(|id| (id.clone(), tracked[id].clone()))
            .collect::<HashMap<_, _>>();
        let mut saved_drafts = Vec::new();
        let disk_changes = proposal
            .targets
            .iter()
            .filter(|t| t.buffer_id.is_none())
            .map(|t| FileChange {
                path: t.path.clone(),
                expected: t.disk.clone(),
                text: t.after.clone(),
                permissions: t.permissions.clone(),
            })
            .collect::<Vec<_>>();
        let mut applied_buffers = Vec::new();
        let mut file_transaction = None;
        let result = (|| -> Result<Vec<WorkspaceBufferUpdate>> {
            for target in &proposal.targets {
                if let Some(id) = &target.buffer_id {
                    let entry = prospective.get_mut(id).expect("validated");
                    let old = drafts.get(&entry.workspace_id, &entry.draft_id)?;
                    saved_drafts.push((entry.workspace_id.clone(), entry.draft_id.clone(), old));
                    entry.text = target.after.clone().expect("text buffer");
                    entry.dirty = true;
                    checkpoint(drafts, &prospective, id)?;
                }
            }
            file_transaction = Some(workspace_transaction::commit(&disk_changes)?);
            for target in proposal.targets.iter().filter(|t| t.buffer_id.is_some()) {
                authority.validate_workspace_edit_path(&target.path)?;
                anyhow::ensure!(
                    disk_generation(&target.path)? == target.disk,
                    "workspace buffer file changed during publication: {}",
                    target.path.display()
                );
            }
            let mut updates = Vec::new();
            for target in &proposal.targets {
                if let Some(id) = &target.buffer_id {
                    let text = target.after.clone().expect("text buffer");
                    activate_tracked(editor, tracked, id)?;
                    let old_cursor = editor.active_cursors().primary();
                    let old = target.before.as_deref().expect("text");
                    let mut prefix = old
                        .bytes()
                        .zip(text.bytes())
                        .take_while(|(a, b)| a == b)
                        .count();
                    while !old.is_char_boundary(prefix) || !text.is_char_boundary(prefix) {
                        prefix -= 1;
                    }
                    let mut suffix = old.as_bytes()[prefix..]
                        .iter()
                        .rev()
                        .zip(text.as_bytes()[prefix..].iter().rev())
                        .take_while(|(a, b)| a == b)
                        .count();
                    while suffix > 0
                        && (!old.is_char_boundary(old.len() - suffix)
                            || !text.is_char_boundary(text.len() - suffix))
                    {
                        suffix -= 1;
                    }
                    let old_end = old.len() - suffix;
                    let new_end = text.len() - suffix;
                    let map = |offset: usize| {
                        if offset <= prefix {
                            offset
                        } else if offset >= old_end {
                            new_end + offset - old_end
                        } else {
                            new_end
                        }
                    };
                    let selection = ByteSelection {
                        anchor: map(old_cursor.anchor.unwrap_or(old_cursor.position)),
                        head: map(old_cursor.position),
                    };
                    let result = range_edit_buffer(
                        editor,
                        tracked,
                        id,
                        "workspace-edit",
                        target.rev.expect("buffer rev"),
                        vec![RangeEdit {
                            start: 0,
                            end: target.before.as_ref().expect("text").len(),
                            text: text.clone(),
                        }],
                        None,
                        selection,
                    )?;
                    anyhow::ensure!(
                        result.accepted,
                        "workspace range edit unexpectedly rejected"
                    );
                    applied_buffers.push(id.clone());
                    updates.push(WorkspaceBufferUpdate {
                        buffer_id: id.clone(),
                        rev: result.rev,
                        text,
                    });
                }
            }
            Ok(updates)
        })();
        editor.switch_buffer(prior_active);
        match result {
            Ok(updates) => {
                if let Some(transaction) = file_transaction {
                    transaction.finish();
                }
                for target in proposal.targets.iter().filter(|t| t.after.is_none()) {
                    if let Some(uri) = fresh::app::types::file_path_to_lsp_uri(&target.path) {
                        let window = editor.active_window_mut();
                        std::sync::Arc::make_mut(&mut window.stored_diagnostics)
                            .remove(uri.as_str());
                        window.stored_push_diagnostics.remove(uri.as_str());
                        window.stored_pull_diagnostics.remove(uri.as_str());
                        window.diagnostic_result_ids.remove(uri.as_str());
                    }
                }
                Ok((proposal.workspace, updates))
            }
            Err(error) => {
                let mut failures = Vec::new();
                for id in applied_buffers.into_iter().rev() {
                    let entry = tracked.get(&id).expect("tracked").clone();
                    if let Err(e) = action_buffer(
                        editor,
                        tracked,
                        &id,
                        "workspace-edit-rollback",
                        entry.rev,
                        EditorAction::Undo,
                        ByteSelection { anchor: 0, head: 0 },
                    ) {
                        failures.push(format!("buffer {id}: {e}"));
                    }
                }
                if let Some(mut transaction) = file_transaction
                    && let Err(e) = transaction.rollback()
                {
                    failures.push(e.to_string());
                }
                for (workspace, id, old) in saved_drafts {
                    let restored = match old {
                        Some(draft) => drafts.checkpoint(&workspace, draft),
                        None => drafts.discard(&workspace, &id),
                    };
                    if let Err(e) = restored {
                        failures.push(format!("draft {id}: {e}"));
                    }
                }
                editor.switch_buffer(prior_active);
                if failures.is_empty() {
                    Err(error).context("workspace edit aborted; all prior changes rolled back")
                } else {
                    bail!(
                        "workspace edit failed: {error}; rollback incomplete: {}",
                        failures.join("; ")
                    )
                }
            }
        }
    }
}

/// Match target URIs structurally, never by substring (a.rs vs a.rs.bak).
pub(super) fn touched_open_buffers(
    edit: &WorkspaceEdit,
    tracked: &HashMap<String, TrackedBuffer>,
) -> Result<Vec<String>> {
    let mut uris = Vec::new();
    if let Some(changes) = &edit.changes {
        uris.extend(changes.keys());
    }
    if let Some(changes) = &edit.document_changes {
        match changes {
            DocumentChanges::Edits(edits) => {
                uris.extend(edits.iter().map(|e| &e.text_document.uri))
            }
            DocumentChanges::Operations(ops) => {
                for op in ops {
                    match op {
                        DocumentChangeOperation::Edit(e) => uris.push(&e.text_document.uri),
                        DocumentChangeOperation::Op(ResourceOp::Create(o)) => uris.push(&o.uri),
                        DocumentChangeOperation::Op(ResourceOp::Delete(o)) => uris.push(&o.uri),
                        DocumentChangeOperation::Op(ResourceOp::Rename(o)) => {
                            uris.push(&o.old_uri);
                            uris.push(&o.new_uri);
                        }
                    }
                }
            }
        }
    }
    let paths = uris.into_iter().map(uri_path).collect::<Result<Vec<_>>>()?;
    let mut ids = tracked
        .iter()
        .filter(|(_, e)| {
            e.path
                .as_deref()
                .or(e.recovery_path.as_deref())
                .and_then(canonical_named)
                .is_some_and(|p| paths.contains(&p))
        })
        .map(|(id, _)| id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    Ok(ids)
}

/// Edits embedded in a rename or code-action reply, retaining their exact wire value.
pub(super) fn response_edits(feature: LspRequestFeature, value: &Value) -> Vec<Value> {
    match feature {
        LspRequestFeature::Rename if value.is_object() => vec![value.clone()],
        LspRequestFeature::CodeActions => value
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.get("edit").filter(|e| e.is_object()).cloned())
            .collect(),
        LspRequestFeature::CodeActionResolve => value
            .get("edit")
            .filter(|e| e.is_object())
            .cloned()
            .into_iter()
            .collect(),
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utf16_edits_are_strict_and_nonoverlapping() {
        let text = "a😀z\r\nsecond";
        let make = |start, end, replacement: &str| lsp_types::TextEdit {
            range: lsp_types::Range::new(
                lsp_types::Position::new(0, start),
                lsp_types::Position::new(0, end),
            ),
            new_text: replacement.into(),
        };
        assert_eq!(
            apply_text(text, vec![make(1, 3, "x"), make(3, 4, "y")]).unwrap(),
            "axy\r\nsecond"
        );
        assert!(apply_text(text, vec![make(2, 3, "x")]).is_err());
        assert!(apply_text(text, vec![make(1, 4, "x"), make(3, 4, "y")]).is_err());
        assert!(apply_text(text, vec![make(4, 8, "x")]).is_err());
    }
}

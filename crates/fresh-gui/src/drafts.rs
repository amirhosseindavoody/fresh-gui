//! Atomic, daemon-owned editor draft storage. Entries are partitioned by the
//! persistent workspace ID so the same file opened in two workspaces cannot
//! share a recovery copy.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Draft {
    pub draft_id: String,
    pub path: Option<String>,
    pub text: String,
    /// Original bytes as text when opened; used to flag changes at restore.
    pub base_text: Option<String>,
    /// Ordered revisioned edits for a lazily loaded source. The source is
    /// identified by a content generation and replayed only when it matches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paged: Option<PagedDraft>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PagedDraft {
    pub generation: String,
    pub edits: Vec<PagedEditTransaction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PagedEditTransaction {
    pub viewport: fresh_gui_protocol::ByteRange,
    pub edits: Vec<fresh_gui_protocol::RangeEdit>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DraftFile {
    version: u32,
    drafts: Vec<Draft>,
}

#[derive(Debug, Clone)]
pub struct DraftStore {
    root: PathBuf,
}

fn hex_id(value: &str) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Isolate foreground daemons that do not opt into workspace persistence.
pub fn temporary_root(root: &Path) -> PathBuf {
    let key = hex_id(&root.to_string_lossy());
    key.as_bytes().chunks(100).fold(
        std::env::temp_dir().join("fresh-gui-drafts"),
        |path, chunk| path.join(std::str::from_utf8(chunk).expect("hex is ASCII")),
    )
}

pub fn named_draft_id(workspace_id: &str, path: &Path) -> String {
    format!(
        "named:{}:{}",
        hex_id(workspace_id),
        hex_id(&path.to_string_lossy())
    )
}

impl DraftStore {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, workspace_id: &str) -> PathBuf {
        self.root.join(format!("{}.json", hex_id(workspace_id)))
    }

    fn read(&self, workspace_id: &str) -> Result<DraftFile> {
        let path = self.path(workspace_id);
        let read_path = if path.exists() {
            path.clone()
        } else {
            path.with_extension("json.bak")
        };
        match std::fs::read(&read_path) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(DraftFile {
                version: 1,
                drafts: Vec::new(),
            }),
            Err(err) => Err(err).with_context(|| format!("read {}", read_path.display())),
        }
    }

    fn write(&self, workspace_id: &str, state: &DraftFile) -> Result<()> {
        let path = self.path(workspace_id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true).mode(0o700);
            builder
                .create(&self.root)
                .with_context(|| format!("create {}", self.root.display()))?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("create {}", self.root.display()))?;
        let bytes = serde_json::to_vec(state)?;
        let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temp)
            .with_context(|| format!("create {}", temp.display()))?;
        file.write_all(&bytes)
            .with_context(|| format!("write {}", temp.display()))?;
        file.sync_all()
            .with_context(|| format!("sync {}", temp.display()))?;
        drop(file);
        // Publish only a completely written, synced record. Renaming within
        // the directory replaces the old record atomically on supported hosts.
        // The old recovery copy remains authoritative if publishing fails.
        if let Err(err) = std::fs::rename(&temp, &path) {
            let _ = std::fs::remove_file(&temp);
            return Err(err).with_context(|| format!("replace {}", path.display()));
        }
        let _ = std::fs::remove_file(path.with_extension("json.bak"));
        #[cfg(unix)]
        std::fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }

    pub fn checkpoint(&self, workspace_id: &str, draft: Draft) -> Result<()> {
        let mut state = self.read(workspace_id)?;
        if let Some(existing) = state
            .drafts
            .iter_mut()
            .find(|item| item.draft_id == draft.draft_id)
        {
            *existing = draft;
        } else {
            state.drafts.push(draft);
        }
        state.version = 1;
        self.write(workspace_id, &state)
    }

    pub fn list(&self, workspace_id: &str) -> Result<Vec<Draft>> {
        Ok(self.read(workspace_id)?.drafts)
    }

    pub fn get(&self, workspace_id: &str, draft_id: &str) -> Result<Option<Draft>> {
        Ok(self
            .read(workspace_id)?
            .drafts
            .into_iter()
            .find(|draft| draft.draft_id == draft_id))
    }

    pub fn discard(&self, workspace_id: &str, draft_id: &str) -> Result<()> {
        let mut state = self.read(workspace_id)?;
        state.drafts.retain(|draft| draft.draft_id != draft_id);
        if state.drafts.is_empty() {
            let path = self.path(workspace_id);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err).context("remove empty draft store"),
            }
            let _ = std::fs::remove_file(path.with_extension("json.bak"));
            return Ok(());
        }
        self.write(workspace_id, &state)
    }

    pub fn source_changed(draft: &Draft) -> bool {
        if let Some(paged) = draft.paged.as_ref() {
            return draft
                .path
                .as_deref()
                .and_then(|path| crate::editor_worker::disk_generation(Path::new(path)).ok())
                .is_none_or(|current| current.signature != paged.generation);
        }
        match (&draft.path, &draft.base_text) {
            (Some(path), Some(base)) => std::fs::read_to_string(Path::new(path))
                .map(|current| current != *base)
                .unwrap_or(true),
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drafts_are_atomic_persistent_and_workspace_scoped() {
        let temp = std::env::temp_dir().join(format!("draft-store-{}", uuid::Uuid::new_v4()));
        let store = DraftStore::new(temp.clone());
        let draft = Draft {
            draft_id: "untitled-1".into(),
            path: None,
            text: "draft".into(),
            base_text: None,
            paged: None,
        };
        store.checkpoint("workspace-a", draft.clone()).unwrap();
        assert_eq!(store.get("workspace-a", "untitled-1").unwrap(), Some(draft));
        assert!(store.list("workspace-b").unwrap().is_empty());
        let restarted = DraftStore::new(temp.clone());
        assert_eq!(restarted.list("workspace-a").unwrap().len(), 1);
        restarted.discard("workspace-a", "untitled-1").unwrap();
        assert!(restarted.list("workspace-a").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn changed_or_missing_source_is_reviewable() {
        let temp = std::env::temp_dir().join(format!("draft-source-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&temp).unwrap();
        let source = temp.join("source.txt");
        std::fs::write(&source, "changed").unwrap();
        let draft = Draft {
            draft_id: "named".into(),
            path: Some(source.display().to_string()),
            text: "draft".into(),
            base_text: Some("original".into()),
            paged: None,
        };
        assert!(DraftStore::source_changed(&draft));
        std::fs::remove_file(source).unwrap();
        assert!(DraftStore::source_changed(&draft));
        let _ = std::fs::remove_dir_all(temp);
    }

    #[test]
    fn recovery_uses_backup_after_interrupted_replacement() {
        let root = std::env::temp_dir().join(format!("draft-backup-{}", uuid::Uuid::new_v4()));
        let store = DraftStore::new(root.clone());
        let draft = Draft {
            draft_id: "named".into(),
            path: None,
            text: "durable old copy".into(),
            base_text: None,
            paged: None,
        };
        store.checkpoint("workspace", draft.clone()).unwrap();
        let path = store.path("workspace");
        std::fs::rename(&path, path.with_extension("json.bak")).unwrap();
        assert_eq!(store.get("workspace", "named").unwrap(), Some(draft));
        let _ = std::fs::remove_dir_all(root);
    }
}

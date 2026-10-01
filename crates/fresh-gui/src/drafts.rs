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
    /// Encoding selected when this dirty draft was checkpointed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// Line ending selected when this dirty draft was checkpointed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line_ending: Option<String>,
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

    fn paged_path(&self, workspace_id: &str) -> PathBuf {
        self.root
            .join(format!("{}.paged.json", hex_id(workspace_id)))
    }

    fn partition_path(&self, workspace_id: &str, paged: bool) -> PathBuf {
        if paged {
            self.paged_path(workspace_id)
        } else {
            self.path(workspace_id)
        }
    }

    fn read_partition(&self, workspace_id: &str, paged: bool) -> Result<DraftFile> {
        let path = self.partition_path(workspace_id, paged);
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

    fn write_partition(&self, workspace_id: &str, state: &DraftFile, paged: bool) -> Result<()> {
        let path = self.partition_path(workspace_id, paged);
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

    fn read(&self, workspace_id: &str) -> Result<DraftFile> {
        let ordinary = self.read_partition(workspace_id, false)?;
        let paged = self.read_partition(workspace_id, true)?;
        let mut drafts = ordinary.drafts;
        for draft in paged.drafts {
            if let Some(existing) = drafts
                .iter_mut()
                .find(|item| item.draft_id == draft.draft_id)
            {
                *existing = draft;
            } else {
                drafts.push(draft);
            }
        }
        Ok(DraftFile { version: 1, drafts })
    }

    fn remove_from_partition(&self, workspace_id: &str, draft_id: &str, paged: bool) -> Result<()> {
        let mut state = self.read_partition(workspace_id, paged)?;
        let before = state.drafts.len();
        state.drafts.retain(|draft| draft.draft_id != draft_id);
        if state.drafts.len() == before {
            return Ok(());
        }
        if state.drafts.is_empty() {
            let path = self.partition_path(workspace_id, paged);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err).with_context(|| format!("remove {}", path.display())),
            }
            let _ = std::fs::remove_file(path.with_extension("json.bak"));
            Ok(())
        } else {
            self.write_partition(workspace_id, &state, paged)
        }
    }

    pub fn checkpoint(&self, workspace_id: &str, draft: Draft) -> Result<()> {
        let paged = draft.paged.is_some();
        let draft_id = draft.draft_id.clone();
        let mut state = self.read_partition(workspace_id, paged)?;
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
        self.write_partition(workspace_id, &state, paged)?;
        self.remove_from_partition(workspace_id, &draft_id, !paged)
    }

    pub fn list(&self, workspace_id: &str) -> Result<Vec<Draft>> {
        Ok(self.read(workspace_id)?.drafts)
    }

    /// Paths held by recovery records in any workspace. Multi-file operations
    /// must not bypass a dirty buffer simply because its workspace is detached.
    pub(crate) fn protected_paths(&self) -> Result<Vec<PathBuf>> {
        let entries = match std::fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => {
                return Err(error).context("inspect recovery records before workspace edit");
            }
        };
        let mut paths = Vec::new();
        for entry in entries {
            let path = entry?.path();
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("");
            if name.ends_with(".json.bak") {
                if path.with_extension("").exists() {
                    continue;
                }
            } else if !name.ends_with(".json") {
                continue;
            }
            let state: DraftFile = serde_json::from_slice(&std::fs::read(&path)?)
                .with_context(|| format!("inspect recovery record {}", path.display()))?;
            paths.extend(
                state
                    .drafts
                    .into_iter()
                    .filter_map(|draft| draft.path.map(PathBuf::from)),
            );
        }
        Ok(paths)
    }

    pub fn get(&self, workspace_id: &str, draft_id: &str) -> Result<Option<Draft>> {
        Ok(self
            .read(workspace_id)?
            .drafts
            .into_iter()
            .find(|draft| draft.draft_id == draft_id))
    }

    pub fn discard(&self, workspace_id: &str, draft_id: &str) -> Result<()> {
        self.remove_from_partition(workspace_id, draft_id, false)?;
        self.remove_from_partition(workspace_id, draft_id, true)
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
            (Some(path), Some(base)) => {
                let Some(current) = crate::editor_worker::disk_generation(Path::new(path))
                    .ok()
                    .and_then(|generation| generation.text)
                else {
                    return true;
                };
                let normalized_base =
                    fresh::model::buffer::format::normalize_line_endings(base.as_bytes().to_vec());
                String::from_utf8(normalized_base)
                    .map(|base| current != base)
                    .unwrap_or(true)
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protected_paths_include_detached_workspaces_and_backup_recovery() {
        let root = std::env::temp_dir().join(format!("draft-protection-{}", uuid::Uuid::new_v4()));
        let store = DraftStore::new(root.clone());
        let path = root.join("source.txt");
        store
            .checkpoint(
                "detached",
                Draft {
                    draft_id: "dirty".into(),
                    path: Some(path.display().to_string()),
                    text: "draft".into(),
                    base_text: Some("base".into()),
                    encoding: None,
                    line_ending: None,
                    paged: None,
                },
            )
            .unwrap();
        assert_eq!(store.protected_paths().unwrap(), vec![path.clone()]);
        let record = store.path("detached");
        std::fs::rename(&record, record.with_extension("json.bak")).unwrap();
        assert_eq!(store.protected_paths().unwrap(), vec![path]);
        std::fs::write(&record, b"broken").unwrap();
        assert!(
            store.protected_paths().is_err(),
            "unreadable recovery records block destructive workspace edits"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn drafts_are_atomic_persistent_and_workspace_scoped() {
        let temp = std::env::temp_dir().join(format!("draft-store-{}", uuid::Uuid::new_v4()));
        let store = DraftStore::new(temp.clone());
        let draft = Draft {
            draft_id: "untitled-1".into(),
            path: None,
            text: "draft".into(),
            base_text: None,
            encoding: None,
            line_ending: None,
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
            encoding: None,
            line_ending: None,
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
            encoding: None,
            line_ending: None,
            paged: None,
        };
        store.checkpoint("workspace", draft.clone()).unwrap();
        let path = store.path("workspace");
        std::fs::rename(&path, path.with_extension("json.bak")).unwrap();
        assert_eq!(store.get("workspace", "named").unwrap(), Some(draft));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn paged_journals_stay_out_of_the_legacy_workspace_file() {
        let root =
            std::env::temp_dir().join(format!("draft-paged-partition-{}", uuid::Uuid::new_v4()));
        let store = DraftStore::new(root.clone());
        let ordinary = Draft {
            draft_id: "ordinary-buffer".into(),
            path: None,
            text: "ordinary recovery".into(),
            base_text: None,
            encoding: None,
            line_ending: None,
            paged: None,
        };
        let paged = Draft {
            draft_id: "large-buffer".into(),
            path: Some("/tmp/large.txt".into()),
            text: String::new(),
            base_text: None,
            encoding: None,
            line_ending: None,
            paged: Some(PagedDraft {
                generation: "source-generation".into(),
                edits: vec![PagedEditTransaction {
                    viewport: fresh_gui_protocol::ByteRange {
                        start: 1024,
                        len: 4096,
                    },
                    edits: vec![fresh_gui_protocol::RangeEdit {
                        start: 2048,
                        end: 2049,
                        text: "z".into(),
                    }],
                }],
            }),
        };
        store.checkpoint("workspace", ordinary.clone()).unwrap();
        store.checkpoint("workspace", paged.clone()).unwrap();

        let legacy_path = store.path("workspace");
        let legacy_json: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&legacy_path).unwrap()).unwrap();
        let legacy_drafts = legacy_json["drafts"].as_array().unwrap();
        assert_eq!(legacy_drafts.len(), 1);
        assert_eq!(legacy_drafts[0]["draft_id"], "ordinary-buffer");
        assert!(legacy_drafts[0].get("paged").is_none());
        assert!(store.paged_path("workspace").exists());

        assert_eq!(store.list("workspace").unwrap().len(), 2);
        assert_eq!(store.get("workspace", "large-buffer").unwrap(), Some(paged));
        store.discard("workspace", "large-buffer").unwrap();
        assert_eq!(store.list("workspace").unwrap(), vec![ordinary]);
        assert!(legacy_path.exists());
        assert!(!store.paged_path("workspace").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn switching_draft_kind_removes_the_obsolete_partition_entry() {
        let root = std::env::temp_dir().join(format!("draft-kind-switch-{}", uuid::Uuid::new_v4()));
        let store = DraftStore::new(root.clone());
        let paged = Draft {
            draft_id: "same-id".into(),
            path: Some("/tmp/large.txt".into()),
            text: String::new(),
            base_text: None,
            encoding: None,
            line_ending: None,
            paged: Some(PagedDraft {
                generation: "g".into(),
                edits: vec![],
            }),
        };
        store.checkpoint("workspace", paged).unwrap();
        let ordinary = Draft {
            draft_id: "same-id".into(),
            path: None,
            text: "ordinary".into(),
            base_text: None,
            encoding: None,
            line_ending: None,
            paged: None,
        };
        store.checkpoint("workspace", ordinary.clone()).unwrap();
        assert_eq!(store.list("workspace").unwrap(), vec![ordinary]);
        assert!(!store.paged_path("workspace").exists());
        let _ = std::fs::remove_dir_all(root);
    }
}

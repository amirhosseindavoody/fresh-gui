//! Revision-checked transaction for closed workspace files.
//!
//! Fresh's filesystem API saves one buffer atomically, but it does not expose a
//! multi-file transaction. This module supplies that daemon-side boundary while
//! keeping file identity/revision checks aligned with `editor_worker`'s #143
//! `disk_generation` implementation.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::editor_worker::{DiskGeneration, disk_generation};
use fresh_gui_protocol::MAX_SNAPSHOT_BYTES;

/// A desired closed-file state. `None` deletes; `Some` creates or replaces.
pub(crate) struct FileChange {
    pub(crate) path: PathBuf,
    pub(crate) expected: DiskGeneration,
    pub(crate) text: Option<String>,
    pub(crate) permissions: Option<fs::Permissions>,
}

struct Entry {
    path: PathBuf,
    original: DiskGeneration,
    backup: Option<PathBuf>,
    backed_up: bool,
    published: Option<DiskGeneration>,
    published_state: bool,
}

/// A committed transaction whose backups remain available until `finish`.
pub(crate) struct FileTransaction {
    entries: Vec<Entry>,
    finished: bool,
}

impl FileTransaction {
    /// Keep the published files and remove rollback backups.
    pub(crate) fn finish(mut self) {
        for entry in &self.entries {
            if entry.backed_up
                && let Some(backup) = &entry.backup
            {
                if let Err(error) = fs::remove_file(backup) {
                    tracing::warn!(path = %backup.display(), %error, "workspace edit committed but backup cleanup failed");
                }
            }
        }
        self.finished = true;
    }

    /// Restore the original state, refusing to overwrite a concurrent change.
    pub(crate) fn rollback(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        for entry in self.entries.iter_mut().rev() {
            if !entry.published_state && !entry.backed_up {
                continue;
            }
            let current = match disk_generation(&entry.path) {
                Ok(generation) => generation,
                Err(error) => {
                    failures.push(format!(
                        "{}: inspect before rollback: {error}",
                        entry.path.display()
                    ));
                    continue;
                }
            };
            match &entry.backup {
                Some(backup) if entry.backed_up => {
                    match disk_generation(backup) {
                        Ok(generation) if generation == entry.original => {}
                        Ok(_) => {
                            failures.push(format!(
                                "{}: original backup changed; preserving backup at {}",
                                entry.path.display(),
                                backup.display()
                            ));
                            continue;
                        }
                        Err(error) => {
                            failures.push(format!(
                                "{}: cannot verify original backup {}: {error}",
                                entry.path.display(),
                                backup.display()
                            ));
                            continue;
                        }
                    }
                    if !entry.published_state && current == entry.original {
                        if let Err(error) = fs::remove_file(backup) {
                            failures.push(format!(
                                "{}: remove unused backup: {error}",
                                backup.display()
                            ));
                        } else {
                            entry.backup = None;
                            entry.backed_up = false;
                        }
                        continue;
                    }
                    // A missing destination is the expected intermediate state after
                    // moving the original aside, or after a failed publication.
                    if current.signature.starts_with("missing:") {
                        if let Err(error) = fs::hard_link(backup, &entry.path) {
                            failures
                                .push(format!("{}: restore backup: {error}", entry.path.display()));
                        } else {
                            if let Err(error) = fs::remove_file(backup) {
                                failures.push(format!(
                                    "{}: remove restored backup: {error}",
                                    backup.display()
                                ));
                            } else {
                                entry.backup = None;
                                entry.backed_up = false;
                            }
                            entry.published_state = false;
                        }
                    } else if entry.published.as_ref() != Some(&current) {
                        failures.push(format!("{}: changed after publication; preserving external contents and backup", entry.path.display()));
                    } else if let Err(error) = fs::remove_file(&entry.path) {
                        failures.push(format!(
                            "{}: remove published file before restore: {error}",
                            entry.path.display()
                        ));
                    } else if let Err(error) = fs::hard_link(backup, &entry.path) {
                        failures.push(format!("{}: restore backup: {error}", entry.path.display()));
                    } else {
                        // The original is restored even when removing its extra
                        // backup link fails. A retry must recognize that state.
                        entry.published_state = false;
                        entry.published = None;
                        if let Err(error) = fs::remove_file(backup) {
                            failures.push(format!(
                                "{}: remove restored backup: {error}",
                                backup.display()
                            ));
                        } else {
                            entry.backup = None;
                            entry.backed_up = false;
                            entry.published_state = false;
                        }
                    }
                }
                Some(_) => {}
                None if entry.published_state && entry.published.as_ref() == Some(&current) => {
                    match fs::remove_file(&entry.path) {
                        Ok(()) => entry.published_state = false,
                        Err(error) => failures.push(format!(
                            "{}: remove created file: {error}",
                            entry.path.display()
                        )),
                    }
                }
                None if entry.published_state => failures.push(format!(
                    "{}: changed after publication; preserving external contents",
                    entry.path.display()
                )),
                None => {}
            }
        }
        if failures.is_empty() {
            self.finished = true;
            Ok(())
        } else {
            bail!(
                "workspace edit rollback incomplete: {}",
                failures.join("; ")
            )
        }
    }
}

impl Drop for FileTransaction {
    fn drop(&mut self) {
        if !self.finished {
            if let Err(error) = self.rollback() {
                tracing::error!(%error, "workspace file transaction rollback failed");
            }
        }
        // Any remaining backup may be the only copy of original contents.
    }
}

pub(crate) fn commit(changes: &[FileChange]) -> Result<FileTransaction> {
    commit_inner(changes, None)
}

fn commit_inner(
    changes: &[FileChange],
    fail_before_publish: Option<usize>,
) -> Result<FileTransaction> {
    if changes.is_empty() {
        return Ok(FileTransaction {
            entries: Vec::new(),
            finished: false,
        });
    }
    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::with_capacity(changes.len());
    let mut stages = Vec::with_capacity(changes.len());
    let setup = (|| -> Result<()> {
        for change in changes {
            if !seen.insert(change.path.clone()) {
                bail!("duplicate workspace edit path: {}", change.path.display());
            }
            let parent = change
                .path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            if !fs::metadata(parent)
                .with_context(|| format!("inspect parent {}", parent.display()))?
                .is_dir()
            {
                bail!(
                    "workspace edit parent is not a directory: {}",
                    parent.display()
                );
            }
            match fs::symlink_metadata(&change.path) {
                Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => bail!(
                    "workspace edit target is not a regular non-symlink file: {}",
                    change.path.display()
                ),
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("inspect {}", change.path.display()));
                }
            }
            let actual = disk_generation(&change.path)?;
            if actual != change.expected {
                bail!("stale workspace edit target: {}", change.path.display());
            }
            let original_exists =
                actual.text.is_some() || !actual.signature.starts_with("missing:");
            if original_exists && actual.text.is_none() {
                bail!(
                    "workspace edit target exceeds the {} byte snapshot limit: {}",
                    MAX_SNAPSHOT_BYTES,
                    change.path.display()
                );
            }
            if change
                .text
                .as_ref()
                .is_some_and(|text| text.len() > MAX_SNAPSHOT_BYTES)
            {
                bail!(
                    "workspace edit text exceeds the {} byte snapshot limit: {}",
                    MAX_SNAPSHOT_BYTES,
                    change.path.display()
                );
            }
            let backup = if original_exists {
                Some(temp_sibling(&change.path, "backup"))
            } else {
                None
            };
            let stage = if change.text.is_some() {
                Some(temp_sibling(&change.path, "stage"))
            } else {
                None
            };
            stages.push(stage.clone());
            entries.push(Entry {
                path: change.path.clone(),
                original: actual,
                backup,
                backed_up: false,
                published: None,
                published_state: false,
            });
            if let Some(stage) = &stage {
                let mut options = OpenOptions::new();
                options.write(true).create_new(true);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::OpenOptionsExt;
                    options.mode(0o600);
                }
                let mut file = options
                    .open(stage)
                    .with_context(|| format!("stage {}", change.path.display()))?;
                file.write_all(change.text.as_ref().unwrap().as_bytes())?;
                let permissions = change.permissions.clone().or_else(|| {
                    fs::metadata(&change.path)
                        .ok()
                        .map(|metadata| metadata.permissions())
                });
                if let Some(permissions) = permissions {
                    fs::set_permissions(stage, permissions)?;
                }
                file.sync_all()?;
            }
        }
        Ok(())
    })();
    if let Err(error) = setup {
        for stage in stages.into_iter().flatten() {
            let _ = fs::remove_file(stage);
        }
        return Err(error);
    }

    for i in 0..changes.len() {
        if fail_before_publish == Some(i) {
            cleanup_stages(&stages);
            let mut txn = FileTransaction {
                entries,
                finished: false,
            };
            let rollback = txn.rollback();
            return Err(append_rollback_error(
                anyhow!("injected workspace edit publication failure"),
                rollback,
            ));
        }
        let current = match disk_generation(&changes[i].path) {
            Ok(current) => current,
            Err(error) => {
                cleanup_stages(&stages);
                let mut txn = FileTransaction {
                    entries,
                    finished: false,
                };
                let rollback = txn.rollback();
                return Err(append_rollback_error(
                    error.context("recheck workspace edit target before publish"),
                    rollback,
                ));
            }
        };
        if current != changes[i].expected {
            cleanup_stages(&stages);
            let mut txn = FileTransaction {
                entries,
                finished: false,
            };
            let rollback = txn.rollback();
            return Err(append_rollback_error(
                anyhow!(
                    "stale workspace edit target before publish: {}",
                    changes[i].path.display()
                ),
                rollback,
            ));
        }
        match fs::symlink_metadata(&changes[i].path) {
            Ok(meta) if meta.file_type().is_symlink() || !meta.is_file() => {
                cleanup_stages(&stages);
                let mut txn = FileTransaction {
                    entries,
                    finished: false,
                };
                let rollback = txn.rollback();
                return Err(append_rollback_error(
                    anyhow!(
                        "workspace edit target became a symlink or non-file: {}",
                        changes[i].path.display()
                    ),
                    rollback,
                ));
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::NotFound
                    && changes[i].expected.signature.starts_with("missing:") => {}
            Err(error) => {
                cleanup_stages(&stages);
                let mut txn = FileTransaction {
                    entries,
                    finished: false,
                };
                let rollback = txn.rollback();
                return Err(append_rollback_error(
                    anyhow::Error::new(error).context("recheck workspace edit target type"),
                    rollback,
                ));
            }
        }
        if let Some(backup) = entries[i].backup.clone() {
            if let Err(error) = fs::hard_link(&changes[i].path, &backup) {
                cleanup_stages(&stages);
                let mut txn = FileTransaction {
                    entries,
                    finished: false,
                };
                let rollback = txn.rollback();
                return Err(append_rollback_error(
                    anyhow::Error::new(error)
                        .context(format!("preserve {}", changes[i].path.display())),
                    rollback,
                ));
            }
            entries[i].backed_up = true;
            let verify = (|| -> Result<()> {
                anyhow::ensure!(
                    disk_generation(&backup)? == changes[i].expected
                        && disk_generation(&changes[i].path)? == changes[i].expected,
                    "workspace edit target changed while preserving original: {}",
                    changes[i].path.display()
                );
                Ok(())
            })();
            if let Err(error) = verify {
                cleanup_stages(&stages);
                let mut txn = FileTransaction {
                    entries,
                    finished: false,
                };
                let rollback = txn.rollback();
                return Err(append_rollback_error(error, rollback));
            }
            if let Err(error) = fs::remove_file(&changes[i].path) {
                cleanup_stages(&stages);
                let mut txn = FileTransaction {
                    entries,
                    finished: false,
                };
                let rollback = txn.rollback();
                return Err(append_rollback_error(
                    anyhow::Error::new(error).context(format!(
                        "stage original {} for replacement",
                        changes[i].path.display()
                    )),
                    rollback,
                ));
            }
            entries[i].published_state = true;
        }
        if let Some(stage) = &stages[i] {
            if let Err(error) = fs::hard_link(stage, &changes[i].path) {
                cleanup_stages(&stages);
                let mut txn = FileTransaction {
                    entries,
                    finished: false,
                };
                let rollback = txn.rollback();
                return Err(append_rollback_error(
                    anyhow::Error::new(error)
                        .context(format!("publish {}", changes[i].path.display())),
                    rollback,
                ));
            }
            entries[i].published_state = true;
            let _ = fs::remove_file(stage);
        }
        if changes[i].text.is_none() {
            entries[i].published_state = true;
        }
        match disk_generation(&changes[i].path) {
            Ok(generation) => entries[i].published = Some(generation),
            Err(error) => {
                // The replacement is present but cannot be safely identified for rollback.
                // Leave its backup intact and report the recovery path.
                let backup = entries[i]
                    .backup
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| "none (new file)".into());
                cleanup_stages(&stages);
                let mut txn = FileTransaction {
                    entries,
                    finished: false,
                };
                let rollback = txn.rollback();
                return Err(append_rollback_error(error.context(format!("inspect newly published workspace file; original backup retained at {backup}")), rollback));
            }
        }
    }
    Ok(FileTransaction {
        entries,
        finished: false,
    })
}

fn append_rollback_error(primary: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => primary,
        Err(rollback_error) => anyhow!("{primary}; {rollback_error}"),
    }
}

fn cleanup_stages(stages: &[Option<PathBuf>]) {
    for stage in stages.iter().flatten() {
        let _ = fs::remove_file(stage);
    }
}

fn temp_sibling(path: &Path, kind: &str) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!(
        ".{name}.fresh-{kind}-{}-{}",
        std::process::id(),
        uuid::Uuid::new_v4()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "fresh-workspace-txn-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4()
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn change(path: PathBuf, text: Option<&str>) -> FileChange {
        FileChange {
            expected: disk_generation(&path).unwrap(),
            path,
            text: text.map(str::to_owned),
            permissions: None,
        }
    }

    #[test]
    fn stale_second_file_is_detected_before_any_publish() {
        let dir = TempDir::new();
        let first = dir.path("a");
        let second = dir.path("b");
        fs::write(&first, "old-a").unwrap();
        fs::write(&second, "old-b").unwrap();
        let a = change(first.clone(), Some("new-a"));
        let b = change(second.clone(), Some("new-b"));
        fs::write(&second, "external").unwrap();
        assert!(commit(&[a, b]).is_err());
        assert_eq!(fs::read_to_string(first).unwrap(), "old-a");
        assert_eq!(fs::read_to_string(second).unwrap(), "external");
    }

    #[test]
    fn failed_midway_publish_rolls_back_prior_files() {
        let dir = TempDir::new();
        let a_path = dir.path("a");
        let b_path = dir.path("b");
        fs::write(&a_path, "old-a").unwrap();
        fs::write(&b_path, "old-b").unwrap();
        let changes = [
            change(a_path.clone(), Some("new-a")),
            change(b_path.clone(), Some("new-b")),
        ];
        let error = commit_inner(&changes, Some(1)).err().unwrap();
        assert!(!format!("{error:#}").contains("rollback incomplete"));
        assert_eq!(fs::read_to_string(a_path).unwrap(), "old-a");
        assert_eq!(fs::read_to_string(b_path).unwrap(), "old-b");
    }

    #[test]
    fn supports_create_and_delete() {
        let dir = TempDir::new();
        let existing = dir.path("existing");
        let created = dir.path("created");
        fs::write(&existing, "old").unwrap();
        let changes = [
            change(existing.clone(), None),
            change(created.clone(), Some("new")),
        ];
        let transaction = commit(&changes).unwrap();
        assert!(!existing.exists());
        assert_eq!(fs::read_to_string(&created).unwrap(), "new");
        transaction.finish();
    }

    #[test]
    fn replacement_keeps_original_permissions() {
        let dir = TempDir::new();
        let path = dir.path("file");
        fs::write(&path, "old").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        }
        let transaction = commit(&[change(path.clone(), Some("new"))]).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o640
            );
        }
        transaction.finish();
    }

    #[cfg(unix)]
    #[test]
    fn explicit_permissions_override_destination_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let path = dir.path("renamed");
        let mut replacement = change(path.clone(), Some("source contents"));
        replacement.permissions = Some(fs::Permissions::from_mode(0o750));
        let transaction = commit(&[replacement]).unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o750
        );
        transaction.finish();
    }

    #[test]
    fn rollback_preserves_external_change_and_original_backup() {
        let dir = TempDir::new();
        let path = dir.path("file");
        fs::write(&path, "old").unwrap();
        let mut transaction = commit(&[change(path.clone(), Some("new"))]).unwrap();
        fs::write(&path, "external").unwrap();
        let error = transaction.rollback().unwrap_err();
        assert!(format!("{error:#}").contains("preserving external contents"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "external");
        let backups: Vec<_> = fs::read_dir(&dir.0)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|candidate| candidate != &path)
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(fs::read_to_string(&backups[0]).unwrap(), "old");
        // Keep Drop from retrying this deliberately unrecoverable case in the test.
        transaction.finished = true;
    }

    #[test]
    fn rollback_restores_original_files() {
        let dir = TempDir::new();
        let path = dir.path("file");
        fs::write(&path, "old").unwrap();
        let mut transaction = commit(&[change(path.clone(), Some("new"))]).unwrap();
        transaction.rollback().unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "old");
    }
}

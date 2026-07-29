//! Sandboxed filesystem watches via `notify`, forwarding ADE `fs_changed` events.
//!
//! Recursive watches deliberately skip heavyweight trees (`.git`, `target`,
//! `node_modules`, …). A naive `RecursiveMode::Recursive` on the sandbox root
//! walks every subdirectory to install inotify watches — on large workspaces
//! that stalls the ADE WebSocket task for seconds and then floods the same
//! channel as PTY output, which surfaces as slow shell init and command lag.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use fresh_gui_protocol::Message;
use notify::event::{CreateKind, EventKind};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::fs::FsRoot;

/// Directory names that must not be recursively watched. Kept in sync with the
/// host UI `WATCH_IGNORE_DIRS` set — client-side filtering alone is not enough,
/// because inotify registration and event delivery still pay the full cost.
const WATCH_IGNORE_DIRS: &[&str] = &[
    ".git",
    "target",
    ".pixi",
    "node_modules",
    "vendor",
    ".cursor",
    "dist",
    ".hg",
    ".svn",
    "__pycache__",
    ".next",
    ".cache",
    "build",
];

struct WatchEntry {
    /// Keep the watcher alive for the lifetime of the watch. `Option` so the
    /// notify callback can be constructed before the watcher exists, then share
    /// the same slot for late directory follows.
    _watcher: Arc<Mutex<Option<RecommendedWatcher>>>,
}

/// Process-wide watch registry; each watch pushes `Message::FsChanged` on `out`.
#[derive(Clone, Default)]
pub struct FsWatchStore {
    inner: Arc<Mutex<HashMap<String, WatchEntry>>>,
}

impl FsWatchStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn watch(
        &self,
        fs_root: &FsRoot,
        path: PathBuf,
        recursive: bool,
        out: mpsc::UnboundedSender<Message>,
    ) -> Result<(String, String)> {
        if !path.starts_with(fs_root.root_path()) {
            bail!("watch path escapes FS root: {}", path.display());
        }
        let watch_id = Uuid::new_v4().to_string();
        let watch_id_cb = watch_id.clone();
        let root = fs_root.root_path().to_path_buf();

        // Shared before construction so the notify callback can follow newly
        // created directories without RecursiveMode::Recursive (which would
        // reintroduce walks under ignored trees via the notify backend).
        let shared: Arc<Mutex<Option<RecommendedWatcher>>> = Arc::new(Mutex::new(None));
        let shared_cb = Arc::clone(&shared);
        let recursive_cb = recursive;

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                let event = match res {
                    Ok(e) => e,
                    Err(err) => {
                        warn!(error = %err, "fs watch error");
                        return;
                    }
                };

                if recursive_cb {
                    maybe_watch_new_dirs(&shared_cb, &root, &event);
                }

                let paths: Vec<String> = event
                    .paths
                    .iter()
                    .filter(|p| p.starts_with(&root))
                    .filter(|p| !path_is_noisy(p))
                    .map(|p| p.display().to_string())
                    .collect();
                if paths.is_empty() {
                    return;
                }
                debug!(%watch_id_cb, count = paths.len(), "fs changed");
                let _ = out.send(Message::FsChanged {
                    watch_id: watch_id_cb.clone(),
                    paths,
                });
            },
            notify::Config::default(),
        )
        .context("create notify watcher")?;

        let dirs = collect_watch_dirs(&path, recursive);
        debug!(
            path = %path.display(),
            recursive,
            dir_count = dirs.len(),
            "installing fs watches"
        );
        for dir in &dirs {
            if let Err(err) = watcher.watch(dir, RecursiveMode::NonRecursive) {
                // Skip unreadable / vanished dirs; keep covering the rest.
                warn!(path = %dir.display(), error = %err, "skip watch dir");
            }
        }

        *shared.lock().expect("watcher slot lock") = Some(watcher);

        let display = path.display().to_string();
        self.inner.lock().expect("fs watch lock").insert(
            watch_id.clone(),
            WatchEntry {
                _watcher: shared,
            },
        );

        Ok((watch_id, display))
    }

    pub fn unwatch(&self, watch_id: &str) -> bool {
        self.inner
            .lock()
            .expect("fs watch lock")
            .remove(watch_id)
            .is_some()
    }
}

fn maybe_watch_new_dirs(
    shared: &Arc<Mutex<Option<RecommendedWatcher>>>,
    root: &Path,
    event: &Event,
) {
    let is_create_dir = matches!(
        event.kind,
        EventKind::Create(CreateKind::Folder) | EventKind::Create(CreateKind::Any)
    );
    if !is_create_dir {
        return;
    }
    let Ok(mut guard) = shared.lock() else {
        return;
    };
    let Some(watcher) = guard.as_mut() else {
        return;
    };
    for path in &event.paths {
        if !path.starts_with(root) || path_is_noisy(path) {
            continue;
        }
        let is_dir =
            matches!(event.kind, EventKind::Create(CreateKind::Folder)) || path.is_dir();
        if !is_dir {
            continue;
        }
        // Cover only the new directory; deeper children are watched as they appear.
        if let Err(err) = watcher.watch(path, RecursiveMode::NonRecursive) {
            debug!(path = %path.display(), error = %err, "late watch dir failed");
        } else {
            debug!(path = %path.display(), "late watch dir");
        }
    }
}

/// Collect directories to watch. Recursive mode walks the tree but skips ignored
/// names entirely (no inotify watches under `node_modules` / `target` / …).
pub(crate) fn collect_watch_dirs(root: &Path, recursive: bool) -> Vec<PathBuf> {
    if !recursive {
        return vec![root.to_path_buf()];
    }
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        out.push(dir.clone());
        let entries = match std::fs::read_dir(&dir) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            if is_ignored_dir_name(&name) {
                continue;
            }
            let path = entry.path();
            let meta = match std::fs::symlink_metadata(&path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            // Follow only real directories; skip symlinks to avoid cycles / escapes.
            if meta.file_type().is_symlink() || !meta.is_dir() {
                continue;
            }
            stack.push(path);
        }
    }
    out
}

pub(crate) fn path_is_noisy(path: &Path) -> bool {
    path.components().any(|c| match c {
        Component::Normal(name) => is_ignored_dir_name(name),
        _ => false,
    })
}

fn is_ignored_dir_name(name: &OsStr) -> bool {
    WATCH_IGNORE_DIRS.iter().any(|d| name == *d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Instant;

    #[test]
    fn noisy_paths_detected() {
        assert!(path_is_noisy(Path::new("/proj/node_modules/pkg/index.js")));
        assert!(path_is_noisy(Path::new("/proj/target/debug/fresh-gui")));
        assert!(path_is_noisy(Path::new("/proj/.git/objects/aa")));
        assert!(!path_is_noisy(Path::new(
            "/proj/crates/fresh-gui/src/lib.rs"
        )));
    }

    #[test]
    fn collect_skips_ignored_trees() {
        let tmp = std::env::temp_dir().join(format!(
            "fresh-gui-watch-ignore-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::create_dir_all(tmp.join("node_modules/left-pad")).unwrap();
        fs::create_dir_all(tmp.join("target/debug/incremental")).unwrap();
        fs::create_dir_all(tmp.join(".git/objects")).unwrap();
        fs::create_dir_all(tmp.join("crates/foo")).unwrap();

        let dirs = collect_watch_dirs(&tmp, true);
        let as_str: Vec<_> = dirs
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        assert!(as_str.iter().any(|p| p.ends_with("src")));
        assert!(as_str.iter().any(|p| p.ends_with("crates")));
        assert!(as_str.iter().any(|p| p.ends_with("foo")));
        assert!(!as_str.iter().any(|p| p.contains("node_modules")));
        assert!(!as_str.iter().any(|p| p.contains("target")));
        assert!(!as_str.iter().any(|p| p.contains(".git")));

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_non_recursive_is_just_root() {
        let tmp = std::env::temp_dir().join(format!(
            "fresh-gui-watch-nonrec-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("a/b")).unwrap();
        let dirs = collect_watch_dirs(&tmp, false);
        assert_eq!(dirs, vec![tmp.clone()]);
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn collect_ignores_large_noise_quickly() {
        let tmp = std::env::temp_dir().join(format!(
            "fresh-gui-watch-bench-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        // Simulate a fat dependency tree that recursive inotify would otherwise walk.
        for i in 0..200 {
            fs::create_dir_all(tmp.join(format!("node_modules/pkg{i}/lib"))).unwrap();
            fs::create_dir_all(tmp.join(format!("target/debug/incremental/crate{i}"))).unwrap();
        }
        let start = Instant::now();
        let dirs = collect_watch_dirs(&tmp, true);
        let elapsed = start.elapsed();
        assert!(dirs.len() <= 4, "unexpected watch dirs: {dirs:?}");
        assert!(
            elapsed.as_millis() < 500,
            "ignore walk too slow: {elapsed:?}"
        );
        let _ = fs::remove_dir_all(&tmp);
    }
}

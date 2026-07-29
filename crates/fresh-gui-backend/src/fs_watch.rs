//! Sandboxed filesystem watches via `notify`, forwarding ADE `fs_changed` events.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use fresh_gui_protocol::Message;
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::fs::FsRoot;

struct WatchEntry {
    /// Keep the watcher alive for the lifetime of the watch.
    _watcher: RecommendedWatcher,
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
        let mode = if recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        let watch_id = Uuid::new_v4().to_string();
        let watch_id_cb = watch_id.clone();
        let root = fs_root.root_path().to_path_buf();

        let watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                let event = match res {
                    Ok(e) => e,
                    Err(err) => {
                        warn!(error = %err, "fs watch error");
                        return;
                    }
                };
                let paths: Vec<String> = event
                    .paths
                    .iter()
                    .filter(|p| p.starts_with(&root))
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

        let mut watcher = watcher;
        watcher
            .watch(&path, mode)
            .with_context(|| format!("watch {}", path.display()))?;

        let display = path.display().to_string();
        self.inner
            .lock()
            .expect("fs watch lock")
            .insert(watch_id.clone(), WatchEntry { _watcher: watcher });

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

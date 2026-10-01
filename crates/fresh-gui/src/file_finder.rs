//! Daemon-side, gitignore-aware fuzzy workspace file enumeration.
use fresh_gui_protocol::Message;
use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::sync::mpsc;

use fresh::input::fuzzy::FuzzyMatcher;

const MAX_RESULTS: usize = 200;
const MAX_QUERY_BYTES: usize = 4096;

pub struct FinderResult {
    pub paths: Vec<String>,
    pub truncated: bool,
    pub cancelled: bool,
}

pub fn scan(root: PathBuf, query: &str, cancel: Arc<AtomicBool>) -> Result<FinderResult, String> {
    if query.len() > MAX_QUERY_BYTES {
        return Err("file finder query is limited to 4 KiB".into());
    }
    let root =
        std::fs::canonicalize(&root).map_err(|e| format!("cannot resolve workspace root: {e}"))?;
    if !root.is_dir() {
        return Err("workspace root is not a directory".into());
    }
    let mut matcher = FuzzyMatcher::new(query);
    // Keep only the best bounded set while walking. The heap root is the
    // weakest retained candidate (lower score, then lexically later path).
    let mut ranked = BinaryHeap::<Reverse<(i32, Reverse<String>)>>::new();
    let mut total = 0usize;
    for entry in crate::project_search::workspace_walker(&root, false).build() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(FinderResult {
                paths: Vec::new(),
                truncated: false,
                cancelled: true,
            });
        }
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if !entry.file_type().is_some_and(|kind| kind.is_file()) {
            continue;
        }
        let Ok(relative) = entry.path().strip_prefix(&root) else {
            continue;
        };
        let path = relative.to_string_lossy().into_owned();
        let path = if std::path::MAIN_SEPARATOR == '\\' {
            path.replace('\\', "/")
        } else {
            path
        };
        let matched = matcher.match_target(&path);
        if !matched.matched {
            continue;
        }
        total += 1;
        let candidate = Reverse((matched.score, Reverse(path)));
        if ranked.len() < MAX_RESULTS {
            ranked.push(candidate);
        } else if ranked.peek().is_some_and(|worst| candidate > *worst) {
            ranked.pop();
            ranked.push(candidate);
        }
    }
    let mut ranked: Vec<_> = ranked
        .into_iter()
        .map(|Reverse((score, Reverse(path)))| (score, path))
        .collect();
    ranked.sort_unstable_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(FinderResult {
        paths: ranked.into_iter().map(|(_, path)| path).collect(),
        truncated: total > MAX_RESULTS,
        cancelled: false,
    })
}

#[derive(Clone)]
pub struct FinderOutput {
    pub generation: Arc<AtomicBool>,
    pub message: Message,
}
struct ScanGeneration {
    id: String,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct FileFinderSession {
    current: Arc<Mutex<Option<ScanGeneration>>>,
}
impl FileFinderSession {
    pub fn clear(&self) {
        if let Some(scan) = self.current.lock().expect("file finder session").take() {
            scan.cancel.store(true, Ordering::Relaxed);
        }
    }
    pub fn cancel(&self, id: &str) {
        let mut current = self.current.lock().expect("file finder session");
        if current.as_ref().is_some_and(|scan| scan.id == id)
            && let Some(scan) = current.take()
        {
            scan.cancel.store(true, Ordering::Relaxed);
        }
    }
    pub fn is_current(&self, id: &str) -> bool {
        self.current
            .lock()
            .expect("file finder session")
            .as_ref()
            .is_some_and(|scan| scan.id == id)
    }
    pub fn start(&self, id: String, root: PathBuf, query: String, tx: mpsc::Sender<FinderOutput>) {
        self.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.current.lock().expect("file finder session") = Some(ScanGeneration {
            id: id.clone(),
            cancel: cancel.clone(),
        });
        let generation = cancel.clone();
        tokio::spawn(async move {
            let worker_cancel = cancel.clone();
            let result =
                tokio::task::spawn_blocking(move || scan(root, &query, worker_cancel)).await;
            let message = match result {
                Ok(Ok(result)) => Message::FileFinderResults {
                    request_id: id,
                    paths: result.paths,
                    truncated: result.truncated,
                    cancelled: result.cancelled,
                    error: None,
                },
                Ok(Err(error)) => Message::FileFinderResults {
                    request_id: id,
                    paths: Vec::new(),
                    truncated: false,
                    cancelled: false,
                    error: Some(error),
                },
                Err(error) => Message::FileFinderResults {
                    request_id: id,
                    paths: Vec::new(),
                    truncated: false,
                    cancelled: false,
                    error: Some(error.to_string()),
                },
            };
            let _ = tx
                .send(FinderOutput {
                    generation,
                    message,
                })
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn finder_uses_fresh_ranking_and_gitignore_walker() {
        let root = std::env::temp_dir().join(format!("fresh-file-finder-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("nested/deep")).unwrap();
        fs::create_dir_all(root.join("ignored")).unwrap();
        fs::write(root.join(".gitignore"), "ignored/\n").unwrap();
        fs::write(root.join("nested/deep/foo_bar.rs"), "").unwrap();
        fs::write(root.join("ignored/foo_bar.rs"), "").unwrap();
        fs::write(root.join("foobar.rs"), "").unwrap();
        let result = scan(root.clone(), "fbr", Arc::new(AtomicBool::new(false))).unwrap();
        let mut expected = vec!["foobar.rs".to_owned(), "nested/deep/foo_bar.rs".to_owned()];
        let mut matcher = FuzzyMatcher::new("fbr");
        expected.sort_by(|a, b| {
            matcher
                .match_target(b)
                .score
                .cmp(&matcher.match_target(a).score)
                .then_with(|| a.cmp(b))
        });
        assert_eq!(result.paths, expected);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finder_honors_cancellation() {
        let root = std::env::temp_dir().join(format!("fresh-file-finder-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let cancel = Arc::new(AtomicBool::new(true));
        assert!(scan(root.clone(), "file", cancel).unwrap().cancelled);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn finder_matches_multiple_terms_and_caps_ranked_results() {
        let root = std::env::temp_dir().join(format!("fresh-file-finder-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("src")).unwrap();
        for index in 0..250 {
            fs::write(root.join(format!("src/file_{index:03}_name.rs")), "").unwrap();
        }
        fs::write(root.join("src/other.rs"), "").unwrap();
        let result = scan(root.clone(), "src fnm", Arc::new(AtomicBool::new(false))).unwrap();
        assert_eq!(result.paths.len(), MAX_RESULTS);
        assert!(result.truncated);
        assert!(
            result
                .paths
                .iter()
                .all(|path| path.starts_with("src/file_") && path.ends_with("_name.rs"))
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn finder_session_cancels_superseded_and_cleared_generations() {
        let session = FileFinderSession::default();
        let (tx, _rx) = mpsc::channel(1);
        session.start(
            "old".into(),
            PathBuf::from("/missing"),
            "old".into(),
            tx.clone(),
        );
        let old_cancel = session
            .current
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .cancel
            .clone();
        session.start("new".into(), PathBuf::from("/missing"), "new".into(), tx);
        assert!(old_cancel.load(Ordering::Relaxed));
        assert!(!session.is_current("old"));
        assert!(session.is_current("new"));
        let new_cancel = session
            .current
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .cancel
            .clone();
        session.clear();
        assert!(new_cancel.load(Ordering::Relaxed));
        assert!(!session.is_current("new"));
    }
}

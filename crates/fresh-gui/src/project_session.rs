//! Per-connection bounded project search snapshots and cancellation.
use crate::{editor_worker::EditorHandle, project_search::ScanResult};
use fresh_gui_protocol::{
    Message, ProjectReplaceFileResult, ProjectSearchRequest, ProjectSearchSelection, RangeEdit,
};
use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio::sync::mpsc;

/// A private generation guard travels with queued frames. Reusing a wire
/// request ID cannot make a cancelled generation visible again.
pub struct SearchOutput {
    pub generation: Arc<AtomicBool>,
    pub message: Message,
}

struct SearchState {
    id: String,
    workspace: String,
    root: PathBuf,
    cancel: Arc<AtomicBool>,
    result: Option<ScanResult>,
}

#[derive(Default)]
pub struct ProjectSession {
    current: Arc<Mutex<Option<SearchState>>>,
}
impl ProjectSession {
    pub fn is_current(&self, id: &str) -> bool {
        self.current
            .lock()
            .expect("project session")
            .as_ref()
            .is_some_and(|state| state.id == id)
    }
    pub fn clear(&self) {
        if let Some(old) = self.current.lock().expect("project session").take() {
            old.cancel.store(true, Ordering::Relaxed);
        }
    }
    pub fn cancel(&self, id: &str) {
        let mut current = self.current.lock().expect("project session");
        if current.as_ref().is_some_and(|s| s.id == id)
            && let Some(old) = current.take()
        {
            old.cancel.store(true, Ordering::Relaxed);
        }
    }
    pub fn start(
        &self,
        id: String,
        workspace: String,
        root: PathBuf,
        request: ProjectSearchRequest,
        editor: EditorHandle,
        tx: mpsc::Sender<SearchOutput>,
    ) {
        self.clear();
        let cancel = Arc::new(AtomicBool::new(false));
        *self.current.lock().expect("project session") = Some(SearchState {
            id: id.clone(),
            workspace: workspace.clone(),
            root: root.clone(),
            cancel: cancel.clone(),
            result: None,
        });
        let current = self.current.clone();
        tokio::spawn(async move {
            let buffers = match editor.project_snapshots(workspace, root.clone()).await {
                Ok(buffers) => buffers,
                Err(error) => {
                    let _ = tx
                        .send(SearchOutput {
                            generation: cancel.clone(),
                            message: Message::ProjectSearchDone {
                                request_id: id,
                                truncated: false,
                                cancelled: false,
                                warnings: vec![],
                                error: Some(error.to_string()),
                            },
                        })
                        .await;
                    return;
                }
            };
            let stream = tx.clone();
            let stream_id = id.clone();
            let scan_cancel = cancel.clone();
            let stream_generation = cancel.clone();
            let scan = tokio::task::spawn_blocking(move || {
                crate::project_search::scan(root, &request, buffers, scan_cancel, |file| {
                    stream
                        .blocking_send(SearchOutput {
                            generation: stream_generation.clone(),
                            message: Message::ProjectSearchFile {
                                request_id: stream_id.clone(),
                                file,
                            },
                        })
                        .is_ok()
                })
            })
            .await;
            let (truncated, cancelled, warnings, error) = match scan {
                Ok(Ok(result)) => {
                    let summary = (
                        result.truncated,
                        result.cancelled,
                        result.warnings.clone(),
                        None,
                    );
                    let mut state = current.lock().expect("project session");
                    if let Some(state) = state.as_mut().filter(|s| Arc::ptr_eq(&s.cancel, &cancel))
                        && !result.cancelled
                    {
                        state.result = Some(result);
                    }
                    summary
                }
                Ok(Err(error)) => (false, cancel.load(Ordering::Relaxed), vec![], Some(error)),
                Err(error) => (false, false, vec![], Some(error.to_string())),
            };
            let _ = tx
                .send(SearchOutput {
                    generation: cancel.clone(),
                    message: Message::ProjectSearchDone {
                        request_id: id,
                        truncated,
                        cancelled,
                        warnings,
                        error,
                    },
                })
                .await;
        });
    }

    pub async fn replace(
        &self,
        search_id: &str,
        workspace: &str,
        root: &std::path::Path,
        selections: Vec<ProjectSearchSelection>,
        editor: &EditorHandle,
    ) -> Result<
        Vec<(
            ProjectReplaceFileResult,
            Option<crate::editor_worker::ProjectReplaceResult>,
        )>,
        String,
    > {
        // Consume the review snapshot once; retry requires a fresh search.
        let state = {
            let mut guard = self.current.lock().expect("project session");
            let state = guard.as_ref().ok_or("Search expired; run it again")?;
            if state.id != search_id || state.workspace != workspace || state.root != root {
                return Err("Search scope changed; run it again".into());
            }
            if state.result.is_none() {
                return Err("Search is still running or failed".into());
            }
            guard.take().expect("checked")
        };
        state.cancel.store(true, Ordering::Relaxed);
        let result = state.result.expect("checked");
        let mut output = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for selection in selections {
            let id = selection.file_id;
            let applied = async {
                if !seen.insert(id.clone()) {
                    return Err("Duplicate file selection".to_owned());
                }
                let snapshot = result
                    .files
                    .iter()
                    .find(|s| s.result.id == id)
                    .ok_or("Unknown search file")?;
                let mut indices = selection.match_indices;
                indices.sort_unstable();
                indices.dedup();
                if indices.is_empty() {
                    return Err("No matches selected".into());
                }
                let mut edits = Vec::new();
                for index in indices.into_iter().rev() {
                    let matched = snapshot
                        .result
                        .matches
                        .get(index)
                        .ok_or("Unknown match index")?;
                    edits.push(RangeEdit {
                        start: matched.start,
                        end: matched.end,
                        text: matched.replacement.clone(),
                    });
                }
                if let Some(path) = &snapshot.result.path {
                    let source_path = PathBuf::from(path);
                    let scope_root = root.to_path_buf();
                    let confined = tokio::task::spawn_blocking(move || {
                        crate::project_search::confined_path(&scope_root, &source_path)
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                    if confined.is_none() {
                        return Err("Path moved outside workspace".into());
                    }
                }
                editor
                    .project_replace(
                        workspace.to_owned(),
                        root.to_path_buf(),
                        snapshot.result.path.clone(),
                        snapshot.result.buffer_id.clone(),
                        snapshot.result.rev,
                        snapshot.text.clone(),
                        edits,
                    )
                    .await
                    .map_err(|e| e.to_string())
            }
            .await;
            match applied {
                Ok(applied) => output.push((
                    ProjectReplaceFileResult {
                        file_id: id,
                        error: None,
                        buffer: None,
                    },
                    Some(applied),
                )),
                Err(error) => output.push((
                    ProjectReplaceFileResult {
                        file_id: id,
                        error: Some(error),
                        buffer: None,
                    },
                    None,
                )),
            }
        }
        Ok(output)
    }
}
impl Drop for ProjectSession {
    fn drop(&mut self) {
        self.clear();
    }
}

//! In-process Fresh `Editor` on a dedicated `!Send` thread.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result, bail};
use fresh::app::Editor;
use fresh::config::Config;
use fresh::config_io::DirectoryContext;
use fresh::model::event::{BufferId, Event};
use fresh::model::filesystem::{FileSystem, StdFileSystem};
use fresh::types::LspFeature;
use fresh::view::color_support::ColorCapability;
use fresh_gui_protocol::{BufferDiagnostic, SceneBuffer};
use tokio::sync::{mpsc, oneshot};
use tracing::{info, warn};

const MAX_SNAPSHOT_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct OpenedBuffer {
    pub buffer_id: String,
    pub path: String,
    pub language: Option<String>,
    pub rev: u64,
    pub text: String,
}

#[derive(Debug, Clone)]
struct TrackedBuffer {
    path: PathBuf,
    text: String,
    rev: u64,
    dirty: bool,
    language: Option<String>,
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

enum Cmd {
    Open {
        path: PathBuf,
        preview: bool,
        reply: oneshot::Sender<Result<OpenedBuffer>>,
    },
    Edit {
        buffer_id: String,
        base_rev: u64,
        text: String,
        reply: oneshot::Sender<Result<u64>>,
    },
    Save {
        buffer_id: String,
        base_rev: u64,
        reply: oneshot::Sender<Result<(String, u64)>>,
    },
    Close {
        buffer_id: String,
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
}

/// Handle to the editor thread. Cloneable; commands are serialized on the worker.
#[derive(Clone)]
pub struct EditorHandle {
    tx: mpsc::UnboundedSender<Cmd>,
}

impl EditorHandle {
    /// Spawn the Fresh editor on a dedicated OS thread. Returns `None` if init fails.
    pub fn spawn(working_dir: PathBuf, gui_config: crate::config::Config) -> Option<Self> {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();
        let (tx, rx) = mpsc::unbounded_channel::<Cmd>();
        let dir_for_log = working_dir.clone();

        thread::Builder::new()
            .name("fresh-editor".into())
            .spawn(move || match build_editor(&working_dir, &gui_config) {
                Ok(editor) => {
                    let _ = ready_tx.send(Ok(()));
                    run_loop(editor, rx);
                }
                Err(err) => {
                    let _ = ready_tx.send(Err(err));
                }
            })
            .ok()?;

        match ready_rx.recv() {
            Ok(Ok(())) => {
                info!(dir = %dir_for_log.display(), "Fresh editor worker ready");
                Some(Self { tx })
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

    pub async fn open(&self, path: PathBuf, preview: bool) -> Result<OpenedBuffer> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Open {
                path,
                preview,
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

    pub async fn save(&self, buffer_id: String, base_rev: u64) -> Result<(String, u64)> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Save {
                buffer_id,
                base_rev,
                reply: reply_tx,
            })
            .map_err(|_| anyhow::anyhow!("editor worker stopped"))?;
        reply_rx
            .await
            .map_err(|_| anyhow::anyhow!("editor worker dropped reply"))?
    }

    pub async fn close(&self, buffer_id: String) -> Result<()> {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx
            .send(Cmd::Close {
                buffer_id,
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
    // Fresh owns LSP lifecycle; only the servers explicitly configured for
    // fresh-gui are started on this daemon host.
    cfg.lsp = gui_config.lsp.clone();
    cfg.lsp_enabled = !cfg.lsp.is_empty();
    apply_language_associations(&mut cfg, &gui_config.languages);
    for language in cfg.lsp.keys() {
        if let Some(config) = cfg.languages.get_mut(language) {
            // The GUI's Format action should use the configured LSP server,
            // not Fresh's unrelated built-in external formatter command.
            config.formatter = None;
        }
    }
    let fs: Arc<dyn FileSystem + Send + Sync> = Arc::new(StdFileSystem);
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

fn run_loop(mut editor: Editor, mut rx: mpsc::UnboundedReceiver<Cmd>) {
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

    let mut tracked: HashMap<String, TrackedBuffer> = HashMap::new();

    // Borrow editor/tracked into the future (no `async move`) so `Editor` is
    // dropped *after* `block_on` returns — Fresh's Drop must not run while a
    // Tokio runtime is still in an async teardown path.
    rt.block_on(async {
        let mut ticks = tokio::time::interval(std::time::Duration::from_millis(50));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let cmd = tokio::select! {
                _ = ticks.tick() => {
                    if let Err(err) = fresh::app::editor_tick(&mut editor, || Ok(())) {
                        warn!(%err, "Fresh editor tick failed");
                    }
                    continue;
                }
                cmd = rx.recv() => match cmd { Some(cmd) => cmd, None => break },
            };
            match cmd {
                Cmd::Open {
                    path,
                    preview,
                    reply,
                } => {
                    let result = open_buffer(&mut editor, &mut tracked, &path, preview);
                    let _ = reply.send(result);
                }
                Cmd::Edit {
                    buffer_id,
                    base_rev,
                    text,
                    reply,
                } => {
                    let result =
                        edit_buffer(&mut editor, &mut tracked, &buffer_id, base_rev, &text);
                    let _ = reply.send(result);
                }
                Cmd::Save {
                    buffer_id,
                    base_rev,
                    reply,
                } => {
                    let result = save_buffer(&mut editor, &mut tracked, &buffer_id, base_rev);
                    let _ = reply.send(result);
                }
                Cmd::Close { buffer_id, reply } => {
                    let result = close_buffer(&mut editor, &mut tracked, &buffer_id);
                    let _ = reply.send(result);
                }
                Cmd::LspGet {
                    buffer_id,
                    known_rev,
                    reply,
                } => {
                    let _ = reply.send(lsp_state(&mut editor, &mut tracked, &buffer_id, known_rev));
                }
                Cmd::Format {
                    buffer_id,
                    base_rev,
                    reply,
                } => {
                    let result =
                        format_buffer(&mut editor, &mut tracked, &buffer_id, base_rev).await;
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
                    apply_language_associations(&mut fresh_config, &config.languages);
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
                Cmd::Scene { reply } => {
                    let active = editor.active_buffer().0.to_string();
                    let buffers = tracked
                        .iter()
                        .map(|(id, t)| SceneBuffer {
                            buffer_id: id.clone(),
                            path: t.path.display().to_string(),
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

fn apply_language_associations(
    config: &mut Config,
    associations: &HashMap<String, fresh::config::LanguageConfig>,
) {
    for (name, override_config) in associations {
        if let Some(existing) = config.languages.get_mut(name) {
            if !override_config.extensions.is_empty() {
                existing.extensions = override_config.extensions.clone();
            }
            if !override_config.filenames.is_empty() {
                existing.filenames = override_config.filenames.clone();
            }
        } else {
            config
                .languages
                .insert(name.clone(), override_config.clone());
        }
    }
}

fn open_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    path: &Path,
    preview: bool,
) -> Result<OpenedBuffer> {
    if !path.is_file() {
        bail!("not a file: {}", path.display());
    }
    if crate::binary::is_binary_file(path)? {
        return Err(anyhow::Error::new(crate::binary::BinaryFile {
            path: path.to_path_buf(),
        }));
    }
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    if meta.len() as usize > MAX_SNAPSHOT_BYTES {
        bail!(
            "file too large for snapshot ({} bytes; max {MAX_SNAPSHOT_BYTES})",
            meta.len()
        );
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

    let language = Some(editor.active_state().language.clone());
    let text = editor
        .active_state()
        .buffer
        .to_string()
        .context("buffer has unloaded regions (large-file mode); cannot snapshot")?;

    if text.len() > MAX_SNAPSHOT_BYTES {
        bail!(
            "snapshot too large ({} bytes; max {MAX_SNAPSHOT_BYTES})",
            text.len()
        );
    }

    let id = buffer_id.0.to_string();
    let rev = tracked.get(&id).map(|t| t.rev).unwrap_or(0);
    tracked.insert(
        id.clone(),
        TrackedBuffer {
            path: path.to_path_buf(),
            text: text.clone(),
            rev,
            dirty: false,
            language: language.clone(),
        },
    );

    Ok(OpenedBuffer {
        buffer_id: id,
        path: path.display().to_string(),
        language,
        rev,
        text,
    })
}

fn activate_tracked(
    editor: &mut Editor,
    tracked: &HashMap<String, TrackedBuffer>,
    buffer_id: &str,
) -> Result<()> {
    let Some(entry) = tracked.get(buffer_id) else {
        bail!("unknown buffer_id {buffer_id}");
    };
    // open_file switches to an already-open buffer when the path matches.
    editor
        .open_file(&entry.path)
        .with_context(|| format!("activate {}", entry.path.display()))?;
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
    if text.len() > MAX_SNAPSHOT_BYTES {
        bail!(
            "edit too large ({} bytes; max {MAX_SNAPSHOT_BYTES})",
            text.len()
        );
    }
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
        if !old.is_empty() {
            editor.log_and_apply_event(&Event::Delete {
                range: 0..old.len(),
                deleted_text: old,
                cursor_id,
            });
        }
        if !text.is_empty() {
            editor.log_and_apply_event(&Event::Insert {
                position: 0,
                text: text.to_owned(),
                cursor_id,
            });
        }
    }
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    entry.text = text.to_owned();
    entry.rev += 1;
    entry.dirty = true;
    Ok(entry.rev)
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
    let text = editor
        .active_window()
        .buffers
        .get(&BufferId(id))
        .and_then(|state| state.buffer.to_string())
        .context("Fresh buffer unavailable")?;
    let entry = tracked
        .get_mut(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?;
    // Fresh may apply asynchronous LSP formatting. Advance the ADE revision
    // when that happens so the host receives an authoritative new snapshot.
    if entry.text == text {
        return Ok(None);
    }
    entry.text = text.clone();
    entry.rev += 1;
    entry.dirty = true;
    Ok(Some(text))
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
    let diagnostics = fresh::services::lsp::manager::path_to_uri(&path)
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
        let id: usize = buffer_id.parse().context("invalid buffer_id")?;
        editor
            .active_window()
            .buffers
            .get(&BufferId(id))
            .and_then(|state| state.buffer.to_string())
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
) -> Result<FormatState> {
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

fn save_buffer(
    editor: &mut Editor,
    tracked: &mut HashMap<String, TrackedBuffer>,
    buffer_id: &str,
    base_rev: u64,
) -> Result<(String, u64)> {
    let current = tracked
        .get(buffer_id)
        .with_context(|| format!("unknown buffer_id {buffer_id}"))?
        .rev;
    if current != base_rev {
        bail!("revision conflict: base_rev={base_rev} current={current}");
    }
    activate_tracked(editor, tracked, buffer_id)?;
    editor.save().context("Editor::save")?;
    let entry = tracked.get_mut(buffer_id).expect("tracked");
    entry.dirty = false;
    // Bump rev so peers know disk matches this generation.
    entry.rev += 1;
    Ok((entry.path.display().to_string(), entry.rev))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    // A stdio LSP exercised through Fresh and the ADE worker, including
    // two Python servers and a TOML server. No developer-installed binary is needed.
    const FAKE_LSP: &str = r#"#!/usr/bin/env python3
import json, sys
source = sys.argv[1]
log_path = sys.argv[0] + '.' + source + '.log'
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
while True:
    msg = read()
    if msg is None: break
    method = msg.get('method')
    if method:
        with open(log_path, 'a') as log: log.write(method + '\n')
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':msg['id'],'result':{
            'capabilities':{'textDocumentSync':1,'documentFormattingProvider':True}}})
    elif method in ('textDocument/didOpen', 'textDocument/didChange'):
        uri = msg['params']['textDocument']['uri']
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

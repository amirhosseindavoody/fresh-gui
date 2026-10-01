//! End-to-end checks for LSP workspace edits through the daemon worker.
use super::*;
use fresh_gui_protocol::{LspRequest, LspRequestFeature};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const WORKSPACE_LSP: &str = r#"#!/usr/bin/env python3
import json, pathlib, sys
paths = [pathlib.Path(p).resolve().as_uri() for p in sys.argv[1:]]
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
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':msg['id'],'result':{'capabilities':{
            'textDocumentSync':2, 'renameProvider':{'prepareProvider':True}}}})
    elif method == 'textDocument/prepareRename':
        send({'jsonrpc':'2.0','id':msg['id'],'result':{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':3}},'placeholder':'old'}})
    elif method == 'textDocument/rename':
        new_name = msg['params']['newName']
        changes = {uri:[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':3}},'newText':new_name}] for uri in paths}
        send({'jsonrpc':'2.0','id':msg['id'],'result':{'changes':changes}})
    elif method == 'shutdown':
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
    elif 'id' in msg:
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
"#;

const RESOURCE_LSP: &str = r##"#!/usr/bin/env python3
import json, pathlib, sys
source, open_target, create, old, new, delete, symlink, outside, recovery = [pathlib.Path(p).absolute().as_uri() for p in sys.argv[1:]]
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
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':msg['id'],'result':{'capabilities':{'textDocumentSync':2,'renameProvider':True}}})
    elif method == 'textDocument/rename':
        if msg['params']['newName'] == 'resources':
            result = {'documentChanges':[
                {'textDocument':{'uri':source,'version':None},'edits':[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':6}},'newText':'changed'}]},
                {'kind':'create','uri':create}, {'kind':'rename','oldUri':old,'newUri':new}, {'kind':'delete','uri':delete}]}
        elif msg['params']['newName'] == 'symlink':
            result = {'changes':{symlink:[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':4}},'newText':'bad'}]}}
        elif msg['params']['newName'] == 'outside':
            result = {'changes':{outside:[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':4}},'newText':'bad'}]}}
        elif msg['params']['newName'] == 'recovery':
            result = {'changes':{recovery:[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':4}},'newText':'bad'}]}}
        else:
            result = {'documentChanges':[{'kind':'delete','uri':open_target}]}
        send({'jsonrpc':'2.0','id':msg['id'],'result':result})
    elif method == 'shutdown':
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
    elif 'id' in msg:
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
"##;

const ACTION_LSP: &str = r##"#!/usr/bin/env python3
import json, pathlib, sys
source = pathlib.Path(sys.argv[1]).resolve().as_uri()
name = sys.argv[2]
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
    if method == 'initialize':
        send({'jsonrpc':'2.0','id':msg['id'],'result':{'capabilities':{
            'textDocumentSync':2,'codeActionProvider':{'resolveProvider':True},
            'executeCommandProvider':{'commands':['fresh.test']}}}})
    elif method == 'textDocument/codeAction':
        action = {'title':'Fix '+name,'kind':'quickfix','data':{'provider':name}}
        send({'jsonrpc':'2.0','id':msg['id'],'result':[action]})
    elif method == 'codeAction/resolve':
        action = msg['params']
        action['edit'] = {'changes':{source:[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':3}},'newText':name.lower()}]}}
        send({'jsonrpc':'2.0','id':msg['id'],'result':action})
    elif method == 'workspace/executeCommand':
        if msg['params']['command'] == 'fresh.staleVersion':
            edit = {'documentChanges':[{'textDocument':{'uri':source,'version':-1},'edits':[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':3}},'newText':'bad'}]}]}
        else:
            edit = {'changes':{source:[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':3}},'newText':'cmd'}]}}
        send({'jsonrpc':'2.0','id':700,'method':'workspace/applyEdit','params':{'label':'Command edit','edit':edit}})
        read()  # Fresh replies to workspace/applyEdit before executeCommand completes.
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
    elif method == 'shutdown':
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
    elif 'id' in msg:
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
"##;

const PREPARE_FALLBACK_LSP: &str = r##"#!/usr/bin/env python3
import json, pathlib, sys
source = pathlib.Path(sys.argv[1]).resolve().as_uri()
name, log_path = sys.argv[2], sys.argv[3]
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
        send({'jsonrpc':'2.0','id':msg['id'],'result':{'capabilities':{'textDocumentSync':2,'renameProvider':True}}})
    elif method == 'textDocument/prepareRename':
        send({'jsonrpc':'2.0','id':msg['id'],'error':{'code':-32601,'message':'Method not found'}})
    elif method == 'textDocument/rename':
        changes = {source:[{'range':{'start':{'line':0,'character':0},'end':{'line':0,'character':3}},'newText':name.lower()}]}
        send({'jsonrpc':'2.0','id':msg['id'],'result':{'changes':changes}})
    elif method == 'shutdown':
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
    elif 'id' in msg:
        send({'jsonrpc':'2.0','id':msg['id'],'result':None})
"##;

fn temp_root(label: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fresh-gui-workspace-edit-{label}-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&root).unwrap();
    root
}

fn fake_rename_config(script: &Path, paths: &[&Path]) -> crate::config::Config {
    let mut args = vec![script.display().to_string()];
    args.extend(paths.iter().map(|path| path.display().to_string()));
    crate::config::Config::parse(
        &json!({"lsp":{"python":{"name":"WorkspaceEdits","command":"python3","args":args,"only_features":["rename"]}}}).to_string(),
    )
    .unwrap()
}

fn rename_request_named(
    buffer_id: String,
    base_rev: u64,
    request_id: u64,
    name: &str,
) -> LspRequest {
    LspRequest {
        request_id,
        buffer_id,
        view_id: "test-owner:view".into(),
        base_rev,
        offset: 0,
        feature: LspRequestFeature::Rename,
        trigger_character: None,
        item: Some(json!({"newName":name})),
        server: Some("WorkspaceEdits".into()),
    }
}

async fn await_lsp_result(
    results: &mut tokio::sync::broadcast::Receiver<LspResult>,
    request_id: u64,
) -> LspResult {
    tokio::time::timeout(Duration::from_secs(8), async {
        loop {
            match results.recv().await {
                Ok(result) if result.request_id == request_id => return result,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(error) => panic!("LSP result channel closed: {error}"),
            }
        }
    })
    .await
    .expect("LSP response timed out")
}

async fn lsp_rename_preview(
    editor: &EditorHandle,
    source: &OpenedBuffer,
) -> (LspResult, fresh_gui_protocol::WorkspaceEditPreview) {
    lsp_rename_preview_named(editor, source, 147, "new").await
}

async fn lsp_rename_preview_named(
    editor: &EditorHandle,
    source: &OpenedBuffer,
    request_id: u64,
    new_name: &str,
) -> (LspResult, fresh_gui_protocol::WorkspaceEditPreview) {
    let prepared = lsp_response(
        editor,
        LspRequest {
            request_id: request_id - 1,
            buffer_id: source.buffer_id.clone(),
            view_id: "test-owner:view".into(),
            base_rev: source.rev,
            offset: 0,
            feature: LspRequestFeature::PrepareRename,
            trigger_character: None,
            item: None,
            server: Some("WorkspaceEdits".into()),
        },
    )
    .await;
    assert_eq!(prepared.responses.len(), 1);
    assert_eq!(prepared.responses[0].result["placeholder"], "old");
    lsp_edit_preview(
        editor,
        source,
        rename_request_named(source.buffer_id.clone(), source.rev, request_id, new_name),
    )
    .await
}

async fn lsp_edit_preview(
    editor: &EditorHandle,
    source: &OpenedBuffer,
    request: LspRequest,
) -> (LspResult, fresh_gui_protocol::WorkspaceEditPreview) {
    let result = lsp_response(editor, request).await;
    assert!(
        result.status.is_none(),
        "LSP edit failed: {:?}",
        result.status
    );
    assert!(
        !result.stale,
        "LSP response should match its source revision"
    );
    let edit = result
        .responses
        .first()
        .expect("fake server response")
        .result
        .clone();
    let preview = editor
        .prepare_workspace_edit(
            source.buffer_id.clone(),
            source.rev,
            "test-owner".into(),
            edit,
        )
        .await
        .unwrap();
    (result, preview)
}

async fn lsp_response(editor: &EditorHandle, request: LspRequest) -> LspResult {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    for attempt in 0..80_u64 {
        let mut results = editor.subscribe_lsp();
        let request_id = request.request_id + attempt;
        let mut next = request.clone();
        next.request_id = request_id;
        editor.request_lsp(next).unwrap();
        let result = tokio::time::timeout_at(deadline, async {
            loop {
                match results.recv().await {
                    Ok(result) if result.request_id == request_id => return result,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(error) => panic!("LSP result channel closed: {error}"),
                }
            }
        })
        .await
        .expect("LSP server initialization/request timed out");
        if result.status.as_deref() == Some("no eligible language server") {
            tokio::time::sleep(Duration::from_millis(100)).await;
            continue;
        }
        return result;
    }
    panic!("LSP server did not become eligible before the deadline")
}

#[test]
fn lsp_rename_previews_open_and_closed_files_and_applies_as_buffer_undo_groups() {
    if !fresh::services::lsp::command_exists("python3") {
        return;
    }
    let root = temp_root("rename");
    let recovery = root.join("recovery");
    let script = root.join("fake_lsp.py");
    std::fs::write(&script, WORKSPACE_LSP).unwrap();
    let source = root.join("source.py");
    let other = root.join("other.py");
    let closed = root.join("closed.py");
    std::fs::write(&source, "old = 1\n").unwrap();
    std::fs::write(&other, "old = 2\n").unwrap();
    std::fs::write(&closed, "old = 3\n").unwrap();
    let editor = EditorHandle::spawn_with_recovery_dir(
        root.clone(),
        fake_rename_config(&script, &[&source, &other, &closed]),
        recovery,
    )
    .unwrap();
    editor.set_workspace_authority(crate::fs::FsRoot::new(root.clone()).unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let opened_source = editor.open(source.clone(), false).await.unwrap();
        let opened_other = editor.open(other.clone(), false).await.unwrap();
        let source_dirty = editor
            .edit(
                opened_source.buffer_id.clone(),
                opened_source.rev,
                "old = 10\n".into(),
            )
            .await
            .unwrap();
        let other_dirty = editor
            .edit(
                opened_other.buffer_id.clone(),
                opened_other.rev,
                "old = 20\n".into(),
            )
            .await
            .unwrap();
        let source_state = editor.sync(opened_source.buffer_id.clone()).await.unwrap();
        let other_state = editor.sync(opened_other.buffer_id.clone()).await.unwrap();
        assert_eq!(source_state.rev, source_dirty);
        assert_eq!(other_state.rev, other_dirty);

        let (_, preview) = lsp_rename_preview(
            &editor,
            &OpenedBuffer {
                rev: source_state.rev,
                text: source_state.text.clone(),
                ..opened_source.clone()
            },
        )
        .await;
        assert_eq!(preview.files.len(), 3);
        assert!(
            preview
                .files
                .iter()
                .any(|file| file.buffer_id.as_deref() == Some(&opened_source.buffer_id))
        );
        assert!(
            preview
                .files
                .iter()
                .any(|file| file.buffer_id.as_deref() == Some(&opened_other.buffer_id))
        );
        assert!(
            preview
                .files
                .iter()
                .any(|file| file.path == closed.display().to_string() && file.buffer_id.is_none())
        );

        let token = preview.token.clone();
        let wrong_owner = editor
            .apply_workspace_edit(
                opened_source.buffer_id.clone(),
                "another-owner".into(),
                token.clone(),
            )
            .await
            .unwrap_err();
        assert!(format!("{wrong_owner:#}").contains("another connection"));
        let updates = editor
            .apply_workspace_edit(
                opened_source.buffer_id.clone(),
                "test-owner".into(),
                token.clone(),
            )
            .await
            .unwrap();
        assert_eq!(updates.len(), 2);
        assert!(updates.iter().any(
            |update| update.buffer_id == opened_source.buffer_id && update.text == "new = 10\n"
        ));
        assert!(updates.iter().any(
            |update| update.buffer_id == opened_other.buffer_id && update.text == "new = 20\n"
        ));
        assert_eq!(std::fs::read_to_string(&closed).unwrap(), "new = 3\n");
        let consumed = editor
            .apply_workspace_edit(opened_source.buffer_id.clone(), "test-owner".into(), token)
            .await
            .unwrap_err();
        assert!(format!("{consumed:#}").contains("expired or was already used"));

        let current = editor.sync(opened_source.buffer_id.clone()).await.unwrap();
        let refreshed_source = OpenedBuffer {
            rev: current.rev,
            text: current.text.clone(),
            ..opened_source.clone()
        };
        let (_, cancelled) =
            lsp_rename_preview_named(&editor, &refreshed_source, 148, "next").await;
        editor
            .cancel_workspace_edit(
                opened_source.buffer_id.clone(),
                "test-owner".into(),
                cancelled.token.clone(),
            )
            .unwrap();
        let cancelled = editor
            .apply_workspace_edit(
                opened_source.buffer_id.clone(),
                "test-owner".into(),
                cancelled.token,
            )
            .await
            .unwrap_err();
        assert!(format!("{cancelled:#}").contains("expired or was already used"));

        // Each workspace edit is one Fresh undo group per buffer, preserving
        // the dirty draft that existed before the rename.
        let source_after = updates
            .iter()
            .find(|update| update.buffer_id == opened_source.buffer_id)
            .unwrap();
        let source_undo = editor
            .action(
                opened_source.buffer_id.clone(),
                "test-owner:view".into(),
                source_after.rev,
                EditorAction::Undo,
                ByteSelection { anchor: 0, head: 0 },
            )
            .await
            .unwrap();
        assert_eq!(source_undo.text, "old = 10\n");
        let other_after = updates
            .iter()
            .find(|update| update.buffer_id == opened_other.buffer_id)
            .unwrap();
        let other_undo = editor
            .action(
                opened_other.buffer_id.clone(),
                "test-owner:view".into(),
                other_after.rev,
                EditorAction::Undo,
                ByteSelection { anchor: 0, head: 0 },
            )
            .await
            .unwrap();
        assert_eq!(other_undo.text, "old = 20\n");
        // A second undo reaches the earlier unrelated draft edit, proving the
        // rename did not split its own replacement into multiple undo steps.
        assert_eq!(
            editor
                .action(
                    opened_source.buffer_id.clone(),
                    "test-owner:view".into(),
                    source_undo.rev,
                    EditorAction::Undo,
                    source_undo.selection,
                )
                .await
                .unwrap()
                .text,
            "old = 1\n"
        );
        editor.close(opened_source.buffer_id).await.unwrap();
        editor.close(opened_other.buffer_id).await.unwrap();
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn prepare_rename_method_not_found_uses_default_behavior_and_keeps_server_route() {
    if !fresh::services::lsp::command_exists("python3") {
        return;
    }
    let root = temp_root("prepare-fallback");
    let script = root.join("fake_lsp.py");
    std::fs::write(&script, PREPARE_FALLBACK_LSP).unwrap();
    let source = root.join("sample.py");
    let legacy_log = root.join("legacy.log");
    let other_log = root.join("other.log");
    std::fs::write(&source, "old = 1\n").unwrap();
    let server = |name: &str, log: &Path| {
        json!({
            "name":name,
            "command":"python3",
            "args":[script.display().to_string(),source.display().to_string(),name,log.display().to_string()],
            "only_features":["rename"]
        })
    };
    let config = crate::config::Config::parse(
        &json!({"lsp":{"python":[server("LegacyRename", &legacy_log), server("OtherRename", &other_log)]}}).to_string(),
    ).unwrap();
    let editor =
        EditorHandle::spawn_with_recovery_dir(root.clone(), config, root.join("recovery")).unwrap();
    editor.set_workspace_authority(crate::fs::FsRoot::new(root.clone()).unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let opened = editor.open(source.clone(), false).await.unwrap();
        let prepared = lsp_response(
            &editor,
            LspRequest {
                request_id: 601,
                buffer_id: opened.buffer_id.clone(),
                view_id: "test-owner:view".into(),
                base_rev: opened.rev,
                offset: 0,
                feature: LspRequestFeature::PrepareRename,
                trigger_character: None,
                item: None,
                server: None,
            },
        )
        .await;
        assert_eq!(prepared.responses.len(), 1, "prepareRename is exclusive");
        let selected_server = prepared.responses[0].server.clone();
        assert!(matches!(
            selected_server.as_str(),
            "LegacyRename" | "OtherRename"
        ));
        assert_eq!(
            prepared.responses[0].result,
            json!({"defaultBehavior":true})
        );

        let renamed = lsp_response(
            &editor,
            LspRequest {
                request_id: 602,
                buffer_id: opened.buffer_id.clone(),
                view_id: "test-owner:view".into(),
                base_rev: opened.rev,
                offset: 0,
                feature: LspRequestFeature::Rename,
                trigger_character: None,
                item: Some(json!({"newName":"next"})),
                server: Some(selected_server.clone()),
            },
        )
        .await;
        assert_eq!(renamed.responses.len(), 1);
        assert_eq!(renamed.responses[0].server, selected_server);
        let edit = renamed.responses[0].result.clone();
        let preview = editor
            .prepare_workspace_edit(
                opened.buffer_id.clone(),
                opened.rev,
                "test-owner".into(),
                edit,
            )
            .await
            .unwrap();
        let applied = editor
            .apply_workspace_edit(opened.buffer_id.clone(), "test-owner".into(), preview.token)
            .await
            .unwrap();
        assert_eq!(
            applied[0].text,
            format!("{} = 1\n", selected_server.to_lowercase())
        );

        let legacy_calls = std::fs::read_to_string(legacy_log).unwrap_or_default();
        let other_calls = std::fs::read_to_string(other_log).unwrap_or_default();
        let (selected_calls, other_calls) = if selected_server == "LegacyRename" {
            (legacy_calls.as_str(), other_calls.as_str())
        } else {
            (other_calls.as_str(), legacy_calls.as_str())
        };
        assert!(
            selected_calls
                .lines()
                .any(|line| line == "textDocument/prepareRename")
        );
        assert!(
            selected_calls
                .lines()
                .any(|line| line == "textDocument/rename")
        );
        assert!(
            !other_calls
                .lines()
                .any(|line| line == "textDocument/rename")
        );
        editor.close(opened.buffer_id).await.unwrap();
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn stale_second_file_aborts_lsp_workspace_edit_before_any_target_changes() {
    if !fresh::services::lsp::command_exists("python3") {
        return;
    }
    let root = temp_root("stale");
    let script = root.join("fake_lsp.py");
    std::fs::write(&script, WORKSPACE_LSP).unwrap();
    let source = root.join("source.py");
    let stale = root.join("stale.py");
    std::fs::write(&source, "old = 1\n").unwrap();
    std::fs::write(&stale, "old = 2\n").unwrap();
    let editor = EditorHandle::spawn_with_recovery_dir(
        root.clone(),
        fake_rename_config(&script, &[&source, &stale]),
        root.join("recovery"),
    )
    .unwrap();
    editor.set_workspace_authority(crate::fs::FsRoot::new(root.clone()).unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let opened = editor.open(source.clone(), false).await.unwrap();
        let (_, preview) = lsp_rename_preview(&editor, &opened).await;
        std::fs::write(&stale, "external = 2\n").unwrap();
        let error = editor
            .apply_workspace_edit(opened.buffer_id.clone(), "test-owner".into(), preview.token)
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("changed since preview"));
        assert_eq!(
            editor.sync(opened.buffer_id.clone()).await.unwrap().text,
            "old = 1\n"
        );
        assert_eq!(std::fs::read_to_string(&source).unwrap(), "old = 1\n");
        assert_eq!(std::fs::read_to_string(&stale).unwrap(), "external = 2\n");

        let current = editor.sync(opened.buffer_id.clone()).await.unwrap();
        let fresh_source = OpenedBuffer {
            rev: current.rev,
            text: current.text,
            ..opened.clone()
        };
        let (_, stale_open) = lsp_rename_preview(&editor, &fresh_source).await;
        editor
            .edit(
                opened.buffer_id.clone(),
                fresh_source.rev,
                "local = 1\n".into(),
            )
            .await
            .unwrap();
        let error = editor
            .apply_workspace_edit(
                opened.buffer_id.clone(),
                "test-owner".into(),
                stale_open.token,
            )
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("changed since preview"));
        assert_eq!(
            editor.sync(opened.buffer_id.clone()).await.unwrap().text,
            "local = 1\n"
        );
        assert_eq!(std::fs::read_to_string(&stale).unwrap(), "external = 2\n");
        editor.close(opened.buffer_id).await.unwrap();
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn workspace_resource_operations_are_previewed_and_reject_open_targets() {
    let root = temp_root("resources");
    let script = root.join("fake_lsp.py");
    std::fs::write(&script, RESOURCE_LSP).unwrap();
    let source = root.join("source.py");
    let open_target = root.join("open.py");
    let create = root.join("created.py");
    let renamed = root.join("renamed.py");
    let deleted = root.join("deleted.py");
    let removed = root.join("removed.py");
    let symlink = root.join("symbolic.py");
    let outside = root.parent().unwrap().join(format!(
        "fresh-gui-unauthorized-{}.py",
        uuid::Uuid::new_v4()
    ));
    let symlink_source = root.join("symlink-source.py");
    let recovery_target = root.join("recovery.py");
    std::fs::write(&source, "source\n").unwrap();
    std::fs::write(&open_target, "open\n").unwrap();
    std::fs::write(&deleted, "remove\n").unwrap();
    std::fs::write(&removed, "delete\n").unwrap();
    std::fs::write(&outside, "keep\n").unwrap();
    std::fs::write(&symlink_source, "link\n").unwrap();
    std::fs::write(&recovery_target, "draft\n").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&symlink_source, &symlink).unwrap();
    #[cfg(not(unix))]
    std::fs::write(&symlink, "link\n").unwrap();
    let mut args = vec![script.display().to_string()];
    args.extend(
        [
            &source,
            &open_target,
            &create,
            &deleted,
            &renamed,
            &removed,
            &symlink,
            &outside,
            &recovery_target,
        ]
        .iter()
        .map(|p| p.display().to_string()),
    );
    let editor = EditorHandle::spawn_with_recovery_dir(
        root.clone(),
        crate::config::Config::parse(&json!({"lsp":{"python":{"name":"WorkspaceEdits","command":"python3","args":args,"only_features":["rename"]}}}).to_string()).unwrap(),
        root.join("recovery"),
    ).unwrap();
    editor.set_workspace_authority(crate::fs::FsRoot::new(root.clone()).unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let opened_source = editor.open(source.clone(), false).await.unwrap();
        let opened_target = editor.open(open_target.clone(), false).await.unwrap();
        let detached = editor
            .open_in_workspace(recovery_target.clone(), false, "detached-workspace".into())
            .await
            .unwrap();
        editor
            .edit(
                detached.buffer_id.clone(),
                detached.rev,
                "dirty draft\n".into(),
            )
            .await
            .unwrap();
        editor
            .close_in_workspace(detached.buffer_id, "detached-workspace".into())
            .await
            .unwrap();
        let owner = "test-owner".to_string();
        let request = rename_request_named(
            opened_source.buffer_id.clone(),
            opened_source.rev,
            201,
            "resources",
        );
        let (_, preview) = lsp_edit_preview(&editor, &opened_source, request).await;
        assert!(
            preview
                .files
                .iter()
                .any(|f| f.operation == "create" && f.path == create.display().to_string())
        );
        assert!(
            preview
                .files
                .iter()
                .any(|f| f.operation == "delete" && f.path == deleted.display().to_string())
        );
        assert!(
            preview
                .files
                .iter()
                .any(|f| f.operation == "create" && f.path == renamed.display().to_string())
        );
        assert!(
            preview
                .files
                .iter()
                .any(|f| f.operation == "delete" && f.path == removed.display().to_string())
        );
        let applied = editor
            .apply_workspace_edit(
                opened_source.buffer_id.clone(),
                owner.clone(),
                preview.token,
            )
            .await
            .unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].text, "changed\n");
        assert!(create.exists());
        assert!(!deleted.exists());
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "remove\n");
        assert!(!removed.exists());

        let current = editor.sync(opened_source.buffer_id.clone()).await.unwrap();
        let bad_request =
            rename_request_named(opened_source.buffer_id.clone(), current.rev, 202, "bad");
        let bad_result = lsp_response(&editor, bad_request).await;
        assert!(
            bad_result
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("resource operations on open buffers")
        );

        #[cfg(unix)]
        {
            let current = editor.sync(opened_source.buffer_id.clone()).await.unwrap();
            let symlink_result = lsp_response(
                &editor,
                rename_request_named(opened_source.buffer_id.clone(), current.rev, 203, "symlink"),
            )
            .await;
            assert!(
                symlink_result
                    .status
                    .as_deref()
                    .unwrap_or_default()
                    .contains("symlink")
            );
        }

        let current = editor.sync(opened_source.buffer_id.clone()).await.unwrap();
        let outside_request =
            rename_request_named(opened_source.buffer_id.clone(), current.rev, 204, "outside");
        let outside_result = lsp_response(&editor, outside_request).await;
        assert!(
            outside_result
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("escapes FS root")
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep\n");

        let current = editor.sync(opened_source.buffer_id.clone()).await.unwrap();
        let recovery_result = lsp_response(
            &editor,
            rename_request_named(
                opened_source.buffer_id.clone(),
                current.rev,
                205,
                "recovery",
            ),
        )
        .await;
        assert!(
            recovery_result
                .status
                .as_deref()
                .unwrap_or_default()
                .contains("recovery draft")
        );
        assert_eq!(
            std::fs::read_to_string(&recovery_target).unwrap(),
            "draft\n"
        );
        editor.close(opened_source.buffer_id).await.unwrap();
        editor.close(opened_target.buffer_id).await.unwrap();
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(outside);
}

#[test]
fn code_actions_merge_servers_and_resolve_on_the_selected_provider() {
    if !fresh::services::lsp::command_exists("python3") {
        return;
    }
    let root = temp_root("actions");
    let script = root.join("fake_lsp.py");
    std::fs::write(&script, ACTION_LSP).unwrap();
    let source = root.join("sample.py");
    std::fs::write(&source, "old = 1\n").unwrap();
    let command = |name: &str| {
        json!({
            "name":name, "command":"python3", "args":[script.display().to_string(),source.display().to_string(),name],
            "only_features":["code_action"]
        })
    };
    let config = crate::config::Config::parse(
        &json!({"lsp":{"python":[command("QuickA"),command("QuickB")]}}).to_string(),
    )
    .unwrap();
    let editor =
        EditorHandle::spawn_with_recovery_dir(root.clone(), config, root.join("recovery")).unwrap();
    editor.set_workspace_authority(crate::fs::FsRoot::new(root.clone()).unwrap());
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        let opened = editor.open(source.clone(), false).await.unwrap();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let merged = tokio::time::timeout_at(deadline, async {
            for request_id in 301..340 {
                let result = lsp_response(
                    &editor,
                    LspRequest {
                        request_id,
                        buffer_id: opened.buffer_id.clone(),
                        view_id: "test-owner:view".into(),
                        base_rev: opened.rev,
                        offset: 0,
                        feature: LspRequestFeature::CodeActions,
                        trigger_character: None,
                        item: None,
                        server: None,
                    },
                )
                .await;
                if result.responses.len() == 2 {
                    return result;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            panic!("both code-action servers did not become available")
        })
        .await
        .expect("waiting for both code-action servers timed out");
        assert_eq!(merged.responses.len(), 2);
        assert!(merged.responses.iter().any(|r| r.server == "QuickA"));
        assert!(merged.responses.iter().any(|r| r.server == "QuickB"));

        let (selected_server, action) = merged
            .responses
            .iter()
            .find_map(|response| {
                response
                    .result
                    .as_array()?
                    .first()
                    .cloned()
                    .map(|action| (response.server.clone(), action))
            })
            .unwrap();
        let mut resolved_rx = editor.subscribe_lsp();
        editor
            .request_lsp(LspRequest {
                request_id: 401,
                buffer_id: opened.buffer_id.clone(),
                view_id: "test-owner:view".into(),
                base_rev: opened.rev,
                offset: 0,
                feature: LspRequestFeature::CodeActionResolve,
                trigger_character: None,
                item: Some(action),
                server: Some(selected_server.clone()),
            })
            .unwrap();
        let resolved = await_lsp_result(&mut resolved_rx, 401).await;
        assert_eq!(resolved.responses.len(), 1);
        assert_eq!(resolved.responses[0].server, selected_server);
        assert!(resolved.responses[0].result.get("edit").is_some());

        let edit = resolved.responses[0].result.get("edit").unwrap().clone();
        let preview = editor
            .prepare_workspace_edit(
                opened.buffer_id.clone(),
                opened.rev,
                "test-owner".into(),
                edit,
            )
            .await
            .unwrap();
        assert_eq!(preview.files.len(), 1);
        let updates = editor
            .apply_workspace_edit(opened.buffer_id.clone(), "test-owner".into(), preview.token)
            .await
            .unwrap();
        assert_eq!(
            updates[0].text,
            format!("{} = 1\n", selected_server.to_lowercase())
        );

        let mut notices = editor.subscribe_workspace_edits();
        let mut command_results = editor.subscribe_lsp();
        editor
            .request_lsp(LspRequest {
                request_id: 402,
                buffer_id: opened.buffer_id.clone(),
                view_id: "test-owner:view".into(),
                base_rev: updates[0].rev,
                offset: 0,
                feature: LspRequestFeature::ExecuteCommand,
                trigger_character: None,
                item: Some(json!({"command":"fresh.test","arguments":[]})),
                server: Some(selected_server.clone()),
            })
            .unwrap();
        let command_result = await_lsp_result(&mut command_results, 402).await;
        assert_eq!(command_result.responses.len(), 1);
        let notice = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match notices.recv().await {
                    Ok(WorkspaceNotice::Preview { preview, .. }) => break preview,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(error) => panic!("workspace edit notice channel closed: {error}"),
                }
            }
        })
        .await
        .expect("server applyEdit preview timed out");
        assert_eq!(
            editor.sync(opened.buffer_id.clone()).await.unwrap().text,
            updates[0].text,
            "server workspace/applyEdit must wait for user acceptance"
        );
        let applied = editor
            .apply_workspace_edit(opened.buffer_id.clone(), "test-owner".into(), notice.token)
            .await
            .unwrap();
        assert_eq!(
            applied[0].text,
            format!("cmd{} = 1\n", &selected_server.to_lowercase()[3..])
        );

        let mut rejected_notices = editor.subscribe_workspace_edits();
        let mut stale_command_results = editor.subscribe_lsp();
        editor
            .request_lsp(LspRequest {
                request_id: 403,
                buffer_id: opened.buffer_id.clone(),
                view_id: "test-owner:view".into(),
                base_rev: applied[0].rev,
                offset: 0,
                feature: LspRequestFeature::ExecuteCommand,
                trigger_character: None,
                item: Some(json!({"command":"fresh.staleVersion","arguments":[]})),
                server: Some(selected_server.clone()),
            })
            .unwrap();
        let _ = await_lsp_result(&mut stale_command_results, 403).await;
        let rejected = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match rejected_notices.recv().await {
                    Ok(WorkspaceNotice::Rejected { message, .. }) => break message,
                    Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(error) => panic!("workspace edit notice channel closed: {error}"),
                }
            }
        })
        .await
        .expect("stale LSP version was not rejected");
        assert!(rejected.contains("document version"));
        assert_eq!(
            editor.sync(opened.buffer_id.clone()).await.unwrap().text,
            applied[0].text,
            "a stale versioned server edit must not mutate the buffer"
        );
        editor.close(opened.buffer_id).await.unwrap();
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

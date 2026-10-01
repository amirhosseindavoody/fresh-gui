use super::*;

fn test_root(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("fresh-gui-{name}-{}", uuid::Uuid::new_v4()))
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn external_formatter_without_lsp_and_format_on_save_refreshes_snapshot() {
    if !fresh::services::lsp::command_exists("python3") {
        return;
    }
    let root = test_root("external-format");
    std::fs::create_dir_all(&root).unwrap();
    let formatter = root.join("formatter.py");
    std::fs::write(
        &formatter,
        "#!/usr/bin/env python3\nimport sys\nsys.stdout.write(sys.stdin.read().replace('bad', 'good'))\n",
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&formatter).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&formatter, permissions).unwrap();
    }
    let cfg = crate::config::Config::parse(
        &serde_json::json!({
            "languages": {
                "python": {
                    "formatter": {"command": formatter.display().to_string(), "args": [], "stdin": true},
                    "format_on_save": true
                }
            }
        })
        .to_string(),
    )
    .unwrap();
    let path = root.join("sample.py");
    std::fs::write(&path, "bad = 1\n").unwrap();
    let editor = EditorHandle::spawn(root.clone(), cfg).unwrap();
    runtime().block_on(async {
        let opened = editor.open(path.clone(), false).await.unwrap();
        let formatted = editor.format(opened.buffer_id.clone(), opened.rev).await.unwrap();
        assert_eq!(formatted.text.as_deref(), Some("good = 1\n"));

        let dirty_rev = editor
            .edit(opened.buffer_id.clone(), formatted.rev, "bad = 2\n".into())
            .await
            .unwrap();
        let (saved_path, saved_rev) = editor
            .save(opened.buffer_id.clone(), dirty_rev, None)
            .await
            .unwrap();
        assert_eq!(saved_path, path.display().to_string());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "good = 2\n");
        let snapshot = editor.lsp_get(opened.buffer_id.clone(), dirty_rev).await.unwrap();
        assert_eq!(snapshot.rev, saved_rev);
        assert_eq!(snapshot.text.as_deref(), Some("good = 2\n"));
        editor.close(opened.buffer_id).await.unwrap();
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn format_range_uses_lsp_utf16_positions_and_replaces_only_the_range() {
    if !fresh::services::lsp::command_exists("python3") {
        return;
    }
    let root = test_root("range-format");
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("fake_lsp.py");
    let source = super::tests::FAKE_LSP
        .replace("'documentFormattingProvider':True", "'documentFormattingProvider':True,'documentRangeFormattingProvider':True")
        .replace(
            "    elif method == 'textDocument/formatting':",
            "    elif method == 'textDocument/rangeFormatting':\n        with open(log_path + '.range', 'w') as log: log.write(json.dumps(msg['params']['range']))\n        selected = msg['params']['range']\n        send({'jsonrpc':'2.0','id':msg['id'],'result':[{'range':selected,'newText':'X'}]})\n    elif method == 'textDocument/formatting':",
        );
    std::fs::write(&script, source).unwrap();
    let cfg = crate::config::Config::parse(
        &serde_json::json!({
            "lsp": {"python": {"name":"Formatter", "command":"python3", "args":[script.display().to_string(), "Formatter"], "only_features":["format","diagnostics"]}}
        })
        .to_string(),
    )
    .unwrap();
    let path = root.join("unicode.py");
    std::fs::write(&path, "a😀b\n").unwrap();
    let editor = EditorHandle::spawn(root.clone(), cfg).unwrap();
    runtime().block_on(async {
        let opened = editor.open(path.clone(), false).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let state = editor.lsp_get(opened.buffer_id.clone(), opened.rev).await.unwrap();
            if !state.diagnostics.is_empty() { break; }
            assert!(tokio::time::Instant::now() < deadline, "range formatting server must initialize");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let invalid = editor
            .format_range(
                opened.buffer_id.clone(),
                opened.rev,
                Some(ByteRange { start: 2, len: 1 }),
            )
            .await;
        assert!(invalid.is_err(), "range boundaries must align with UTF-8");
        // UTF-8 byte offset 5 is UTF-16 character offset 3 (`a` + surrogate pair).
        let formatted = editor
            .format_range(opened.buffer_id.clone(), opened.rev, Some(ByteRange { start: 5, len: 1 }))
            .await
            .unwrap();
        assert_eq!(formatted.text.as_deref(), Some("a😀X\n"));
        let range: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(root.join("fake_lsp.py.Formatter.log.range")).unwrap(),
        )
        .unwrap();
        assert_eq!(range["start"]["line"], 0);
        assert_eq!(range["start"]["character"], 3);
        assert_eq!(range["end"]["character"], 4);
        editor.close(opened.buffer_id).await.unwrap();
    });
    drop(editor);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn configured_servers_stop_stay_stopped_and_restart_with_language_logs() {
    use fresh_gui_protocol::LanguageServerAction as Action;
    if !fresh::services::lsp::command_exists("python3") { return; }
    let root = test_root("server-control");
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("server.py");
    let source = super::tests::FAKE_LSP.replace("    elif method in ('textDocument/didOpen', 'textDocument/didChange'):",
        "    elif method == 'initialized':\n        send({'jsonrpc':'2.0','method':'window/logMessage','params':{'type':3,'message':'fixture ready'}})\n    elif method in ('textDocument/didOpen', 'textDocument/didChange'):");
    std::fs::write(&script, source).unwrap();
    let cfg = crate::config::Config::parse(&serde_json::json!({ "lsp": { "python": { "name":"Fixture", "command":"python3", "args":[script.display().to_string(),"Fixture"] } } }).to_string()).unwrap();
    let path = root.join("test.py"); std::fs::write(&path, "bad = 1\n").unwrap();
    let editor = EditorHandle::spawn(root.clone(), cfg).unwrap();
    runtime().block_on(async {
        let opened = editor.open(path, false).await.unwrap();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let states = editor.language_servers(opened.buffer_id.clone(), Action::Status).await.unwrap();
            if states[0].status.starts_with("running") && states[0].logs.iter().any(|line| line.contains("fixture ready")) { break; }
            assert!(tokio::time::Instant::now() < deadline, "configured server must run and expose language logs");
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let states = editor.language_servers(opened.buffer_id.clone(), Action::Stop).await.unwrap();
        assert!(states[0].status.starts_with("stopped"));
        let rev = editor.edit(opened.buffer_id.clone(), opened.rev, "new = 2\n".into()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(editor.language_servers(opened.buffer_id.clone(), Action::Status).await.unwrap()[0].status.starts_with("stopped"), "editing must not auto-start a stopped language");
        for action in [Action::Start, Action::Restart] {
            editor.language_servers(opened.buffer_id.clone(), action).await.unwrap();
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                if editor.language_servers(opened.buffer_id.clone(), Action::Status).await.unwrap()[0].status.starts_with("running") { break; }
                assert!(tokio::time::Instant::now() < deadline, "manual start/restart must initialize");
                tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            }
        }
        assert!(editor.lsp_get(opened.buffer_id.clone(), rev).await.unwrap().rev >= rev);
        editor.close(opened.buffer_id).await.unwrap();
    });
    drop(editor); let _ = std::fs::remove_dir_all(root);
}

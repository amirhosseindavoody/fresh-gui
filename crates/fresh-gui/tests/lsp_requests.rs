//! Revision-aware LSP requests over the ADE websocket.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{
    ByteSelection, CAP_LSP_REQUESTS, EditorAction, LspRequest, LspRequestFeature, Message,
    RangeEdit,
};

fn free_loopback() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local address");
    drop(listener);
    addr
}

fn wait_health(addr: SocketAddr) {
    let url = format!("http://{addr}/healthz");
    for _ in 0..100 {
        if Command::new("curl")
            .args(["-sf", &url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not become healthy at {url}");
}

struct Backend(Child);

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_backend(addr: SocketAddr, root: &std::path::Path, config: &std::path::Path) -> Backend {
    Backend(
        Command::new(env!("CARGO_BIN_EXE_fresh-gui"))
            .args(["--foreground", "--listen"])
            .arg(addr.to_string())
            .args(["--allow-no-auth", "--root"])
            .arg(root)
            .arg("--config")
            .arg(config)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn daemon"),
    )
}

fn temp_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fresh-gui-lsp-requests-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

async fn wait_lsp(client: &mut Client, request_id: u64) -> fresh_gui_protocol::LspResult {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive LSP response") {
                Message::BufferLspResult { result } if result.request_id == request_id => {
                    return result;
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. }
                | Message::BufferLspResult { .. }
                | Message::BufferChanged { .. }
                | Message::BufferPaged { .. }
                | Message::BufferEditResult { .. } => {}
                other => panic!("unexpected message while waiting for LSP response: {other:?}"),
            }
        }
    })
    .await
    .expect("LSP response timed out")
}

async fn await_range(client: &mut Client, request_id: &str) -> u64 {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive edit response") {
                Message::BufferEditResult {
                    request_id: id,
                    rev,
                    ..
                } if id == request_id => {
                    return rev;
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. }
                | Message::BufferLspResult { .. }
                | Message::BufferChanged { .. }
                | Message::BufferPaged { .. }
                | Message::BufferEditResult { .. } => {}
                other => panic!("unexpected message while waiting for edit: {other:?}"),
            }
        }
    })
    .await
    .expect("edit response timed out")
}

async fn await_edit_result(
    client: &mut Client,
    request_id: &str,
) -> (u64, String, ByteSelection, bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive edit result") {
                Message::BufferEditResult {
                    request_id: id,
                    rev,
                    text,
                    selection,
                    accepted,
                    ..
                } if id == request_id => return (rev, text, selection, accepted),
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. }
                | Message::BufferLspResult { .. }
                | Message::BufferChanged { .. }
                | Message::BufferPaged { .. }
                | Message::BufferEditResult { .. } => {}
                other => panic!("unexpected message while waiting for edit result: {other:?}"),
            }
        }
    })
    .await
    .expect("edit result timed out")
}

fn request(
    request_id: u64,
    buffer_id: &str,
    rev: u64,
    offset: usize,
    feature: LspRequestFeature,
) -> Message {
    request_for_view(request_id, buffer_id, "lsp-test-view", rev, offset, feature)
}

fn request_for_view(
    request_id: u64,
    buffer_id: &str,
    view_id: &str,
    rev: u64,
    offset: usize,
    feature: LspRequestFeature,
) -> Message {
    Message::BufferLspRequest {
        request: LspRequest {
            request_id,
            buffer_id: buffer_id.into(),
            view_id: view_id.into(),
            base_rev: rev,
            offset,
            feature,
            trigger_character: None,
            item: None,
            server: None,
        },
    }
}

#[tokio::test]
async fn lsp_request_bridge_routes_tracks_revisions_and_handles_unavailable_buffers() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 unavailable; skipping fake LSP integration test");
        return;
    }
    let root = temp_root();
    let fixture =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/lsp_intelligence.py");
    let alpha_log = root.join("alpha.log");
    let beta_log = root.join("beta.log");
    let slow_log = root.join("slow.log");
    let config_path = root.join("config.json");
    let target_name = if cfg!(windows) { "target name😀.py" } else { "target name😀.py:12" };
    let target_path = root.join(target_name);
    let mut target_uri = None;
    let paged_path = root.join("paged.py");
    let config = serde_json::json!({
        "lsp": { "python": [
            {"name":"Alpha","command":"python3","args":[fixture.display().to_string(),"Alpha",alpha_log.display().to_string(), "0"],"only_features":["completion"]},
            {"name":"Beta","command":"python3","args":[fixture.display().to_string(),"Beta",beta_log.display().to_string(), "0", target_path.display().to_string(), paged_path.display().to_string()],"except_features":["completion"]},
            {"name":"Slow","command":"python3","args":[fixture.display().to_string(),"Slow",slow_log.display().to_string(), "0.6"],"only_features":["hover"]}
        ], "rust": [{"name":"MissingBinary","command":"fresh-gui-missing-language-server-145","only_features":["completion"]}]}
    });
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::write(root.join("sample.py"), "a😀b\ncallme\n").unwrap();
    fs::write(&target_path, "a😀target\nsecond\n").unwrap();
    fs::write(&paged_path, "a😀x\n".repeat(400_000)).unwrap();
    fs::write(root.join("words.rs"), "pref\nprefix_word\n").unwrap();

    let addr = free_loopback();
    let _backend = spawn_backend(addr, &root, &config_path);
    wait_health(addr);
    let url = format!("ws://{addr}/ws");
    let mut client = Client::connect(ConnectOptions::new(url))
        .await
        .expect("connect daemon");
    assert!(client.supports_capability(CAP_LSP_REQUESTS));
    assert!(client.supports_capability(fresh_gui_protocol::CAP_LSP_NAVIGATION));

    let (buffer_id, _, _, mut rev, text) = client
        .open_editor("sample.py", false)
        .await
        .expect("open python buffer");
    assert_eq!(text, "a😀b\ncallme\n");

    // Wait until Fresh has initialized the servers and published their routed
    // trigger metadata before asserting which server handles each feature.
    let mut ready = false;
    for attempt in 0..80_u64 {
        client
            .send(request(
                10_000 + attempt,
                &buffer_id,
                rev,
                0,
                LspRequestFeature::Capabilities,
            ))
            .await
            .unwrap();
        let capabilities = wait_lsp(&mut client, 10_000 + attempt).await;
        if capabilities.completion_triggers.contains(&".".into())
            && capabilities.signature_triggers.contains(&"(".into())
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "Fresh LSP server capabilities did not become ready");

    // Byte offset 5 is after a (1 byte) and 😀 (4 bytes), but its LSP column is 3 UTF-16 units.
    client
        .send(request(
            101,
            &buffer_id,
            rev,
            5,
            LspRequestFeature::Completion,
        ))
        .await
        .unwrap();
    let completion = wait_lsp(&mut client, 101).await;
    assert_eq!(completion.buffer_id, buffer_id);
    assert_eq!(completion.view_id, "lsp-test-view");
    assert_eq!(completion.rev, rev);
    assert_eq!(completion.offset, 5);
    assert_eq!(completion.responses.len(), 1, "{completion:?}");
    assert_eq!(completion.responses[0].server, "Alpha");
    let completion_item = &completion.responses[0].result["items"][0];
    assert_eq!(completion_item["insertText"], "call()\n");
    assert_eq!(completion_item["data"]["_fresh_cursor_offset"], 7);
    assert_eq!(completion_item["textEdit"]["newText"], "call()\n");
    assert_eq!(
        completion_item["additionalTextEdits"][0]["newText"],
        "import package_name\n"
    );
    assert_eq!(
        completion_item["data"]["_fresh_original"]["data"]["completionToken"],
        "Alpha"
    );

    // Navigation and symbol requests use the same revision-aware bridge and
    // preserve opaque daemon-side URIs with UTF-16 positions.
    let navigation_features = [
        LspRequestFeature::Definition,
        LspRequestFeature::Declaration,
        LspRequestFeature::TypeDefinition,
        LspRequestFeature::Implementation,
        LspRequestFeature::References,
        LspRequestFeature::DocumentSymbols,
        LspRequestFeature::WorkspaceSymbols,
    ];
    let mut paged_target = None;
    for (index, feature) in navigation_features.into_iter().enumerate() {
        let mut message = request(200 + index as u64, &buffer_id, rev, 0, feature);
        if let Message::BufferLspRequest { request } = &mut message
            && feature == LspRequestFeature::WorkspaceSymbols
        {
            request.item = Some(serde_json::json!({"query": "target"}));
        }
        client.send(message).await.unwrap();
        let result = wait_lsp(&mut client, 200 + index as u64).await;
        assert_eq!(result.responses.len(), 1, "{feature:?}: {result:?}");
        assert_eq!(result.responses[0].server, "Beta");
        let expected_targets = if feature == LspRequestFeature::References {
            2
        } else {
            1
        };
        assert_eq!(
            result.navigation_targets.len(),
            expected_targets,
            "{feature:?}: {result:?}"
        );
        if feature == LspRequestFeature::References {
            paged_target = result.navigation_targets.get(1).cloned();
        }
        if feature == LspRequestFeature::DocumentSymbols {
            assert_eq!(
                result.navigation_targets[0].name.as_deref(),
                Some("symbol-from-Beta")
            );
            assert_eq!(
                result.navigation_targets[0].uri,
                format!("file://{}", root.join("sample.py").display())
            );
        } else {
            let wire = fresh::app::types::LspUri::from_wire(
                serde_json::from_value(serde_json::json!(result.navigation_targets[0].uri)).unwrap(),
            );
            assert_eq!(wire.to_host_path(None).unwrap(), target_path);
            if feature == LspRequestFeature::Definition {
                target_uri = Some(result.navigation_targets[0].uri.clone());
            }
            assert_eq!(result.navigation_targets[0].line, 0);
            assert_eq!(result.navigation_targets[0].character, 3);
        }
    }

    let target_uri = target_uri.expect("definition returns a target URI");
    client
        .send(Message::EditorOpenLocation {
            request_id: "lsp-location-open".into(),
            uri: target_uri.clone(),
            line: 0,
            character: 3,
        })
        .await
        .unwrap();
    let (opened_path, opened_offset, opened_text) =
        tokio::time::timeout(Duration::from_secs(10), async {
            let mut opened_text = None;
            loop {
                match client.recv().await.expect("receive location open response") {
                    Message::EditorOpened { request_id, .. }
                        if request_id == "lsp-location-open" => {}
                    Message::BufferSnapshot { text, path, .. } if path.ends_with(target_name) => {
                        opened_text = Some(text)
                    }
                    Message::EditorLocationOpened {
                        request_id,
                        path,
                        offset,
                        ..
                    } if request_id == "lsp-location-open" => {
                        return (
                            path,
                            offset,
                            opened_text.expect("snapshot precedes location completion"),
                        );
                    }
                    Message::PtyData { .. }
                    | Message::FsChanged { .. }
                    | Message::Pong { .. }
                    | Message::Ping { .. }
                    | Message::BufferLspState { .. }
                    | Message::BufferLspResult { .. }
                    | Message::BufferChanged { .. }
                    | Message::BufferPaged { .. }
                    | Message::BufferEditResult { .. }
                    | Message::EditorOpened { .. }
                    | Message::BufferSnapshot { .. } => {}
                    other => panic!("unexpected location-open response: {other:?}"),
                }
            }
        })
        .await
        .expect("location open timed out");
    assert!(opened_path.ends_with(target_name));
    assert_eq!(
        opened_offset, 5,
        "UTF-16 character 3 follows `a😀` (five UTF-8 bytes)"
    );
    assert_eq!(opened_text, "a😀target\nsecond\n");

    let paged_target = paged_target.expect("references include the paged destination");
    client
        .send(Message::EditorOpenLocation {
            request_id: "lsp-paged-location-open".into(),
            uri: paged_target.uri,
            line: paged_target.line,
            character: paged_target.character,
        })
        .await
        .unwrap();
    let paged_offset = tokio::time::timeout(Duration::from_secs(20), async {
        let mut saw_paged = false;
        loop {
            match client
                .recv()
                .await
                .expect("receive paged location open response")
            {
                Message::BufferPaged { path, .. } if path.ends_with("paged.py") => saw_paged = true,
                Message::EditorLocationOpened {
                    request_id,
                    offset,
                    path,
                    ..
                } if request_id == "lsp-paged-location-open" => {
                    assert!(
                        saw_paged,
                        "paged metadata precedes location completion for {path}"
                    );
                    return offset;
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. }
                | Message::BufferLspResult { .. }
                | Message::BufferChanged { .. }
                | Message::BufferSnapshot { .. }
                | Message::BufferEditResult { .. }
                | Message::EditorOpened { .. } => {}
                other => panic!("unexpected paged location-open response: {other:?}"),
            }
        }
    })
    .await
    .expect("paged location open timed out");
    assert_eq!(paged_offset, 350_000 * 7 + 5);

    client
        .send(Message::EditorOpenLocation {
            request_id: "lsp-non-file-location".into(),
            uri: "untitled:external".into(),
            line: 0,
            character: 0,
        })
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match client.recv().await.expect("receive non-file URI error") {
                Message::Error { code, message } => return (code, message),
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. }
                | Message::BufferLspResult { .. }
                | Message::BufferChanged { .. }
                | Message::BufferPaged { .. }
                | Message::BufferSnapshot { .. }
                | Message::BufferEditResult { .. }
                | Message::EditorOpened { .. }
                | Message::EditorLocationOpened { .. } => {}
                other => panic!("unexpected response to non-file URI: {other:?}"),
            }
        }
    })
    .await
    .expect("non-file URI error timed out");
    assert_eq!(error.0, "editor_open_failed");
    assert!(error.1.contains("not a file URI"));

    let alpha_request = fs::read_to_string(&alpha_log)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| message["method"] == "textDocument/completion")
        .expect("completion request reached Alpha");
    assert_eq!(alpha_request["params"]["position"]["line"], 0);
    assert_eq!(alpha_request["params"]["position"]["character"], 3);
    assert!(
        fs::read_to_string(&beta_log)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .all(|message| message["method"] != "textDocument/completion")
    );

    // Model accepting this item through #141 as one atomic edit containing
    // both its primary textEdit and import additionalTextEdit. One Fresh Undo
    // must restore the exact pre-completion buffer.
    client
        .send(Message::BufferRangeEdit {
            request_id: "completion-transaction".into(),
            buffer_id: buffer_id.clone(),
            view_id: "lsp-test-view".into(),
            base_rev: rev,
            // The native adapter composes both LSP edits into one spanning
            // replacement; #141 offsets refer to the evolving transaction.
            edits: vec![RangeEdit {
                start: 0,
                end: 5,
                text: "import package_name\na😀call()\n".into(),
            }],
            viewport: None,
            selection: ByteSelection {
                anchor: 32,
                head: 32,
            },
        })
        .await
        .unwrap();
    let (completion_rev, completed_text, _, accepted) =
        await_edit_result(&mut client, "completion-transaction").await;
    assert!(accepted);
    assert_eq!(
        completed_text,
        "import package_name\na😀call()\nb\ncallme\n"
    );
    client
        .send(Message::BufferAction {
            request_id: "undo-completion-transaction".into(),
            buffer_id: buffer_id.clone(),
            view_id: "lsp-test-view".into(),
            base_rev: completion_rev,
            action: EditorAction::Undo,
            selection: ByteSelection {
                anchor: 32,
                head: 32,
            },
        })
        .await
        .unwrap();
    let (undo_rev, undone_text, _, accepted) =
        await_edit_result(&mut client, "undo-completion-transaction").await;
    assert!(accepted);
    assert_eq!(undone_text, text);
    rev = undo_rev;

    // UTF-8 byte offsets must land on scalar boundaries; a byte inside the
    // non-BMP character is rejected rather than rounded to an LSP position.
    client
        .send(request(108, &buffer_id, rev, 2, LspRequestFeature::Hover))
        .await
        .unwrap();
    let invalid = wait_lsp(&mut client, 108).await;
    assert!(
        invalid
            .status
            .as_deref()
            .is_some_and(|status| status.contains("splits a UTF-8"))
    );

    // Beta's except_features admits hover while Alpha is completion-only.
    client
        .send(request(102, &buffer_id, rev, 5, LspRequestFeature::Hover))
        .await
        .unwrap();
    let hover = wait_lsp(&mut client, 102).await;
    assert_eq!(hover.responses.len(), 2);
    assert!(
        hover
            .responses
            .iter()
            .any(|response| response.server == "Beta")
    );
    assert!(
        hover
            .responses
            .iter()
            .any(|response| response.server == "Slow")
    );

    client
        .send(request(
            106,
            &buffer_id,
            rev,
            6,
            LspRequestFeature::SignatureHelp,
        ))
        .await
        .unwrap();
    let signature = wait_lsp(&mut client, 106).await;
    assert_eq!(signature.responses.len(), 1);
    assert_eq!(signature.responses[0].server, "Beta");
    let beta_signature = fs::read_to_string(&beta_log)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| message["method"] == "textDocument/signatureHelp")
        .expect("signature request reached Beta");
    assert_eq!(beta_signature["params"]["context"]["triggerKind"], 1);

    // The delayed request does not block an edit, and its eventual response is marked stale.
    client
        .send(request(103, &buffer_id, rev, 5, LspRequestFeature::Hover))
        .await
        .unwrap();
    client
        .send(Message::BufferRangeEdit {
            request_id: "edit-during-lsp".into(),
            buffer_id: buffer_id.clone(),
            view_id: "lsp-test-view".into(),
            base_rev: rev,
            edits: vec![RangeEdit {
                start: 5,
                end: 5,
                text: "x".into(),
            }],
            viewport: None,
            selection: ByteSelection { anchor: 6, head: 6 },
        })
        .await
        .unwrap();
    let next_rev = await_range(&mut client, "edit-during-lsp").await;
    assert_eq!(next_rev, rev + 1);
    let stale = wait_lsp(&mut client, 103).await;
    assert!(stale.stale);
    assert!(stale.responses.is_empty());

    // Cancellation is keyed by request, buffer, and view and does not starve subsequent edits.
    client
        .send(request(
            104,
            &buffer_id,
            next_rev,
            6,
            LspRequestFeature::Hover,
        ))
        .await
        .unwrap();
    client
        .send(Message::BufferLspCancel {
            request_id: 104,
            buffer_id: buffer_id.clone(),
            view_id: "lsp-test-view".into(),
        })
        .await
        .unwrap();
    client
        .send(Message::BufferRangeEdit {
            request_id: "edit-after-cancel".into(),
            buffer_id: buffer_id.clone(),
            view_id: "lsp-test-view".into(),
            base_rev: next_rev,
            edits: vec![RangeEdit {
                start: 6,
                end: 6,
                text: "y".into(),
            }],
            viewport: None,
            selection: ByteSelection { anchor: 7, head: 7 },
        })
        .await
        .unwrap();
    assert_eq!(
        await_range(&mut client, "edit-after-cancel").await,
        next_rev + 1
    );
    let cancelled_response = tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            match client.recv().await.expect("receive after cancellation") {
                Message::BufferLspResult { result } if result.request_id == 104 => return true,
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. }
                | Message::BufferLspResult { .. }
                | Message::BufferChanged { .. }
                | Message::BufferPaged { .. }
                | Message::BufferEditResult { .. } => {}
                other => panic!("unexpected post-cancel message: {other:?}"),
            }
        }
    })
    .await;
    assert!(
        cancelled_response.is_err(),
        "cancelled request returned a result"
    );

    // A language without an eligible server still offers local buffer words for completion.
    let (words_id, _, _, words_rev, _) = client
        .open_editor("words.rs", false)
        .await
        .expect("open serverless Rust buffer");
    client
        .send(request(
            105,
            &words_id,
            words_rev,
            4,
            LspRequestFeature::Completion,
        ))
        .await
        .unwrap();
    let fallback = wait_lsp(&mut client, 105).await;
    assert_eq!(fallback.responses[0].server, "buffer_words");
    assert_eq!(fallback.responses[0].result[0]["label"], "prefix_word");

    // LSP is unavailable for a buffer whose document is deliberately paged.
    fs::write(root.join("large.py"), vec![b'x'; 2 * 1024 * 1024 + 32]).unwrap();
    client
        .send(Message::EditorOpen {
            request_id: "open-paged-lsp".into(),
            path: "large.py".into(),
            preview: false,
            cwd: None,
            line: None,
            column: None,
        })
        .await
        .unwrap();
    let paged_id = tokio::time::timeout(Duration::from_secs(10), async {
        let mut opened = None;
        loop {
            match client.recv().await.expect("receive paged open") {
                Message::EditorOpened {
                    request_id,
                    buffer_id,
                    ..
                } if request_id == "open-paged-lsp" => {
                    opened = Some(buffer_id);
                }
                Message::BufferPaged { buffer_id, rev, .. }
                    if opened.as_deref() == Some(&buffer_id) =>
                {
                    return (buffer_id, rev);
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. }
                | Message::BufferLspResult { .. } => {}
                other => panic!("unexpected paged open response: {other:?}"),
            }
        }
    })
    .await
    .expect("paged open timed out");
    client
        .send(request(
            107,
            &paged_id.0,
            paged_id.1,
            0,
            LspRequestFeature::Completion,
        ))
        .await
        .unwrap();
    let unavailable = wait_lsp(&mut client, 107).await;
    assert!(
        unavailable
            .status
            .as_deref()
            .is_some_and(|status| status.contains("paged"))
    );

    // Wire ids are scoped to their websocket. Reusing request id 101 from a
    // second connection must not deliver its result to the first client.
    let mut second = Client::connect(ConnectOptions::new(format!("ws://{addr}/ws")))
        .await
        .expect("connect second client");
    second
        .send(request_for_view(
            101,
            &buffer_id,
            "second-view",
            next_rev + 1,
            5,
            LspRequestFeature::Completion,
        ))
        .await
        .unwrap();
    let second_result = wait_lsp(&mut second, 101).await;
    assert_eq!(second_result.view_id, "second-view");
    assert_eq!(second_result.responses[0].server, "Alpha");
    let leaked = tokio::time::timeout(Duration::from_millis(250), client.recv()).await;
    assert!(
        leaked.is_err(),
        "another connection's request leaked to this websocket"
    );

    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
#[ignore = "optional smoke test; set FRESH_GUI_SMOKE_RA to an installed rust-analyzer binary"]
async fn rust_analyzer_local_symbol_completion_hover_and_signature_smoke() {
    let Some(rust_analyzer) = std::env::var_os("FRESH_GUI_SMOKE_RA") else {
        panic!("set FRESH_GUI_SMOKE_RA to the rust-analyzer binary");
    };
    let root = temp_root();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp_smoke\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let source =
        "fn glimmer_signal(value: u32) -> u32 { value }\nfn caller() { glimmer_signal(0); }\n";
    fs::write(root.join("src/lib.rs"), source).unwrap();
    let config_path = root.join("config.json");
    fs::write(
        &config_path,
        serde_json::to_vec(&serde_json::json!({
            "lsp": {"rust": {"name":"rust-analyzer","command":rust_analyzer.to_string_lossy().to_string(),"args":[]}}
        }))
        .unwrap(),
    )
    .unwrap();
    let addr = free_loopback();
    let _backend = spawn_backend(addr, &root, &config_path);
    wait_health(addr);
    let mut client = Client::connect(ConnectOptions::new(format!("ws://{addr}/ws")))
        .await
        .expect("connect daemon");
    let (buffer_id, _, _, rev, _) = client
        .open_editor("src/lib.rs", false)
        .await
        .expect("open rust source");
    let mut ready = false;
    for attempt in 0..100_u64 {
        client
            .send(request(
                20_000 + attempt,
                &buffer_id,
                rev,
                0,
                LspRequestFeature::Capabilities,
            ))
            .await
            .unwrap();
        let capabilities = wait_lsp(&mut client, 20_000 + attempt).await;
        if capabilities
            .completion_triggers
            .iter()
            .any(|trigger| trigger == ".")
            && !capabilities.signature_triggers.is_empty()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(ready, "rust-analyzer capabilities did not become ready");

    let completion_offset = source.rfind("glim").unwrap() + "glim".len();
    // initialize advertises capabilities before Cargo project indexing finishes.
    let mut completion_ready = false;
    let mut last_completion = None;
    for attempt in 0..60_u64 {
        let id = 30_000 + attempt;
        client
            .send(request(
                id,
                &buffer_id,
                rev,
                completion_offset,
                LspRequestFeature::Completion,
            ))
            .await
            .unwrap();
        let completion = wait_lsp(&mut client, id).await;
        assert!(!completion.stale);
        completion_ready = completion.responses.iter().any(|response| {
            let items = response
                .result
                .as_array()
                .or_else(|| response.result["items"].as_array());
            items.is_some_and(|items| {
                items.iter().any(|item| {
                    item["label"]
                        .as_str()
                        .is_some_and(|label| label.starts_with("glimmer_signal"))
                })
            })
        });
        last_completion = Some(completion);
        if completion_ready {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        completion_ready,
        "rust-analyzer did not index local symbol: {last_completion:?}"
    );

    let hover_offset = source.rfind("glimmer_signal").unwrap() + 5;
    client
        .send(request(
            202,
            &buffer_id,
            rev,
            hover_offset,
            LspRequestFeature::Hover,
        ))
        .await
        .unwrap();
    let hover = wait_lsp(&mut client, 202).await;
    assert!(!hover.stale);
    assert!(
        hover
            .responses
            .iter()
            .any(|response| !response.result.is_null())
    );

    let signature_offset = source.rfind("glimmer_signal(").unwrap() + "glimmer_signal(".len();
    client
        .send(request(
            203,
            &buffer_id,
            rev,
            signature_offset,
            LspRequestFeature::SignatureHelp,
        ))
        .await
        .unwrap();
    let signature = wait_lsp(&mut client, 203).await;
    assert!(!signature.stale);
    assert!(
        signature
            .responses
            .iter()
            .any(|response| !response.result.is_null())
    );
    let _ = fs::remove_dir_all(root);
}

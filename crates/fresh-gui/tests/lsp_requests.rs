//! Revision-aware LSP requests over the ADE websocket.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{
    ByteSelection, CAP_LSP_REQUESTS, LspRequest, LspRequestFeature, Message, RangeEdit,
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
                Message::BufferEditResult { request_id: id, rev, .. } if id == request_id => {
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
    let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/lsp_intelligence.py");
    let alpha_log = root.join("alpha.log");
    let beta_log = root.join("beta.log");
    let slow_log = root.join("slow.log");
    let config_path = root.join("config.json");
    let config = serde_json::json!({
        "lsp": { "python": [
            {"name":"Alpha","command":"python3","args":[fixture.display().to_string(),"Alpha",alpha_log.display().to_string(), "0"],"only_features":["completion"]},
            {"name":"Beta","command":"python3","args":[fixture.display().to_string(),"Beta",beta_log.display().to_string(), "0"],"except_features":["completion"]},
            {"name":"Slow","command":"python3","args":[fixture.display().to_string(),"Slow",slow_log.display().to_string(), "0.6"],"only_features":["hover"]}
        ], "rust": [{"name":"MissingBinary","command":"fresh-gui-missing-language-server-145","only_features":["completion"]}]}
    });
    fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    fs::write(root.join("sample.py"), "a😀b\ncallme\n").unwrap();
    fs::write(root.join("words.rs"), "pref\nprefix_word\n").unwrap();

    let addr = free_loopback();
    let _backend = spawn_backend(addr, &root, &config_path);
    wait_health(addr);
    let url = format!("ws://{addr}/ws");
    let mut client = Client::connect(ConnectOptions::new(url))
        .await
        .expect("connect daemon");
    assert!(client.supports_capability(CAP_LSP_REQUESTS));

    let (buffer_id, _, _, rev, text) = client
        .open_editor("sample.py", false)
        .await
        .expect("open python buffer");
    assert_eq!(text, "a😀b\ncallme\n");

    // Wait until Fresh has initialized the servers and published their routed
    // trigger metadata before asserting which server handles each feature.
    let mut ready = false;
    for attempt in 0..80_u64 {
        client
            .send(request(10_000 + attempt, &buffer_id, rev, 0, LspRequestFeature::Capabilities))
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
        .send(request(101, &buffer_id, rev, 5, LspRequestFeature::Completion))
        .await
        .unwrap();
    let completion = wait_lsp(&mut client, 101).await;
    assert_eq!(completion.buffer_id, buffer_id);
    assert_eq!(completion.view_id, "lsp-test-view");
    assert_eq!(completion.rev, rev);
    assert_eq!(completion.offset, 5);
    assert_eq!(completion.responses.len(), 1);
    assert_eq!(completion.responses[0].server, "Alpha");
    let completion_item = &completion.responses[0].result["items"][0];
    assert_eq!(completion_item["insertText"], "call()\n");
    assert_eq!(completion_item["data"]["_fresh_cursor_offset"], 7);
    assert_eq!(completion_item["textEdit"]["newText"], "call()\n");
    assert_eq!(completion_item["additionalTextEdits"][0]["newText"], "import package_name\n");
    assert_eq!(completion_item["data"]["_fresh_original_data"]["completionToken"], "Alpha");

    let alpha_request = fs::read_to_string(&alpha_log)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .find(|message| message["method"] == "textDocument/completion")
        .expect("completion request reached Alpha");
    assert_eq!(alpha_request["params"]["position"]["line"], 0);
    assert_eq!(alpha_request["params"]["position"]["character"], 3);
    assert!(fs::read_to_string(&beta_log)
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
        .all(|message| message["method"] != "textDocument/completion"));

    // UTF-8 byte offsets must land on scalar boundaries; a byte inside the
    // non-BMP character is rejected rather than rounded to an LSP position.
    client
        .send(request(108, &buffer_id, rev, 2, LspRequestFeature::Hover))
        .await
        .unwrap();
    let invalid = wait_lsp(&mut client, 108).await;
    assert!(invalid.status.as_deref().is_some_and(|status| status.contains("splits a UTF-8")));

    // Beta's except_features admits hover while Alpha is completion-only.
    client
        .send(request(102, &buffer_id, rev, 5, LspRequestFeature::Hover))
        .await
        .unwrap();
    let hover = wait_lsp(&mut client, 102).await;
    assert_eq!(hover.responses.len(), 2);
    assert!(hover.responses.iter().any(|response| response.server == "Beta"));
    assert!(hover.responses.iter().any(|response| response.server == "Slow"));

    client
        .send(request(106, &buffer_id, rev, 6, LspRequestFeature::SignatureHelp))
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
            edits: vec![RangeEdit { start: 5, end: 5, text: "x".into() }],
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
        .send(request(104, &buffer_id, next_rev, 6, LspRequestFeature::Hover))
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
            edits: vec![RangeEdit { start: 6, end: 6, text: "y".into() }],
            viewport: None,
            selection: ByteSelection { anchor: 7, head: 7 },
        })
        .await
        .unwrap();
    assert_eq!(await_range(&mut client, "edit-after-cancel").await, next_rev + 1);
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
    assert!(cancelled_response.is_err(), "cancelled request returned a result");

    // A language without an eligible server still offers local buffer words for completion.
    let (words_id, _, _, words_rev, _) = client
        .open_editor("words.rs", false)
        .await
        .expect("open serverless Rust buffer");
    client
        .send(request(105, &words_id, words_rev, 4, LspRequestFeature::Completion))
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
                Message::EditorOpened { request_id, buffer_id, .. } if request_id == "open-paged-lsp" => {
                    opened = Some(buffer_id);
                }
                Message::BufferPaged { buffer_id, rev, .. } if opened.as_deref() == Some(&buffer_id) => {
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
        .send(request(107, &paged_id.0, paged_id.1, 0, LspRequestFeature::Completion))
        .await
        .unwrap();
    let unavailable = wait_lsp(&mut client, 107).await;
    assert!(unavailable.status.as_deref().is_some_and(|status| status.contains("paged")));

    // Wire ids are scoped to their websocket. Reusing request id 101 from a
    // second connection must not deliver its result to the first client.
    let mut second = Client::connect(ConnectOptions::new(format!("ws://{addr}/ws")))
        .await
        .expect("connect second client");
    second
        .send(request_for_view(101, &buffer_id, "second-view", rev, 5, LspRequestFeature::Completion))
        .await
        .unwrap();
    let second_result = wait_lsp(&mut second, 101).await;
    assert_eq!(second_result.view_id, "second-view");
    assert_eq!(second_result.responses[0].server, "Alpha");
    let leaked = tokio::time::timeout(Duration::from_millis(250), client.recv()).await;
    assert!(leaked.is_err(), "another connection's request leaked to this websocket");

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
    let source = "fn glimmer_signal(value: u32) -> u32 { value }\nfn caller() { glimmer_signal( }\n";
    fs::write(root.join("src/lib.rs"), source).unwrap();
    let config_path = root.join("config.json");
    fs::write(
        &config_path,
        serde_json::to_vec(&serde_json::json!({
            "lsp": {"rust": {"name":"rust-analyzer","command":rust_analyzer.to_string_lossy().to_string(),"args":["--stdio"]}}
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
            .send(request(20_000 + attempt, &buffer_id, rev, 0, LspRequestFeature::Capabilities))
            .await
            .unwrap();
        let capabilities = wait_lsp(&mut client, 20_000 + attempt).await;
        if capabilities.completion_triggers.iter().any(|trigger| trigger == ".")
            && !capabilities.signature_triggers.is_empty()
        {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    assert!(ready, "rust-analyzer capabilities did not become ready");

    let completion_offset = source.rfind("glim").unwrap() + "glim".len();
    client
        .send(request(201, &buffer_id, rev, completion_offset, LspRequestFeature::Completion))
        .await
        .unwrap();
    let completion = wait_lsp(&mut client, 201).await;
    assert!(!completion.stale);
    assert!(completion.responses.iter().any(|response| !response.result.is_null()));

    let hover_offset = source.rfind("glimmer_signal").unwrap() + 5;
    client
        .send(request(202, &buffer_id, rev, hover_offset, LspRequestFeature::Hover))
        .await
        .unwrap();
    let hover = wait_lsp(&mut client, 202).await;
    assert!(!hover.stale);
    assert!(hover.responses.iter().any(|response| !response.result.is_null()));

    let signature_offset = source.rfind("glimmer_signal(").unwrap() + "glimmer_signal(".len();
    client
        .send(request(203, &buffer_id, rev, signature_offset, LspRequestFeature::SignatureHelp))
        .await
        .unwrap();
    let signature = wait_lsp(&mut client, 203).await;
    assert!(!signature.stale);
    assert!(signature.responses.iter().any(|response| !response.result.is_null()));
    let _ = fs::remove_dir_all(root);
}

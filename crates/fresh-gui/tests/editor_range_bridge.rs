//! WebSocket coverage for the revisioned Fresh editor bridge.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{
    ByteSelection, CAP_EDITOR_RANGE_EDITS, EditorAction, Hello, Message, RangeEdit,
};

fn wait_health(addr: SocketAddr) {
    let url = format!("http://{addr}/healthz");
    for _ in 0..100 {
        if Command::new("curl")
            .args(["-sf", &url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("backend did not become healthy at {url}");
}

struct Backend(Child);

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_backend(addr: SocketAddr, root: &std::path::Path) -> Backend {
    Backend(
        Command::new(env!("CARGO_BIN_EXE_fresh-gui"))
            .arg("--foreground")
            .arg("--listen")
            .arg(addr.to_string())
            .arg("--token")
            .arg("editor-bridge-test-token")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn backend"),
    )
}

fn free_loopback() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

async fn result(client: &mut Client, request_id: &str) -> (u64, String, ByteSelection, bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive bridge response") {
                Message::BufferEditResult {
                    request_id: rid,
                    rev,
                    text,
                    selection,
                    accepted,
                    ..
                } if rid == request_id => return (rev, text, selection, accepted),
                Message::Error { code, message } if message.starts_with(request_id) => {
                    panic!("request {request_id} failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. } => {}
                other => panic!("unexpected response while waiting for {request_id}: {other:?}"),
            }
        }
    })
    .await
    .expect("bridge response timed out")
}

async fn legacy_edit_result(client: &mut Client, request_id: &str) -> u64 {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive legacy response") {
                Message::BufferChanged {
                    request_id: rid,
                    rev,
                    ..
                } if rid == request_id => return rev,
                Message::Error { code, message } if message.starts_with(request_id) => {
                    panic!("legacy request {request_id} failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. } => {}
                other => panic!("unexpected legacy response: {other:?}"),
            }
        }
    })
    .await
    .expect("legacy response timed out")
}

async fn expect_capability_error(client: &mut Client, request_id: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive capability response") {
                Message::Error { code, message } if message.starts_with(request_id) => {
                    assert_eq!(code, "capability_unavailable");
                    assert!(message.contains(CAP_EDITOR_RANGE_EDITS));
                    return;
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. } => {}
                other => panic!("unexpected capability response: {other:?}"),
            }
        }
    })
    .await
    .expect("capability response timed out");
}

async fn expect_error_code(client: &mut Client, request_id: Option<&str>, expected_code: &str) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive error response") {
                Message::Error { code, message }
                    if code == expected_code
                        && request_id.is_none_or(|id| message.starts_with(id)) =>
                {
                    return;
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. } => {}
                other => panic!("unexpected error response: {other:?}"),
            }
        }
    })
    .await
    .expect("error response timed out");
}

#[tokio::test]
async fn range_bridge_revisions_undo_resync_and_legacy_fallback() {
    let tmp = std::env::temp_dir().join(format!("fresh-gui-range-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("note.txt"), "a🦀你好\n").unwrap();

    let addr = free_loopback();
    let _backend = spawn_backend(addr, &tmp);
    wait_health(addr);
    let url = format!("ws://{addr}/ws");
    let opts = || ConnectOptions::new(&url).with_token("editor-bridge-test-token");

    // The websocket hello is allowed before authentication, while editor
    // transactions remain protected by the daemon's auth gate.
    let mut unauthenticated = Client::connect(ConnectOptions::new(&url))
        .await
        .expect("unauthenticated hello");
    unauthenticated
        .send(Message::BufferSync {
            request_id: "unauthenticated-sync".into(),
            buffer_id: "unknown".into(),
            view_id: "test-view".into(),
        })
        .await
        .unwrap();
    expect_error_code(&mut unauthenticated, None, "unauthorized").await;

    let mut client = Client::connect(opts())
        .await
        .expect("connect authenticated client");
    assert!(client.supports_capability(CAP_EDITOR_RANGE_EDITS));

    let (buffer_id, _, _, mut rev, text) = client
        .open_editor("note.txt", false)
        .await
        .expect("open editor");
    assert_eq!(text, "a🦀你好\n");
    assert_eq!(rev, 0);

    // CJK occupies three UTF-8 bytes per scalar. One range edit is one
    // Fresh transaction and the second request builds on its acknowledged rev.
    client
        .send(Message::BufferRangeEdit {
            request_id: "range-1".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            edits: vec![
                RangeEdit {
                    start: 5,
                    end: 11,
                    text: "世界".into(),
                },
                RangeEdit {
                    start: 11,
                    end: 11,
                    text: "?".into(),
                },
            ],
            viewport: None,
            selection: ByteSelection {
                anchor: 12,
                head: 12,
            },
        })
        .await
        .unwrap();
    let (next_rev, text, selection, accepted) = result(&mut client, "range-1").await;
    assert!(accepted);
    assert_eq!((next_rev, text.as_str()), (1, "a🦀世界?\n"));
    assert_eq!(
        selection,
        ByteSelection {
            anchor: 12,
            head: 12
        }
    );
    rev = next_rev;

    client
        .send(Message::BufferRangeEdit {
            request_id: "range-2".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            edits: vec![RangeEdit {
                start: 12,
                end: 12,
                text: "!".into(),
            }],
            viewport: None,
            selection: ByteSelection {
                anchor: 13,
                head: 13,
            },
        })
        .await
        .unwrap();
    let (next_rev, text, _, accepted) = result(&mut client, "range-2").await;
    assert!(accepted);
    assert_eq!((next_rev, text.as_str()), (2, "a🦀世界?!\n"));
    rev = next_rev;

    // A valid first edit followed by an invalid range must reject the whole
    // message before applying any of its edits.
    client
        .send(Message::BufferRangeEdit {
            request_id: "invalid-late-range".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            edits: vec![
                RangeEdit {
                    start: 0,
                    end: 1,
                    text: "z".into(),
                },
                RangeEdit {
                    start: 99,
                    end: 100,
                    text: "x".into(),
                },
            ],
            viewport: None,
            selection: ByteSelection { anchor: 1, head: 1 },
        })
        .await
        .unwrap();
    expect_error_code(
        &mut client,
        Some("invalid-late-range"),
        "buffer_edit_failed",
    )
    .await;
    client
        .send(Message::BufferSync {
            request_id: "after-invalid".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
        })
        .await
        .unwrap();
    let (after_invalid_rev, after_invalid_text, _, accepted) =
        result(&mut client, "after-invalid").await;
    assert!(accepted);
    assert_eq!(
        (after_invalid_rev, after_invalid_text.as_str()),
        (rev, "a🦀世界?!\n")
    );

    // A stale base is rejected with enough authoritative state to preserve
    // the client draft and reconcile it.
    client
        .send(Message::BufferRangeEdit {
            request_id: "stale".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: 0,
            edits: vec![RangeEdit {
                start: 0,
                end: 1,
                text: "x".into(),
            }],
            viewport: None,
            selection: ByteSelection { anchor: 1, head: 1 },
        })
        .await
        .unwrap();
    let (stale_rev, stale_text, _, accepted) = result(&mut client, "stale").await;
    assert!(!accepted);
    assert_eq!((stale_rev, stale_text.as_str()), (rev, "a🦀世界?!\n"));

    client
        .send(Message::BufferAction {
            request_id: "undo".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            action: EditorAction::Undo,
            selection: ByteSelection {
                anchor: 13,
                head: 13,
            },
        })
        .await
        .unwrap();
    let (next_rev, text, _, accepted) = result(&mut client, "undo").await;
    assert!(accepted);
    assert_eq!(text, "a🦀世界?\n");
    rev = next_rev;

    client
        .send(Message::BufferAction {
            request_id: "redo".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            action: EditorAction::Redo,
            selection: ByteSelection {
                anchor: 12,
                head: 12,
            },
        })
        .await
        .unwrap();
    let (next_rev, text, _, accepted) = result(&mut client, "redo").await;
    assert!(accepted);
    assert_eq!(text, "a🦀世界?!\n");
    rev = next_rev;

    // Undo the second transaction again, then one more undo rolls back both
    // ordered edits from range-1 as a single Fresh transaction.
    client
        .send(Message::BufferAction {
            request_id: "undo-second-again".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            action: EditorAction::Undo,
            selection: ByteSelection {
                anchor: 13,
                head: 13,
            },
        })
        .await
        .unwrap();
    let (next_rev, text, _, accepted) = result(&mut client, "undo-second-again").await;
    assert!(accepted);
    assert_eq!(text, "a🦀世界?\n");
    rev = next_rev;

    client
        .send(Message::BufferAction {
            request_id: "undo-group".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            action: EditorAction::Undo,
            selection: ByteSelection {
                anchor: 12,
                head: 12,
            },
        })
        .await
        .unwrap();
    let (next_rev, text, _, accepted) = result(&mut client, "undo-group").await;
    assert!(accepted);
    assert_eq!(text, "a🦀你好\n");
    rev = next_rev;

    client
        .send(Message::BufferAction {
            request_id: "redo-group".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            action: EditorAction::Redo,
            selection: ByteSelection { anchor: 5, head: 5 },
        })
        .await
        .unwrap();
    let (next_rev, text, _, accepted) = result(&mut client, "redo-group").await;
    assert!(accepted);
    assert_eq!(text, "a🦀世界?\n");
    rev = next_rev;

    client
        .send(Message::BufferAction {
            request_id: "redo-second".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            action: EditorAction::Redo,
            selection: ByteSelection {
                anchor: 12,
                head: 12,
            },
        })
        .await
        .unwrap();
    let (next_rev, text, _, accepted) = result(&mut client, "redo-second").await;
    assert!(accepted);
    assert_eq!(text, "a🦀世界?!\n");
    rev = next_rev;

    client
        .send(Message::BufferSync {
            request_id: "sync".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
        })
        .await
        .unwrap();
    let (sync_rev, sync_text, _, accepted) = result(&mut client, "sync").await;
    assert!(accepted);
    assert_eq!((sync_rev, sync_text.as_str()), (rev, "a🦀世界?!\n"));

    // A fresh connection can request the authoritative state of the same
    // daemon-owned buffer after reconnecting.
    let mut reconnected = Client::connect(opts()).await.expect("reconnect");
    reconnected
        .send(Message::BufferSync {
            request_id: "reconnect-sync".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
        })
        .await
        .unwrap();
    let (sync_rev, sync_text, _, accepted) = result(&mut reconnected, "reconnect-sync").await;
    assert!(accepted);
    assert_eq!((sync_rev, sync_text.as_str()), (rev, "a🦀世界?!\n"));

    // Another attached client mutates the same Fresh buffer; the original
    // connection can reconcile the server's newer snapshot on demand.
    reconnected
        .send(Message::BufferRangeEdit {
            request_id: "other-client-edit".into(),
            buffer_id: buffer_id.clone(),
            view_id: "other-view".into(),
            base_rev: rev,
            edits: vec![RangeEdit {
                start: 13,
                end: 13,
                text: "~".into(),
            }],
            viewport: None,
            selection: ByteSelection {
                anchor: 14,
                head: 14,
            },
        })
        .await
        .unwrap();
    let (next_rev, text, _, accepted) = result(&mut reconnected, "other-client-edit").await;
    assert!(accepted);
    assert_eq!(text, "a🦀世界?!~\n");
    rev = next_rev;
    client
        .send(Message::BufferSync {
            request_id: "live-resync".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
        })
        .await
        .unwrap();
    let (sync_rev, sync_text, _, accepted) = result(&mut client, "live-resync").await;
    assert!(accepted);
    assert_eq!((sync_rev, sync_text.as_str()), (rev, "a🦀世界?!~\n"));

    // A peer that omits the new capability receives an explicit rejection for
    // range edits and can continue using the existing full-text CAS message.
    let mut legacy = Client::connect(opts()).await.expect("legacy peer connect");
    let mut legacy_caps = Hello::default_client_caps();
    legacy_caps.retain(|cap| cap != CAP_EDITOR_RANGE_EDITS);
    legacy
        .send(Message::Hello(Hello::client("legacy-test", legacy_caps)))
        .await
        .unwrap();
    legacy
        .send(Message::BufferRangeEdit {
            request_id: "legacy-range".into(),
            buffer_id: buffer_id.clone(),
            view_id: "test-view".into(),
            base_rev: rev,
            edits: vec![RangeEdit {
                start: 0,
                end: 1,
                text: "z".into(),
            }],
            viewport: None,
            selection: ByteSelection { anchor: 1, head: 1 },
        })
        .await
        .unwrap();
    expect_capability_error(&mut legacy, "legacy-range").await;
    legacy
        .send(Message::BufferEdit {
            request_id: "legacy-edit".into(),
            buffer_id: buffer_id.clone(),
            base_rev: rev,
            text: "legacy full text\n".into(),
        })
        .await
        .unwrap();
    assert_eq!(
        legacy_edit_result(&mut legacy, "legacy-edit").await,
        rev + 1
    );

    drop(_backend);
    let _ = fs::remove_dir_all(&tmp);
}

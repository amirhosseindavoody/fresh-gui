//! ADE transport coverage for revisioned external file reconciliation.

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{CAP_EDITOR_EXTERNAL_CHANGES, ExternalResolution, Hello, Message};

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("local addr");
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
            .map(|status| status.success())
            .unwrap_or(false)
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

fn start(addr: SocketAddr, root: &std::path::Path) -> Backend {
    Backend(
        Command::new(env!("CARGO_BIN_EXE_fresh-gui"))
            .arg("--foreground")
            .arg("--listen")
            .arg(addr.to_string())
            .arg("--token")
            .arg("external-change-test-token")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start daemon"),
    )
}

async fn external_change(client: &mut Client, buffer_id: &str) -> (u64, String, String, bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive external change") {
                Message::BufferExternalChanged {
                    buffer_id: id,
                    rev,
                    generation,
                    disk_text,
                    dirty,
                    ..
                } if id == buffer_id => {
                    return (
                        rev,
                        generation,
                        disk_text.expect("changed file has valid text"),
                        dirty,
                    );
                }
                Message::BufferExternalChanged { .. }
                | Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. } => {}
                other => panic!("unexpected while waiting for external change: {other:?}"),
            }
        }
    })
    .await
    .expect("external change timed out")
}

async fn sync_text(client: &mut Client, buffer_id: &str, request_id: &str) -> (u64, String, bool) {
    client
        .send(Message::BufferSync {
            request_id: request_id.into(),
            buffer_id: buffer_id.into(),
            view_id: "external-test-view".into(),
        })
        .await
        .expect("send sync");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive sync") {
                Message::BufferEditResult {
                    request_id: id,
                    rev,
                    text,
                    accepted,
                    ..
                } if id == request_id => return (rev, text, accepted),
                Message::BufferExternalChanged { .. }
                | Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. } => {}
                other => panic!("unexpected while waiting for sync: {other:?}"),
            }
        }
    })
    .await
    .expect("sync timed out")
}

async fn save_error(client: &mut Client, buffer_id: &str, base_rev: u64, request_id: &str) {
    client
        .send(Message::BufferSave {
            request_id: request_id.into(),
            buffer_id: buffer_id.into(),
            base_rev,
            path: String::new(),
        })
        .await
        .expect("send save");
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut notice_seen = false;
        loop {
            match client.recv().await.expect("receive save conflict") {
                Message::BufferExternalChanged { buffer_id: id, .. } if id == buffer_id => {
                    notice_seen = true;
                }
                Message::Error { code, message } if message.starts_with(request_id) => {
                    assert_eq!(code, "buffer_save_failed");
                    assert!(
                        notice_seen,
                        "save conflict should send structured disk state before its error"
                    );
                    return;
                }
                Message::BufferExternalChanged { .. }
                | Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. } => {}
                other => panic!("unexpected while waiting for save conflict: {other:?}"),
            }
        }
    })
    .await
    .expect("save conflict timed out");
}

async fn resolve(
    client: &mut Client,
    buffer_id: &str,
    base_rev: u64,
    generation: &str,
    resolution: ExternalResolution,
    request_id: &str,
) -> (u64, String, bool, bool) {
    client
        .send(Message::BufferExternalResolve {
            request_id: request_id.into(),
            buffer_id: buffer_id.into(),
            base_rev,
            generation: generation.into(),
            resolution,
        })
        .await
        .expect("send external resolution");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive external resolution") {
                Message::BufferExternalResolved {
                    request_id: id,
                    rev,
                    text,
                    dirty,
                    accepted,
                    resolution: got_resolution,
                    ..
                } if id == request_id => {
                    assert_eq!(got_resolution, resolution);
                    return (rev, text, dirty, accepted);
                }
                Message::BufferExternalChanged { .. }
                | Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::BufferLspState { .. } => {}
                other => panic!("unexpected while waiting for external resolution: {other:?}"),
            }
        }
    })
    .await
    .expect("external resolution timed out")
}

#[tokio::test]
async fn external_changes_reload_clean_buffers_and_preserve_dirty_drafts_until_resolution() {
    let scratch = std::env::temp_dir().join(format!("fresh-gui-external-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&scratch).unwrap();
    let clean_path = scratch.join("clean.txt");
    let draft_path = scratch.join("draft.txt");
    std::fs::write(&clean_path, "before clean\n").unwrap();
    std::fs::write(&draft_path, "before draft\n").unwrap();

    let addr = free_addr();
    let _backend = start(addr, &scratch);
    wait_health(addr);
    let mut client = Client::connect(
        ConnectOptions::new(format!("ws://{addr}/ws")).with_token("external-change-test-token"),
    )
    .await
    .expect("connect client");
    assert!(client.supports_capability(CAP_EDITOR_EXTERNAL_CHANGES));

    let (clean_id, _, _, _, clean_text) = client
        .open_editor("clean.txt", false)
        .await
        .expect("open clean buffer");
    assert_eq!(clean_text, "before clean\n");
    // This write runs on the daemon host, the same path taken by agent writes
    // when the client connects to a remote daemon over SSH forwarding.
    std::fs::write(&clean_path, "after clean\n").unwrap();
    let (clean_rev, _, disk_text, dirty) = external_change(&mut client, &clean_id).await;
    assert!(!dirty);
    assert_eq!(disk_text, "after clean\n");
    let (synced_rev, synced_text, accepted) = sync_text(&mut client, &clean_id, "clean-sync").await;
    assert!(accepted);
    assert_eq!(synced_rev, clean_rev);
    assert_eq!(synced_text, "after clean\n");

    let (draft_id, _, _, draft_rev, draft_text) = client
        .open_editor("draft.txt", false)
        .await
        .expect("open draft buffer");
    assert_eq!(draft_text, "before draft\n");
    let draft_rev = client
        .edit_buffer(&draft_id, draft_rev, "my local draft\n")
        .await
        .expect("edit draft");
    std::fs::write(&draft_path, "agent update\n").unwrap();
    let (_, generation, disk_text, dirty) = external_change(&mut client, &draft_id).await;
    assert!(dirty);
    assert_eq!(disk_text, "agent update\n");
    let (still_draft_rev, still_draft, accepted) =
        sync_text(&mut client, &draft_id, "draft-sync").await;
    assert!(accepted);
    assert_eq!(still_draft_rev, draft_rev);
    assert_eq!(still_draft, "my local draft\n");

    // A stale disk generation blocks Save and returns the structured notice
    // before the ordinary request error so GUI code can keep the draft open.
    save_error(&mut client, &draft_id, draft_rev, "conflicted-save").await;
    let (kept_rev, kept_text, kept_dirty, accepted) = resolve(
        &mut client,
        &draft_id,
        draft_rev,
        &generation,
        ExternalResolution::Keep,
        "keep-draft",
    )
    .await;
    assert!(accepted);
    assert!(kept_dirty);
    assert_eq!(kept_rev, draft_rev);
    assert_eq!(kept_text, "my local draft\n");
    save_error(&mut client, &draft_id, draft_rev, "save-after-keep").await;

    let (reloaded_rev, reloaded_text, still_dirty, accepted) = resolve(
        &mut client,
        &draft_id,
        draft_rev,
        &generation,
        ExternalResolution::Reload,
        "reload-disk",
    )
    .await;
    assert!(accepted);
    assert!(!still_dirty);
    assert_eq!(reloaded_rev, draft_rev + 1);
    assert_eq!(reloaded_text, "agent update\n");
    let (sync_rev, sync_text, accepted) = sync_text(&mut client, &draft_id, "reload-sync").await;
    assert!(accepted);
    assert_eq!(
        (sync_rev, sync_text.as_str()),
        (reloaded_rev, "agent update\n")
    );

    drop(client);
    drop(_backend);
    let _ = std::fs::remove_dir_all(scratch);
}

#[tokio::test]
async fn external_change_requests_from_peer_without_capability_are_rejected() {
    let scratch = std::env::temp_dir().join(format!(
        "fresh-gui-external-legacy-{}",
        uuid::Uuid::new_v4()
    ));
    std::fs::create_dir_all(&scratch).unwrap();
    let addr = free_addr();
    let _backend = start(addr, &scratch);
    wait_health(addr);
    let mut client = Client::connect(
        ConnectOptions::new(format!("ws://{addr}/ws")).with_token("external-change-test-token"),
    )
    .await
    .expect("connect legacy client");
    let mut old_caps = Hello::default_client_caps();
    old_caps.retain(|cap| cap != CAP_EDITOR_EXTERNAL_CHANGES);
    client
        .send(Message::Hello(Hello::client("legacy-ade-test", old_caps)))
        .await
        .unwrap();
    client
        .send(Message::BufferExternalCheck {
            request_id: "legacy-external-check".into(),
            buffer_id: "unknown-buffer".into(),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.unwrap() {
                Message::Error { code, message }
                    if message.starts_with("legacy-external-check") =>
                {
                    assert_eq!(code, "capability_unavailable");
                    assert!(message.contains(CAP_EDITOR_EXTERNAL_CHANGES));
                    break;
                }
                Message::PtyData { .. } | Message::Pong { .. } | Message::Ping { .. } => {}
                other => panic!("unexpected legacy response: {other:?}"),
            }
        }
    })
    .await
    .expect("legacy capability response timed out");
    drop(client);
    drop(_backend);
    let _ = std::fs::remove_dir_all(scratch);
}

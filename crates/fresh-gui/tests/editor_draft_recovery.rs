//! ADE-level coverage for daemon-owned workspace draft recovery.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{CAP_EDITOR_DRAFT_RECOVERY, EditorDraftInfo, Message};

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
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

fn start(addr: SocketAddr, root: &Path, workspace_state: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_fresh-gui"))
        .arg("--foreground")
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--allow-no-auth")
        .arg("--root")
        .arg(root)
        .env("FRESH_GUI_WORKSPACES_FILE", workspace_state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("start daemon")
}

fn stop(mut child: Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

async fn list_drafts(client: &mut Client, request_id: &str) -> Vec<EditorDraftInfo> {
    client
        .send(Message::EditorDraftList {
            request_id: request_id.into(),
        })
        .await
        .unwrap();
    loop {
        match client.recv().await.unwrap() {
            Message::EditorDrafts {
                request_id: response_id,
                drafts,
            } if response_id == request_id => return drafts,
            Message::PtyData { .. } | Message::Pong { .. } | Message::Ping { .. } => {}
            other => panic!("unexpected draft-list response: {other:?}"),
        }
    }
}

#[tokio::test]
async fn named_draft_is_scoped_and_survives_daemon_restart() {
    let scratch = std::env::temp_dir().join(format!("fresh-gui-draft-ws-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&scratch).unwrap();
    let root = scratch.join("project");
    std::fs::create_dir_all(&root).unwrap();
    let source = root.join("note.txt");
    std::fs::write(&source, "original").unwrap();
    let workspace_state = scratch.join("workspaces.json");
    let addr = free_addr();
    let child = start(addr, &root, &workspace_state);
    wait_health(addr);
    let url = format!("ws://{addr}/ws");
    let mut client = Client::connect(ConnectOptions::new(&url)).await.unwrap();
    assert!(
        client
            .backend_hello
            .capabilities
            .iter()
            .any(|cap| cap == CAP_EDITOR_DRAFT_RECOVERY)
    );
    let workspace = client
        .create_workspace(Some("draft-test".into()), Some(root.display().to_string()))
        .await
        .unwrap();
    let other_workspace = client
        .create_workspace(Some("other".into()), Some(root.display().to_string()))
        .await
        .unwrap();
    client.switch_workspace(&workspace.id).await.unwrap();
    let (buffer_id, _, _, rev, _) = client
        .open_editor(source.display().to_string(), false)
        .await
        .unwrap();
    client
        .edit_buffer(buffer_id, rev, "unsaved draft")
        .await
        .unwrap();
    assert_eq!(std::fs::read_to_string(&source).unwrap(), "original");
    assert_eq!(list_drafts(&mut client, "draft-list-1").await.len(), 1);
    client.switch_workspace(&other_workspace.id).await.unwrap();
    assert!(list_drafts(&mut client, "draft-list-other").await.is_empty());
    assert!(client.close_workspace(&workspace.id).await.is_err());
    client.switch_workspace(&workspace.id).await.unwrap();
    drop(client);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(&workspace_state)
            .map(|text| text.contains(&workspace.id))
            .unwrap_or(false)
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        std::fs::read_to_string(&workspace_state)
            .unwrap()
            .contains(&workspace.id)
    );
    let mut child = child;
    let _ = child.kill();
    let _ = child.wait();

    let addr = free_addr();
    let child = start(addr, &root, &workspace_state);
    wait_health(addr);
    let url = format!("ws://{addr}/ws");
    let mut client = Client::connect(ConnectOptions::new(&url)).await.unwrap();
    client.switch_workspace(&workspace.id).await.unwrap();
    let drafts = list_drafts(&mut client, "draft-list-2").await;
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0].path.as_deref(), Some(source.to_str().unwrap()));
    assert!(!drafts[0].source_changed);
    client
        .send(Message::EditorDraftRestore {
            request_id: "draft-restore-1".into(),
            draft_id: drafts[0].draft_id.clone(),
        })
        .await
        .unwrap();
    let restored_id = loop {
        match client.recv().await.unwrap() {
            Message::EditorOpened {
                request_id,
                buffer_id,
                draft_id,
                ..
            } if request_id == "draft-restore-1" => {
                assert_eq!(draft_id.as_deref(), Some(drafts[0].draft_id.as_str()));
                break buffer_id;
            }
            Message::PtyData { .. } | Message::Pong { .. } | Message::Ping { .. } => {}
            other => panic!("unexpected restore response: {other:?}"),
        }
    };
    loop {
        match client.recv().await.unwrap() {
            Message::BufferSnapshot {
                buffer_id, text, ..
            } if buffer_id == restored_id => {
                assert_eq!(text, "unsaved draft");
                break;
            }
            Message::PtyData { .. } | Message::Pong { .. } | Message::Ping { .. } => {}
            other => panic!("unexpected restored snapshot: {other:?}"),
        }
    }
    client
        .send(Message::EditorDraftDiscard {
            request_id: "draft-discard-1".into(),
            buffer_id: restored_id,
        })
        .await
        .unwrap();
    loop {
        match client.recv().await.unwrap() {
            Message::EditorDraftDiscarded { request_id, .. } if request_id == "draft-discard-1" => {
                break;
            }
            Message::PtyData { .. } | Message::Pong { .. } | Message::Ping { .. } => {}
            other => panic!("unexpected discard response: {other:?}"),
        }
    }
    assert!(list_drafts(&mut client, "draft-list-3").await.is_empty());
    drop(client);
    stop(child);
    let _ = std::fs::remove_dir_all(scratch);
}

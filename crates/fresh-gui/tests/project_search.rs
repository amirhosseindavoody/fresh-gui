//! WebSocket coverage for daemon-side project search and replacement.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{
    CAP_PROJECT_SEARCH, Hello, Message, ProjectSearchFile, ProjectSearchRequest,
    ProjectSearchSelection, SearchOptions,
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
            .map(|status| status.success())
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
            .arg("--allow-no-auth")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn local daemon"),
    )
}

fn temp_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fresh-gui-project-search-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create test root");
    root
}

async fn connect(addr: SocketAddr) -> Client {
    Client::connect(ConnectOptions::new(format!("ws://{addr}/ws")))
        .await
        .expect("connect to daemon")
}

fn request(query: &str, replacement: &str) -> ProjectSearchRequest {
    ProjectSearchRequest {
        query: query.into(),
        replacement: replacement.into(),
        options: SearchOptions::default(),
        globs: Vec::new(),
        include_ignored: false,
        max_matches: 1_000,
    }
}

async fn search(
    client: &mut Client,
    id: &str,
    query: &str,
    replacement: &str,
) -> Vec<ProjectSearchFile> {
    client
        .send(Message::ProjectSearch {
            request_id: id.into(),
            search: request(query, replacement),
        })
        .await
        .expect("send project search");
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut files = Vec::new();
        loop {
            match client.recv().await.expect("receive project search") {
                Message::ProjectSearchFile { request_id, file } if request_id == id => {
                    files.push(file);
                }
                Message::ProjectSearchDone {
                    request_id,
                    error,
                    cancelled,
                    ..
                } if request_id == id => {
                    assert!(!cancelled, "search unexpectedly cancelled");
                    assert!(error.is_none(), "project search failed: {error:?}");
                    return files;
                }
                Message::Error { code, message } if message.starts_with(id) => {
                    panic!("project search failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. } => {}
                other => panic!("unexpected project search response: {other:?}"),
            }
        }
    })
    .await
    .expect("project search timed out")
}

async fn replace(
    client: &mut Client,
    request_id: &str,
    search_id: &str,
    selections: Vec<ProjectSearchSelection>,
) -> (
    Vec<(String, u64, String)>,
    Vec<fresh_gui_protocol::ProjectReplaceFileResult>,
) {
    client
        .send(Message::ProjectReplace {
            request_id: request_id.into(),
            search_id: search_id.into(),
            selections,
        })
        .await
        .expect("send project replace");
    tokio::time::timeout(Duration::from_secs(10), async {
        let mut snapshots = Vec::new();
        loop {
            match client.recv().await.expect("receive project replace") {
                Message::BufferSnapshot {
                    buffer_id,
                    rev,
                    text,
                    path: _,
                } => snapshots.push((buffer_id, rev, text)),
                Message::ProjectReplaceResult {
                    request_id: id,
                    files,
                } if id == request_id => {
                    snapshots.extend(
                        files
                            .iter()
                            .filter_map(|file| file.buffer.as_ref())
                            .map(|update| {
                                (update.buffer_id.clone(), update.rev, update.text.clone())
                            }),
                    );
                    return (snapshots, files);
                }
                Message::Error { code, message } if message.starts_with(request_id) => {
                    panic!("project replacement request failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. } => {}
                other => panic!("unexpected project replacement response: {other:?}"),
            }
        }
    })
    .await
    .expect("project replacement timed out")
}

fn selection(file: &ProjectSearchFile, indices: Vec<usize>) -> ProjectSearchSelection {
    ProjectSearchSelection {
        file_id: file.id.clone(),
        match_indices: indices,
    }
}

#[tokio::test]
async fn project_search_scopes_to_workspace_and_replaces_open_and_disk_sources_safely() {
    let scratch = temp_root();
    let root = scratch.join("workspace");
    fs::create_dir_all(&root).unwrap();
    let outside = scratch.join("outside.txt");
    let dirty_path = root.join("dirty.txt");
    let disk_path = root.join("disk.txt");
    fs::write(&outside, "needle outside").unwrap();
    fs::write(&dirty_path, "needle on disk").unwrap();
    fs::write(&disk_path, "needle on disk too").unwrap();

    let addr = free_loopback();
    let _backend = spawn_backend(addr, &scratch);
    wait_health(addr);
    let mut client = connect(addr).await;
    assert!(client.supports_capability(CAP_PROJECT_SEARCH));
    let workspace = client
        .create_workspace(
            Some("project-search-test".into()),
            Some(root.display().to_string()),
        )
        .await
        .unwrap();
    client.switch_workspace(&workspace.id).await.unwrap();

    let (buffer_id, _, _, rev, _) = client
        .open_editor(dirty_path.display().to_string(), false)
        .await
        .unwrap();
    let dirty_rev = client
        .edit_buffer(buffer_id.clone(), rev, "needle in dirty buffer")
        .await
        .unwrap();

    let files = search(&mut client, "search-open", "needle", "fresh").await;
    assert_eq!(
        files.len(),
        2,
        "workspace root excludes its parent directory"
    );
    assert!(
        !files
            .iter()
            .any(|file| file.path.as_deref() == Some(outside.to_str().unwrap()))
    );
    let open_file = files
        .iter()
        .find(|file| file.buffer_id.as_deref() == Some(buffer_id.as_str()))
        .expect("search used the dirty open buffer");
    let disk_file = files
        .iter()
        .find(|file| file.path.as_deref() == Some(disk_path.to_str().unwrap()))
        .expect("search found unopened disk file");
    assert_eq!(open_file.rev, Some(dirty_rev));

    let (snapshots, results) = replace(
        &mut client,
        "replace-open-and-disk",
        "search-open",
        vec![selection(open_file, vec![0]), selection(disk_file, vec![0])],
    )
    .await;
    assert!(
        results.iter().all(|file| file.error.is_none()),
        "{results:?}"
    );
    assert!(
        snapshots
            .iter()
            .any(|(id, _, text)| id == &buffer_id && text == "fresh in dirty buffer")
    );
    assert_eq!(fs::read_to_string(&dirty_path).unwrap(), "needle on disk");
    assert_eq!(fs::read_to_string(&disk_path).unwrap(), "fresh on disk too");

    // A revision change after search invalidates the cached replacement.
    let stale_files = search(&mut client, "search-stale", "fresh", "new").await;
    let stale_open = stale_files
        .iter()
        .find(|file| file.buffer_id.as_deref() == Some(buffer_id.as_str()))
        .unwrap();
    let latest_rev = stale_open.rev.unwrap();
    let changed_rev = client
        .edit_buffer(buffer_id.clone(), latest_rev, "fresh changed after search")
        .await
        .unwrap();
    let (_, stale_results) = replace(
        &mut client,
        "replace-stale",
        "search-stale",
        vec![selection(stale_open, vec![0])],
    )
    .await;
    assert!(stale_results[0].error.is_some());
    assert!(changed_rev > latest_rev);

    // Older clients must renegotiate without the additive capability and be rejected.
    let mut old_caps = Hello::default_client_caps();
    old_caps.retain(|capability| capability != CAP_PROJECT_SEARCH);
    client
        .send(Message::Hello(Hello::client(
            "legacy-project-search",
            old_caps,
        )))
        .await
        .unwrap();
    client
        .send(Message::ProjectSearch {
            request_id: "legacy-project-search".into(),
            search: request("fresh", "old"),
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.unwrap() {
                Message::Error { code, message }
                    if message.starts_with("legacy-project-search") =>
                {
                    assert_eq!(code, "capability_unavailable");
                    assert!(message.contains(CAP_PROJECT_SEARCH));
                    break;
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. } => {}
                other => panic!("unexpected legacy response: {other:?}"),
            }
        }
    })
    .await
    .expect("legacy capability rejection timed out");
    let _ = fs::remove_dir_all(scratch);
}

#[tokio::test]
async fn cancelled_search_and_new_query_keep_streams_correlated() {
    let root = temp_root();
    for index in 0..2_000 {
        fs::write(root.join(format!("file-{index}.txt")), "needle").unwrap();
    }
    let addr = free_loopback();
    let _backend = spawn_backend(addr, &root);
    wait_health(addr);
    let mut client = connect(addr).await;
    client
        .send(Message::ProjectSearch {
            request_id: "cancel-me".into(),
            search: request("needle", "one"),
        })
        .await
        .unwrap();
    client
        .send(Message::ProjectSearchCancel {
            request_id: "cancel-me".into(),
        })
        .await
        .unwrap();
    // Drain through the cancellation acknowledgement, then deliberately reuse
    // the wire ID. Queued frames must retain their private generation identity.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match client.recv().await.unwrap() {
                Message::ProjectSearchDone {
                    request_id,
                    cancelled: true,
                    ..
                } if request_id == "cancel-me" => break,
                Message::ProjectSearchFile { request_id, .. }
                | Message::ProjectSearchDone { request_id, .. }
                    if request_id == "cancel-me" => {}
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. } => {}
                other => panic!("unexpected cancellation response: {other:?}"),
            }
        }
    })
    .await
    .expect("cancellation acknowledgement timed out");
    client
        .send(Message::ProjectSearch {
            request_id: "cancel-me".into(),
            search: request("needle", "two"),
        })
        .await
        .unwrap();

    tokio::time::timeout(Duration::from_secs(15), async {
        let mut next_files = 0usize;
        loop {
            match client.recv().await.unwrap() {
                Message::ProjectSearchFile { request_id, file } if request_id == "cancel-me" => {
                    assert!(
                        file.matches
                            .iter()
                            .all(|matched| matched.replacement == "two")
                    );
                    next_files += 1;
                }
                Message::ProjectSearchDone {
                    request_id,
                    error,
                    cancelled,
                    ..
                } if request_id == "cancel-me" => {
                    assert!(!cancelled);
                    assert!(error.is_none());
                    break;
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. } => {}
                other => panic!("unexpected cancellation response: {other:?}"),
            }
        }
        assert!(next_files > 0);
    })
    .await
    .expect("cancel/new-search correlation timed out");
    let _ = fs::remove_dir_all(root);
}

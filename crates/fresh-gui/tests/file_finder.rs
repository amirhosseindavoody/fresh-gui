//! WebSocket coverage for daemon-side fuzzy file finding.
use std::{
    fs,
    net::SocketAddr,
    process::{Child, Command, Stdio},
    time::Duration,
};

use fresh::input::fuzzy::FuzzyMatcher;
use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{CAP_FILE_FINDER, Message};

fn free_loopback() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    address
}

fn temp_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fresh-gui-file-finder-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    root
}

struct Backend(Child);
impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_backend(address: SocketAddr, root: &std::path::Path) -> Backend {
    Backend(
        Command::new(env!("CARGO_BIN_EXE_fresh-gui"))
            .args(["--foreground", "--listen"])
            .arg(address.to_string())
            .arg("--allow-no-auth")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    )
}

fn wait_health(address: SocketAddr) {
    let url = format!("http://{address}/healthz");
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
    panic!("backend did not become healthy at {url}");
}

async fn connect(address: SocketAddr) -> Client {
    Client::connect(ConnectOptions::new(format!("ws://{address}/ws")))
        .await
        .unwrap()
}

async fn await_results(client: &mut Client, request_id: &str) -> (Vec<String>, bool, bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.unwrap() {
                Message::FileFinderResults {
                    request_id: id,
                    paths,
                    truncated,
                    cancelled,
                    error,
                } if id == request_id => {
                    assert!(error.is_none(), "file finder failed: {error:?}");
                    return (paths, truncated, cancelled);
                }
                Message::Error { code, message } if message.starts_with(request_id) => {
                    panic!("finder error {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::FsChanged { .. }
                | Message::Pong { .. }
                | Message::Ping { .. } => {}
                other => panic!("unexpected file finder response: {other:?}"),
            }
        }
    })
    .await
    .expect("file finder timed out")
}

#[tokio::test]
async fn finder_scopes_to_workspace_and_returns_fresh_ranked_gitignore_aware_paths() {
    let scratch = temp_root();
    let workspace_root = scratch.join("workspace");
    fs::create_dir_all(workspace_root.join("nested/deep")).unwrap();
    fs::create_dir_all(workspace_root.join("ignored")).unwrap();
    fs::write(workspace_root.join(".gitignore"), "ignored/\n").unwrap();
    fs::write(workspace_root.join("nested/deep/foo_bar.rs"), "").unwrap();
    fs::write(workspace_root.join("ignored/foo_bar.rs"), "").unwrap();
    fs::write(workspace_root.join("foobar.rs"), "").unwrap();
    fs::write(scratch.join("foobar.rs"), "outside").unwrap();
    let address = free_loopback();
    let _backend = spawn_backend(address, &scratch);
    wait_health(address);
    let mut client = connect(address).await;
    assert!(client.supports_capability(CAP_FILE_FINDER));
    let workspace = client
        .create_workspace(
            Some("finder-test".into()),
            Some(workspace_root.display().to_string()),
        )
        .await
        .unwrap();
    client.switch_workspace(&workspace.id).await.unwrap();
    client
        .send(Message::FileFinder {
            request_id: "finder-1".into(),
            query: "fbr".into(),
        })
        .await
        .unwrap();
    let (paths, truncated, cancelled) = await_results(&mut client, "finder-1").await;
    assert!(!truncated && !cancelled);
    let mut expected = vec!["foobar.rs".to_owned(), "nested/deep/foo_bar.rs".to_owned()];
    let mut matcher = FuzzyMatcher::new("fbr");
    expected.sort_by(|a, b| {
        matcher
            .match_target(b)
            .score
            .cmp(&matcher.match_target(a).score)
            .then_with(|| a.cmp(b))
    });
    assert_eq!(paths, expected);
    fs::remove_dir_all(scratch).unwrap();
}

#[tokio::test]
async fn finder_cancel_acknowledges_cancelled_request() {
    let root = temp_root();
    for directory in 0..64 {
        let nested = root.join(format!("dir-{directory:02}"));
        fs::create_dir_all(&nested).unwrap();
        for file in 0..100 {
            fs::write(nested.join(format!("file-{file:03}.rs")), "").unwrap();
        }
    }
    let address = free_loopback();
    let _backend = spawn_backend(address, &root);
    wait_health(address);
    let mut client = connect(address).await;
    client
        .send(Message::FileFinder {
            request_id: "finder-cancel".into(),
            query: "".into(),
        })
        .await
        .unwrap();
    client
        .send(Message::FileFinderCancel {
            request_id: "finder-cancel".into(),
        })
        .await
        .unwrap();
    let (paths, truncated, cancelled) = await_results(&mut client, "finder-cancel").await;
    assert!(paths.is_empty() && !truncated && cancelled);
    fs::remove_dir_all(root).unwrap();
}

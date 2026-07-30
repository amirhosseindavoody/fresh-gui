//! End-to-end: fs_watch delivers fs_changed when a file is written.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::Message;
use tokio::time::timeout;

fn wait_health(addr: SocketAddr) {
    let url = format!("http://{addr}/healthz");
    for _ in 0..100 {
        let ok = Command::new("curl")
            .args(["-sf", &url])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("backend did not become healthy at {url}");
}

fn spawn_backend(addr: SocketAddr, root: &std::path::Path) -> Child {
    let bin = env!("CARGO_BIN_EXE_fresh-gui");
    Command::new(bin)
        .arg("--foreground")
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--allow-no-auth")
        .arg("--root")
        .arg(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn backend")
}

#[tokio::test]
async fn fs_watch_notifies_on_write() {
    let tmp = std::env::temp_dir().join(format!("fresh-gui-watch-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut child = spawn_backend(addr, &tmp);
    wait_health(addr);

    let mut client = Client::connect(ConnectOptions::new(format!("ws://{addr}/ws")))
        .await
        .expect("connect");

    let (watch_id, _path) = client.watch_fs("", true).await.expect("watch");

    let marker = tmp.join("watched.txt");
    fs::write(&marker, b"hello-watch\n").expect("write");

    let deadline = Duration::from_secs(5);
    let start = std::time::Instant::now();
    let mut saw = false;
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        let msg = match timeout(remaining, client.recv()).await {
            Ok(Ok(m)) => m,
            _ => break,
        };
        if let Message::FsChanged {
            watch_id: wid,
            paths,
        } = msg
            && wid == watch_id
            && paths.iter().any(|p| p.contains("watched.txt"))
        {
            saw = true;
            break;
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&tmp);
    assert!(saw, "did not receive fs_changed for watched.txt");
}

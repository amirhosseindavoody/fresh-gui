//! End-to-end: backend FS list under sandboxed root.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::FsKind;

fn wait_health(addr: SocketAddr) {
    let url = format!("http://{addr}/healthz");
    for _ in 0..50 {
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
    let bin = env!("CARGO_BIN_EXE_fresh-gui-backend");
    Command::new(bin)
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
async fn fs_list_root() {
    let tmp = std::env::temp_dir().join(format!("fresh-gui-fs-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    fs::write(tmp.join("hello.txt"), b"hi").unwrap();
    fs::create_dir(tmp.join("nested")).unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut child = spawn_backend(addr, &tmp);
    wait_health(addr);

    let mut client = Client::connect(ConnectOptions::new(format!("ws://{addr}/ws")))
        .await
        .expect("connect");
    assert!(
        client
            .backend_hello
            .capabilities
            .iter()
            .any(|c| c == "fs")
    );

    let (path, entries) = client.list_dir("").await.expect("list");
    assert!(path.contains("fresh-gui-fs-e2e") || entries.len() >= 2);
    assert!(entries.iter().any(|e| e.name == "hello.txt" && e.kind == FsKind::File));
    assert!(entries.iter().any(|e| e.name == "nested" && e.kind == FsKind::Dir));

    let (_nested_path, nested) = client.list_dir("nested").await.expect("list nested");
    assert!(nested.is_empty());

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&tmp);
}

//! End-to-end: open a file via Fresh editor and receive a buffer snapshot.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::CAP_EDITOR;

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
    let bin = env!("CARGO_BIN_EXE_fresh-gui-backend");
    Command::new(bin)
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--root")
        .arg(root)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn backend")
}

#[tokio::test]
async fn editor_open_snapshot() {
    let tmp = std::env::temp_dir().join(format!("fresh-gui-ed-e2e-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    let marker = "fresh-gui-editor-phase3a";
    fs::write(tmp.join("sample.rs"), format!("// {marker}\nfn main() {{}}\n")).unwrap();

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
            .any(|c| c == CAP_EDITOR),
        "backend hello missing editor cap: {:?}",
        client.backend_hello.capabilities
    );

    let (buffer_id, path, _lang, rev, text) = client
        .open_editor("sample.rs", false)
        .await
        .expect("open_editor");
    assert!(!buffer_id.is_empty());
    assert!(path.contains("sample.rs"), "path={path}");
    assert_eq!(rev, 0);
    assert!(
        text.contains(marker),
        "snapshot missing marker; got {text:?}"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&tmp);
}

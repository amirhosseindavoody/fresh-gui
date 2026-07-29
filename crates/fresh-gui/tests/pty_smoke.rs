//! End-to-end: backend + client PTY smoke.

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::smoke_echo;

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

fn spawn_backend(addr: SocketAddr) -> Child {
    let bin = env!("CARGO_BIN_EXE_fresh-gui");
    Command::new(bin)
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--allow-no-auth")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn backend")
}

#[tokio::test]
async fn pty_smoke_echo() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut child = spawn_backend(addr);
    wait_health(addr);

    let ws = format!("ws://{addr}/ws");
    let out = smoke_echo(&ws, None).await.expect("smoke");
    assert!(out.contains("fresh-gui-ok"), "output was: {out:?}");

    let _ = child.kill();
    let _ = child.wait();
}

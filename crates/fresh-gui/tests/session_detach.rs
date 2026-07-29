//! Session survives WebSocket disconnect; multi-PTY attach restores ids.

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::Message;
use tokio::time::timeout;

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
async fn session_detach_reattach_keeps_pty() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut child = spawn_backend(addr);
    wait_health(addr);
    let ws = format!("ws://{addr}/ws");

    let mut c1 = Client::connect(ConnectOptions::new(&ws)).await.expect("c1");
    let sid = c1.create_session(None).await.expect("create");
    let pty = c1
        .open_pty(80, 24, None, Some("/bin/bash".into()))
        .await
        .expect("open pty");
    c1.write_pty(&pty, b"export FRESH_GUI_MARK=alive\\n")
        .await
        .expect("write");
    // Drain a bit of output so scrollback is non-empty.
    let _ = timeout(Duration::from_millis(300), c1.recv()).await;
    drop(c1); // disconnect — session should survive

    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut c2 = Client::connect(ConnectOptions::new(&ws)).await.expect("c2");
    let (ptys, _) = c2.attach_session(&sid).await.expect("attach");
    assert!(
        ptys.iter().any(|p| p.id == pty),
        "expected pty {pty} in {:?}",
        ptys
    );

    c2.write_pty(&pty, b"printf '%s\\n' \"$FRESH_GUI_MARK\"\\n")
        .await
        .expect("write2");

    let mut collected = String::new();
    let deadline = Duration::from_secs(5);
    let start = std::time::Instant::now();
    while start.elapsed() < deadline {
        let remaining = deadline.saturating_sub(start.elapsed());
        let msg = match timeout(remaining, c2.recv()).await {
            Ok(Ok(m)) => m,
            _ => break,
        };
        if let Message::PtyData { id, data } = msg
            && id == pty
        {
            let bytes = Client::decode_pty_data(&data).unwrap();
            collected.push_str(&String::from_utf8_lossy(&bytes));
            if collected.contains("alive") {
                let _ = child.kill();
                let _ = child.wait();
                return;
            }
        }
    }

    let _ = child.kill();
    let _ = child.wait();
    panic!("did not see alive marker after reattach; got {collected:?}");
}

#[tokio::test]
async fn multi_pty_in_session() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    let mut child = spawn_backend(addr);
    wait_health(addr);
    let ws = format!("ws://{addr}/ws");

    let mut c = Client::connect(ConnectOptions::new(&ws)).await.unwrap();
    c.create_session(None).await.unwrap();
    let a = c.open_pty(80, 24, None, Some("/bin/bash".into())).await.unwrap();
    let b = c.open_pty(80, 24, None, Some("/bin/bash".into())).await.unwrap();
    assert_ne!(a, b);
    let listed = c.list_sessions().await.unwrap();
    assert_eq!(listed[0].pty_count, 2);

    let _ = child.kill();
    let _ = child.wait();
}

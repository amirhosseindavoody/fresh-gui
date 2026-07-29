//! Closing a PTY must kill the shell process (not just drop the slot).

use std::net::SocketAddr;
use std::path::PathBuf;
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

fn proc_alive(pid: u32) -> bool {
    PathBuf::from(format!("/proc/{pid}")).is_dir()
}

#[tokio::test]
async fn pty_close_kills_shell_process() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut child = spawn_backend(addr);
    wait_health(addr);
    let ws = format!("ws://{addr}/ws");

    let mut c = Client::connect(ConnectOptions::new(&ws)).await.expect("connect");
    let _sid = c.create_session(None).await.expect("create");
    let pty = c
        .open_pty(80, 24, None, Some("/bin/bash".into()))
        .await
        .expect("open pty");

    let marker = std::env::temp_dir().join(format!("fresh-gui-pty-kill-{pty}"));
    let _ = std::fs::remove_file(&marker);

    // Publish shell PID, then replace the shell with a long sleep so kill targets a live process.
    let cmd = format!(
        "echo $$ > {}; exec sleep 600\n",
        marker.to_string_lossy()
    );
    c.write_pty(&pty, cmd.as_bytes()).await.expect("write");

    let pid = {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                if let Ok(pid) = text.trim().parse::<u32>() {
                    break pid;
                }
            }
            if std::time::Instant::now() > deadline {
                let _ = child.kill();
                panic!("shell did not write pid marker at {}", marker.display());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    assert!(proc_alive(pid), "expected shell pid {pid} to be alive before close");

    c.close_pty(&pty).await.expect("close");

    // Wait for protocol ack and OS reaping.
    let closed = timeout(Duration::from_secs(5), async {
        loop {
            match c.recv().await.expect("recv") {
                Message::PtyClosed { id, .. } if id == pty => break,
                Message::PtyData { .. } | Message::Pong { .. } | Message::Ping { .. } => {}
                other => panic!("unexpected while waiting for pty_closed: {other:?}"),
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "timed out waiting for pty_closed");

    let dead = {
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        loop {
            if !proc_alive(pid) {
                break true;
            }
            if std::time::Instant::now() > deadline {
                break false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    };
    let _ = std::fs::remove_file(&marker);
    let _ = child.kill();
    let _ = child.wait();
    assert!(dead, "shell pid {pid} still alive after pty_close");
}

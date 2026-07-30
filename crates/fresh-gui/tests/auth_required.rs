//! Auth policy: default requires a token; `--allow-no-auth` is loopback-only.

use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};

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

fn free_loopback() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);
    addr
}

#[tokio::test]
async fn default_requires_auth_token() {
    let addr = free_loopback();
    let bin = env!("CARGO_BIN_EXE_fresh-gui");
    let mut child = Command::new(bin)
        .arg("--foreground")
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--token")
        .arg("test-token-please")
        .arg("--no-editor")
        .arg("--no-ui")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn backend");
    wait_health(addr);

    let ws = format!("ws://{addr}/ws");

    // No token → privileged calls fail closed.
    let mut denied = Client::connect(ConnectOptions::new(&ws))
        .await
        .expect("ws hello without token still connects");
    let err = denied.create_session(None).await;
    assert!(err.is_err(), "expected session create without auth to fail");

    // Correct token → handshake succeeds.
    let mut ok = Client::connect(ConnectOptions::new(&ws).with_token("test-token-please"))
        .await
        .expect("authed connect");
    let _ = ok.create_session(None).await.expect("create session");

    let _ = child.kill();
    let _ = child.wait();
}

#[tokio::test]
async fn invalid_token_is_rejected() {
    let addr = free_loopback();
    let bin = env!("CARGO_BIN_EXE_fresh-gui");
    let mut child = Command::new(bin)
        .arg("--foreground")
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--token")
        .arg("correct-token")
        .arg("--no-editor")
        .arg("--no-ui")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn backend");
    wait_health(addr);

    let ws = format!("ws://{addr}/ws");
    match Client::connect(ConnectOptions::new(&ws).with_token("wrong-token")).await {
        Ok(_) => panic!("expected auth failure for invalid token"),
        Err(err) => {
            let msg = format!("{err:#}");
            assert!(
                msg.to_ascii_lowercase().contains("auth"),
                "unexpected error: {msg}"
            );
        }
    }

    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn allow_no_auth_rejected_on_non_loopback_cli() {
    let bin = env!("CARGO_BIN_EXE_fresh-gui");
    let status = Command::new(bin)
        .arg("--foreground")
        .arg("--listen")
        .arg("0.0.0.0:17999")
        .arg("--allow-no-auth")
        .arg("--no-editor")
        .arg("--no-ui")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run backend");
    assert!(!status.success(), "non-loopback --allow-no-auth must fail");
}

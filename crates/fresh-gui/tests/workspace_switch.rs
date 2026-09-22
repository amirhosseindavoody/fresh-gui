//! Workspaces stay on the daemon across client switches.
//!
//! Opening a terminal in workspace A must not show up in workspace B's session,
//! and switching back must still find A's PTY alive.

use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{Message, WorkspaceTab, WorkspaceTabKind};
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
        .arg("--foreground")
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--allow-no-auth")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn backend")
}

struct Stop(Child);

impl Drop for Stop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn term(title: &str, pty: &str) -> WorkspaceTab {
    WorkspaceTab {
        kind: WorkspaceTabKind::Terminal,
        title: title.to_owned(),
        pty_id: Some(pty.to_owned()),
        path: None,
    }
}

#[tokio::test]
async fn two_workspaces_keep_independent_ptys_across_switch() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let child = spawn_backend(addr);
    let _stop = Stop(child);
    wait_health(addr);
    let ws = format!("ws://{addr}/ws");

    let mut client = Client::connect(ConnectOptions::new(&ws))
        .await
        .expect("connect");
    assert!(
        client
            .backend_hello
            .capabilities
            .iter()
            .any(|cap| cap == "workspace")
    );

    let (listed, _) = client.list_workspaces().await.expect("list");
    assert!(listed.is_empty());

    let alpha = client
        .create_workspace(Some("alpha".into()), None)
        .await
        .expect("alpha");
    let beta = client
        .create_workspace(Some("beta".into()), None)
        .await
        .expect("beta");
    assert_ne!(alpha.session_id, beta.session_id);
    assert_ne!(alpha.id, beta.id);

    let side_dir = std::env::temp_dir().join(format!("fresh-gui-ws-{}", std::process::id()));
    std::fs::create_dir_all(&side_dir).expect("side dir");
    let side = client
        .create_workspace(Some("side".into()), Some(side_dir.display().to_string()))
        .await
        .expect("side");
    assert_eq!(
        side.root,
        side_dir.canonicalize().unwrap().display().to_string()
    );
    let renamed = client
        .rename_workspace(&alpha.id, "Alpha")
        .await
        .expect("rename");
    assert_eq!(renamed.name, "Alpha");
    client.close_workspace(&side.id).await.expect("close side");
    let (listed, _) = client.list_workspaces().await.expect("list after close");
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|workspace| workspace.id != side.id));

    let (alpha_now, _, _, ptys) = client
        .switch_workspace(&alpha.id)
        .await
        .expect("switch alpha");
    assert_eq!(alpha_now.name, "Alpha");
    assert!(ptys.is_empty());
    let pty_a = client
        .open_pty(80, 24, None, Some("/bin/bash".into()))
        .await
        .expect("pty a");
    client
        .write_pty(&pty_a, b"export FRESH_GUI_MARK=alpha\n")
        .await
        .expect("mark");
    let _ = timeout(Duration::from_millis(300), client.recv()).await;
    client
        .set_workspace_layout(&alpha.id, vec![term("alpha-term", &pty_a)], 0)
        .await
        .expect("layout a");

    let (beta_now, tabs_b, _, ptys_b) = client
        .switch_workspace(&beta.id)
        .await
        .expect("switch beta");
    assert_eq!(beta_now.id, beta.id);
    assert!(
        ptys_b.iter().all(|pty| pty.id != pty_a),
        "workspace B must not see A's pty, got {ptys_b:?}"
    );
    assert!(
        tabs_b
            .iter()
            .all(|tab| tab.pty_id.as_deref() != Some(pty_a.as_str())),
        "workspace B tabs leaked A's pty: {tabs_b:?}"
    );
    let pty_b = client
        .open_pty(80, 24, None, Some("/bin/bash".into()))
        .await
        .expect("pty b");
    assert_ne!(pty_a, pty_b);
    client
        .set_workspace_layout(&beta.id, vec![term("beta-term", &pty_b)], 0)
        .await
        .expect("layout b");

    let (back, tabs_a, _, ptys_a) = client
        .switch_workspace(&alpha.id)
        .await
        .expect("back to alpha");
    assert_eq!(back.session_id, alpha.session_id);
    assert!(
        ptys_a.iter().any(|pty| pty.id == pty_a),
        "A's pty should still be alive, got {ptys_a:?}"
    );
    assert!(
        ptys_a.iter().all(|pty| pty.id != pty_b),
        "B's pty should not be attached to A, got {ptys_a:?}"
    );
    assert!(
        tabs_a
            .iter()
            .any(|tab| tab.title == "alpha-term" && tab.pty_id.as_deref() == Some(pty_a.as_str())),
        "A's tab list was not restored: {tabs_a:?}"
    );

    client
        .write_pty(&pty_a, b"printf '%s\\n' \"$FRESH_GUI_MARK\"\n")
        .await
        .expect("read mark");
    let mut collected = String::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while std::time::Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let msg = match timeout(remaining, client.recv()).await {
            Ok(Ok(msg)) => msg,
            _ => break,
        };
        if let Message::PtyData { id, data } = msg
            && id == pty_a
        {
            let bytes = Client::decode_pty_data(&data).unwrap();
            collected.push_str(&String::from_utf8_lossy(&bytes));
            if collected.contains("alpha") {
                break;
            }
        }
    }
    assert!(
        collected.contains("alpha"),
        "A's shell did not survive the switch; output was {collected:?}"
    );

    drop(client);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut again = Client::connect(ConnectOptions::new(&ws))
        .await
        .expect("reconnect");
    let (listed, focused) = again.list_workspaces().await.expect("list after reconnect");
    assert_eq!(listed.len(), 2);
    assert_eq!(focused.as_deref(), Some(alpha.id.as_str()));
    let (_, _, _, ptys) = again
        .switch_workspace(&alpha.id)
        .await
        .expect("reattach alpha");
    assert!(
        ptys.iter().any(|pty| pty.id == pty_a),
        "daemon dropped A's pty while the client was gone: {ptys:?}"
    );

    let _ = std::fs::remove_dir_all(&side_dir);
}

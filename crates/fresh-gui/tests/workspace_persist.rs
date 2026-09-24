//! Workspaces survive a daemon restart when a state file is configured.
//!
//! PTYs do not: the restored tab keeps its title and its old `pty_id`, and the
//! new daemon reports no live PTYs so the host knows to start a fresh shell.

use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{Message, WorkspaceTab, WorkspaceTabKind};

fn free_addr() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().unwrap()
}

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

fn spawn_backend(addr: SocketAddr, state: &Path) -> Child {
    Command::new(env!("CARGO_BIN_EXE_fresh-gui"))
        .arg("--foreground")
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--allow-no-auth")
        .arg("--no-editor")
        .env("FRESH_GUI_WORKSPACES_FILE", state)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn backend")
}

/// SIGTERM so the daemon takes its graceful path and writes a final save.
fn stop(mut child: Child) {
    #[cfg(unix)]
    unsafe {
        libc::kill(child.id() as i32, libc::SIGTERM);
    }
    #[cfg(not(unix))]
    let _ = child.kill();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = child.kill();
    let _ = child.wait();
}

struct Guard(Option<Child>);

impl Drop for Guard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[tokio::test]
async fn workspaces_tabs_and_explorer_state_survive_daemon_restart() {
    let dir = std::env::temp_dir().join(format!("fresh-gui-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let project = dir.join("project");
    std::fs::create_dir_all(project.join("src")).unwrap();
    let project = project.canonicalize().unwrap().display().to_string();
    let state = dir.join("workspaces.json");

    let addr = free_addr();
    let mut guard = Guard(Some(spawn_backend(addr, &state)));
    wait_health(addr);
    let ws = format!("ws://{addr}/ws");

    let mut client = Client::connect(ConnectOptions::new(&ws)).await.unwrap();
    let alpha = client
        .create_workspace(Some("alpha".into()), None)
        .await
        .unwrap();
    let beta = client
        .create_workspace(Some("beta".into()), Some(project.clone()))
        .await
        .unwrap();
    client.switch_workspace(&beta.id).await.unwrap();
    let pty = client
        .open_pty(80, 24, None, Some("/bin/sh".into()))
        .await
        .unwrap();
    let src = format!("{project}/src");
    client
        .send(Message::WorkspaceLayoutSet {
            workspace_id: beta.id.clone(),
            tabs: vec![
                WorkspaceTab {
                    kind: WorkspaceTabKind::Terminal,
                    title: "build".into(),
                    pty_id: Some(pty.clone()),
                    path: None,
                },
                WorkspaceTab {
                    kind: WorkspaceTabKind::Editor,
                    title: "main.rs".into(),
                    pty_id: None,
                    path: Some(format!("{src}/main.rs")),
                },
            ],
            active_tab: 1,
            explorer_expanded: vec![src.clone()],
            extra: fresh_gui_protocol::WorkspaceLayoutExtra::default(),
        })
        .await
        .unwrap();
    client.rename_workspace(&alpha.id, "Alpha").await.unwrap();
    drop(client);

    stop(guard.0.take().unwrap());
    assert!(state.exists(), "daemon did not write {}", state.display());

    let addr = free_addr();
    guard.0 = Some(spawn_backend(addr, &state));
    wait_health(addr);
    let ws = format!("ws://{addr}/ws");
    let mut client = Client::connect(ConnectOptions::new(&ws)).await.unwrap();

    let (listed, focused) = client.list_workspaces().await.unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|ws| (ws.id.as_str(), ws.name.as_str()))
            .collect::<Vec<_>>(),
        [(alpha.id.as_str(), "Alpha"), (beta.id.as_str(), "beta")]
    );
    assert_eq!(focused.as_deref(), Some(beta.id.as_str()));
    assert_eq!(listed[1].root, project);

    client
        .send(Message::WorkspaceSwitch {
            workspace_id: beta.id.clone(),
        })
        .await
        .unwrap();
    let (tabs, active_tab, ptys, expanded) = loop {
        match client.recv().await.unwrap() {
            Message::WorkspaceSwitched {
                tabs,
                active_tab,
                ptys,
                explorer_expanded,
                ..
            } => break (tabs, active_tab, ptys, explorer_expanded),
            Message::Error { code, message } => panic!("{code}: {message}"),
            _ => continue,
        }
    };
    assert!(ptys.is_empty(), "no PTY outlives the daemon: {ptys:?}");
    assert_eq!(active_tab, 1);
    assert_eq!(tabs.len(), 2);
    assert_eq!(tabs[0].title, "build");
    assert_eq!(tabs[0].pty_id.as_deref(), Some(pty.as_str()));
    assert_eq!(
        tabs[1].path.as_deref(),
        Some(format!("{src}/main.rs").as_str())
    );
    assert_eq!(expanded, vec![src.clone()]);

    // The saved root is authorized again, so the explorer can list it.
    let (_, listing) = client.list_dir(&project).await.unwrap();
    assert!(listing.iter().any(|entry| entry.name == "src"));

    drop(client);
    drop(guard);
    let _ = std::fs::remove_dir_all(&dir);
}

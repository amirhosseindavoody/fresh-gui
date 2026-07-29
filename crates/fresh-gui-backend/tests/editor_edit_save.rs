//! End-to-end: edit + save via Fresh editor, then reopen shows new contents.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};

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
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn backend")
}

#[tokio::test]
async fn editor_edit_save_reopen() {
    let tmp = std::env::temp_dir().join(format!("fresh-gui-ed-save-{}", std::process::id()));
    let _ = fs::remove_dir_all(&tmp);
    fs::create_dir_all(&tmp).unwrap();
    let file = tmp.join("note.txt");
    fs::write(&file, b"version-one\n").unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().unwrap();
    drop(listener);

    let mut child = spawn_backend(addr, &tmp);
    wait_health(addr);

    let mut client = Client::connect(ConnectOptions::new(format!("ws://{addr}/ws")))
        .await
        .expect("connect");

    let (buffer_id, _path, _lang, rev, text) = client
        .open_editor("note.txt", false)
        .await
        .expect("open");
    assert!(text.contains("version-one"));
    assert_eq!(rev, 0);

    let rev = client
        .edit_buffer(&buffer_id, rev, "version-two\n")
        .await
        .expect("edit");
    assert_eq!(rev, 1);

    let (saved_path, rev) = client.save_buffer(&buffer_id, rev).await.expect("save");
    assert!(saved_path.contains("note.txt"));
    assert_eq!(rev, 2);

    let on_disk = fs::read_to_string(&file).expect("read disk");
    assert_eq!(on_disk, "version-two\n");

    let (_buffers, active) = client.scene_get().await.expect("scene");
    assert_eq!(active.as_deref(), Some(buffer_id.as_str()));

    // Re-open should see saved content at latest rev.
    let (_id2, _p2, _l2, rev2, text2) = client
        .open_editor("note.txt", false)
        .await
        .expect("reopen");
    assert!(text2.contains("version-two"));
    assert_eq!(rev2, 2);

    let _ = child.kill();
    let _ = child.wait();
    let _ = fs::remove_dir_all(&tmp);
}

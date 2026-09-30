//! Paged editor integration coverage and an opt-in large-file measurement.
//!
//! Run the measurement with:
//! `LARGE_FILE_MIB=100 pixi run -- cargo test -p fresh-gui --test editor_paged -- --ignored --nocapture`

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{
    ByteRange, ByteSelection, CAP_EDITOR_PAGED_READS, Hello, Message, RangeEdit, MAX_PAGE_BYTES,
};

fn free_loopback() -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let addr = listener.local_addr().expect("local address");
    drop(listener);
    addr
}

fn wait_health(addr: SocketAddr) {
    let url = format!("http://{addr}/healthz");
    for _ in 0..100 {
        if Command::new("curl")
            .args(["-sf", &url])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("backend did not become healthy at {url}");
}

struct Backend(Child);

impl Drop for Backend {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn spawn_backend(addr: SocketAddr, root: &std::path::Path) -> Backend {
    let child = Command::new(env!("CARGO_BIN_EXE_fresh-gui"))
        .arg("--foreground")
        .arg("--listen")
        .arg(addr.to_string())
        .arg("--allow-no-auth")
        .arg("--root")
        .arg(root)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn local daemon");
    Backend(child)
}

fn temp_root(tag: &str) -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fresh-gui-paged-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create test root");
    root
}

fn json_size(message: &Message) -> usize {
    serde_json::to_vec(message)
        .expect("serialize protocol message")
        .len()
}

async fn connect(addr: SocketAddr) -> Client {
    let url = format!("ws://{addr}/ws");
    let client = Client::connect(ConnectOptions::new(url))
        .await
        .expect("connect to daemon");
    assert!(client.supports_capability(CAP_EDITOR_PAGED_READS));
    client
}

struct PagedOpen {
    buffer_id: String,
    rev: u64,
    total_bytes: usize,
    wire_bytes: usize,
}

async fn open_paged(client: &mut Client, path: &str, request_id: &str) -> PagedOpen {
    let request = Message::EditorOpen {
        request_id: request_id.to_owned(),
        path: path.to_owned(),
        preview: false,
        cwd: None,
        line: None,
        column: None,
    };
    let mut wire_bytes = json_size(&request);
    client
        .send(request)
        .await
        .expect("send EditorOpen");
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut opened = None;
        loop {
            let message = client.recv().await.expect("receive open response");
            wire_bytes += json_size(&message);
            match message {
                Message::EditorOpened {
                    request_id: rid,
                    buffer_id,
                    ..
                } if rid == request_id => opened = Some(buffer_id),
                Message::BufferPaged {
                    buffer_id,
                    rev,
                    total_bytes,
                    ..
                } if opened.as_deref() == Some(buffer_id.as_str()) => {
                    return PagedOpen {
                        buffer_id,
                        rev,
                        total_bytes,
                        wire_bytes,
                    }
                }
                Message::Error { code, message } => {
                    panic!("paged open failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::PtyClosed { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::FsChanged { .. } => {}
                other => panic!("unexpected open response: {other:?}"),
            }
        }
    })
    .await
    .expect("paged open timed out")
}

async fn read_page(
    client: &mut Client,
    buffer_id: &str,
    start: usize,
    len: usize,
    request_id: &str,
) -> (u64, usize, usize, String, usize) {
    let request = Message::BufferRead {
        request_id: request_id.to_owned(),
        buffer_id: buffer_id.to_owned(),
        view_id: "paged-test-view".into(),
        start,
        len,
    };
    let mut wire_bytes = json_size(&request);
    client
        .send(request)
        .await
        .expect("send BufferRead");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let message = client.recv().await.expect("receive page");
            wire_bytes += json_size(&message);
            match message {
                Message::BufferPage {
                    request_id: rid,
                    rev,
                    start,
                    total_bytes,
                    text,
                    accepted,
                    ..
                } if rid == request_id => {
                    assert!(accepted, "page request was rejected");
                    return (rev, start, total_bytes, text, wire_bytes);
                }
                Message::Error { code, message } if message.starts_with(request_id) => {
                    panic!("page request failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::PtyClosed { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::FsChanged { .. } => {}
                other => panic!("unexpected page response: {other:?}"),
            }
        }
    })
    .await
    .expect("page request timed out")
}

async fn range_edit(
    client: &mut Client,
    buffer_id: &str,
    base_rev: u64,
    edit: RangeEdit,
    request_id: &str,
) -> (u64, bool, String, usize) {
    let request = Message::BufferRangeEdit {
        request_id: request_id.to_owned(),
        buffer_id: buffer_id.to_owned(),
        view_id: "paged-test-view".into(),
        base_rev,
        edits: vec![edit],
        viewport: Some(ByteRange {
            start: 0,
            len: 128,
        }),
        selection: ByteSelection { anchor: 0, head: 0 },
    };
    let mut wire_bytes = json_size(&request);
    client
        .send(request)
        .await
        .expect("send range edit");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let message = client.recv().await.expect("receive edit result");
            wire_bytes += json_size(&message);
            match message {
                Message::BufferPage {
                    request_id: rid,
                    rev,
                    accepted,
                    text,
                    ..
                } if rid == request_id => return (rev, accepted, text, wire_bytes),
                Message::Error { code, message } if message.starts_with(request_id) => {
                    panic!("range edit failed: {code}: {message}")
                }
                Message::PtyData { .. }
                | Message::PtyClosed { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::FsChanged { .. } => {}
                other => panic!("unexpected edit response: {other:?}"),
            }
        }
    })
    .await
    .expect("range edit timed out")
}

fn fixture_bytes() -> Vec<u8> {
    // 3 MiB of ordinary UTF-8 text, with a long line and multibyte scalars
    // positioned around a page boundary. A zero byte would be classified as
    // binary by the editor and would not exercise the text paging path.
    let mut bytes = vec![b'x'; 3 * 1024 * 1024];
    bytes[0..8].copy_from_slice(b"start---");
    bytes[65_533..65_543].copy_from_slice("🦀界e\u{301}".as_bytes());
    let end = bytes.len();
    bytes[end - 8..].copy_from_slice(b"---end!!");
    bytes
}

#[tokio::test]
async fn paged_reads_and_incremental_edits_preserve_large_utf8_file() {
    let root = temp_root("functional");
    let file = root.join("large.txt");
    let original = fixture_bytes();
    fs::write(&file, &original).expect("write fixture");
    let addr = free_loopback();
    let _backend = spawn_backend(addr, &root);
    wait_health(addr);
    let mut client = connect(addr).await;

    let opened = open_paged(&mut client, "large.txt", "paged-open").await;
    assert_eq!(opened.total_bytes, original.len());

    let (_, start, total, page, _) = read_page(
        &mut client,
        &opened.buffer_id,
        0,
        MAX_PAGE_BYTES,
        "page-zero",
    )
    .await;
    assert_eq!(start, 0);
    assert_eq!(total, original.len());
    assert!(page.len() <= MAX_PAGE_BYTES);
    assert!(page.starts_with("start---"));

    // A requested offset inside a multibyte scalar must be rejected without
    // changing the buffer; a valid boundary page around the same scalar works.
    let boundary = 65_533;
    let scalar_page = read_page(
        &mut client,
        &opened.buffer_id,
        boundary,
        64,
        "page-utf8-boundary",
    )
    .await;
    assert!(scalar_page.3.starts_with("🦀界e\u{301}"));
    assert!(scalar_page.3.len() <= 64);

    let insertion = RangeEdit {
        start: 8,
        end: 8,
        text: "paged-edit-🦀".into(),
    };
    let (rev, accepted, viewport, _) = range_edit(
        &mut client,
        &opened.buffer_id,
        opened.rev,
        insertion,
        "paged-edit",
    )
    .await;
    assert!(accepted);
    assert_eq!(rev, opened.rev + 1);
    assert!(viewport.starts_with("start---paged-edit-🦀"));

    let before_bad_edit = fs::read(&file).expect("read original disk file");
    let (stale_rev, stale_accepted, _, _) = range_edit(
        &mut client,
        &opened.buffer_id,
        opened.rev,
        RangeEdit {
            start: 0,
            end: 0,
            text: "must-not-apply".into(),
        },
        "paged-stale-edit",
    )
    .await;
    assert!(!stale_accepted);
    assert_eq!(stale_rev, rev);
    assert_eq!(fs::read(&file).expect("disk remains unchanged"), before_bad_edit);

    client
        .send(Message::BufferRangeEdit {
            request_id: "paged-invalid-utf8-edit".into(),
            buffer_id: opened.buffer_id.clone(),
            view_id: "paged-test-view".into(),
            base_rev: rev,
            edits: vec![RangeEdit {
                start: boundary + 1,
                end: boundary + 1,
                text: "bad-offset".into(),
            }],
            viewport: Some(ByteRange { start: 0, len: 64 }),
            selection: ByteSelection { anchor: 0, head: 0 },
        })
        .await
        .expect("send invalid UTF-8 edit");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match client.recv().await.expect("receive malformed-edit response") {
                Message::Error { code, message }
                    if message.starts_with("paged-invalid-utf8-edit") =>
                {
                    assert_eq!(code, "buffer_edit_failed");
                    break;
                }
                Message::PtyData { .. }
                | Message::PtyClosed { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::FsChanged { .. } => {}
                other => panic!("unexpected malformed-edit response: {other:?}"),
            }
        }
    })
    .await
    .expect("malformed-edit response timed out");
    let (unchanged_rev, _, _, unchanged_page, _) = read_page(
        &mut client,
        &opened.buffer_id,
        0,
        64,
        "paged-read-after-invalid-edit",
    )
    .await;
    assert_eq!(unchanged_rev, rev);
    assert!(unchanged_page.starts_with("start---paged-edit-🦀"));

    client
        .send(Message::BufferSave {
            request_id: "paged-save".into(),
            buffer_id: opened.buffer_id.clone(),
            base_rev: rev,
            path: String::new(),
        })
        .await
        .expect("send save");
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match client.recv().await.expect("receive save") {
                Message::BufferSaved {
                    request_id,
                    rev: saved_rev,
                    ..
                } if request_id == "paged-save" => {
                    assert_eq!(saved_rev, rev + 1);
                    break;
                }
                Message::Error { code, message } => panic!("save failed: {code}: {message}"),
                Message::PtyData { .. }
                | Message::PtyClosed { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::FsChanged { .. } => {}
                other => panic!("unexpected save response: {other:?}"),
            }
        }
    })
    .await
    .expect("save timed out");

    let mut expected = original;
    expected.splice(8..8, "paged-edit-🦀".bytes());
    assert_eq!(fs::read(&file).expect("read saved file"), expected);
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn older_client_without_paged_capability_gets_explicit_rejection() {
    let root = temp_root("old-client");
    fs::write(root.join("large.txt"), fixture_bytes()).expect("write fixture");
    let addr = free_loopback();
    let _backend = spawn_backend(addr, &root);
    wait_health(addr);
    let mut client = connect(addr).await;
    let mut old_caps = Hello::default_client_caps();
    old_caps.retain(|capability| capability != CAP_EDITOR_PAGED_READS);
    client
        .send(Message::Hello(Hello::client("legacy-test-client", old_caps)))
        .await
        .expect("renegotiate as a legacy client");
    client
        .send(Message::EditorOpen {
            request_id: "legacy-paged-open".into(),
            path: "large.txt".into(),
            preview: false,
            cwd: None,
            line: None,
            column: None,
        })
        .await
        .expect("send legacy open");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match client.recv().await.expect("receive legacy open response") {
                Message::Error { code, message }
                    if message.starts_with("legacy-paged-open") =>
                {
                    assert_eq!(code, "capability_unavailable");
                    assert!(message.contains(CAP_EDITOR_PAGED_READS));
                    break;
                }
                Message::PtyData { .. }
                | Message::PtyClosed { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::FsChanged { .. } => {}
                other => panic!("unexpected legacy response: {other:?}"),
            }
        }
    })
    .await
    .expect("legacy rejection timed out");
    let _ = fs::remove_dir_all(root);
}

#[tokio::test]
async fn paged_buffers_remain_scoped_to_their_workspace() {
    let root = temp_root("workspace-daemon-root");
    let alpha_root = temp_root("workspace-alpha");
    let beta_root = temp_root("workspace-beta");
    let mut alpha_text = fixture_bytes();
    alpha_text[..5].copy_from_slice(b"alpha");
    let mut beta_text = fixture_bytes();
    beta_text[..4].copy_from_slice(b"beta");
    fs::write(alpha_root.join("same.txt"), alpha_text).expect("write alpha");
    fs::write(beta_root.join("same.txt"), beta_text).expect("write beta");
    let addr = free_loopback();
    let _backend = spawn_backend(addr, &root);
    wait_health(addr);
    let mut client = connect(addr).await;
    let alpha = client
        .create_workspace(Some("paged alpha".into()), Some(alpha_root.display().to_string()))
        .await
        .expect("create alpha workspace");
    let beta = client
        .create_workspace(Some("paged beta".into()), Some(beta_root.display().to_string()))
        .await
        .expect("create beta workspace");

    client.switch_workspace(&alpha.id).await.expect("switch alpha");
    let alpha_buffer = open_paged(&mut client, "same.txt", "alpha-open").await;
    let (_, _, _, alpha_page, _) =
        read_page(&mut client, &alpha_buffer.buffer_id, 0, 64, "alpha-read").await;
    assert!(alpha_page.contains("alpha contents"));

    client.switch_workspace(&beta.id).await.expect("switch beta");
    client
        .send(Message::BufferRead {
            request_id: "cross-workspace-read".into(),
            buffer_id: alpha_buffer.buffer_id,
            view_id: "paged-test-view".into(),
            start: 0,
            len: 64,
        })
        .await
        .expect("send cross-workspace read");
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            match client.recv().await.expect("receive workspace isolation response") {
                Message::Error { message, .. } if message.starts_with("cross-workspace-read") => {
                    assert!(message.contains("workspace"), "unexpected error: {message}");
                    break;
                }
                Message::PtyData { .. }
                | Message::PtyClosed { .. }
                | Message::Pong { .. }
                | Message::Ping { .. }
                | Message::FsChanged { .. } => {}
                other => panic!("unexpected workspace isolation response: {other:?}"),
            }
        }
    })
    .await
    .expect("workspace isolation response timed out");

    let beta_buffer = open_paged(&mut client, "same.txt", "beta-open").await;
    let (_, _, _, beta_page, _) =
        read_page(&mut client, &beta_buffer.buffer_id, 0, 64, "beta-read").await;
    assert!(beta_page.contains("beta contents"));
    let _ = fs::remove_dir_all(root);
    let _ = fs::remove_dir_all(alpha_root);
    let _ = fs::remove_dir_all(beta_root);
}

#[tokio::test]
#[ignore = "measurement harness; run with LARGE_FILE_MIB=100 and --ignored --nocapture"]
async fn measure_large_file_open_range_edit_and_daemon_rss() {
    let mib = std::env::var("LARGE_FILE_MIB")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(100);
    let bytes = mib.saturating_mul(1024 * 1024);
    assert!(bytes > 2 * 1024 * 1024, "fixture must exceed snapshot cap");
    let root = temp_root("measurement");
    let file = root.join("measurement.txt");
    let mut output = std::fs::File::create(&file).expect("create fixture");
    let chunk = vec![b'a'; 1024 * 1024];
    for _ in 0..mib {
        use std::io::Write;
        output.write_all(&chunk).expect("write fixture chunk");
    }
    drop(output);

    let addr = free_loopback();
    let mut backend = spawn_backend(addr, &root);
    wait_health(addr);
    let pid = backend.0.id();
    let mut client = connect(addr).await;

    let start = Instant::now();
    let opened = open_paged(&mut client, "measurement.txt", "measure-open").await;
    let open_ms = start.elapsed().as_millis();
    let start = Instant::now();
    let (rev, _page_start, reported_total, viewport, page_transfer_bytes) = read_page(
        &mut client,
        &opened.buffer_id,
        bytes / 2,
        MAX_PAGE_BYTES,
        "measure-range",
    )
    .await;
    let page_ms = start.elapsed().as_millis();
    assert_eq!(reported_total, bytes);
    assert!(viewport.len() <= MAX_PAGE_BYTES);

    let start = Instant::now();
    let (edit_rev, accepted, _, edit_transfer_bytes) = range_edit(
        &mut client,
        &opened.buffer_id,
        rev,
        RangeEdit {
            start: bytes / 2,
            end: bytes / 2 + 1,
            text: "b".into(),
        },
        "measure-edit",
    )
    .await;
    let edit_ms = start.elapsed().as_millis();
    assert!(accepted);

    // /proc reports the child's high-water resident set; sample too, so this
    // still records a useful peak if the daemon has not updated VmHWM yet.
    let peak_rss_kib = peak_rss_kib(pid);
    let transfer_bytes = opened.wire_bytes + page_transfer_bytes + edit_transfer_bytes;
    println!(
        "LARGE_FILE_RESULT {{\"fixture_bytes\":{bytes},\"open_ms\":{open_ms},\"page_ms\":{page_ms},\"edit_ack_ms\":{edit_ms},\"page_payload_bytes\":{},\"measured_full_duplex_json_bytes\":{transfer_bytes},\"daemon_peak_rss_kib\":{peak_rss_kib},\"resulting_rev\":{edit_rev}}}",
        viewport.len()
    );

    let _ = backend.0.kill();
    let _ = backend.0.wait();
    let _ = fs::remove_dir_all(root);
}

fn peak_rss_kib(pid: u32) -> u64 {
    let status = fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
    status
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse().ok())
        })
        .unwrap_or(0)
}

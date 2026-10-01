//! WebSocket protocol coverage for Fresh-backed in-buffer search previews.

use std::fs;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use fresh_gui_client::{Client, ConnectOptions};
use fresh_gui_protocol::{
    ByteRange, CAP_EDITOR_SEARCH, Hello, Message, SearchOptions, SearchMatch,
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
    Backend(
        Command::new(env!("CARGO_BIN_EXE_fresh-gui"))
            .arg("--foreground")
            .arg("--listen")
            .arg(addr.to_string())
            .arg("--allow-no-auth")
            .arg("--root")
            .arg(root)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn local daemon"),
    )
}

fn temp_root() -> std::path::PathBuf {
    let root = std::env::temp_dir().join(format!(
        "fresh-gui-search-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos()
    ));
    fs::create_dir_all(&root).expect("create test root");
    root
}

async fn connect(addr: SocketAddr) -> Client {
    Client::connect(ConnectOptions::new(format!("ws://{addr}/ws")))
        .await
        .expect("connect to daemon")
}

async fn send_search(client: &mut Client, request_id: &str, text: &str, query: &str,
    replacement: &str, options: SearchOptions, scope: Option<ByteRange>) -> (Vec<SearchMatch>, Option<String>, bool) {
    client.send(Message::BufferSearch {
        request_id: request_id.into(),
        text: text.into(),
        query: query.into(),
        replacement: replacement.into(),
        options,
        scope,
    }).await.expect("send search request");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive search result") {
                Message::BufferSearchResult { request_id: rid, matches, error, capped }
                    if rid == request_id => return (matches, error, capped),
                Message::Error { code, message } if message.starts_with(request_id) => {
                    panic!("search request failed: {code}: {message}")
                }
                Message::PtyData { .. } | Message::FsChanged { .. }
                | Message::Pong { .. } | Message::Ping { .. } => {}
                other => panic!("unexpected response: {other:?}"),
            }
        }
    }).await.expect("search response timed out")
}

#[tokio::test]
async fn daemon_advertises_and_serves_fresh_compatible_search_preview() {
    let root = temp_root();
    let addr = free_loopback();
    let _backend = spawn_backend(addr, &root);
    wait_health(addr);
    let mut client = connect(addr).await;
    assert!(client.supports_capability(CAP_EDITOR_SEARCH));

    let (matches, error, capped) = send_search(
        &mut client, "literal", "a.b ab", "a.b", "$1\\n",
        SearchOptions::default(), None,
    ).await;
    assert!(error.is_none());
    assert!(!capped);
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0], SearchMatch { start: 0, end: 3, replacement: "$1\\n".into() });

    let (matches, error, _) = send_search(
        &mut client, "regex", "ab", "(a)(b)", "$2-$1\\n",
        SearchOptions { use_regex: true, ..SearchOptions::default() }, None,
    ).await;
    assert!(error.is_none());
    assert_eq!(matches[0].replacement, "b-a\n");

    let (matches, error, _) = send_search(
        &mut client, "invalid-regex", "abc", "[", "x",
        SearchOptions { use_regex: true, ..SearchOptions::default() }, None,
    ).await;
    assert!(matches.is_empty());
    assert!(error.is_some());

    let text = "head-éx-tail";
    let scope_start = "head-".len();
    let scope = ByteRange { start: scope_start, len: "éx".len() };
    let (matches, error, _) = send_search(
        &mut client, "scoped-zero-width", text, "()", "!",
        SearchOptions { use_regex: true, ..SearchOptions::default() }, Some(scope),
    ).await;
    assert!(error.is_none());
    assert_eq!(matches.iter().map(|matched| matched.start).collect::<Vec<_>>(), [5, 7, 8]);

    // Older clients renegotiate their capabilities before issuing a request.
    let mut old_caps = Hello::default_client_caps();
    old_caps.retain(|capability| capability != CAP_EDITOR_SEARCH);
    client.send(Message::Hello(Hello::client("legacy-search-client", old_caps)))
        .await.expect("renegotiate without search support");
    client.send(Message::BufferSearch {
        request_id: "legacy-search".into(),
        text: "hello".into(),
        query: "hello".into(),
        replacement: "hi".into(),
        options: SearchOptions::default(),
        scope: None,
    }).await.expect("send legacy search request");
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match client.recv().await.expect("receive legacy response") {
                Message::Error { code, message } if message.starts_with("legacy-search") => {
                    assert_eq!(code, "capability_unavailable");
                    assert!(message.contains(CAP_EDITOR_SEARCH));
                    break;
                }
                Message::PtyData { .. } | Message::FsChanged { .. }
                | Message::Pong { .. } | Message::Ping { .. } => {}
                other => panic!("unexpected legacy response: {other:?}"),
            }
        }
    }).await.expect("legacy capability rejection timed out");

    let _ = fs::remove_dir_all(root);
}

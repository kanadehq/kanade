//! Minimal fake HTTP backend shared by the HTTP subcommand tests.
//!
//! `cmd::group` carries its own private copy of this helper. The copy here is
//! deliberate: it also records request headers, and keeping it separate lets
//! the two evolve without touching each other's tests.

use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One request as the fake backend saw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    /// Request target, query string included.
    pub target: String,
    pub body: String,
    /// The body exactly as received. `body` is lossy UTF-8, which cannot tell
    /// a faithful binary upload from a mangled one.
    pub raw: Vec<u8>,
    /// Lower-cased header block, one `name: value` per line.
    pub headers: String,
}

/// Serve on an ephemeral port: the Nth request gets the Nth `(status, body)`.
/// Returns the base URL and the log of what was received.
pub async fn fake_backend(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    fake_backend_bytes(
        replies
            .into_iter()
            .map(|(status, body)| (status, body.as_bytes().to_vec()))
            .collect(),
    )
    .await
}

/// Same as [`fake_backend`] with raw reply bodies, for download endpoints.
pub async fn fake_backend_bytes(replies: Vec<(u16, Vec<u8>)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
    let log = seen.clone();
    tokio::spawn(async move {
        for (status, body) in replies {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let mut buf = Vec::new();
            let (head_end, content_len) = loop {
                let mut chunk = [0u8; 4096];
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                    let len = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    break (pos + 4, len);
                }
            };
            while buf.len() < head_end + content_len {
                let mut chunk = [0u8; 4096];
                let n = sock.read(&mut chunk).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let head_txt = String::from_utf8_lossy(&buf[..head_end.min(buf.len())]).to_string();
            let head_txt = head_txt.as_str();
            let mut first = head_txt.lines().next().unwrap_or("").split_whitespace();
            let method = first.next().unwrap_or("").to_string();
            let target = first.next().unwrap_or("").to_string();
            let headers = head_txt
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            let raw = buf[head_end.min(buf.len())..].to_vec();
            log.lock().unwrap().push(Seen {
                method,
                target,
                body: String::from_utf8_lossy(&raw).to_string(),
                raw,
                headers,
            });
            let mut resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            )
            .into_bytes();
            resp.extend_from_slice(&body);
            let _ = sock.write_all(&resp).await;
        }
    });
    (base, seen)
}

/// Snapshot of the requests received so far.
pub fn seen(log: &Arc<Mutex<Vec<Seen>>>) -> Vec<Seen> {
    log.lock().unwrap().clone()
}

/// Token the tests export so the Bearer header is observable. It is set once
/// and never removed: every test sees the same value, so concurrent tests
/// cannot disturb each other.
pub const TEST_TOKEN: &str = "test-operator-token";

pub fn export_test_token() {
    // SAFETY: only ever writes the same constant, and nothing in the test
    // binary reads this variable expecting it to be absent.
    unsafe { std::env::set_var("KANADE_AUTH_TOKEN", TEST_TOKEN) };
}

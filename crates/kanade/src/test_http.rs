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
    /// Lower-cased header block, one `name: value` per line.
    pub headers: String,
}

/// Serve on an ephemeral port: the Nth request gets the Nth `(status, body)`.
/// Returns the base URL and the log of what was received.
pub async fn fake_backend(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
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
            let text = String::from_utf8_lossy(&buf).to_string();
            let head_txt = &text[..head_end.min(text.len())];
            let mut first = head_txt.lines().next().unwrap_or("").split_whitespace();
            let method = first.next().unwrap_or("").to_string();
            let target = first.next().unwrap_or("").to_string();
            let headers = head_txt
                .lines()
                .skip(1)
                .collect::<Vec<_>>()
                .join("\n")
                .to_lowercase();
            let body_txt = text[head_end.min(text.len())..].to_string();
            log.lock().unwrap().push(Seen {
                method,
                target,
                body: body_txt,
                headers,
            });
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes()).await;
        }
    });
    (base, seen)
}

/// Snapshot of the requests received so far.
pub fn seen(log: &Arc<Mutex<Vec<Seen>>>) -> Vec<Seen> {
    log.lock().unwrap().clone()
}

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use serde::Deserialize;

#[derive(Args, Debug)]
pub struct JetstreamArgs {
    #[command(subcommand)]
    pub sub: JetstreamSub,
}

#[derive(Subcommand, Debug)]
pub enum JetstreamSub {
    /// Print current state of streams + KV buckets + object stores, as the
    /// backend sees them. Goes through the backend API (needs
    /// KANADE_AUTH_TOKEN), not NATS.
    Status,
}

/// One resource in the backend's `GET /api/jetstream/status` snapshot. Usage
/// numbers are optional so a missing value is never shown as a made-up zero.
#[derive(Deserialize, Debug)]
struct Probe {
    name: String,
    exists: bool,
    #[serde(default)]
    bytes: Option<u64>,
    #[serde(default)]
    messages: Option<u64>,
}

#[derive(Deserialize, Debug)]
struct Snapshot {
    streams: Vec<Probe>,
    kv_buckets: Vec<Probe>,
    object_stores: Vec<Probe>,
}

fn render_status(snap: &Snapshot) -> String {
    use std::fmt::Write;
    fn num(v: Option<u64>) -> String {
        v.map_or_else(|| "?".to_string(), |n| n.to_string())
    }
    let mut out = String::from("streams:\n");
    for p in &snap.streams {
        if p.exists {
            let _ = writeln!(
                out,
                "  {}: messages={}, bytes={}",
                p.name,
                num(p.messages),
                num(p.bytes)
            );
        } else {
            let _ = writeln!(out, "  {}: NOT FOUND", p.name);
        }
    }
    for (title, probes) in [
        ("KV buckets:", &snap.kv_buckets),
        ("object stores:", &snap.object_stores),
    ] {
        let _ = writeln!(out, "{title}");
        for p in probes {
            let state = if p.exists { "OK" } else { "NOT FOUND" };
            let _ = writeln!(out, "  {}: {state}", p.name);
        }
    }
    out
}

pub async fn status(backend_url: &str) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    let url = format!("{base}/api/jetstream/status");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("jetstream status failed: {status} — {body}");
    }
    let snap: Snapshot = resp
        .json()
        .await
        .with_context(|| format!("parse JSON response from GET {url}"))?;
    print!("{}", render_status(&snap));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{fake_backend, seen};

    const SNAP: &str = r#"{
        "streams":[{"name":"EXEC","exists":true,"bytes":10,"max_bytes":100,"messages":3},
                   {"name":"AUDIT","exists":false}],
        "kv_buckets":[{"name":"jobs","exists":true,"bytes":1,"messages":1}],
        "object_stores":[{"name":"agent_releases","exists":false}]
    }"#;

    #[test]
    fn render_keeps_the_three_sections_and_not_found() {
        let snap: Snapshot = serde_json::from_str(SNAP).unwrap();
        assert_eq!(
            render_status(&snap),
            "streams:\n  EXEC: messages=3, bytes=10\n  AUDIT: NOT FOUND\n\
             KV buckets:\n  jobs: OK\nobject stores:\n  agent_releases: NOT FOUND\n"
        );
    }

    #[test]
    fn missing_usage_is_not_shown_as_zero() {
        let snap: Snapshot = serde_json::from_str(
            r#"{"streams":[{"name":"EXEC","exists":true}],"kv_buckets":[],"object_stores":[]}"#,
        )
        .unwrap();
        assert!(render_status(&snap).contains("EXEC: messages=?, bytes=?"));
    }

    #[tokio::test]
    async fn status_sends_get_to_the_snapshot_route() {
        let (base, log) = fake_backend(vec![(200, SNAP)]).await;
        status(&format!("{base}/")).await.unwrap();
        let got = seen(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].target, "/api/jetstream/status");
        assert!(got[0].headers.contains("x-kanade-source: cli"));
    }

    #[tokio::test]
    async fn auth_failures_report_status_and_body() {
        for (code, body) in [(401, "bad token"), (403, "feature disabled")] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let err = status(&base).await.unwrap_err().to_string();
            assert!(err.contains("jetstream status failed"), "{err}");
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(body), "{err}");
        }
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error() {
        let (base, _log) = fake_backend(vec![(200, "{")]).await;
        let err = status(&base).await.unwrap_err().to_string();
        assert!(err.contains("parse JSON response"), "{err}");
    }

    #[tokio::test]
    async fn connection_failure_names_the_request() {
        let err = status("http://127.0.0.1:1").await.unwrap_err().to_string();
        assert!(
            err.contains("GET http://127.0.0.1:1/api/jetstream/status"),
            "{err}"
        );
    }
}

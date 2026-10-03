//! `kanade meta …` — per-PC operator metadata (free-form key/value: the
//! primary user's name / email / department, an ad-hoc note), all through
//! the backend HTTP API (`/api/agents/{pc_id}/meta`).
//!
//! Going through the backend (rather than the `agent_meta` KV bucket over
//! NATS) means every change passes its authentication, role check and
//! audit trail, and the CLI needs only `KANADE_AUTH_TOKEN`, never a broker
//! credential. The flip side is that `kanade meta` needs the backend up.
//!
//! The intended bulk producer is an operator AD-sync job on the (domain-
//! joined) backend host: `kanade query` fetches the roster
//! (`agents.last_logon_user`), resolves each user's directory attributes
//! via ADSI, and calls `meta set` here. That job therefore authenticates
//! with an auth token (operator or above) like the other HTTP subcommands,
//! even though it runs on the backend host. It overwrites only its own
//! synced keys and leaves hand-entered keys alone, which is why `set` /
//! `rm` hit the backend's single-key routes instead of fetching, merging
//! and PUT-ing the whole set: the backend does that read-modify-write as a
//! compare-and-swap, so a concurrent writer of another key is not lost to
//! a stale copy held by the CLI. `clear` is the whole-set PUT with no
//! entries.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use kanade_shared::wire::{AgentMeta, MetaEntry, MetaUpdate};

#[derive(Args, Debug)]
pub struct MetaArgs {
    #[command(subcommand)]
    pub sub: MetaSub,
}

#[derive(Subcommand, Debug)]
pub enum MetaSub {
    /// Show all key/value attributes for a PC.
    Get { pc_id: String },
    /// Set (upsert) one key on a PC. Overwrites the value if the key
    /// already exists; other keys are left untouched. An empty value
    /// keeps the key with a blank value — use `rm` to drop a key.
    Set {
        pc_id: String,
        key: String,
        value: String,
    },
    /// Remove one key from a PC (idempotent).
    Rm { pc_id: String, key: String },
    /// Clear ALL attributes for a PC.
    Clear { pc_id: String },
}

pub async fn execute(backend_url: &str, args: MetaArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        MetaSub::Get { pc_id } => get(base, &pc_id).await,
        MetaSub::Set { pc_id, key, value } => set(base, &pc_id, &key, &value).await,
        MetaSub::Rm { pc_id, key } => rm(base, &pc_id, &key).await,
        MetaSub::Clear { pc_id } => clear(base, &pc_id).await,
    }
}

/// `/api/agents/{pc_id}/meta` plus `tail` segments. The PC id is pushed as
/// one percent-encoded path segment, so an id containing `/` or `?` cannot
/// change which endpoint is hit.
fn meta_url(base: &str, pc_id: &str, tail: &[&str]) -> Result<reqwest::Url> {
    if pc_id.is_empty() {
        bail!("pc_id must not be empty");
    }
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid backend URL '{base}'"))?;
    {
        let mut seg = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("backend URL '{base}' cannot be a base"))?;
        seg.pop_if_empty().extend(["api", "agents", pc_id, "meta"]);
        seg.extend(tail);
    }
    Ok(url)
}

/// Send a prepared request. Connection failures get the request line as
/// context; non-2xx responses (401 / 403 / 400 included) surface the status
/// and body the same way the other HTTP subcommands do.
async fn send(
    req: reqwest::RequestBuilder,
    op: &str,
    method: &str,
    url: &reqwest::Url,
) -> Result<reqwest::Response> {
    let resp = req
        .send()
        .await
        .with_context(|| format!("{method} {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{op} failed: {status} — {body}");
    }
    Ok(resp)
}

async fn send_json<T: serde::de::DeserializeOwned>(
    req: reqwest::RequestBuilder,
    op: &str,
    method: &str,
    url: &reqwest::Url,
) -> Result<T> {
    send(req, op, method, url)
        .await?
        .json()
        .await
        .with_context(|| format!("parse JSON response from {method} {url}"))
}

/// An empty key is refused before any request: clearer than a confusing
/// "no change", and it saves a round-trip (the backend refuses it too).
fn check_key(key: &str) -> Result<()> {
    if key.trim().is_empty() {
        bail!("metadata key must not be empty");
    }
    Ok(())
}

async fn get(base: &str, pc_id: &str) -> Result<()> {
    let url = meta_url(base, pc_id, &[])?;
    let req = crate::http_client::authed_client()?.get(url.clone());
    let m: AgentMeta = send_json(req, "get", "GET", &url).await?;
    if m.is_empty() {
        println!("{pc_id}: (no attributes)");
    } else {
        for e in &m.entries {
            println!("{}: {}", e.key, e.value);
        }
    }
    Ok(())
}

async fn set(base: &str, pc_id: &str, key: &str, value: &str) -> Result<()> {
    check_key(key)?;
    let url = meta_url(base, pc_id, &["key"])?;
    let req = crate::http_client::authed_client()?
        .put(url.clone())
        .json(&MetaEntry::new(key, value));
    let update: MetaUpdate = send_json(req, "set", "PUT", &url).await?;
    if update.changed {
        println!("{pc_id}: set '{}' = '{}'", key.trim(), value.trim());
    } else {
        println!(
            "{pc_id}: '{}' already '{}' (no change)",
            key.trim(),
            value.trim()
        );
    }
    Ok(())
}

async fn rm(base: &str, pc_id: &str, key: &str) -> Result<()> {
    check_key(key)?;
    let mut url = meta_url(base, pc_id, &["key"])?;
    url.query_pairs_mut().append_pair("key", key);
    let req = crate::http_client::authed_client()?.delete(url.clone());
    let update: MetaUpdate = send_json(req, "rm", "DELETE", &url).await?;
    if update.changed {
        println!("{pc_id}: removed '{}'", key.trim());
    } else {
        println!("{pc_id}: no key '{}' (no change)", key.trim());
    }
    Ok(())
}

async fn clear(base: &str, pc_id: &str) -> Result<()> {
    let url = meta_url(base, pc_id, &[])?;
    let req = crate::http_client::authed_client()?
        .put(url.clone())
        .json(&AgentMeta::default());
    send(req, "clear", "PUT", &url).await?;
    println!("{pc_id}: cleared all attributes");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{fake_backend, seen};

    const CHANGED: &str = r#"{"meta":{"entries":[{"key":"name","value":"Alice"}]},"changed":true}"#;
    const UNCHANGED: &str =
        r#"{"meta":{"entries":[{"key":"name","value":"Alice"}]},"changed":false}"#;

    fn args(sub: MetaSub) -> MetaArgs {
        MetaArgs { sub }
    }

    fn set_sub(pc: &str, key: &str, value: &str) -> MetaSub {
        MetaSub::Set {
            pc_id: pc.into(),
            key: key.into(),
            value: value.into(),
        }
    }

    #[tokio::test]
    async fn get_reads_the_meta_route_with_auth_source() {
        let (base, log) = fake_backend(vec![(200, r#"{"entries":[]}"#)]).await;
        execute(
            &format!("{base}/"),
            args(MetaSub::Get {
                pc_id: "PC-01".into(),
            }),
        )
        .await
        .unwrap();
        let got = seen(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].target, "/api/agents/PC-01/meta");
        assert!(got[0].headers.contains("x-kanade-source: cli"));
    }

    #[tokio::test]
    async fn set_puts_key_and_value_in_the_body() {
        let (base, log) = fake_backend(vec![(200, CHANGED)]).await;
        execute(&base, args(set_sub("PC-01", "name", "Alice")))
            .await
            .unwrap();
        let got = seen(&log);
        assert_eq!(got[0].method, "PUT");
        assert_eq!(got[0].target, "/api/agents/PC-01/meta/key");
        assert_eq!(got[0].body, r#"{"key":"name","value":"Alice"}"#);
    }

    #[tokio::test]
    async fn set_accepts_an_unchanged_reply_and_an_empty_value() {
        let (base, log) = fake_backend(vec![(200, UNCHANGED)]).await;
        execute(&base, args(set_sub("PC-01", "note", "")))
            .await
            .unwrap();
        assert_eq!(seen(&log)[0].body, r#"{"key":"note","value":""}"#);
    }

    #[tokio::test]
    async fn rm_deletes_with_the_key_in_the_query() {
        let (base, log) = fake_backend(vec![(200, CHANGED), (200, UNCHANGED)]).await;
        for _ in 0..2 {
            execute(
                &base,
                args(MetaSub::Rm {
                    pc_id: "PC-01".into(),
                    key: "a/b?c d".into(),
                }),
            )
            .await
            .unwrap();
        }
        for got in seen(&log) {
            assert_eq!(got.method, "DELETE");
            assert_eq!(got.target, "/api/agents/PC-01/meta/key?key=a%2Fb%3Fc+d");
            assert_eq!(got.body, "");
        }
    }

    #[tokio::test]
    async fn clear_puts_an_empty_set() {
        let (base, log) = fake_backend(vec![(200, r#"{"entries":[]}"#)]).await;
        execute(
            &base,
            args(MetaSub::Clear {
                pc_id: "PC-01".into(),
            }),
        )
        .await
        .unwrap();
        let got = seen(&log);
        assert_eq!(got[0].method, "PUT");
        assert_eq!(got[0].target, "/api/agents/PC-01/meta");
        assert_eq!(got[0].body, r#"{"entries":[]}"#);
    }

    #[tokio::test]
    async fn pc_id_is_one_encoded_path_segment() {
        let (base, log) = fake_backend(vec![(200, CHANGED)]).await;
        execute(&base, args(set_sub("pc/1 ?x", "k", "v")))
            .await
            .unwrap();
        assert_eq!(seen(&log)[0].target, "/api/agents/pc%2F1%20%3Fx/meta/key");
    }

    #[tokio::test]
    async fn empty_input_never_reaches_the_backend() {
        let (base, log) = fake_backend(vec![(200, CHANGED)]).await;
        for sub in [
            set_sub("PC-01", "  ", "v"),
            MetaSub::Rm {
                pc_id: "PC-01".into(),
                key: "".into(),
            },
        ] {
            let err = execute(&base, args(sub)).await.unwrap_err();
            assert!(err.to_string().contains("must not be empty"), "{err:#}");
        }
        let err = execute(
            &base,
            args(MetaSub::Clear {
                pc_id: String::new(),
            }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err:#}");
        assert!(seen(&log).is_empty(), "nothing may be sent");
    }

    #[tokio::test]
    async fn http_errors_report_status_and_body() {
        for (code, body) in [
            (400, "bad key"),
            (401, "bad token"),
            (403, "operator role required"),
            (500, "boom"),
        ] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let err = execute(&base, args(set_sub("PC-01", "k", "v")))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("set failed"), "{err}");
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(body), "{err}");
        }
        let (base, _log) = fake_backend(vec![(403, "no")]).await;
        let err = execute(
            &base,
            args(MetaSub::Clear {
                pc_id: "PC-01".into(),
            }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("clear failed") && err.contains("403"), "{err}");
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error() {
        let (base, _log) = fake_backend(vec![(200, "not json")]).await;
        let err = execute(
            &base,
            args(MetaSub::Get {
                pc_id: "PC-01".into(),
            }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("parse JSON response"), "{err}");
    }

    #[tokio::test]
    async fn connection_failure_names_the_request() {
        let err = execute(
            "http://127.0.0.1:1",
            args(MetaSub::Get {
                pc_id: "PC-01".into(),
            }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("GET http://127.0.0.1:1/api/agents/PC-01/meta"),
            "{err}"
        );
    }
}

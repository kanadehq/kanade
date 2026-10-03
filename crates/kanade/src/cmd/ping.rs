use anyhow::{Context, Result, bail};
use clap::Args;
use kanade_shared::Heartbeat;
use serde::Deserialize;
use tracing::info;

#[derive(Args, Debug)]
pub struct PingArgs {
    /// Target PC, as the agent registered itself — its OS hostname,
    /// VERBATIM (NATS subjects are case-sensitive, and casing is not
    /// uniform across a fleet). The backend asks the agent directly and
    /// the agent answers at once with a fresh heartbeat, so a healthy
    /// agent replies in milliseconds. Goes through the backend API
    /// (needs KANADE_AUTH_TOKEN and an operator-capable role), not NATS.
    /// An agent that does not answer — offline, or too old to serve
    /// pings — is reported as no reply.
    pub pc_id: String,
    /// Seconds the backend waits for the agent's reply before giving
    /// up (sent as `wait_secs`; the backend raises 0 to 1).
    #[arg(long, default_value_t = 5)]
    pub wait: u64,
}

#[derive(Deserialize)]
struct PingResponse {
    heartbeat: Heartbeat,
}

/// Build `{base}/api/agents/{pc_id}/ping?wait_secs=N` with the pc_id
/// percent-encoded as one path segment, casing untouched.
fn ping_url(base: &str, pc_id: &str, wait: u64) -> Result<reqwest::Url> {
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid backend URL '{base}'"))?;
    {
        let mut seg = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("backend URL '{base}' cannot be a base"))?;
        seg.pop_if_empty().extend(["api", "agents", pc_id, "ping"]);
    }
    url.query_pairs_mut()
        .append_pair("wait_secs", &wait.to_string());
    Ok(url)
}

pub async fn execute(backend_url: &str, args: PingArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    let url = ping_url(base, &args.pc_id, args.wait)?;
    info!(pc_id = %args.pc_id, wait = args.wait, "pinging via backend");
    // The backend already times its own NATS request out after `wait_secs`;
    // the HTTP client has no overall timeout, so the 408 arrives on its own.
    let resp = crate::http_client::authed_client()?
        .post(url.clone())
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = resp.status();
    if status == reqwest::StatusCode::REQUEST_TIMEOUT {
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "no ping reply from {} within {}s: {status} — {body}",
            args.pc_id,
            args.wait.max(1)
        );
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("ping failed: {status} — {body}");
    }
    let PingResponse { heartbeat: hb } = resp
        .json()
        .await
        .with_context(|| format!("parse JSON response from POST {url}"))?;
    println!("pc_id         : {}", hb.pc_id);
    println!("at            : {}", hb.at);
    println!("agent_version : {}", hb.agent_version);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{fake_backend, seen};

    const HB: &str =
        r#"{"heartbeat":{"pc_id":"PC-1","at":"2026-01-02T03:04:05Z","agent_version":"1.2.3"}}"#;

    fn args(pc: &str, wait: u64) -> PingArgs {
        PingArgs {
            pc_id: pc.into(),
            wait,
        }
    }

    #[tokio::test]
    async fn sends_post_with_wait_secs_and_verbatim_pc_id() {
        let (base, log) = fake_backend(vec![(200, HB)]).await;
        execute(&format!("{base}/"), args("PC-1", 7)).await.unwrap();
        let got = seen(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].target, "/api/agents/PC-1/ping?wait_secs=7");
        assert!(got[0].headers.contains("x-kanade-source: cli"));
    }

    #[tokio::test]
    async fn pc_id_is_encoded_as_one_segment() {
        let (base, log) = fake_backend(vec![(200, HB)]).await;
        execute(&base, args("a/b c", 5)).await.unwrap();
        assert_eq!(
            seen(&log)[0].target,
            "/api/agents/a%2Fb%20c/ping?wait_secs=5"
        );
    }

    #[tokio::test]
    async fn no_reply_is_reported_from_the_408() {
        let (base, _log) = fake_backend(vec![(408, "no ping reply from PC-1 within 3s")]).await;
        let err = execute(&base, args("PC-1", 3))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("no ping reply from PC-1 within 3s"), "{err}");
        assert!(err.contains("408"), "{err}");
    }

    #[tokio::test]
    async fn auth_failures_report_status_and_body() {
        for (code, body) in [(401, "bad token"), (403, "operator role required")] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let err = execute(&base, args("PC-1", 5))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("ping failed"), "{err}");
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(body), "{err}");
        }
    }

    #[tokio::test]
    async fn malformed_json_is_a_parse_error() {
        let (base, _log) = fake_backend(vec![(200, "not json")]).await;
        let err = execute(&base, args("PC-1", 5))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("parse JSON response"), "{err}");
    }

    #[tokio::test]
    async fn connection_failure_names_the_request() {
        let err = execute("http://127.0.0.1:1", args("PC-1", 5))
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("POST http://127.0.0.1:1/api/agents/PC-1/ping?wait_secs=5"),
            "{err}"
        );
    }
}

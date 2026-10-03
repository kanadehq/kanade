//! `kanade revoke` / `kanade unrevoke` — flip a command id between REVOKED
//! and ACTIVE through the backend API. The backend authenticates the caller,
//! applies the operator-role check and records the audit entry attributed to
//! that account, so none of it rests on what this client claims.

use anyhow::{Context, Result, bail};
use clap::Args;
use kanade_shared::kv::{SCRIPT_STATUS_ACTIVE, SCRIPT_STATUS_REVOKED};
use tracing::info;

#[derive(Args, Debug)]
pub struct RevokeArgs {
    /// Command id to revoke (matches Command.id from the YAML manifest).
    pub cmd_id: String,
}

#[derive(Args, Debug)]
pub struct UnrevokeArgs {
    /// Command id to re-activate (matches Command.id from the YAML
    /// manifest) — the counterpart of `kanade revoke`'s cmd_id, undoing
    /// it back to ACTIVE.
    pub cmd_id: String,
}

pub async fn revoke(backend_url: &str, args: RevokeArgs) -> Result<()> {
    post_action(backend_url, &args.cmd_id, "revoke").await?;
    info!(cmd_id = %args.cmd_id, "revoked");
    println!("revoked: {} → {}", args.cmd_id, SCRIPT_STATUS_REVOKED);
    Ok(())
}

pub async fn unrevoke(backend_url: &str, args: UnrevokeArgs) -> Result<()> {
    post_action(backend_url, &args.cmd_id, "unrevoke").await?;
    info!(cmd_id = %args.cmd_id, "unrevoked");
    println!("unrevoked: {} → {}", args.cmd_id, SCRIPT_STATUS_ACTIVE);
    Ok(())
}

/// Build `{base}/api/scripts/{cmd_id}/{action}` with the id percent-encoded as
/// one path segment, so an id containing `/` or `?` cannot hit another route.
fn action_url(base: &str, cmd_id: &str, action: &str) -> Result<reqwest::Url> {
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid backend URL '{base}'"))?;
    {
        let mut seg = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("backend URL '{base}' cannot be a base"))?;
        seg.pop_if_empty()
            .extend(["api", "scripts", cmd_id, action]);
    }
    Ok(url)
}

/// The backend answers 204 with no body on success, so nothing is parsed.
async fn post_action(backend_url: &str, cmd_id: &str, action: &str) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    let url = action_url(base, cmd_id, action)?;
    let resp = crate::http_client::authed_client()?
        .post(url.clone())
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!("{action} failed: {status} — {body}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{fake_backend, seen};

    #[tokio::test]
    async fn revoke_posts_to_the_revoke_route() {
        let (base, log) = fake_backend(vec![(204, "")]).await;
        // A trailing slash on the backend URL is tolerated.
        revoke(
            &format!("{base}/"),
            RevokeArgs {
                cmd_id: "Cmd-1".into(),
            },
        )
        .await
        .unwrap();
        let got = seen(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].target, "/api/scripts/Cmd-1/revoke");
        assert!(got[0].headers.contains("x-kanade-source: cli"));
    }

    #[tokio::test]
    async fn unrevoke_posts_to_the_unrevoke_route() {
        let (base, log) = fake_backend(vec![(204, "")]).await;
        unrevoke(&base, UnrevokeArgs { cmd_id: "c".into() })
            .await
            .unwrap();
        let got = seen(&log);
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].target, "/api/scripts/c/unrevoke");
    }

    #[tokio::test]
    async fn cmd_id_is_encoded_as_one_segment() {
        let (base, log) = fake_backend(vec![(204, "")]).await;
        revoke(
            &base,
            RevokeArgs {
                cmd_id: "a/b?x".into(),
            },
        )
        .await
        .unwrap();
        assert_eq!(seen(&log)[0].target, "/api/scripts/a%2Fb%3Fx/revoke");
    }

    #[tokio::test]
    async fn auth_failures_report_status_and_body() {
        for (code, body) in [(401, "bad token"), (403, "operator role required")] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let err = revoke(&base, RevokeArgs { cmd_id: "c".into() })
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("revoke failed"), "{err}");
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(body), "{err}");
        }
    }

    #[tokio::test]
    async fn unrevoke_error_names_the_operation() {
        let (base, _log) = fake_backend(vec![(503, "script_status KV bucket unavailable")]).await;
        let err = unrevoke(&base, UnrevokeArgs { cmd_id: "c".into() })
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("unrevoke failed: 503"), "{err}");
    }

    #[tokio::test]
    async fn connection_failure_names_the_request() {
        // Port 1 is never listening.
        let err = revoke("http://127.0.0.1:1", RevokeArgs { cmd_id: "c".into() })
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("POST http://127.0.0.1:1/api/scripts/c/revoke"),
            "{err}"
        );
    }
}

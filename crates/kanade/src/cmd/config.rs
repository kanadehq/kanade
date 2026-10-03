//! `kanade config …` — operate the layered agent_config scopes (global /
//! per-group / per-pc), all through the backend HTTP API
//! (`/api/config`, `/api/groups/{name}/config`, `/api/pcs/{pc_id}/config`,
//! `/api/agents/{pc_id}/effective_config`).
//!
//! Going through the backend (rather than the KV bucket over NATS) means
//! every change passes its authentication, role check and audit trail —
//! these settings include `target_version`, i.e. which agent binary the
//! fleet runs — and the CLI needs only `KANADE_AUTH_TOKEN`, never a broker
//! credential. Automation that calls `kanade config` therefore
//! authenticates with an auth token like the other HTTP subcommands.
//!
//! `set` / `unset` send one field to the backend's field routes rather
//! than fetching, merging and PUT-ing the scope: the backend does the
//! read-modify-write as a compare-and-swap, so a concurrent writer of
//! another field on the same scope (a rollout writing `target_version`)
//! is not clobbered by a stale copy held by the CLI. The field grammar
//! lives in `kanade_shared::config_field` and is checked here first so a
//! typo fails without a round-trip.

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use kanade_shared::config_field::{FieldUpdate, FieldValue, apply_field, parse_set_spec};
use kanade_shared::kv::{KEY_AGENT_CONFIG_GLOBAL, agent_config_group_key, agent_config_pc_key};
use kanade_shared::wire::{ConfigScope, EffectiveConfig};
use serde::Deserialize;

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub sub: ConfigSub,
}

#[derive(Args, Debug, Clone)]
pub struct ScopeSel {
    /// Operate on the per-group override `groups.<name>` instead of
    /// the global scope. Mutually exclusive with `--pc`.
    #[arg(long, conflicts_with = "pc", value_name = "NAME")]
    pub group: Option<String>,

    /// Operate on the per-pc override `pcs.<pc_id>` instead of the
    /// global scope. Mutually exclusive with `--group`.
    #[arg(long, conflicts_with = "group", value_name = "PC_ID")]
    pub pc: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum ConfigSub {
    /// Print the scope's current ConfigScope as pretty-printed JSON.
    Get {
        #[command(flatten)]
        scope: ScopeSel,
    },
    /// Set one field. `<spec>` is `<field>=<value>` (e.g.
    /// `heartbeat_interval=15s`, `host_perf_interval=2m`,
    /// `process_perf_enabled=true`,
    /// `process_perf_expires_at=2026-05-24T15:30:00Z`,
    /// `process_perf_top_n=20`, `target_version_jitter=30m`,
    /// `target_version=0.3.0`,
    /// `client_display_name=端末管理支援ツール`).
    Set {
        spec: String,
        #[command(flatten)]
        scope: ScopeSel,
    },
    /// Clear one field. Equivalent to PUT-ing the same scope back
    /// without that field set.
    Unset {
        field: String,
        #[command(flatten)]
        scope: ScopeSel,
    },
    /// Delete the whole scope row.
    Clear {
        #[command(flatten)]
        scope: ScopeSel,
    },
    /// Print the resolved EffectiveConfig for one pc_id — the backend's
    /// view, the same resolution the agent's config_supervisor computes
    /// locally.
    Effective { pc_id: String },
}

/// `GET /api/agents/{pc_id}/effective_config`, the fields the CLI prints.
#[derive(Deserialize)]
struct EffectiveResponse {
    pc_id: String,
    effective: EffectiveConfig,
    /// Rendered by the backend (the CLI no longer sees the raw resolver
    /// output). Absent from a backend that predates the field.
    #[serde(default)]
    warnings: Vec<String>,
    #[serde(default)]
    my_groups: Vec<String>,
}

pub async fn execute(backend_url: &str, args: ConfigArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        ConfigSub::Get { scope } => get(base, &scope).await,
        ConfigSub::Set { spec, scope } => set(base, &scope, &spec).await,
        ConfigSub::Unset { field, scope } => unset(base, &scope, &field).await,
        ConfigSub::Clear { scope } => clear(base, &scope).await,
        ConfigSub::Effective { pc_id } => effective(base, &pc_id).await,
    }
}

fn scope_key(sel: &ScopeSel) -> Result<String> {
    match (&sel.group, &sel.pc) {
        (None, None) => Ok(KEY_AGENT_CONFIG_GLOBAL.to_string()),
        (Some(g), None) => Ok(agent_config_group_key(g)),
        (None, Some(p)) => Ok(agent_config_pc_key(p)),
        // clap's conflicts_with should keep this unreachable.
        (Some(_), Some(_)) => bail!("--group and --pc are mutually exclusive"),
    }
}

fn scope_label(sel: &ScopeSel) -> String {
    match (&sel.group, &sel.pc) {
        (None, None) => "global".into(),
        (Some(g), None) => format!("groups.{g}"),
        (None, Some(p)) => format!("pcs.{p}"),
        (Some(_), Some(_)) => "<invalid>".into(),
    }
}

/// Build the scope's config URL, optionally followed by `tail` segments
/// (`["fields", field]`). Every user-supplied name is pushed as one
/// percent-encoded path segment, so a name containing `/` or `?` cannot
/// change which endpoint is hit.
fn scope_url(base: &str, sel: &ScopeSel, tail: &[&str]) -> Result<reqwest::Url> {
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid backend URL '{base}'"))?;
    {
        let mut seg = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("backend URL '{base}' cannot be a base"))?;
        seg.pop_if_empty().push("api");
        match (&sel.group, &sel.pc) {
            (None, None) => seg.push("config"),
            (Some(g), None) => seg.extend(["groups", g.as_str(), "config"]),
            (None, Some(p)) => seg.extend(["pcs", p.as_str(), "config"]),
            (Some(_), Some(_)) => bail!("--group and --pc are mutually exclusive"),
        };
        seg.extend(tail);
    }
    Ok(url)
}

/// An empty name would produce an empty path segment (`/api/groups//config`)
/// that matches no route and answers a confusing 404.
fn check_scope_name(sel: &ScopeSel) -> Result<()> {
    match (&sel.group, &sel.pc) {
        (Some(g), _) if g.is_empty() => bail!("--group must not be empty"),
        (_, Some(p)) if p.is_empty() => bail!("--pc must not be empty"),
        _ => Ok(()),
    }
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

async fn get(base: &str, sel: &ScopeSel) -> Result<()> {
    check_scope_name(sel)?;
    let key = scope_key(sel)?;
    let url = scope_url(base, sel, &[])?;
    let req = crate::http_client::authed_client()?.get(url.clone());
    let scope: ConfigScope = send_json(req, "get", "GET", &url).await?;
    println!("# {} = {}", scope_label(sel), key);
    println!("{}", serde_json::to_string_pretty(&scope)?);
    Ok(())
}

async fn set(base: &str, sel: &ScopeSel, spec: &str) -> Result<()> {
    let (field, value) = parse_set_spec(spec)?;
    // Validate up front so a typo fails before any request is made; the
    // backend validates again with the same function.
    apply_field(&mut ConfigScope::default(), field, Some(value))?;
    check_scope_name(sel)?;
    let url = scope_url(base, sel, &["fields", field])?;
    let req = crate::http_client::authed_client()?
        .put(url.clone())
        .json(&FieldValue {
            value: value.to_string(),
        });
    let _: FieldUpdate = send_json(req, "set", "PUT", &url).await?;
    println!("set {field} = {value} on {}", scope_label(sel));
    Ok(())
}

async fn unset(base: &str, sel: &ScopeSel, field: &str) -> Result<()> {
    apply_field(&mut ConfigScope::default(), field, None)?;
    check_scope_name(sel)?;
    let url = scope_url(base, sel, &["fields", field])?;
    let req = crate::http_client::authed_client()?.delete(url.clone());
    let _: FieldUpdate = send_json(req, "unset", "DELETE", &url).await?;
    println!("unset {field} on {}", scope_label(sel));
    Ok(())
}

async fn clear(base: &str, sel: &ScopeSel) -> Result<()> {
    check_scope_name(sel)?;
    let key = scope_key(sel)?;
    let url = scope_url(base, sel, &[])?;
    let req = crate::http_client::authed_client()?.delete(url.clone());
    send(req, "clear", "DELETE", &url).await?;
    println!("cleared {} ({})", scope_label(sel), key);
    Ok(())
}

async fn effective(base: &str, pc_id: &str) -> Result<()> {
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid backend URL '{base}'"))?;
    {
        let mut seg = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("backend URL '{base}' cannot be a base"))?;
        seg.pop_if_empty()
            .extend(["api", "agents", pc_id, "effective_config"]);
    }
    let req = crate::http_client::authed_client()?.get(url.clone());
    let resp: EffectiveResponse = send_json(req, "effective", "GET", &url).await?;
    println!("# pc_id      = {}", resp.pc_id);
    println!("# my_groups  = {:?}", resp.my_groups);
    println!("{}", serde_json::to_string_pretty(&resp.effective)?);
    for w in &resp.warnings {
        println!("# warning: {w}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{fake_backend, seen};

    const UPDATE: &str = r#"{"scope":{},"changed":true}"#;

    fn sel(group: Option<&str>, pc: Option<&str>) -> ScopeSel {
        ScopeSel {
            group: group.map(String::from),
            pc: pc.map(String::from),
        }
    }

    fn args(sub: ConfigSub) -> ConfigArgs {
        ConfigArgs { sub }
    }

    #[test]
    fn scope_key_routing() {
        assert_eq!(scope_key(&sel(None, None)).unwrap(), "global");
        assert_eq!(
            scope_key(&sel(Some("canary"), None)).unwrap(),
            "groups.canary"
        );
        assert_eq!(scope_key(&sel(None, Some("PC-01"))).unwrap(), "pcs.PC-01");
    }

    #[tokio::test]
    async fn get_hits_each_scope_with_auth_source_and_prints_nothing_sent() {
        for (s, path) in [
            (sel(None, None), "/api/config"),
            (sel(Some("canary"), None), "/api/groups/canary/config"),
            (sel(None, Some("PC-01")), "/api/pcs/PC-01/config"),
        ] {
            let (base, log) = fake_backend(vec![(200, r#"{"target_version":"1.2.3"}"#)]).await;
            execute(&format!("{base}/"), args(ConfigSub::Get { scope: s }))
                .await
                .unwrap();
            let got = seen(&log);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].method, "GET");
            assert_eq!(got[0].target, path);
            assert!(got[0].headers.contains("x-kanade-source: cli"));
        }
    }

    #[tokio::test]
    async fn set_puts_the_field_value_on_each_scope() {
        for (s, path) in [
            (sel(None, None), "/api/config/fields/heartbeat_interval"),
            (
                sel(Some("canary"), None),
                "/api/groups/canary/config/fields/heartbeat_interval",
            ),
            (
                sel(None, Some("PC-01")),
                "/api/pcs/PC-01/config/fields/heartbeat_interval",
            ),
        ] {
            let (base, log) = fake_backend(vec![(200, UPDATE)]).await;
            execute(
                &base,
                args(ConfigSub::Set {
                    spec: "heartbeat_interval=15s".into(),
                    scope: s,
                }),
            )
            .await
            .unwrap();
            let got = seen(&log);
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].method, "PUT");
            assert_eq!(got[0].target, path);
            assert_eq!(got[0].body, r#"{"value":"15s"}"#);
        }
    }

    #[tokio::test]
    async fn set_value_keeps_everything_after_the_first_equals() {
        let (base, log) = fake_backend(vec![(200, UPDATE)]).await;
        execute(
            &base,
            args(ConfigSub::Set {
                spec: "client_display_name=a=b".into(),
                scope: sel(None, None),
            }),
        )
        .await
        .unwrap();
        assert_eq!(seen(&log)[0].body, r#"{"value":"a=b"}"#);
    }

    #[tokio::test]
    async fn unset_deletes_the_field_route_on_each_scope() {
        for (s, path) in [
            (sel(None, None), "/api/config/fields/target_version"),
            (
                sel(Some("canary"), None),
                "/api/groups/canary/config/fields/target_version",
            ),
            (
                sel(None, Some("PC-01")),
                "/api/pcs/PC-01/config/fields/target_version",
            ),
        ] {
            let (base, log) = fake_backend(vec![(200, UPDATE)]).await;
            execute(
                &base,
                args(ConfigSub::Unset {
                    field: "target_version".into(),
                    scope: s,
                }),
            )
            .await
            .unwrap();
            let got = seen(&log);
            assert_eq!(got[0].method, "DELETE");
            assert_eq!(got[0].target, path);
            assert_eq!(got[0].body, "");
        }
    }

    #[tokio::test]
    async fn clear_deletes_the_whole_scope_including_global() {
        for (s, path) in [
            (sel(None, None), "/api/config"),
            (sel(Some("canary"), None), "/api/groups/canary/config"),
            (sel(None, Some("PC-01")), "/api/pcs/PC-01/config"),
        ] {
            let (base, log) = fake_backend(vec![(204, "")]).await;
            execute(&base, args(ConfigSub::Clear { scope: s }))
                .await
                .unwrap();
            let got = seen(&log);
            assert_eq!(got[0].method, "DELETE");
            assert_eq!(got[0].target, path);
        }
    }

    #[tokio::test]
    async fn names_are_encoded_as_single_path_segments() {
        let (base, log) = fake_backend(vec![(200, UPDATE)]).await;
        execute(
            &base,
            args(ConfigSub::Set {
                spec: "target_version=1.0.0".into(),
                scope: sel(Some("a/b c"), None),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            seen(&log)[0].target,
            "/api/groups/a%2Fb%20c/config/fields/target_version"
        );
        let (base, log) = fake_backend(vec![(204, "")]).await;
        execute(
            &base,
            args(ConfigSub::Clear {
                scope: sel(None, Some("pc?x#y")),
            }),
        )
        .await
        .unwrap();
        assert_eq!(seen(&log)[0].target, "/api/pcs/pc%3Fx%23y/config");
    }

    #[tokio::test]
    async fn effective_uses_the_effective_config_route() {
        // A real EffectiveConfig so the test tracks the wire shape.
        let eff = serde_json::to_string(&EffectiveConfig::builtin_defaults()).unwrap();
        let body: &'static str = Box::leak(
            format!(r#"{{"pc_id":"PC-01","effective":{eff},"warnings":["w"],"my_groups":["g"]}}"#)
                .into_boxed_str(),
        );
        let (base, log) = fake_backend(vec![(200, body)]).await;
        execute(
            &base,
            args(ConfigSub::Effective {
                pc_id: "PC 01".into(),
            }),
        )
        .await
        .unwrap();
        let got = seen(&log);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].target, "/api/agents/PC%2001/effective_config");
    }

    #[tokio::test]
    async fn invalid_input_never_reaches_the_backend() {
        let (base, log) = fake_backend(vec![(200, UPDATE)]).await;
        for spec in ["heartbeat_interval", "heartbeat_interval=soon", "nope=1"] {
            let err = execute(
                &base,
                args(ConfigSub::Set {
                    spec: spec.into(),
                    scope: sel(None, None),
                }),
            )
            .await
            .unwrap_err();
            assert!(!format!("{err:#}").is_empty());
        }
        let err = execute(
            &base,
            args(ConfigSub::Unset {
                field: "nope".into(),
                scope: sel(None, None),
            }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err:#}");
        let err = execute(
            &base,
            args(ConfigSub::Clear {
                scope: sel(Some(""), None),
            }),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("must not be empty"), "{err:#}");
        assert!(seen(&log).is_empty(), "nothing may be sent");
    }

    #[tokio::test]
    async fn validation_message_matches_the_shared_grammar() {
        let err = execute(
            "http://127.0.0.1:1",
            args(ConfigSub::Set {
                spec: "heartbeat_interval=soon".into(),
                scope: sel(None, None),
            }),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("heartbeat_interval: expected a humantime duration"),
            "{err:#}"
        );
    }

    #[tokio::test]
    async fn http_errors_report_status_and_body() {
        for (code, body) in [
            (400, "unknown field 'x'"),
            (401, "bad token"),
            (403, "operator role required"),
            (500, "boom"),
        ] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let err = execute(
                &base,
                args(ConfigSub::Set {
                    spec: "target_version=1.0.0".into(),
                    scope: sel(None, None),
                }),
            )
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
            args(ConfigSub::Clear {
                scope: sel(None, None),
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
            args(ConfigSub::Get {
                scope: sel(None, None),
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
            args(ConfigSub::Get {
                scope: sel(Some("canary"), None),
            }),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("GET http://127.0.0.1:1/api/groups/canary/config"),
            "{err}"
        );
    }
}

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use kanade_shared::kv::{BUCKET_AGENT_CONFIG, OBJECT_AGENT_RELEASES};
use kanade_shared::wire::ConfigScope;
use reqwest::multipart::Form;
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use tokio::fs;
use tracing::info;

use super::object_http;
use super::validate_segment;

/// Upload ceiling the backend enforces on `POST /api/agents/publish`; named in
/// the hint when it answers 413.
const PUBLISH_LIMIT_NOTE: &str = "the backend refused the upload size; agent releases are capped \
                                  at 64 MB for the whole multipart body";

#[derive(Args, Debug)]
pub struct AgentArgs {
    #[command(subcommand)]
    pub sub: AgentSub,
}

#[derive(Subcommand, Debug)]
pub enum AgentSub {
    /// Upload a new agent binary to the agent_releases Object Store through
    /// the backend API (needs KANADE_AUTH_TOKEN with the operator role; no
    /// NATS access). No KV is touched — agents only start downloading once a
    /// follow-up `kanade agent rollout` flips `target_version` on
    /// some scope (global / group / pc). Two-step on purpose, so a
    /// typo doesn't fan a half-baked binary out to the whole fleet.
    ///
    /// v0.13.1+: the Object Store key is auto-extracted from the
    /// binary's embedded VERSIONINFO resource — no chance of a
    /// label/binary mismatch. Cross-arch publish works too (the
    /// extractor is pure-Rust `pelite`, no spawn).
    ///
    /// A non-PE binary (a Linux ELF or macOS Mach-O) carries no VERSIONINFO
    /// resource, so it can't be auto-labelled — pass `--version` for
    /// those. When a PE version AND `--version` are both present they
    /// must agree, preserving the no-mismatch guarantee.
    Publish {
        /// Path to the new agent binary (e.g. `target/release/kanade-agent.exe`).
        binary: PathBuf,
        /// Explicit version label. Omit for a Windows PE (read from its
        /// VERSIONINFO); required for a non-PE binary (Linux ELF / macOS Mach-O).
        #[arg(long)]
        version: Option<String>,
    },
    /// Flip `target_version` (and optionally `target_version_jitter`)
    /// on one scope of the layered agent_config bucket, through the
    /// backend API (operator role). The backend verifies the binary exists
    /// in the Object Store first — fail-fast on typos.
    ///
    /// Pick exactly one scope:
    ///   --global             roll out fleet-wide
    ///   --group <name>       canary / wave / dept overlay
    ///   --pc    <pc_id>      single-host pin
    Rollout(RolloutArgs),
    /// Print the currently broadcast global target_version (read through the
    /// backend API). Group / pc overlays are not shown: use
    /// `kanade config get --group/--pc` for those.
    Current,
    /// Tail the agent's log file through the backend API, which asks the
    /// agent itself. The agent reads its local rolling log file and
    /// returns the last N lines as UTF-8.
    Logs {
        /// PC id of the agent to query (must be online).
        pc_id: String,
        /// Trailing line count. Defaults to 500.
        #[arg(long, default_value_t = 500)]
        tail: u32,
    },
}

#[derive(Args, Debug)]
pub struct RolloutArgs {
    /// Version label to point the chosen scope at. Must match an
    /// object already in the agent_releases Object Store (i.e. a
    /// previous `kanade agent publish` round).
    pub version: String,

    /// Roll out to the global scope (`agent_config.global`). Mutually
    /// exclusive with `--group` / `--pc`.
    #[arg(long, conflicts_with_all = ["group", "pc"])]
    pub global: bool,

    /// Roll out to a single group (`agent_config.groups.<name>`).
    #[arg(long, value_name = "NAME")]
    pub group: Option<String>,

    /// Roll out to a single PC (`agent_config.pcs.<pc_id>`).
    #[arg(long, value_name = "PC_ID")]
    pub pc: Option<String>,

    /// Optional override for `target_version_jitter` on the same
    /// scope (humantime, e.g. `30m`). Recommended ≥ a few minutes
    /// for fleet-wide rollouts so 3000 agents don't synchronise
    /// their downloads. Omit to leave the existing value alone.
    #[arg(long, value_name = "DURATION")]
    pub jitter: Option<String>,
}

pub async fn execute(backend_url: &str, args: AgentArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        AgentSub::Publish { binary, version } => publish(base, binary, version).await,
        AgentSub::Rollout(args) => rollout(base, args).await,
        AgentSub::Current => current(base).await,
        AgentSub::Logs { pc_id, tail } => logs(base, pc_id, tail).await,
    }
}

/// Decide the publish label from the explicit `--version` (if any) and the
/// version extracted from the binary's PE VERSIONINFO (if any). `Ok(None)`
/// means neither was available — the caller falls back to an interactive
/// prompt. Comparison ignores a leading `v` and surrounding whitespace so
/// `v1.2.3`, `1.2.3 ` and `1.2.3` are the same label.
fn resolve_publish_version(
    version_override: Option<String>,
    extracted: Option<String>,
) -> Result<Option<String>> {
    let strip = |s: &str| s.trim().trim_start_matches('v').to_string();
    match (version_override, extracted) {
        (Some(v), Some(pe)) if strip(&v) != strip(&pe) => bail!(
            "--version {v} disagrees with the binary's embedded version {pe}; \
             omit --version to use the embedded one, or pass the matching label"
        ),
        (Some(v), _) => Ok(Some(v)),
        (None, Some(pe)) => Ok(Some(pe)),
        (None, None) => Ok(None),
    }
}

async fn publish(base: &str, binary: PathBuf, version_override: Option<String>) -> Result<()> {
    let bytes = fs::read(&binary)
        .await
        .with_context(|| format!("read {binary:?}"))?;

    // v0.13.1+: for a Windows PE the version comes from the embedded
    // VERSIONINFO resource (pelite, no spawn, cross-arch safe) so the
    // binary IS its label. A Linux ELF / macOS Mach-O has no such
    // resource, so `--version` supplies the label. Precedence:
    //   * both present  → must agree (keeps the no-mismatch guarantee)
    //   * --version only → use it (the ELF / Mach-O case)
    //   * PE only        → use the embedded label
    //   * neither        → interactive prompt, else fail fast
    let extracted = kanade_shared::exe_version::extract_pe_version(&bytes);
    let version = match resolve_publish_version(version_override, extracted)? {
        Some(v) => v,
        // Neither an explicit label nor an embedded one: last-resort
        // interactive prompt; a pipe / CI still fails fast.
        None => match super::prompt_version_if_interactive(binary.clone()).await? {
            Some(v) => v,
            None => bail!(
                "no version: {binary:?} has no embedded VERSIONINFO (a non-PE binary, e.g. a \
                 Linux ELF or macOS Mach-O?) — pass --version <X.Y.Z>. A Windows PE built with \
                 `winres` (kanade ≥ v0.13.1) is auto-labelled."
            ),
        },
    };
    // A pelite-extracted label is always key-safe, but a prompt-entered
    // one is operator input — validate before it becomes the `<version>`
    // object-store key, matching `app publish`.
    validate_segment("version", &version)?;

    // Which platform is this binary? Read from its own bytes, not the
    // filename: PE (Windows) stays at the bare `<version>` key, ELF
    // (Linux) goes to `<version>-linux-<arch>`, thin arm64 Mach-O (macOS,
    // Apple Silicon only) to `<version>-macos-aarch64`; an x86_64 (Intel)
    // or universal Mach-O / unknown is a hard
    // error — a publish that can't name its platform must not silently
    // land on the Windows key (see kanade_shared::bin_platform). The
    // backend repeats these checks; doing them here fails fast before the
    // upload starts.
    let platform =
        kanade_shared::bin_platform::AgentPlatform::detect(&bytes).map_err(|e| anyhow!(e))?;
    let key = platform.release_key(&version);
    kanade_shared::bin_platform::check_release_key(&key).map_err(|e| anyhow!(e))?;

    // The parse needed the whole file in memory; the upload does not.
    let size = bytes.len() as u64;
    drop(bytes);

    info!(
        version,
        platform = platform.as_str(),
        size,
        "uploading new agent binary"
    );

    let client = crate::http_client::authed_client()?;
    let url = object_http::collection_url(base, "api/agents/publish")?;
    let form = Form::new()
        .text("version", version.clone())
        .part("file", object_http::file_part(&binary).await?);
    let resp = client
        .post(url.clone())
        .multipart(form)
        .send()
        .await
        .with_context(|| {
            format!(
                "POST {url} (if the backend closed the connection mid-upload it rejected the \
                 body early — check the 64 MB size limit and KANADE_AUTH_TOKEN)"
            )
        })?;
    if !resp.status().is_success() {
        return Err(object_http::rejected("publish", resp, Some(PUBLISH_LIMIT_NOTE)).await);
    }
    let published: PublishResponse = resp
        .json()
        .await
        .context("parse publish response from server")?;
    info!(
        version = published.version,
        key = published.key,
        digest = ?published.digest,
        "agent binary uploaded"
    );
    // The backend does not read the object back, and agent_releases has no
    // HTTP download route to do it from here; comparing the size it stored
    // with the file we sent still catches a truncated upload.
    if published.size != size {
        bail!(
            "publish size mismatch: sent {size} bytes but the backend stored {} for {:?}",
            published.size,
            published.key
        );
    }

    println!("published: {} ({})", published.version, published.platform);
    println!("  object_store : {OBJECT_AGENT_RELEASES}/{}", published.key);
    println!();
    println!("Next: target a scope with `kanade agent rollout`:");
    let v = &published.version;
    println!("  kanade agent rollout {v} --group canary --jitter 5m   # try on canary first");
    println!("  kanade agent rollout {v} --global --jitter 30m        # fleet-wide");
    Ok(())
}

/// The backend's reply to a publish: the label and key it settled on.
#[derive(Debug, Deserialize)]
struct PublishResponse {
    version: String,
    key: String,
    platform: String,
    size: u64,
    digest: Option<String>,
}

/// Scope selector of `POST /api/agents/rollout`.
#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case", tag = "type", content = "value")]
enum RolloutScope {
    Global,
    Group(String),
    Pc(String),
}

#[derive(Debug, Serialize)]
struct RolloutRequest<'a> {
    version: &'a str,
    scope: RolloutScope,
    #[serde(skip_serializing_if = "Option::is_none")]
    jitter: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct RolloutResponse {
    version: String,
    scope_key: String,
    scope_label: String,
    jitter: Option<String>,
}

async fn rollout(base: &str, args: RolloutArgs) -> Result<()> {
    let scope = match (args.global, args.group.as_deref(), args.pc.as_deref()) {
        (true, None, None) => RolloutScope::Global,
        (false, Some(g), None) => RolloutScope::Group(g.to_string()),
        (false, None, Some(p)) => {
            kanade_shared::subject::validate_pc_id(p).map_err(|e| anyhow!(e))?;
            RolloutScope::Pc(p.to_string())
        }
        (false, None, None) => bail!(
            "must pick a scope: --global / --group <name> / --pc <pc_id>. \
             Refusing to rollout — explicit scope keeps a forgotten flag from \
             fanning a release out to every agent."
        ),
        _ => bail!("--global / --group / --pc are mutually exclusive"),
    };
    if let Some(j) = args.jitter.as_deref() {
        // Validate before the request: the agent's parse failure used to
        // silently fall back, so a typo'd jitter produced exactly the
        // fleet-wide download herd the flag exists to prevent.
        humantime::parse_duration(j).with_context(|| {
            format!("--jitter: expected a humantime duration (e.g. 30s, 10m, 1h), got {j:?}")
        })?;
    }

    let url = object_http::collection_url(base, "api/agents/rollout")?;
    let body = RolloutRequest {
        version: &args.version,
        scope,
        jitter: args.jitter.as_deref(),
    };
    let client = crate::http_client::authed_client()?;
    let resp = client
        .post(url.clone())
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        return Err(object_http::rejected("rollout", resp, None).await);
    }
    let done: RolloutResponse = resp
        .json()
        .await
        .context("parse rollout response from server")?;

    info!(
        scope = %done.scope_label,
        version = %done.version,
        jitter = ?done.jitter,
        "rollout: target_version flipped",
    );
    println!("rolled out: {} -> {}", done.scope_label, done.version);
    println!(
        "  kv           : {BUCKET_AGENT_CONFIG}.{}.target_version = {}",
        done.scope_key, done.version
    );
    if let Some(j) = done.jitter.as_deref() {
        println!(
            "  kv           : {BUCKET_AGENT_CONFIG}.{}.target_version_jitter = {j}",
            done.scope_key
        );
    } else {
        println!(
            "  jitter       : (unchanged; built-in default is 10m — pass `--jitter 0s` to disable the stagger)"
        );
    }
    Ok(())
}

/// Error for the read-only calls (`current`, `logs`): the backend's reason is
/// kept, and 401 / 403 get a token hint that does not presume a role, since
/// the two routes are gated by different features.
async fn read_rejected(op: &str, resp: reqwest::Response) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let mut msg = format!("{op} failed: {status} — {body}");
    if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
        msg.push_str(
            "\nhint: this command goes through the backend API — set KANADE_AUTH_TOKEN \
             (`kanade login`) to an account that may use it",
        );
    }
    anyhow!(msg)
}

async fn logs(base: &str, pc_id: String, tail: u32) -> Result<()> {
    kanade_shared::subject::validate_pc_id(&pc_id).map_err(|e| anyhow!(e))?;
    let mut url = Url::parse(base).with_context(|| format!("invalid backend URL {base:?}"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("backend URL {base:?} cannot carry a path"))?
        .pop_if_empty()
        .extend(["api", "agents"])
        .push(&pc_id)
        .push("logs");
    url.query_pairs_mut().append_pair("tail", &tail.to_string());

    let client = crate::http_client::authed_client()?;
    let resp = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if resp.status() == StatusCode::GATEWAY_TIMEOUT {
        let body = resp.text().await.unwrap_or_default();
        bail!("timeout waiting for {pc_id}: {body} (is the agent online?)");
    }
    if !resp.status().is_success() {
        return Err(read_rejected("logs", resp).await);
    }
    // The body is raw UTF-8 log bytes — pass straight through to stdout.
    let bytes = resp.bytes().await.context("read logs response")?;
    use std::io::Write;
    std::io::stdout().write_all(&bytes).ok();
    Ok(())
}

async fn current(base: &str) -> Result<()> {
    let url = object_http::collection_url(base, "api/config")?;
    let client = crate::http_client::authed_client()?;
    let resp = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(read_rejected("current", resp).await);
    }
    let scope: ConfigScope = resp
        .json()
        .await
        .with_context(|| format!("parse JSON response from GET {url}"))?;
    match scope.target_version {
        Some(v) => println!("global.target_version = {v}"),
        None => println!("global.target_version = (unset)"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{TEST_TOKEN, export_test_token, fake_backend, seen};

    #[test]
    fn version_override_only_is_used() {
        // ELF case: no embedded version, explicit --version wins.
        let v = resolve_publish_version(Some("1.2.3".into()), None).unwrap();
        assert_eq!(v.as_deref(), Some("1.2.3"));
    }

    #[test]
    fn embedded_only_is_used() {
        let v = resolve_publish_version(None, Some("0.44.35".into())).unwrap();
        assert_eq!(v.as_deref(), Some("0.44.35"));
    }

    #[test]
    fn neither_yields_none_for_prompt_fallback() {
        assert_eq!(resolve_publish_version(None, None).unwrap(), None);
    }

    #[test]
    fn agreeing_override_and_embedded_ok_ignoring_v_prefix() {
        // `v1.2.3` (flag) vs `1.2.3` (PE) must be treated as equal.
        let v = resolve_publish_version(Some("v1.2.3".into()), Some("1.2.3".into())).unwrap();
        assert_eq!(v.as_deref(), Some("v1.2.3"));
    }

    #[test]
    fn disagreeing_override_and_embedded_errors() {
        let e = resolve_publish_version(Some("9.9.9".into()), Some("1.2.3".into()));
        assert!(e.is_err(), "a real mismatch must be rejected");
    }

    /// A Linux x86_64 ELF header followed by non-UTF-8 filler larger than one
    /// read chunk, so a mangled or truncated stream cannot pass unnoticed.
    fn elf_payload() -> Vec<u8> {
        let mut b = vec![0u8; 20];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = 2;
        b[5] = 1;
        b[18..20].copy_from_slice(&0x3Eu16.to_le_bytes());
        b.extend((0..=255u8).rev().cycle().take(200_000));
        b
    }

    fn file_with(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("kanade-agent");
        std::fs::write(&p, bytes).unwrap();
        (dir, p)
    }

    fn args(sub: AgentSub) -> AgentArgs {
        AgentArgs { sub }
    }

    fn publish_sub(path: &std::path::Path, version: Option<&str>) -> AgentArgs {
        args(AgentSub::Publish {
            binary: path.to_path_buf(),
            version: version.map(String::from),
        })
    }

    fn published_json(size: usize) -> &'static str {
        Box::leak(
            serde_json::json!({
                "version": "1.2.3",
                "key": "1.2.3-linux-x86_64",
                "platform": "linux-x86_64",
                "size": size,
                "digest": "SHA-256=x",
            })
            .to_string()
            .into_boxed_str(),
        )
    }

    #[tokio::test]
    async fn publish_sends_a_multipart_version_and_the_exact_file_bytes() {
        export_test_token();
        let data = elf_payload();
        let (base, log) = fake_backend(vec![(200, published_json(data.len()))]).await;
        let (_dir, path) = file_with(&data);
        execute(&format!("{base}/"), publish_sub(&path, Some("1.2.3")))
            .await
            .unwrap();

        let got = seen(&log);
        assert_eq!(got.len(), 1, "no read-back request");
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].target, "/api/agents/publish");
        assert!(
            got[0]
                .headers
                .contains(&format!("authorization: bearer {TEST_TOKEN}")),
            "{}",
            got[0].headers
        );
        assert!(got[0].headers.contains("x-kanade-source: cli"));
        assert!(got[0].headers.contains("content-type: multipart/form-data"));
        assert!(
            !got[0].headers.contains("transfer-encoding"),
            "length must be declared, not chunked"
        );
        let head = String::from_utf8_lossy(&got[0].raw[..400.min(got[0].raw.len())]).to_string();
        assert!(head.contains(r#"name="version""#), "{head}");
        assert!(head.contains("\r\n\r\n1.2.3\r\n"), "{head}");
        assert!(head.contains(r#"name="file""#), "{head}");
        assert!(
            got[0].raw.windows(data.len()).any(|w| w == &data[..]),
            "exact file bytes must be in the multipart body"
        );
    }

    #[tokio::test]
    async fn publish_of_a_non_pe_without_version_fails_before_any_request() {
        let (base, log) = fake_backend(vec![]).await;
        let (_dir, path) = file_with(&elf_payload());
        let err = format!(
            "{:#}",
            execute(&base, publish_sub(&path, None)).await.unwrap_err()
        );
        assert!(err.contains("--version"), "{err}");
        assert!(seen(&log).is_empty());
    }

    #[tokio::test]
    async fn publish_validates_label_and_platform_before_any_request() {
        let (base, log) = fake_backend(vec![]).await;
        let (_dir, path) = file_with(&elf_payload());
        assert!(
            execute(&base, publish_sub(&path, Some("a/b")))
                .await
                .is_err()
        );
        let (_dir2, junk) = file_with(b"not a binary at all");
        assert!(
            execute(&base, publish_sub(&junk, Some("1.0")))
                .await
                .is_err()
        );
        assert!(
            execute(
                &base,
                publish_sub(std::path::Path::new("/nonexistent/agent"), Some("1.0"))
            )
            .await
            .is_err()
        );
        assert!(seen(&log).is_empty());
    }

    #[tokio::test]
    async fn publish_rejections_carry_status_body_and_hints() {
        export_test_token();
        for (code, body, hint) in [
            (413, "too big", "64 MB"),
            (403, "operator role required", "KANADE_AUTH_TOKEN"),
            (401, "no token", "KANADE_AUTH_TOKEN"),
            (
                400,
                "version field '9' disagrees with the binary's embedded version",
                "",
            ),
            (500, "boom", ""),
        ] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let (_dir, path) = file_with(&elf_payload());
            let err = format!(
                "{:#}",
                execute(&base, publish_sub(&path, Some("1.2.3")))
                    .await
                    .unwrap_err()
            );
            assert!(err.contains("publish failed"), "{err}");
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(body), "{err}");
            assert!(err.contains(hint), "{err}");
        }
    }

    #[tokio::test]
    async fn publish_with_a_bad_or_mismatched_reply_errors() {
        let (_dir, path) = file_with(&elf_payload());
        let (base, _log) = fake_backend(vec![(200, "not json")]).await;
        let err = format!(
            "{:#}",
            execute(&base, publish_sub(&path, Some("1.2.3")))
                .await
                .unwrap_err()
        );
        assert!(err.contains("parse publish response"), "{err}");
        let (base, _log) = fake_backend(vec![(200, published_json(5))]).await;
        let err = format!(
            "{:#}",
            execute(&base, publish_sub(&path, Some("1.2.3")))
                .await
                .unwrap_err()
        );
        assert!(err.contains("size mismatch"), "{err}");
    }

    fn rollout_args(
        version: &str,
        global: bool,
        group: Option<&str>,
        pc: Option<&str>,
        jitter: Option<&str>,
    ) -> AgentArgs {
        args(AgentSub::Rollout(RolloutArgs {
            version: version.into(),
            global,
            group: group.map(String::from),
            pc: pc.map(String::from),
            jitter: jitter.map(String::from),
        }))
    }

    #[tokio::test]
    async fn rollout_posts_the_scope_for_each_selector() {
        export_test_token();
        let reply = r#"{"version":"1.2.3","scope_key":"k","scope_label":"l","jitter":null}"#;
        let (base, log) = fake_backend(vec![(200, reply), (200, reply), (200, reply)]).await;
        execute(&base, rollout_args("1.2.3", true, None, None, Some("30m")))
            .await
            .unwrap();
        execute(
            &base,
            rollout_args("1.2.3", false, Some("canary"), None, None),
        )
        .await
        .unwrap();
        execute(
            &base,
            rollout_args("1.2.3", false, None, Some("PC-1"), None),
        )
        .await
        .unwrap();
        let got = seen(&log);
        let json = |i: usize| serde_json::from_str::<serde_json::Value>(&got[i].body).unwrap();
        for g in &got {
            assert_eq!(g.method, "POST");
            assert_eq!(g.target, "/api/agents/rollout");
            assert!(g.headers.contains("authorization: bearer"));
            assert!(g.headers.contains("x-kanade-source: cli"));
        }
        assert_eq!(
            json(0),
            serde_json::json!({"version":"1.2.3","scope":{"type":"global"},"jitter":"30m"})
        );
        assert_eq!(
            json(1),
            serde_json::json!({"version":"1.2.3","scope":{"type":"group","value":"canary"}})
        );
        assert_eq!(
            json(2),
            serde_json::json!({"version":"1.2.3","scope":{"type":"pc","value":"PC-1"}})
        );
    }

    #[tokio::test]
    async fn rollout_input_errors_never_reach_the_backend() {
        let (base, log) = fake_backend(vec![]).await;
        for a in [
            rollout_args("1.2.3", false, None, None, None),
            rollout_args("1.2.3", true, Some("g"), None, None),
            rollout_args("1.2.3", true, None, None, Some("soon")),
        ] {
            assert!(execute(&base, a).await.is_err());
        }
        assert!(seen(&log).is_empty());
    }

    #[tokio::test]
    async fn rollout_of_an_unpublished_version_reports_the_backend_404() {
        let body = "version '9.9.9' not found in agent_releases — run `kanade agent publish` first";
        let (base, _log) = fake_backend(vec![(404, body)]).await;
        let err = format!(
            "{:#}",
            execute(&base, rollout_args("9.9.9", true, None, None, None))
                .await
                .unwrap_err()
        );
        assert!(
            err.contains("rollout failed") && err.contains("404") && err.contains(body),
            "{err}"
        );
        let (base, _log) = fake_backend(vec![(403, "no")]).await;
        let err = format!(
            "{:#}",
            execute(&base, rollout_args("1", true, None, None, None))
                .await
                .unwrap_err()
        );
        assert!(err.contains("KANADE_AUTH_TOKEN"), "{err}");
        let (base, _log) = fake_backend(vec![(200, "{")]).await;
        assert!(
            execute(&base, rollout_args("1", true, None, None, None))
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn current_reads_the_global_config() {
        export_test_token();
        let (base, log) =
            fake_backend(vec![(200, r#"{"target_version":"1.2.3"}"#), (200, "{}")]).await;
        execute(&base, args(AgentSub::Current)).await.unwrap();
        execute(&base, args(AgentSub::Current)).await.unwrap();
        let got = seen(&log);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].target, "/api/config");
        assert!(got[0].headers.contains("authorization: bearer"));
        assert!(got[0].headers.contains("x-kanade-source: cli"));
    }

    #[tokio::test]
    async fn current_errors_report_status_and_a_parse_failure() {
        let (base, _log) = fake_backend(vec![(403, "nope")]).await;
        let err = format!(
            "{:#}",
            execute(&base, args(AgentSub::Current)).await.unwrap_err()
        );
        assert!(
            err.contains("current failed")
                && err.contains("403")
                && err.contains("KANADE_AUTH_TOKEN"),
            "{err}"
        );
        let (base, _log) = fake_backend(vec![(200, "not json")]).await;
        assert!(execute(&base, args(AgentSub::Current)).await.is_err());
    }

    #[tokio::test]
    async fn logs_requests_the_tail_with_an_encoded_pc_id() {
        export_test_token();
        let (base, log) = fake_backend(vec![(200, "line1\nline2\n")]).await;
        execute(
            &base,
            args(AgentSub::Logs {
                pc_id: "PC 1/x?".into(),
                tail: 20,
            }),
        )
        .await
        .unwrap();
        let got = seen(&log);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].target, "/api/agents/PC%201%2Fx%3F/logs?tail=20");
        assert!(got[0].headers.contains("authorization: bearer"));
    }

    #[tokio::test]
    async fn logs_timeout_and_rejections_are_clear() {
        let logs = || {
            args(AgentSub::Logs {
                pc_id: "PC-1".into(),
                tail: 500,
            })
        };
        let (base, _log) = fake_backend(vec![(504, "agent 'PC-1' didn't reply within 10s")]).await;
        let err = format!("{:#}", execute(&base, logs()).await.unwrap_err());
        assert!(err.contains("timeout waiting for PC-1"), "{err}");
        let (base, _log) = fake_backend(vec![(403, "no")]).await;
        let err = format!("{:#}", execute(&base, logs()).await.unwrap_err());
        assert!(
            err.contains("logs failed") && err.contains("KANADE_AUTH_TOKEN"),
            "{err}"
        );
    }

    /// A PE with no VERSIONINFO (a build predating the embedded resource) must
    /// still reach the backend under the explicit label.
    #[tokio::test]
    async fn publish_of_a_pe_without_versioninfo_sends_the_explicit_label() {
        let mut pe = vec![0u8; 0x90];
        pe[..2].copy_from_slice(b"MZ");
        pe[0x3C..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        pe[0x80..0x84].copy_from_slice(b"PE\0\0");
        pe[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        let reply = r#"{"version":"0.1.0","key":"0.1.0","platform":"windows-x86_64","size":144,"digest":null}"#;
        let (base, log) = fake_backend(vec![(200, reply)]).await;
        let (_dir, path) = file_with(&pe);
        execute(&base, publish_sub(&path, Some("0.1.0")))
            .await
            .unwrap();
        let got = seen(&log);
        assert_eq!(got.len(), 1);
        assert!(got[0].body.contains("\r\n\r\n0.1.0\r\n"), "{}", got[0].body);
    }
}

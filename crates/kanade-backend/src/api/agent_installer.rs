//! `GET /api/agents/installer` — generate an agent installer ZIP on the fly.
//!
//! Self-service: the route sits in the viewer+ base router, gated by the
//! `agent-install` page feature, so a restricted "download user" account
//! (viewer + ONLY that feature) can kit a fresh Windows PC: extract,
//! right-click `install.cmd` → *Run as administrator*, done. There is no
//! request body and no version parameter — the caller never chooses
//! anything. The ZIP always bundles the latest release (by Object Store
//! `modified`), and the NATS url/token it bakes in come from the
//! `agent_install` section of the server-settings document (falling back
//! to this backend's own `[nats] url`, no token). Contents:
//!
//!   * `kanade-agent.exe` — the bytes of the latest release from the
//!     `agent_releases` Object Store.
//!   * `agent.toml` — the repo's `configs/agent.toml` with the single
//!     `nats_url` line rewritten to the settings (or backend) NATS URL.
//!   * `deploy-agent.ps1` — the canonical `scripts/deploy/agent.ps1`,
//!     verbatim. All install logic lives there; this module only wraps it.
//!   * `install-agent.ps1` — a generated wrapper that invokes
//!     `deploy-agent.ps1` with the configured `-NatsToken` and, when this
//!     backend signs commands, a `-CommandKeys` ring holding the backend's
//!     own PUBLIC key (never a break-glass key — those are distributed
//!     separately, by hand).
//!   * `install.cmd` — a generated CRLF bootstrap for double-click
//!     installs (PowerShell with a bypass policy, pause-on-failure).
//!   * `README.txt` — the two-step instructions.

use axum::body::Body;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use kanade_shared::kv::OBJECT_AGENT_RELEASES;
use kanade_shared::wire::ServerSettings;
use tokio::io::AsyncReadExt;
use tracing::{info, warn};

use super::AppState;
use crate::audit;
use crate::audit::Caller;

/// The stock agent config every fresh install starts from. The single
/// `nats_url` line ([`NATS_URL_LINE`]) is rewritten per request.
const AGENT_TOML_TEMPLATE: &str = include_str!("../../../../configs/agent.toml");

/// The canonical deploy script, shipped unmodified so the installer can
/// never drift from what `scripts/deploy/agent.ps1` documents.
const DEPLOY_AGENT_PS1: &str = include_str!("../../../../scripts/deploy/agent.ps1");

/// The exact `[agent]` line in `configs/agent.toml` that carries the
/// loopback default. Matched verbatim (and replaced exactly once) so a
/// template edit that moves or rewords the line fails loudly at request
/// time instead of silently shipping an unrewritten loopback config.
const NATS_URL_LINE: &str = "nats_url = 'nats://127.0.0.1:4222'";

pub async fn installer(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Response, (StatusCode, String)> {
    let store = state
        .jetstream
        .get_object_store(OBJECT_AGENT_RELEASES)
        .await
        .map_err(|e| {
            warn!(error = %e, "get_object_store agent_releases");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Object Store '{OBJECT_AGENT_RELEASES}' missing — run `kanade jetstream setup`"
                ),
            )
        })?;

    // Always the latest release by `modified` (mirrors
    // `agent_releases::list_releases` ordering) — a self-service download
    // offers no version choice.
    let version = {
        let mut metas =
            crate::projector::object_meta::list_bucket(&state.pool, OBJECT_AGENT_RELEASES)
                .await
                .map_err(|e| {
                    warn!(error = %e, "object_store_meta list agent_releases");
                    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                })?;
        metas.sort_by(|a, b| b.modified.cmp(&a.modified));
        match metas.into_iter().next() {
            Some(m) => m.key,
            None => {
                return Err((
                    StatusCode::NOT_FOUND,
                    format!(
                        "no agent releases in {OBJECT_AGENT_RELEASES} — run `kanade agent publish` first"
                    ),
                ));
            }
        }
    };
    check_version(&version)?;

    // The NATS coordinates baked into the ZIP come from the
    // server-settings document, NOT from the caller — a self-service
    // download user must never choose (or see) them.
    let settings = super::server_settings::load(&state).await.map_err(|e| {
        warn!(error = %format!("{e:#}"), "read server_settings for installer");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read server_settings: {e:#}"),
        )
    })?;
    let (nats_url, nats_token) = resolve_nats(&settings, &state.nats_url)?;
    let agent_toml = render_agent_toml(&nats_url)?;

    // When this backend signs commands, provision the fresh agent's ring
    // with THIS backend's own public key — nothing else. Break-glass keys
    // are never bundled; an operator distributes those separately.
    let keyring = state.commands.keyring_entry();
    let command_keys = match &keyring {
        Some(entry) => Some(serde_json::to_string(&vec![entry]).map_err(|e| {
            warn!(error = %e, "serialize command keyring");
            (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
        })?),
        None => None,
    };

    // Read the release binary. ~20 MB in memory is acceptable — the
    // publish path already buffers 64 MB multipart bodies.
    let mut obj = store.get(&version).await.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("not found") || msg.contains("no objects") {
            return (
                StatusCode::NOT_FOUND,
                format!("version '{version}' not in Object Store"),
            );
        }
        warn!(error = %e, %version, "object_store.get");
        (StatusCode::INTERNAL_SERVER_ERROR, msg)
    })?;
    let mut exe = Vec::with_capacity(obj.info().size);
    obj.read_to_end(&mut exe).await.map_err(|e| {
        warn!(error = %e, %version, "read agent binary");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read agent binary '{version}': {e}"),
        )
    })?;

    let install_ps1 = render_install_ps1(&version, nats_token.as_deref(), command_keys.as_deref());
    let install_cmd = render_install_cmd(&version);
    let readme = render_readme(&version, command_keys.is_some());

    let entries: Vec<(&str, Vec<u8>)> = vec![
        ("kanade-agent.exe", exe),
        ("agent.toml", agent_toml.into_bytes()),
        ("deploy-agent.ps1", DEPLOY_AGENT_PS1.as_bytes().to_vec()),
        ("install-agent.ps1", install_ps1.into_bytes()),
        ("install.cmd", install_cmd.into_bytes()),
        ("README.txt", readme.into_bytes()),
    ];
    // Zip writing is CPU-bound (deflate over ~20 MB) — keep it off the
    // async executor.
    let zip_bytes = tokio::task::spawn_blocking(move || build_zip(entries))
        .await
        .map_err(|e| {
            warn!(error = %e, "installer zip task join");
            (StatusCode::INTERNAL_SERVER_ERROR, format!("zip task: {e}"))
        })?
        .map_err(|e| {
            warn!(error = %e, "build installer zip");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("build installer zip: {e}"),
            )
        })?;

    info!(%version, nats_url = %nats_url, "installer: ZIP generated");

    audit::record(
        &state.nats,
        "operator",
        "agent_installer_download",
        Some(&version),
        Some(&caller),
        // NEVER the token itself — only that one was embedded.
        serde_json::json!({
            "version": version,
            "nats_url": nats_url,
            "token_embedded": nats_token.is_some(),
            "command_keys_embedded": keyring.is_some(),
        }),
    )
    .await;

    Ok((
        [
            (header::CONTENT_TYPE, "application/zip".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"kanade-agent-installer-{version}.zip\""),
            ),
        ],
        Body::from(zip_bytes),
    )
        .into_response())
}

/// What the ZIP bakes in for NATS: the `agent_install` section of the
/// server-settings document where set, else this backend's own `[nats] url`
/// and no token. Split out pure so the fallback/validation rules are
/// testable without a broker.
///
/// A stored `nats_url` that fails the TOML literal-string rules (validated
/// at PUT, but a hand-written KV value can bypass that) is a 500 naming the
/// Settings page — NOT a silent fallback, which would ship installers that
/// dial a different broker than the operator configured.
fn resolve_nats(
    settings: &ServerSettings,
    backend_url: &str,
) -> Result<(String, Option<String>), (StatusCode, String)> {
    let Some(ai) = settings.agent_install.as_ref() else {
        return Ok((backend_url.to_string(), None));
    };
    let url = match ai.nats_url.as_deref() {
        Some(u) if u.is_empty() || u.contains('\'') || u.contains('\n') || u.contains('\r') => {
            return Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_settings agent_install.nats_url is invalid (must be non-empty, with no \
                 single quote or newline) — fix it on the Settings page"
                    .into(),
            ));
        }
        Some(u) => u.to_string(),
        None => backend_url.to_string(),
    };
    // An empty stored token (a hand-written KV value bypassing PUT
    // validation) is no token: embedding `-NatsToken ''` would hand
    // deploy-agent.ps1 an empty-string argument, and the audit record
    // would claim a token was embedded when none was.
    let token = ai.nats_token.clone().filter(|t| !t.is_empty());
    Ok((url, token))
}

/// The version string reaches three sinks that tolerate no hostile bytes:
/// a quoted `Content-Disposition` filename, a PowerShell `#` comment, and
/// batch `REM`/`echo` lines. It is caller-supplied or an Object Store key —
/// and keys can also be written directly over NATS, while the PE
/// VERSIONINFO extraction only trims whitespace — so restrict the charset
/// (semver-ish) rather than escaping three different formats.
fn check_version(version: &str) -> Result<(), (StatusCode, String)> {
    if version.is_empty()
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
    {
        return Err((
            StatusCode::BAD_REQUEST,
            "version must be non-empty and contain only [A-Za-z0-9._+-]".into(),
        ));
    }
    Ok(())
}

/// `agent.toml` with the loopback `nats_url` line rewritten to `nats_url`.
///
/// The value goes into a TOML literal string (single quotes, no escape
/// sequences), so a `'` or newline in the URL would either corrupt the
/// document or inject extra config lines — reject both (400) rather than
/// trying to escape a format that has no escapes.
fn render_agent_toml(nats_url: &str) -> Result<String, (StatusCode, String)> {
    if nats_url.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "nats_url must not be empty".into()));
    }
    if nats_url.contains('\'') || nats_url.contains('\n') || nats_url.contains('\r') {
        return Err((
            StatusCode::BAD_REQUEST,
            "nats_url must not contain a single quote or newline (TOML literal-string safety)"
                .into(),
        ));
    }
    if !AGENT_TOML_TEMPLATE.contains(NATS_URL_LINE) {
        warn!("configs/agent.toml no longer contains the expected nats_url line");
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "configs/agent.toml template changed — expected line `{NATS_URL_LINE}` not found; \
                 update the installer rewrite to match"
            ),
        ));
    }
    Ok(AGENT_TOML_TEMPLATE.replacen(NATS_URL_LINE, &format!("nats_url = '{nats_url}'"), 1))
}

/// A PowerShell single-quoted literal: escape `'` by doubling it.
fn ps_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

/// The generated `install-agent.ps1` wrapper. All real work stays in
/// `deploy-agent.ps1`; this only pins the version banner and the two
/// provisioned knobs (`-NatsToken`, `-CommandKeys`), each omitted from
/// the invocation entirely when not provided. CRLF line endings like
/// every other Windows-facing file in the ZIP.
fn render_install_ps1(
    version: &str,
    nats_token: Option<&str>,
    command_keys: Option<&str>,
) -> String {
    let mut args = String::new();
    if let Some(token) = nats_token {
        args.push_str(&format!(" -NatsToken {}", ps_quote(token)));
    }
    if let Some(keys) = command_keys {
        args.push_str(&format!(" -CommandKeys {}", ps_quote(keys)));
    }
    format!(
        "# Generated by kanade-backend — do not edit.\r\n\
         # Installs kanade-agent {version} as a Windows service. Run as Administrator.\r\n\
         $ErrorActionPreference = 'Stop'\r\n\
         & (Join-Path $PSScriptRoot 'deploy-agent.ps1'){args}\r\n\
         exit $LASTEXITCODE\r\n"
    )
}

/// The generated `install.cmd` double-click bootstrap. MUST be CRLF
/// throughout — cmd.exe's parser misparses LF-only batch files inside
/// parenthesized blocks. Echo lines deliberately avoid `&` (a command
/// separator even inside most quoting) and parentheses outside the
/// `if errorlevel` block.
fn render_install_cmd(version: &str) -> String {
    let mut s = String::new();
    s.push_str("@echo off\r\n");
    s.push_str(&format!(
        "REM kanade-agent installer (generated by kanade-backend). Installs kanade-agent {version}.\r\n"
    ));
    s.push_str(
        "powershell -NoProfile -ExecutionPolicy Bypass -File \"%~dp0install-agent.ps1\"\r\n",
    );
    s.push_str("if errorlevel 1 (\r\n");
    s.push_str("  echo.\r\n");
    s.push_str("  echo INSTALL FAILED - see the output above.\r\n");
    s.push_str("  pause\r\n");
    s.push_str("  exit /b 1\r\n");
    s.push_str(")\r\n");
    s.push_str("echo.\r\n");
    s.push_str(&format!("echo kanade-agent {version} installed.\r\n"));
    s.push_str("pause\r\n");
    s
}

/// `README.txt` — the contents list plus the two-step install. Mentions
/// the embedded command-signing PUBLIC key when the backend signs, so the
/// operator knows what the ZIP carries (and what it deliberately does
/// not: break-glass keys).
fn render_readme(version: &str, signing: bool) -> String {
    let signing_note = if signing {
        "This ZIP embeds the backend's command-signing PUBLIC key (provisioned\r\n\
         into the agent's keyring by the installer, so signed commands verify\r\n\
         from first boot). Public keys are not secrets. Break-glass keys are\r\n\
         NEVER included in this ZIP — distribute those separately.\r\n"
    } else {
        "The backend that generated this ZIP is not signing commands, so no\r\n\
         command-signing keyring is provisioned by the installer.\r\n"
    };
    let mut s = String::new();
    s.push_str(&format!("kanade-agent installer (version {version})\r\n"));
    s.push_str("==========================================\r\n");
    s.push_str("\r\n");
    s.push_str("Contents:\r\n");
    s.push_str("\r\n");
    s.push_str(&format!(
        "  kanade-agent.exe   the agent binary (release {version})\r\n"
    ));
    s.push_str("  agent.toml         agent configuration (NATS URL baked in)\r\n");
    s.push_str("  deploy-agent.ps1   the canonical install/update script\r\n");
    s.push_str("  install-agent.ps1  generated wrapper (tokens and keys baked in)\r\n");
    s.push_str("  install.cmd        double-click bootstrap\r\n");
    s.push_str("  README.txt         this file\r\n");
    s.push_str("\r\n");
    s.push_str("Install:\r\n");
    s.push_str("\r\n");
    s.push_str("  1. Extract this ZIP to a folder on the target PC.\r\n");
    s.push_str("  2. Right-click install.cmd and choose \"Run as administrator\".\r\n");
    s.push_str("\r\n");
    s.push_str("Re-running the installer upgrades the agent in place.\r\n");
    s.push_str("\r\n");
    s.push_str(signing_note);
    s
}

/// Assemble the installer ZIP in memory. Pure/blocking — the handler runs
/// it under `spawn_blocking` (deflate over ~20 MB of exe is CPU-bound).
fn build_zip(entries: Vec<(&str, Vec<u8>)>) -> Result<Vec<u8>, zip::result::ZipError> {
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;

    let mut zw = zip::ZipWriter::new(std::io::Cursor::new(Vec::<u8>::new()));
    let opts = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        zw.start_file(name, opts)?;
        zw.write_all(&data)?;
    }
    Ok(zw.finish()?.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_toml_url_line_is_rewritten() {
        let out = render_agent_toml("nats://broker.corp:4222").unwrap();
        assert!(out.contains("nats_url = 'nats://broker.corp:4222'"));
        assert!(!out.contains(NATS_URL_LINE));
        // Everything else is byte-identical to the template: one line
        // differs, nothing more.
        let template_lines: Vec<&str> = AGENT_TOML_TEMPLATE.lines().collect();
        let out_lines: Vec<&str> = out.lines().collect();
        assert_eq!(template_lines.len(), out_lines.len());
        let differing = template_lines
            .iter()
            .zip(&out_lines)
            .filter(|(a, b)| a != b)
            .count();
        assert_eq!(differing, 1);
    }

    #[test]
    fn version_charset_is_restricted() {
        // Mirrors agent_toml_rejects_literal_string_injection: the version
        // is spliced into a ps1 `#` comment, batch REM/echo lines, and a
        // quoted Content-Disposition filename — a newline or quote in an
        // Object Store key must be a 400, not code execution on install.
        for bad in [
            "",
            "0.43.99\n evil",
            "0.43.99\revil",
            "0.43.99\"x",
            "0.43.99'x",
            "0.43.99&del",
            "0.43.99%PATH%",
        ] {
            let (code, _) = check_version(bad).unwrap_err();
            assert_eq!(code, StatusCode::BAD_REQUEST, "{bad:?}");
        }
        for good in ["0.43.99", "0.43.99-rc.1+build.5", "1.0.0_alpha"] {
            check_version(good).unwrap();
        }
    }

    #[test]
    fn agent_toml_rejects_literal_string_injection() {
        // A quote would terminate the TOML literal string early and let the
        // rest of the value rewrite the document; a newline would inject
        // whole extra lines. Both are 400s, not escapes — TOML literal
        // strings have no escape sequences.
        for bad in [
            "nats://evil'\n[agent]\nid='x'",
            "nats://a\nb",
            "nats://a\rb",
        ] {
            let (code, _) = render_agent_toml(bad).unwrap_err();
            assert_eq!(code, StatusCode::BAD_REQUEST, "{bad:?}");
        }
        let (code, _) = render_agent_toml("").unwrap_err();
        assert_eq!(code, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn install_ps1_embeds_token_and_keys_when_given() {
        let out = render_install_ps1(
            "0.43.99",
            Some("s3cret"),
            Some(r#"[{"kid":"backend-1","public_key":"AAAA","label":"backend"}]"#),
        );
        assert!(out.contains("# Installs kanade-agent 0.43.99 as a Windows service."));
        assert!(out.contains("-NatsToken 's3cret'"));
        assert!(out.contains(
            "-CommandKeys '[{\"kid\":\"backend-1\",\"public_key\":\"AAAA\",\"label\":\"backend\"}]'"
        ));
        assert!(out.contains("exit $LASTEXITCODE"));
    }

    #[test]
    fn install_ps1_omits_args_entirely_when_not_given() {
        let out = render_install_ps1("0.43.99", None, None);
        assert!(!out.contains("-NatsToken"));
        assert!(!out.contains("-CommandKeys"));
        // Nothing trailing the script path but the newline.
        assert!(out.contains("& (Join-Path $PSScriptRoot 'deploy-agent.ps1')\r\n"));
    }

    #[test]
    fn install_ps1_doubles_single_quotes() {
        // PowerShell single-quoted literal escaping: `'` becomes `''`. An
        // unescaped quote would terminate the literal and let the rest of
        // the token run as script.
        let out = render_install_ps1("0.43.99", Some("it's"), None);
        assert!(out.contains("-NatsToken 'it''s'"));
    }

    #[test]
    fn install_cmd_is_crlf_throughout() {
        let out = render_install_cmd("0.43.99");
        assert!(!out.is_empty());
        // No bare LF anywhere: every line ends CRLF. cmd.exe's batch
        // parser misparses LF-only files inside parenthesized blocks.
        for (i, b) in out.bytes().enumerate() {
            if b == b'\n' {
                assert!(
                    i > 0 && out.as_bytes()[i - 1] == b'\r',
                    "bare LF at byte {i}"
                );
            }
        }
        assert!(out.contains("%~dp0install-agent.ps1"));
        assert!(out.contains("REM kanade-agent installer (generated by kanade-backend). Installs kanade-agent 0.43.99."));
        // Echo lines must not carry cmd metacharacters that break out of
        // the echo (`&` separates commands even where quoting is fuzzy).
        for line in out.lines().filter(|l| l.trim_start().starts_with("echo ")) {
            assert!(!line.contains('&'), "echo line with '&': {line}");
        }
    }

    #[test]
    fn zip_round_trips_all_entries() {
        let agent_toml = render_agent_toml("nats://broker.corp:4222").unwrap();
        let install_ps1 = render_install_ps1("0.43.99", Some("tok"), Some("[{\"kid\":\"k\"}]"));
        let entries: Vec<(&str, Vec<u8>)> = vec![
            ("kanade-agent.exe", b"MZ-fake-exe".to_vec()),
            ("agent.toml", agent_toml.clone().into_bytes()),
            ("deploy-agent.ps1", DEPLOY_AGENT_PS1.as_bytes().to_vec()),
            ("install-agent.ps1", install_ps1.clone().into_bytes()),
            ("install.cmd", render_install_cmd("0.43.99").into_bytes()),
            ("README.txt", render_readme("0.43.99", true).into_bytes()),
        ];
        let bytes = build_zip(entries).unwrap();

        let mut archive =
            zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("zip reads back");
        let names: std::collections::HashSet<String> =
            archive.file_names().map(str::to_owned).collect();
        for expected in [
            "kanade-agent.exe",
            "agent.toml",
            "deploy-agent.ps1",
            "install-agent.ps1",
            "install.cmd",
            "README.txt",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
        assert_eq!(names.len(), 6);

        // The generated files round-trip byte-for-byte, and the deploy
        // script ships unmodified.
        use std::io::Read as _;
        let mut buf = String::new();
        archive
            .by_name("agent.toml")
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, agent_toml);
        buf.clear();
        archive
            .by_name("install-agent.ps1")
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, install_ps1);
        buf.clear();
        archive
            .by_name("deploy-agent.ps1")
            .unwrap()
            .read_to_string(&mut buf)
            .unwrap();
        assert_eq!(buf, DEPLOY_AGENT_PS1);
    }

    #[test]
    fn readme_names_the_signing_state() {
        let signed = render_readme("0.43.99", true);
        assert!(signed.contains("command-signing PUBLIC key"));
        assert!(signed.contains("Break-glass keys are\r\nNEVER included"));
        let unsigned = render_readme("0.43.99", false);
        assert!(unsigned.contains("not signing commands"));
        assert!(!unsigned.contains("PUBLIC key (provisioned"));
    }

    #[test]
    fn resolve_nats_falls_back_to_the_backend_url() {
        // No section at all, and a section without a url, both mean "dial
        // the same broker this backend does"; neither embeds a token.
        for settings in [
            ServerSettings::default(),
            ServerSettings {
                agent_install: Some(kanade_shared::wire::AgentInstallSection {
                    nats_token: Some("tok".into()),
                    ..Default::default()
                }),
                ..Default::default()
            },
        ] {
            let (url, _) = resolve_nats(&settings, "nats://backend:4222").unwrap();
            assert_eq!(url, "nats://backend:4222");
        }
        let (_, token) = resolve_nats(&ServerSettings::default(), "nats://backend:4222").unwrap();
        assert_eq!(token, None);
    }

    #[test]
    fn resolve_nats_prefers_the_settings_values() {
        let settings = ServerSettings {
            agent_install: Some(kanade_shared::wire::AgentInstallSection {
                nats_url: Some("nats://broker.corp:4222".into()),
                nats_token: Some("s3cret".into()),
                nats_token_set: false,
            }),
            ..Default::default()
        };
        let (url, token) = resolve_nats(&settings, "nats://backend:4222").unwrap();
        assert_eq!(url, "nats://broker.corp:4222");
        assert_eq!(token.as_deref(), Some("s3cret"));
    }

    #[test]
    fn resolve_nats_treats_an_empty_token_as_no_token() {
        // A hand-written KV value with `nats_token = Some("")` bypasses PUT
        // validation — embedding `-NatsToken ''` would hand deploy-agent.ps1
        // an empty argument and the audit would claim a token was embedded.
        let settings = ServerSettings {
            agent_install: Some(kanade_shared::wire::AgentInstallSection {
                nats_token: Some(String::new()),
                ..Default::default()
            }),
            ..Default::default()
        };
        let (_, token) = resolve_nats(&settings, "nats://backend:4222").unwrap();
        assert_eq!(token, None);
    }

    #[test]
    fn resolve_nats_rejects_a_hostile_stored_url() {
        // PUT validates, but a hand-written KV value can bypass it — a bad
        // stored url is a 500 naming the Settings page, never a ZIP with
        // injected agent.toml lines.
        for bad in ["", "nats://evil'\nx='y'", "nats://a\nb", "nats://a\rb"] {
            let settings = ServerSettings {
                agent_install: Some(kanade_shared::wire::AgentInstallSection {
                    nats_url: Some(bad.into()),
                    ..Default::default()
                }),
                ..Default::default()
            };
            let (code, msg) = resolve_nats(&settings, "nats://backend:4222").unwrap_err();
            assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR, "{bad:?}");
            assert!(msg.contains("Settings"), "{msg}");
        }
    }
}

//! `GET /api/agents/installer` — generate an agent installer archive on the
//! fly, for Windows (ZIP) or Linux (tar.gz). Sibling endpoints
//! `GET /api/agents/installer.ps1` / `GET /api/agents/installer.sh` return
//! generated one-liner scripts that download + extract + run that archive
//! in a single pasted command (each embeds the caller's own Bearer token
//! so the inner download is authenticated).
//!
//! Self-service: the route sits in the viewer+ base router, gated by the
//! `agent-install` page feature, so a restricted "download user" account
//! (viewer + ONLY that feature) can kit a fresh machine: extract, run the
//! bootstrap as admin/root, done. There is no request body and no version
//! parameter — the caller picks only the platform (`?os=windows|linux`,
//! default `windows`; `?arch=x86_64|aarch64`, default `x86_64`). The
//! archive always bundles the latest release FOR THAT PLATFORM (by Object
//! Store `modified`, over the platform's keys — bare keys for Windows,
//! `<version>-linux-<arch>` for Linux; see kanade_shared::bin_platform),
//! and the NATS url/token it bakes in come from the `agent_install`
//! section of the server-settings document (falling back to this
//! backend's own `[nats] url`, no token).
//!
//! Windows ZIP contents:
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
//!
//! Linux tar.gz contents (the `deploy/linux/bundle-agent.sh` layout,
//! relative to the extraction root):
//!
//!   * `bin/kanade-agent` (0755) — the latest `<version>-linux-<arch>`
//!     release.
//!   * `etc/agent.toml` — same rewritten config as the Windows ZIP.
//!   * `systemd/kanade-agent.service` — the repo unit, verbatim.
//!   * `setup-agent.sh` (0755) — the canonical Linux installer, verbatim.
//!     It resolves the token from `KANADE_NATS_TOKEN` → co-located
//!     `nats.env` → existing `agent.env` → hard fail, and overrides
//!     `nats_url` only when `KANADE_NATS_URL` is set.
//!   * `install.sh` (0755) — a generated wrapper: exports
//!     `KANADE_NATS_TOKEN` when (and only when) the settings carry one,
//!     then `exec ./setup-agent.sh`. `KANADE_NATS_URL` is deliberately NOT
//!     exported — the baked agent.toml already carries it.
//!   * `README.txt` — extract + `sudo ./install.sh` instructions, with
//!     the note that command-signing keyring provisioning is Windows-only
//!     today (#1165 gap).

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use kanade_shared::bin_platform::{LINUX_SUFFIX_AARCH64, LINUX_SUFFIX_X86_64, platform_of_key};
use kanade_shared::kv::OBJECT_AGENT_RELEASES;
use kanade_shared::wire::ServerSettings;
use serde::Deserialize;
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

/// The canonical Linux install script + systemd unit, shipped verbatim for
/// the same reason — all Linux install logic lives in `setup-agent.sh`.
const SETUP_AGENT_SH: &str = include_str!("../../../../deploy/linux/setup-agent.sh");
const AGENT_SERVICE: &str = include_str!("../../../../deploy/linux/systemd/kanade-agent.service");

/// The exact `[agent]` line in `configs/agent.toml` that carries the
/// loopback default. Matched verbatim (and replaced exactly once) so a
/// template edit that moves or rewords the line fails loudly at request
/// time instead of silently shipping an unrewritten loopback config.
const NATS_URL_LINE: &str = "nats_url = 'nats://127.0.0.1:4222'";

/// Which OS the generated installer targets. Default Windows (the
/// pre-Linux behavior, so existing links keep working).
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum InstallerOs {
    #[default]
    Windows,
    Linux,
}

impl InstallerOs {
    fn as_str(&self) -> &'static str {
        match self {
            InstallerOs::Windows => "windows",
            InstallerOs::Linux => "linux",
        }
    }
}

/// Which CPU architecture a Linux installer targets. Default x86_64.
/// (Windows releases sit at the bare `<version>` key regardless of arch —
/// the key scheme's backward-compat decision — so this only filters Linux.)
#[derive(Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
enum InstallerArch {
    #[default]
    X86_64,
    Aarch64,
}

impl InstallerArch {
    fn as_str(&self) -> &'static str {
        match self {
            InstallerArch::X86_64 => "x86_64",
            InstallerArch::Aarch64 => "aarch64",
        }
    }

    /// The Object Store key suffix for this arch's Linux releases.
    fn linux_suffix(&self) -> &'static str {
        match self {
            InstallerArch::X86_64 => LINUX_SUFFIX_X86_64,
            InstallerArch::Aarch64 => LINUX_SUFFIX_AARCH64,
        }
    }
}

/// `?os=windows|linux&arch=x86_64|aarch64` — both optional, unknown values
/// rejected 400 by the Query extractor's serde failure.
#[derive(Deserialize, Debug, Default)]
pub struct InstallerParams {
    #[serde(default)]
    os: InstallerOs,
    #[serde(default)]
    arch: InstallerArch,
}

pub async fn installer(
    State(state): State<AppState>,
    caller: Caller,
    Query(params): Query<InstallerParams>,
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

    // Always the latest release FOR THE REQUESTED PLATFORM by `modified`
    // (mirrors `agent_releases::list_releases` ordering) — a self-service
    // download offers no version choice, only os/arch.
    let key = {
        let metas = crate::projector::object_meta::list_bucket(&state.pool, OBJECT_AGENT_RELEASES)
            .await
            .map_err(|e| {
                warn!(error = %e, "object_store_meta list agent_releases");
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
            })?;
        let rows: Vec<(String, Option<String>)> =
            metas.into_iter().map(|m| (m.key, m.modified)).collect();
        match latest_key_for_platform(&rows, params.os, params.arch) {
            Some(k) => k,
            None => {
                let label = match params.os {
                    InstallerOs::Windows => "windows".to_string(),
                    InstallerOs::Linux => format!("linux-{}", params.arch.as_str()),
                };
                return Err((
                    StatusCode::NOT_FOUND,
                    format!(
                        "no {label} agent releases in {OBJECT_AGENT_RELEASES} — publish one first \
                         (`kanade agent publish`)"
                    ),
                ));
            }
        }
    };
    check_version(&key)?;

    // The NATS coordinates baked into the archive come from the
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

    // Read the release binary. ~20 MB in memory is acceptable — the
    // publish path already buffers 64 MB multipart bodies.
    let mut obj = store.get(&key).await.map_err(|e| {
        let msg = e.to_string();
        if msg.contains("not found") || msg.contains("no objects") {
            return (
                StatusCode::NOT_FOUND,
                format!("release '{key}' not in Object Store"),
            );
        }
        warn!(error = %e, %key, "object_store.get");
        (StatusCode::INTERNAL_SERVER_ERROR, msg)
    })?;
    let mut exe = Vec::with_capacity(obj.info().size);
    obj.read_to_end(&mut exe).await.map_err(|e| {
        warn!(error = %e, %key, "read agent binary");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("read agent binary '{key}': {e}"),
        )
    })?;

    // Archive assembly is CPU-bound (deflate/gzip over ~20 MB) — built in
    // a spawn_blocking closure below. The keyring is embedded only in the
    // Windows flow: command-signing keyring provisioning is Windows-only
    // today (#1165 gap), so the Linux tarball never carries one.
    let (content_type, filename, payload, command_keys_embedded) = match params.os {
        InstallerOs::Windows => {
            // When this backend signs commands, provision the fresh agent's
            // ring with THIS backend's own public key — nothing else.
            // Break-glass keys are never bundled; an operator distributes
            // those separately.
            let keyring = state.commands.keyring_entry();
            let command_keys = match &keyring {
                Some(entry) => Some(serde_json::to_string(&vec![entry]).map_err(|e| {
                    warn!(error = %e, "serialize command keyring");
                    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
                })?),
                None => None,
            };
            let install_ps1 =
                render_install_ps1(&key, nats_token.as_deref(), command_keys.as_deref());
            let install_cmd = render_install_cmd(&key);
            let readme = render_readme(&key, command_keys.is_some());
            let entries: Vec<(&str, Vec<u8>)> = vec![
                ("kanade-agent.exe", exe),
                ("agent.toml", agent_toml.into_bytes()),
                ("deploy-agent.ps1", DEPLOY_AGENT_PS1.as_bytes().to_vec()),
                ("install-agent.ps1", install_ps1.into_bytes()),
                ("install.cmd", install_cmd.into_bytes()),
                ("README.txt", readme.into_bytes()),
            ];
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
            (
                "application/zip",
                format!("kanade-agent-installer-{key}.zip"),
                zip_bytes,
                command_keys.is_some(),
            )
        }
        InstallerOs::Linux => {
            let install_sh = render_install_sh(nats_token.as_deref());
            let readme = render_readme_linux(&key);
            let entries: Vec<TarEntry> = vec![
                TarEntry::new("bin/kanade-agent", 0o755, exe),
                TarEntry::new("etc/agent.toml", 0o644, agent_toml.into_bytes()),
                TarEntry::new(
                    "systemd/kanade-agent.service",
                    0o644,
                    AGENT_SERVICE.as_bytes().to_vec(),
                ),
                TarEntry::new("setup-agent.sh", 0o755, SETUP_AGENT_SH.as_bytes().to_vec()),
                TarEntry::new("install.sh", 0o755, install_sh.into_bytes()),
                TarEntry::new("README.txt", 0o644, readme.into_bytes()),
            ];
            let arch = params.arch;
            let tgz_bytes = tokio::task::spawn_blocking(move || build_tar_gz(entries))
                .await
                .map_err(|e| {
                    warn!(error = %e, "installer tar task join");
                    (StatusCode::INTERNAL_SERVER_ERROR, format!("tar task: {e}"))
                })?
                .map_err(|e| {
                    warn!(error = %e, "build installer tarball");
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("build installer tarball: {e}"),
                    )
                })?;
            info!(%key, arch = arch.as_str(), "installer: tar.gz generated");
            (
                "application/gzip",
                format!("kanade-agent-installer-{key}.tar.gz"),
                tgz_bytes,
                false,
            )
        }
    };

    info!(%key, os = params.os.as_str(), nats_url = %nats_url, "installer: archive generated");

    audit::record(
        &state.nats,
        "operator",
        "agent_installer_download",
        Some(&key),
        Some(&caller),
        // NEVER the token itself — only that one was embedded.
        serde_json::json!({
            "version": key,
            "os": params.os.as_str(),
            "arch": params.arch.as_str(),
            "nats_url": nats_url,
            "token_embedded": nats_token.is_some(),
            "command_keys_embedded": command_keys_embedded,
        }),
    )
    .await;

    Ok((
        [
            (header::CONTENT_TYPE, content_type.to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{filename}\""),
            ),
        ],
        Body::from(payload),
    )
        .into_response())
}

// ─── GET /api/agents/installer.ps1 + installer.sh — one-liner scripts ───

/// `GET /api/agents/installer.ps1` — a generated PowerShell one-liner
/// installer: download the Windows ZIP (authenticated as the caller) into
/// a temp dir, extract, run `install-agent.ps1` — elevated via UAC for
/// just the install step when the caller isn't already admin.
pub async fn installer_ps1(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let (base, token) = script_context(&state, &headers)?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        render_installer_ps1(&base, &token),
    )
        .into_response())
}

/// `GET /api/agents/installer.sh` — a generated POSIX sh one-liner
/// installer: map `uname -m` to a release arch, download the Linux tar.gz
/// (authenticated as the caller), extract, run `install.sh`. Meant to be
/// piped through sudo (`curl … | sudo bash`).
pub async fn installer_sh(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, (StatusCode, String)> {
    let (base, token) = script_context(&state, &headers)?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        render_installer_sh(&base, &token),
    )
        .into_response())
}

/// The two values baked into a one-liner script: this backend's absolute
/// base URL and the caller's own Bearer token (so the script's inner
/// archive download authenticates as the same account that generated it —
/// the token expires with that session, which the script header says).
///
/// The base URL rule is exactly the account-email one
/// (`password_setup::link_base`): configured `[server] public_url` when
/// set, else derived from the request `Host` (+ `X-Forwarded-Proto`)
/// header. The `Host` fallback is acceptable here for the same reason it
/// is there: the caller is authenticated, so a spoofed Host only poisons
/// their own script.
fn script_context(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<(String, String), (StatusCode, String)> {
    let base = super::password_setup::link_base(state.public_url.as_deref(), headers).ok_or((
        StatusCode::INTERNAL_SERVER_ERROR,
        "cannot derive the backend's base URL (no [server] public_url configured and no Host \
         header on the request) — set public_url in backend.toml"
            .to_string(),
    ))?;
    let token = bearer_token(headers).ok_or((
        StatusCode::BAD_REQUEST,
        "script generation requires a Bearer Authorization header".to_string(),
    ))?;
    Ok((base, token.to_string()))
}

/// The raw token out of an `Authorization: Bearer <token>` header, or
/// `None` when the header is missing, malformed, or empty. (The header
/// always exists on authenticated calls — `auth::verify` required it —
/// but a service-token or auth-disabled caller may present something
/// else, hence the graceful 400 path.)
fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty())
}

/// The generated `installer.ps1` one-liner (CRLF line endings). `base`
/// and `token` are embedded as PowerShell single-quoted literals
/// ([`ps_quote`]), so a `'` in either can't break out.
fn render_installer_ps1(base: &str, token: &str) -> String {
    let zip_url = ps_quote(&format!("{base}/api/agents/installer"));
    let auth = ps_quote(&format!("Bearer {token}"));
    let mut s = String::new();
    s.push_str(
        "# kanade-agent one-liner installer (generated by kanade-backend — do not edit)\r\n",
    );
    s.push_str("# Installs the latest agent as a Windows service. The embedded token expires\r\n");
    s.push_str("# with the issuer's session.\r\n");
    s.push_str("$ErrorActionPreference = 'Stop'\r\n");
    s.push_str("$tmp = Join-Path $env:TEMP ('kanade-agent-install-' + [guid]::NewGuid())\r\n");
    s.push_str("New-Item -ItemType Directory -Path $tmp | Out-Null\r\n");
    s.push_str("$zip = Join-Path $tmp 'installer.zip'\r\n");
    s.push_str("try {\r\n");
    s.push_str(&format!(
        "    Invoke-WebRequest -Uri {zip_url} -Headers @{{ Authorization = {auth} }} -OutFile $zip\r\n"
    ));
    s.push_str("    Expand-Archive -Path $zip -DestinationPath $tmp\r\n");
    s.push_str("    $installScript = Join-Path $tmp 'install-agent.ps1'\r\n");
    s.push_str("    $isAdmin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)\r\n");
    s.push_str("    if ($isAdmin) {\r\n");
    s.push_str("        & $installScript\r\n");
    s.push_str("        $code = $LASTEXITCODE\r\n");
    s.push_str("    } else {\r\n");
    s.push_str("        # Elevate ONLY the install step (UAC prompt); the download above\r\n");
    s.push_str(
        "        # already ran unelevated, so the token never reaches an admin process.\r\n",
    );
    s.push_str("        $proc = Start-Process powershell -Verb RunAs -ArgumentList '-NoProfile','-ExecutionPolicy','Bypass','-File',\"`\"$installScript`\"\" -Wait -PassThru\r\n");
    s.push_str("        $code = $proc.ExitCode\r\n");
    s.push_str("    }\r\n");
    s.push_str("    if ($code -ne 0) { throw \"install failed (exit $code)\" }\r\n");
    s.push_str("    Write-Host \"kanade-agent installed.\"\r\n");
    s.push_str("} finally {\r\n");
    s.push_str("    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue\r\n");
    s.push_str("}\r\n");
    s
}

/// The generated `installer.sh` one-liner (LF line endings). `base` and
/// `token` are embedded as POSIX single-quoted literals ([`sh_quote`]).
fn render_installer_sh(base: &str, token: &str) -> String {
    let zip_url = sh_quote(&format!("{base}/api/agents/installer?os=linux&arch="));
    // The token rides a curl config heredoc (`-K -`), NOT a `-H` flag:
    // /proc/<pid>/cmdline is world-readable on Linux, so a header on the
    // command line would expose the session token to every local user on
    // the target host. Escape the two bytes that could break out of the
    // double-quoted config value.
    let token_cfg = token.replace('\\', "\\\\").replace('"', "\\\"");
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str("# kanade-agent one-liner installer (generated by kanade-backend — do not edit)\n");
    s.push_str("# Meant to be run via the one-liner on the Agent Install page\n");
    s.push_str(&format!(
        "#   ({base}/agent-install — piped through `sudo bash`).\n"
    ));
    s.push_str("# install.sh needs root; without the sudo pipe it fails there by design.\n");
    s.push_str("# The embedded token expires with the issuer's session.\n");
    s.push_str("set -eu\n");
    s.push_str("case \"$(uname -m)\" in\n");
    s.push_str("    x86_64) ARCH=x86_64 ;;\n");
    s.push_str("    aarch64|arm64) ARCH=aarch64 ;;\n");
    s.push_str("    *) echo \"unsupported architecture: $(uname -m)\" >&2; exit 1 ;;\n");
    s.push_str("esac\n");
    s.push_str("TMP=\"$(mktemp -d)\"\n");
    s.push_str("trap 'rm -rf \"$TMP\"' EXIT\n");
    // The arch var sits OUTSIDE the single-quoted URL so the shell
    // expands it; the base half stays literal-safe.
    s.push_str(&format!(
        "curl -fsSL -K - -o \"$TMP/installer.tar.gz\" {zip_url}\"$ARCH\" <<'KANADE_CURL_CONFIG'\n"
    ));
    s.push_str(&format!("header = \"Authorization: Bearer {token_cfg}\"\n"));
    s.push_str("KANADE_CURL_CONFIG\n");
    s.push_str("tar xzf \"$TMP/installer.tar.gz\" -C \"$TMP\"\n");
    s.push_str("sh \"$TMP/install.sh\"\n");
    s.push_str("echo \"kanade-agent installed.\"\n");
    s
}

/// The latest key (by `modified`, desc) belonging to the requested
/// platform. `rows` are `(key, modified)` pairs from the object_meta
/// index. Pure so the per-platform filtering is testable without a DB.
fn latest_key_for_platform(
    rows: &[(String, Option<String>)],
    os: InstallerOs,
    arch: InstallerArch,
) -> Option<String> {
    let mut matches: Vec<&(String, Option<String>)> = rows
        .iter()
        .filter(|(key, _)| key_matches_platform(key, os, arch))
        .collect();
    matches.sort_by(|a, b| b.1.cmp(&a.1));
    matches.first().map(|(k, _)| k.clone())
}

/// Whether a store key belongs to the requested platform, by SUFFIX only
/// (semver prerelease dashes mid-version are never parsed — see
/// kanade_shared::bin_platform).
fn key_matches_platform(key: &str, os: InstallerOs, arch: InstallerArch) -> bool {
    match os {
        InstallerOs::Windows => platform_of_key(key) == "windows",
        InstallerOs::Linux => key.ends_with(arch.linux_suffix()),
    }
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

/// The store key reaches several sinks that tolerate no hostile bytes: a
/// quoted `Content-Disposition` filename, a PowerShell `#` comment, batch
/// `REM`/`echo` lines, and shell scripts. Keys can also be written
/// directly over NATS, while the PE VERSIONINFO extraction only trims
/// whitespace — so the charset is restricted (semver-ish) rather than
/// escaping four different formats. The rule itself lives in
/// kanade_shared::bin_platform, shared with the publish endpoints.
fn check_version(version: &str) -> Result<(), (StatusCode, String)> {
    kanade_shared::bin_platform::check_release_key(version)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))
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

/// One entry of the Linux installer tarball: path (relative to the
/// extraction root, matching the layout `setup-agent.sh` expects around
/// itself), permission bits, and bytes.
struct TarEntry {
    name: &'static str,
    mode: u32,
    data: Vec<u8>,
}

impl TarEntry {
    fn new(name: &'static str, mode: u32, data: Vec<u8>) -> Self {
        Self { name, mode, data }
    }
}

/// Assemble the Linux installer tar.gz in memory. Pure/blocking — the
/// handler runs it under `spawn_blocking` (gzip over ~20 MB of binary is
/// CPU-bound). Modes are set explicitly per entry: the exec bit on
/// `bin/kanade-agent`, `setup-agent.sh` and `install.sh` is what lets the
/// README say `sudo ./install.sh` instead of `sudo bash install.sh` (a
/// Windows-built tar without it is exactly why setup-agent.sh documents
/// `bash ./setup-agent.sh`).
fn build_tar_gz(entries: Vec<TarEntry>) -> std::io::Result<Vec<u8>> {
    let enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut builder = tar::Builder::new(enc);
    for entry in &entries {
        let mut header = tar::Header::new_gnu();
        header.set_size(entry.data.len() as u64);
        header.set_mode(entry.mode);
        // Reproducible output: no build-machine mtime baked in.
        header.set_mtime(0);
        header.set_cksum();
        builder.append_data(&mut header, entry.name, entry.data.as_slice())?;
    }
    let enc = builder.into_inner()?;
    enc.finish()
}

/// A POSIX shell single-quoted literal: `'` → `'\''` (close, escaped
/// quote, reopen). Newlines never reach here — the settings PUT rejects
/// them in `nats_token`.
fn sh_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

/// The generated `install.sh` wrapper (LF endings, 0755). All real work
/// stays in `setup-agent.sh`; this only provisions the token from the
/// backend's settings and hands over. `KANADE_NATS_URL` is deliberately
/// NOT exported: the baked `etc/agent.toml` already carries it, and
/// setup-agent.sh only rewrites the url when the env var is set — leaving
/// it unset keeps the "preserve an existing deployment's broker on
/// redeploy" logic intact.
fn render_install_sh(nats_token: Option<&str>) -> String {
    let mut s = String::new();
    s.push_str("#!/bin/sh\n");
    s.push_str("# Generated by kanade-backend — do not edit.\n");
    s.push_str("# Installs kanade-agent as a systemd service. Run as root (sudo).\n");
    s.push_str("set -eu\n");
    s.push_str("cd \"$(dirname \"$0\")\"\n");
    if let Some(token) = nats_token {
        s.push_str(&format!("export KANADE_NATS_TOKEN={}\n", sh_quote(token)));
    }
    s.push_str("exec ./setup-agent.sh\n");
    s
}

/// Linux `README.txt` — the contents list plus extract/install steps.
/// Calls out the #1165 gap explicitly: command-signing keyring
/// provisioning is Windows-only today (the Windows install script writes
/// the keyring to the registry; there is no Linux equivalent yet), so a
/// Linux agent installed from this tarball runs with signature
/// verification inactive until that's built.
fn render_readme_linux(key: &str) -> String {
    let mut s = String::new();
    s.push_str(&format!("kanade-agent installer (release {key})\n"));
    s.push_str("=================================================\n");
    s.push('\n');
    s.push_str("Contents:\n");
    s.push('\n');
    s.push_str("  bin/kanade-agent              the agent binary\n");
    s.push_str("  etc/agent.toml                agent configuration (NATS URL baked in)\n");
    s.push_str("  systemd/kanade-agent.service  the systemd unit\n");
    s.push_str("  setup-agent.sh                the canonical install/update script\n");
    s.push_str("  install.sh                    generated wrapper (token baked in, if any)\n");
    s.push_str("  README.txt                    this file\n");
    s.push('\n');
    s.push_str("Install:\n");
    s.push('\n');
    s.push_str("  1. Extract this tarball on the target machine and enter the\n");
    s.push_str("     directory, e.g.:\n");
    s.push_str("       mkdir kanade-agent-installer && cd kanade-agent-installer\n");
    s.push_str(&format!(
        "       tar xzf ../kanade-agent-installer-{key}.tar.gz\n"
    ));
    s.push_str("  2. Run the installer as root:\n");
    s.push_str("       sudo ./install.sh\n");
    s.push('\n');
    s.push_str("Re-running the installer upgrades the agent in place (an existing\n");
    s.push_str("/etc/kanade/agent.env token and broker URL are preserved).\n");
    s.push('\n');
    s.push_str("Note: command-signing keyring provisioning is Windows-only today, so\n");
    s.push_str("signed-command verification is INACTIVE on Linux agents installed\n");
    s.push_str("from this tarball (the #1165 enforcement gap) — break-glass and\n");
    s.push_str("backend public keys must be provisioned separately once a Linux\n");
    s.push_str("provisioning path exists.\n");
    s
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
    fn installer_ps1_embeds_base_and_token_crlf_only() {
        let out = render_installer_ps1("https://kanade.example.com", "tok-123");
        assert!(
            out.contains(
                "Invoke-WebRequest -Uri 'https://kanade.example.com/api/agents/installer'"
            )
        );
        assert!(out.contains("-Headers @{ Authorization = 'Bearer tok-123' }"));
        assert!(out.contains("Expand-Archive"));
        assert!(out.contains("-Verb RunAs"));
        assert!(out.contains("$LASTEXITCODE"));
        assert!(out.contains("token expires"));
        // CRLF throughout — no bare LF (PowerShell tolerates LF, but the
        // file is Windows-facing like every other ps1 we generate).
        for (i, b) in out.bytes().enumerate() {
            if b == b'\n' {
                assert!(
                    i > 0 && out.as_bytes()[i - 1] == b'\r',
                    "bare LF at byte {i}"
                );
            }
        }
    }

    #[test]
    fn installer_ps1_doubles_single_quotes_in_embeds() {
        // An unescaped quote in the token would terminate the literal and
        // let the rest run as script.
        let out = render_installer_ps1("https://k", "it's");
        assert!(out.contains("Authorization = 'Bearer it''s'"));
    }

    #[test]
    fn installer_sh_embeds_base_token_and_arch_map_lf_only() {
        let out = render_installer_sh("https://kanade.example.com", "tok-123");
        assert!(out.starts_with("#!/bin/sh\n"));
        assert!(out.contains("set -eu\n"));
        // The token rides a stdin curl config (-K -), never a -H flag —
        // /proc/<pid>/cmdline is world-readable on Linux.
        assert!(!out.contains("-H 'Authorization:"));
        assert!(out.contains("curl -fsSL -K - -o \"$TMP/installer.tar.gz\""));
        assert!(out.contains("header = \"Authorization: Bearer tok-123\""));
        assert!(out.contains("<<'KANADE_CURL_CONFIG'"));
        assert!(
            out.contains(
                "'https://kanade.example.com/api/agents/installer?os=linux&arch='\"$ARCH\""
            )
        );
        assert!(out.contains("x86_64) ARCH=x86_64 ;;"));
        assert!(out.contains("aarch64|arm64) ARCH=aarch64 ;;"));
        assert!(out.contains("unsupported architecture"));
        assert!(out.contains("tar xzf \"$TMP/installer.tar.gz\" -C \"$TMP\""));
        assert!(out.contains("sh \"$TMP/install.sh\""));
        assert!(out.contains("sudo bash"));
        // LF only.
        assert!(!out.contains('\r'));
    }

    #[test]
    fn installer_sh_escapes_the_token_for_the_curl_config() {
        // A `"` or `\` in the token would break out of the double-quoted
        // config value.
        let out = render_installer_sh("https://k", "we\"ird\\tok");
        assert!(out.contains("header = \"Authorization: Bearer we\\\"ird\\\\tok\""));
    }

    #[test]
    fn bearer_token_parsing() {
        let mut h = HeaderMap::new();
        assert_eq!(bearer_token(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer tok-123".parse().unwrap());
        assert_eq!(bearer_token(&h), Some("tok-123"));
        // Wrong scheme / empty token → None (the handler 400s).
        h.insert(header::AUTHORIZATION, "Basic dXNlcg==".parse().unwrap());
        assert_eq!(bearer_token(&h), None);
        h.insert(header::AUTHORIZATION, "Bearer ".parse().unwrap());
        assert_eq!(bearer_token(&h), None);
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

    #[test]
    fn params_default_to_windows_x86_64_and_reject_unknowns() {
        let p: InstallerParams = serde_urlencoded_defaults();
        assert_eq!(p.os, InstallerOs::Windows);
        assert_eq!(p.arch, InstallerArch::X86_64);
        // Unknown values are a deserialize error — axum's Query extractor
        // turns that into a 400 before the handler runs.
        assert!(serde_json::from_str::<InstallerOs>(r#""macos""#).is_err());
        assert!(serde_json::from_str::<InstallerArch>(r#""armv7""#).is_err());
        assert_eq!(
            serde_json::from_str::<InstallerOs>(r#""linux""#).unwrap(),
            InstallerOs::Linux
        );
        assert_eq!(
            serde_json::from_str::<InstallerArch>(r#""aarch64""#).unwrap(),
            InstallerArch::Aarch64
        );
    }

    fn serde_urlencoded_defaults() -> InstallerParams {
        InstallerParams::default()
    }

    #[test]
    fn latest_key_filters_by_platform() {
        let rows: Vec<(String, Option<String>)> = vec![
            ("0.44.0".into(), Some("2026-07-01T00:00:00Z".into())),
            ("0.45.4".into(), Some("2026-07-03T00:00:00Z".into())),
            (
                "0.45.4-linux-x86_64".into(),
                Some("2026-07-02T00:00:00Z".into()),
            ),
            (
                "0.45.3-linux-x86_64".into(),
                Some("2026-07-04T00:00:00Z".into()),
            ),
            (
                "0.45.4-linux-aarch64".into(),
                Some("2026-07-05T00:00:00Z".into()),
            ),
        ];
        // Windows sees only bare keys, newest by modified — never a
        // linux-suffixed key, even when that's newer overall.
        assert_eq!(
            latest_key_for_platform(&rows, InstallerOs::Windows, InstallerArch::X86_64),
            Some("0.45.4".into())
        );
        // Linux x86_64 sees only its own suffix: 0.45.3-linux-x86_64 is
        // newer than 0.45.4-linux-x86_64 (timestamps, not semver, order).
        assert_eq!(
            latest_key_for_platform(&rows, InstallerOs::Linux, InstallerArch::X86_64),
            Some("0.45.3-linux-x86_64".into())
        );
        assert_eq!(
            latest_key_for_platform(&rows, InstallerOs::Linux, InstallerArch::Aarch64),
            Some("0.45.4-linux-aarch64".into())
        );
        // arch is ignored for Windows (bare keys carry no arch).
        assert_eq!(
            latest_key_for_platform(&rows, InstallerOs::Windows, InstallerArch::Aarch64),
            Some("0.45.4".into())
        );
        // No key for the platform → None (the handler 404s).
        let bare_only: Vec<(String, Option<String>)> =
            vec![("0.44.0".into(), Some("2026-07-01T00:00:00Z".into()))];
        assert_eq!(
            latest_key_for_platform(&bare_only, InstallerOs::Linux, InstallerArch::X86_64),
            None
        );
        // Semver prerelease dashes are never misparsed: the prerelease
        // WINDOWS key must not look like a linux key, and the linux
        // prerelease key still matches its suffix.
        let rc: Vec<(String, Option<String>)> = vec![
            (
                "0.46.0-rc-linux".into(),
                Some("2026-07-01T00:00:00Z".into()),
            ),
            (
                "0.46.0-rc.1-linux-x86_64".into(),
                Some("2026-07-02T00:00:00Z".into()),
            ),
        ];
        assert_eq!(
            latest_key_for_platform(&rc, InstallerOs::Windows, InstallerArch::X86_64),
            Some("0.46.0-rc-linux".into())
        );
        assert_eq!(
            latest_key_for_platform(&rc, InstallerOs::Linux, InstallerArch::X86_64),
            Some("0.46.0-rc.1-linux-x86_64".into())
        );
    }

    #[test]
    fn install_sh_exports_the_token_only_when_given() {
        let with = render_install_sh(Some("s3cret"));
        assert!(with.starts_with("#!/bin/sh\n"));
        assert!(with.contains("set -eu\n"));
        assert!(with.contains("export KANADE_NATS_TOKEN='s3cret'\n"));
        assert!(with.ends_with("exec ./setup-agent.sh\n"));
        // No url export — the baked agent.toml carries it (setup-agent.sh
        // only overrides when KANADE_NATS_URL is set).
        assert!(!with.contains("KANADE_NATS_URL"));
        // LF only.
        assert!(!with.contains('\r'));

        let without = render_install_sh(None);
        assert!(!without.contains("KANADE_NATS_TOKEN"));
        assert!(without.ends_with("exec ./setup-agent.sh\n"));
    }

    #[test]
    fn install_sh_shell_escapes_single_quotes() {
        // POSIX single-quote escaping: `'` → `'\''`. An unescaped quote
        // would terminate the literal and let the rest of the token run
        // as shell.
        let out = render_install_sh(Some("it's"));
        assert!(out.contains("export KANADE_NATS_TOKEN='it'\\''s'\n"));
    }

    #[test]
    fn tar_gz_round_trips_all_entries_with_modes() {
        let agent_toml = render_agent_toml("nats://broker.corp:4222").unwrap();
        let install_sh = render_install_sh(Some("tok"));
        let entries = vec![
            TarEntry::new("bin/kanade-agent", 0o755, b"\x7fELF-fake".to_vec()),
            TarEntry::new("etc/agent.toml", 0o644, agent_toml.clone().into_bytes()),
            TarEntry::new(
                "systemd/kanade-agent.service",
                0o644,
                AGENT_SERVICE.as_bytes().to_vec(),
            ),
            TarEntry::new("setup-agent.sh", 0o755, SETUP_AGENT_SH.as_bytes().to_vec()),
            TarEntry::new("install.sh", 0o755, install_sh.clone().into_bytes()),
            TarEntry::new(
                "README.txt",
                0o644,
                render_readme_linux("0.45.4-linux-x86_64").into_bytes(),
            ),
        ];
        let bytes = build_tar_gz(entries).unwrap();

        let dec = flate2::read::GzDecoder::new(bytes.as_slice());
        let mut archive = tar::Archive::new(dec);
        let mut seen: std::collections::HashMap<String, (u32, Vec<u8>)> =
            std::collections::HashMap::new();
        for entry in archive.entries().unwrap() {
            let mut entry = entry.unwrap();
            let name = entry.path().unwrap().to_string_lossy().into_owned();
            let mode = entry.header().mode().unwrap();
            let mut data = Vec::new();
            use std::io::Read as _;
            entry.read_to_end(&mut data).unwrap();
            seen.insert(name, (mode, data));
        }
        for expected in [
            "bin/kanade-agent",
            "etc/agent.toml",
            "systemd/kanade-agent.service",
            "setup-agent.sh",
            "install.sh",
            "README.txt",
        ] {
            assert!(seen.contains_key(expected), "missing {expected}");
        }
        assert_eq!(seen.len(), 6);
        // Executables carry the exec bit; data files don't.
        for exe_name in ["bin/kanade-agent", "setup-agent.sh", "install.sh"] {
            assert_eq!(seen[exe_name].0, 0o755, "{exe_name} mode");
        }
        for data_name in [
            "etc/agent.toml",
            "systemd/kanade-agent.service",
            "README.txt",
        ] {
            assert_eq!(seen[data_name].0, 0o644, "{data_name} mode");
        }
        // The generated files round-trip byte-for-byte, and the canonical
        // scripts ship unmodified.
        assert_eq!(seen["etc/agent.toml"].1, agent_toml.as_bytes());
        assert_eq!(seen["install.sh"].1, install_sh.as_bytes());
        assert_eq!(seen["setup-agent.sh"].1, SETUP_AGENT_SH.as_bytes());
        assert_eq!(
            seen["systemd/kanade-agent.service"].1,
            AGENT_SERVICE.as_bytes()
        );
    }

    #[test]
    fn readme_linux_documents_the_flow_and_the_signing_gap() {
        let out = render_readme_linux("0.45.4-linux-x86_64");
        assert!(out.contains("release 0.45.4-linux-x86_64"));
        assert!(out.contains("tar xzf"));
        assert!(out.contains("sudo ./install.sh"));
        assert!(out.contains("Windows-only"));
        assert!(out.contains("INACTIVE on Linux agents"));
        // LF only.
        assert!(!out.contains('\r'));
    }
}

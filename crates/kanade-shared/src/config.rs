use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

// ─── Agent config ────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct AgentConfig {
    pub agent: AgentSection,
    pub log: LogSection,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AgentSection {
    pub id: String,
    pub nats_url: String,
    /// DEPRECATED in Sprint 5: group membership is now server-managed
    /// via the `agent_groups` KV bucket. Use
    /// `kanade agent groups set <pc_id> <group> [<group> ...]` to
    /// declare membership. Still parsed for back-compat; the value
    /// is logged-and-ignored at startup. Field removal is scheduled
    /// for v0.4.0.
    #[serde(default)]
    pub groups: Vec<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LogSection {
    pub path: String,
    pub level: String,
    /// Number of rotated daily files (incl. today's) to retain.
    /// Defaults to 14 — covers two weeks of incidents without
    /// blowing up disk. Set to 0 to disable on-disk logging
    /// (stdout only).
    #[serde(default = "default_keep_days")]
    pub keep_days: usize,
}

fn default_keep_days() -> usize {
    14
}

// ─── Backend config ──────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct BackendConfig {
    pub server: ServerSection,
    pub nats: NatsSection,
    pub db: DbSection,
    pub log: LogSection,
}

/// Non-secret SMTP connection settings. Lives here (rather than in `wire`)
/// because it's the shape `mail::Mailer::from_config` builds from, but it's
/// carried operator-editably in the `server_settings` KV bucket
/// (`wire::ServerSettings::mail` / SPA), **not** in `backend.toml` (#884).
/// `Serialize` is derived for the KV / API path; the SMTP password is never
/// a field here — it comes from the `MailPassword` registry secret (or
/// `$KANADE_MAIL_PASSWORD`), keeping secrets out of the KV.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct MailSection {
    /// SMTP relay host (e.g. an internal mail relay).
    pub host: String,
    /// SMTP port — 587 (STARTTLS), 465 (implicit TLS), or 25 (plain).
    pub port: u16,
    #[serde(default)]
    pub encryption: MailEncryption,
    /// Envelope/`From` address every kanade email is sent as.
    pub from: String,
    /// SMTP AUTH username. Omit for an unauthenticated internal relay;
    /// when set, pair it with the `MailPassword` secret.
    #[serde(default)]
    pub username: Option<String>,
}

/// Transport security for the SMTP connection.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum MailEncryption {
    /// Upgrade a plaintext connection via STARTTLS (port 587). Default.
    #[default]
    Starttls,
    /// Implicit TLS from the first byte (port 465).
    Tls,
    /// No transport security (port 25 on a trusted internal segment).
    None,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ServerSection {
    pub bind: String,
    /// Externally-reachable base URL of the SPA (e.g.
    /// `https://kanade.example.com`), used to build absolute links in
    /// emails (password setup / reset). Optional: when unset the backend
    /// derives the base from each request's `Host` header (+
    /// `X-Forwarded-Proto`), which is correct for a direct LAN deploy.
    /// Set this when behind a reverse proxy / TLS terminator, or to harden
    /// the public forgot-password path against `Host`-header poisoning
    /// (`bind` can't be used — it's a wildcard like `0.0.0.0:8080` and
    /// carries no scheme/hostname).
    #[serde(default)]
    pub public_url: Option<String>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct NatsSection {
    pub url: String,
    /// #1270: base URL of the broker's HTTP monitoring endpoint (the
    /// `http_port` in `nats-server.conf`, 8222 by default). The backend
    /// polls `/connz` there to learn which NATS user each agent's live
    /// connection authenticated as.
    ///
    /// Optional: when unset it is derived from [`Self::url`] by swapping
    /// the scheme for `http` and the port for 8222, which is right for the
    /// standard single-broker deploy. Set it when monitoring listens
    /// elsewhere. Unreachable / disabled monitoring is not fatal — the
    /// projection simply stays unpopulated.
    #[serde(default)]
    pub monitor_url: Option<String>,
}

/// Port `nats-server` serves its monitoring endpoints on by default, and
/// the `http_port` shipped in `configs/nats-server.conf`.
const DEFAULT_MONITOR_PORT: u16 = 8222;

impl NatsSection {
    /// The monitoring base URL to poll — configured, or derived from
    /// [`Self::url`].
    ///
    /// Derivation deliberately keeps only the **host**: the client URL's
    /// port is the client port (4222), its scheme may be `nats`/`tls`/`ws`
    /// (none of which the monitoring endpoint speaks), and it may carry
    /// inline credentials that must not be re-sent over plain HTTP.
    pub fn resolved_monitor_url(&self) -> String {
        if let Some(u) = self.monitor_url.as_deref().map(str::trim)
            && !u.is_empty()
        {
            let u = u.trim_end_matches('/');
            // A scheme-less value (`10.0.0.9:8222`) would otherwise be
            // parsed as a URI whose *scheme* is the hostname, failing every
            // poll behind a "monitoring endpoint unreadable" line that names
            // the symptom rather than the typo.
            return if u.contains("://") {
                u.to_string()
            } else {
                format!("http://{u}")
            };
        }
        let host = monitor_host_from_client_url(&self.url);
        format!("http://{host}:{DEFAULT_MONITOR_PORT}")
    }
}

/// Extract the host from a NATS client URL, dropping scheme, credentials,
/// port and path. Returns the input unchanged when it is already a bare
/// host, and falls back to loopback for input with no host at all — a
/// wrong-but-harmless target beats a panic in a background poller.
fn monitor_host_from_client_url(url: &str) -> String {
    let after_scheme = url.split_once("://").map_or(url, |(_scheme, rest)| rest);
    // `user:pass@host:port` — credentials are before the LAST '@' so a
    // password containing '@' does not truncate the host.
    let authority = match after_scheme.rsplit_once('@') {
        Some((_creds, host)) => host,
        None => after_scheme,
    };
    // Strip any path / query the URL carried.
    let authority = authority
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(authority)
        .trim();
    // IPv6 literals are bracketed (`[::1]:4222`) and their colons are not
    // port separators — keep the brackets, which is also the form an HTTP
    // URL needs.
    let host = if let Some(end) = authority.find(']') {
        &authority[..=end]
    } else {
        authority.split(':').next().unwrap_or(authority)
    };
    if host.is_empty() {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    }
}

#[derive(Deserialize, Debug, Clone)]
pub struct DbSection {
    pub sqlite_path: String,
}

// ─── Loader ──────────────────────────────────────────────────────────

fn load_typed<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let mut engine = teravars::Engine::new();
    let ctx = teravars::system_context();
    let paths: Vec<PathBuf> = vec![path.to_path_buf()];
    let merged = teravars::load_merged(&paths, &mut engine, &ctx)
        .with_context(|| format!("teravars load_merged: {path:?}"))?;
    let cfg: T = toml::Value::Table(merged.config)
        .try_into()
        .with_context(|| format!("decode config from {path:?}"))?;
    Ok(cfg)
}

pub fn load_agent_config(path: &Path) -> Result<AgentConfig> {
    load_typed(path)
}

pub fn load_backend_config(path: &Path) -> Result<BackendConfig> {
    load_typed(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nats(url: &str, monitor: Option<&str>) -> NatsSection {
        NatsSection {
            url: url.to_string(),
            monitor_url: monitor.map(str::to_string),
        }
    }

    /// #1270: the derived monitoring URL must keep the host and drop
    /// everything else. The client port is not the monitoring port, and an
    /// inline credential must not be replayed over plain HTTP.
    #[test]
    fn the_monitor_url_derives_from_the_client_url_host_only() {
        for (client, want) in [
            ("nats://127.0.0.1:4222", "http://127.0.0.1:8222"),
            (
                "nats://nats.example.com:4222",
                "http://nats.example.com:8222",
            ),
            // No scheme, no port — a bare host is still a host.
            ("broker-01", "http://broker-01:8222"),
            // Inline credentials: the host is after the last '@'.
            ("nats://user:p@ss@10.0.0.5:4222", "http://10.0.0.5:8222"),
            // wss deploys terminate elsewhere, but the host still answers.
            (
                "wss://kanade.example.com:443/nats",
                "http://kanade.example.com:8222",
            ),
            // IPv6 literals keep their brackets in both URL forms.
            ("nats://[::1]:4222", "http://[::1]:8222"),
        ] {
            assert_eq!(
                nats(client, None).resolved_monitor_url(),
                want,
                "deriving from {client}",
            );
        }
    }

    #[test]
    fn an_explicit_monitor_url_wins_and_is_taken_verbatim() {
        assert_eq!(
            nats("nats://127.0.0.1:4222", Some("http://10.0.0.9:9999")).resolved_monitor_url(),
            "http://10.0.0.9:9999",
        );
        // A trailing slash would produce `//connz` when joined.
        assert_eq!(
            nats("nats://127.0.0.1:4222", Some("http://10.0.0.9:9999/")).resolved_monitor_url(),
            "http://10.0.0.9:9999",
        );
        // Blank / whitespace-only is not a configured value — fall back to
        // derivation rather than polling the empty string.
        assert_eq!(
            nats("nats://127.0.0.1:4222", Some("   ")).resolved_monitor_url(),
            "http://127.0.0.1:8222",
        );
        // A scheme-less value is the likely typo, and it parses as a URI
        // whose scheme is the hostname — which fails on every poll behind an
        // error that names the symptom, not the cause.
        assert_eq!(
            nats("nats://127.0.0.1:4222", Some("10.0.0.9:8222")).resolved_monitor_url(),
            "http://10.0.0.9:8222",
        );
        // ...but an explicit scheme is never rewritten, including https.
        assert_eq!(
            nats("nats://127.0.0.1:4222", Some("https://mon.example.com")).resolved_monitor_url(),
            "https://mon.example.com",
        );
    }

    /// Smoke test the dev-fleet flow against `agent.dev.toml`:
    ///   1. When `KANADE_DEV_AGENT_ID` is set, the teravars template
    ///      resolves `vars.pc_id` to that value and propagates it
    ///      into `agent.id` + `log.path`. Also exercises a `[vars]`
    ///      self-reference (`pc_id` falls back to `vars.hostname`),
    ///      which `load_merged` resolves via its internal
    ///      fixed-point pass.
    ///   2. Without the env, the template falls back to `system.host`
    ///      so vanilla `cargo make agent-dev` still works.
    ///
    /// Both halves live in a single `#[test]` so they execute
    /// sequentially within the cargo test runtime — splitting them
    /// across two tests races on `KANADE_DEV_AGENT_ID` (macOS CI
    /// turned the race up enough to fail consistently).
    #[test]
    fn agent_dev_toml_renders_pc_id_from_env_or_system_host() {
        // The dev config lives at the workspace root; CARGO_MANIFEST_DIR
        // resolves to crates/kanade-shared/, so hop up two.
        let cfg_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("configs")
            .join("agent.dev.toml");

        // (1) env set → pc_id == env value
        // SAFETY: env mutation is process-global; this single test
        // body owns set + remove so no sibling test can race us.
        unsafe {
            std::env::set_var("KANADE_DEV_AGENT_ID", "dev-pc-render-test");
        }
        let cfg = load_agent_config(&cfg_path).expect("load agent.dev.toml (env set)");
        assert_eq!(cfg.agent.id, "dev-pc-render-test");
        assert!(
            cfg.log.path.contains("dev-pc-render-test"),
            "log path should embed pc_id, got {}",
            cfg.log.path,
        );

        // (2) env removed → pc_id falls back to vars.hostname
        // = system.host. The host string varies by box; just assert
        // it's non-empty and not the literal template that would mean
        // teravars failed to render.
        unsafe {
            std::env::remove_var("KANADE_DEV_AGENT_ID");
        }
        let cfg = load_agent_config(&cfg_path).expect("load agent.dev.toml (env unset)");
        assert!(
            !cfg.agent.id.is_empty(),
            "pc_id should fall back to system.host"
        );
        assert_ne!(
            cfg.agent.id, "{{ system.host }}",
            "template should render, not leak"
        );
    }
}

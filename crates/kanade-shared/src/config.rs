use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

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
    /// Outbound SMTP relay for compliance-alert + generic email.
    /// Absent ⇒ email features are no-ops (the in-app/NATS notification
    /// path is unaffected), so an existing deploy keeps working with no
    /// config change. Holds only the *non-secret* connection settings —
    /// the SMTP password is NOT here, it comes from the `MailPassword`
    /// registry secret (or `$KANADE_MAIL_PASSWORD`), the same way
    /// `JwtSecret` / `StaticToken` are resolved (kanade keeps secrets out
    /// of the un-ACL'd `backend.toml`).
    #[serde(default)]
    pub mail: Option<MailSection>,
}

#[derive(Deserialize, Debug, Clone)]
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
#[derive(Deserialize, Debug, Clone, Copy, Default, PartialEq, Eq)]
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

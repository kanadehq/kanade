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
}

#[derive(Deserialize, Debug, Clone)]
pub struct ServerSection {
    pub bind: String,
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

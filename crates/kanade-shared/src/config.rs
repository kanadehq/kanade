use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

// ─── Agent config ────────────────────────────────────────────────────

#[derive(Deserialize, Debug, Clone)]
pub struct AgentConfig {
    pub agent: AgentSection,
    pub log: LogSection,
    #[serde(default)]
    pub inventory: InventorySection,
}

#[derive(Deserialize, Debug, Clone)]
pub struct AgentSection {
    pub id: String,
    pub nats_url: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LogSection {
    pub path: String,
    pub level: String,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InventorySection {
    #[serde(default = "default_hw_interval")]
    pub hw_interval: String,
    #[serde(default = "default_jitter")]
    pub jitter: String,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

impl Default for InventorySection {
    fn default() -> Self {
        Self {
            hw_interval: default_hw_interval(),
            jitter: default_jitter(),
            enabled: default_enabled(),
        }
    }
}

fn default_hw_interval() -> String {
    "24h".into()
}
fn default_jitter() -> String {
    "10m".into()
}
fn default_enabled() -> bool {
    true
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

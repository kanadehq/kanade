use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
pub struct AgentConfig {
    pub agent: AgentSection,
    pub log: LogSection,
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

pub fn load_agent_config(path: &Path) -> Result<AgentConfig> {
    let mut engine = teravars::Engine::new();
    let ctx = teravars::system_context();
    let paths: Vec<PathBuf> = vec![path.to_path_buf()];
    let merged = teravars::load_merged(&paths, &mut engine, &ctx)
        .with_context(|| format!("teravars load_merged: {path:?}"))?;
    let cfg: AgentConfig = toml::Value::Table(merged.config)
        .try_into()
        .with_context(|| format!("decode AgentConfig from {path:?}"))?;
    Ok(cfg)
}

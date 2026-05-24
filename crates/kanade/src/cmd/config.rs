//! `kanade config …` — operate the layered agent_config KV bucket
//! (Sprint 6).
//!
//! Goes straight at JetStream KV (same pattern as `agent publish` /
//! `agent groups`) so the operator workstation doesn't need a
//! reachable backend.

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand};
use futures::StreamExt;
use kanade_shared::kv::{
    BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, KEY_AGENT_CONFIG_GLOBAL, agent_config_group_key,
    agent_config_pc_key, parse_agent_config_group_key,
};
use kanade_shared::wire::{AgentGroups, ConfigScope, resolve};
use tracing::info;

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
    /// `target_version_jitter=30m`, `target_version=0.3.0`).
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
    /// Delete the whole scope row from the bucket.
    Clear {
        #[command(flatten)]
        scope: ScopeSel,
    },
    /// Print the resolved EffectiveConfig for one pc_id — the same
    /// view the agent's config_supervisor computes locally.
    Effective { pc_id: String },
}

pub async fn execute(client: async_nats::Client, args: ConfigArgs) -> Result<()> {
    let js = async_nats::jetstream::new(client);
    let kv = js
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .with_context(|| {
            format!("KV '{BUCKET_AGENT_CONFIG}' missing — run `kanade jetstream setup`")
        })?;
    match args.sub {
        ConfigSub::Get { scope } => get(&kv, &scope).await,
        ConfigSub::Set { spec, scope } => set(&kv, &scope, &spec).await,
        ConfigSub::Unset { field, scope } => unset(&kv, &scope, &field).await,
        ConfigSub::Clear { scope } => clear(&kv, &scope).await,
        ConfigSub::Effective { pc_id } => effective(&js, pc_id).await,
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

async fn get(kv: &async_nats::jetstream::kv::Store, sel: &ScopeSel) -> Result<()> {
    let key = scope_key(sel)?;
    let scope = read_scope(kv, &key).await?;
    println!("# {} = {}", scope_label(sel), key);
    println!("{}", serde_json::to_string_pretty(&scope)?);
    Ok(())
}

async fn set(kv: &async_nats::jetstream::kv::Store, sel: &ScopeSel, spec: &str) -> Result<()> {
    let (field, value) = spec
        .split_once('=')
        .ok_or_else(|| anyhow!("expected <field>=<value>, got '{spec}'"))?;
    let key = scope_key(sel)?;
    let mut scope = read_scope(kv, &key).await?;
    apply_field(&mut scope, field, Some(value))?;
    write_scope(kv, &key, &scope).await?;
    println!("set {field} = {value} on {}", scope_label(sel));
    Ok(())
}

async fn unset(kv: &async_nats::jetstream::kv::Store, sel: &ScopeSel, field: &str) -> Result<()> {
    let key = scope_key(sel)?;
    let mut scope = read_scope(kv, &key).await?;
    apply_field(&mut scope, field, None)?;
    write_scope(kv, &key, &scope).await?;
    println!("unset {field} on {}", scope_label(sel));
    Ok(())
}

async fn clear(kv: &async_nats::jetstream::kv::Store, sel: &ScopeSel) -> Result<()> {
    let key = scope_key(sel)?;
    kv.delete(&key).await.context("kv delete")?;
    println!("cleared {} ({})", scope_label(sel), key);
    Ok(())
}

async fn effective(js: &async_nats::jetstream::Context, pc_id: String) -> Result<()> {
    let cfg_kv = js
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .with_context(|| format!("KV '{BUCKET_AGENT_CONFIG}' missing"))?;
    let groups_kv = js
        .get_key_value(BUCKET_AGENT_GROUPS)
        .await
        .with_context(|| format!("KV '{BUCKET_AGENT_GROUPS}' missing"))?;

    // Snapshot every scope row the resolver will need.
    let global_scope = read_scope_optional(&cfg_kv, KEY_AGENT_CONFIG_GLOBAL).await?;
    let pc_scope = read_scope_optional(&cfg_kv, &agent_config_pc_key(&pc_id)).await?;

    let mut group_scopes: BTreeMap<String, ConfigScope> = BTreeMap::new();
    let mut keys = cfg_kv.keys().await.context("kv keys")?;
    while let Some(k) = keys.next().await {
        let k = k.context("kv key entry")?;
        if let Some(group) = parse_agent_config_group_key(&k)
            && let Some(scope) = read_scope_optional(&cfg_kv, &k).await?
        {
            group_scopes.insert(group.to_string(), scope);
        }
    }

    let my_groups = match groups_kv.get(&pc_id).await? {
        Some(bytes) => serde_json::from_slice::<AgentGroups>(&bytes)
            .map(|g| g.groups)
            .unwrap_or_default(),
        None => Vec::new(),
    };

    let (eff, warns) = resolve(
        global_scope.as_ref(),
        &group_scopes,
        pc_scope.as_ref(),
        &my_groups,
    );

    println!("# pc_id      = {pc_id}");
    println!("# my_groups  = {my_groups:?}");
    println!("{}", serde_json::to_string_pretty(&eff)?);
    for w in &warns {
        println!("# warning: {w:?}");
    }
    Ok(())
}

async fn read_scope(kv: &async_nats::jetstream::kv::Store, key: &str) -> Result<ConfigScope> {
    Ok(read_scope_optional(kv, key).await?.unwrap_or_default())
}

async fn read_scope_optional(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
) -> Result<Option<ConfigScope>> {
    match kv.get(key).await.context("kv get")? {
        Some(bytes) => Ok(Some(
            serde_json::from_slice(&bytes).context("decode ConfigScope")?,
        )),
        None => Ok(None),
    }
}

async fn write_scope(
    kv: &async_nats::jetstream::kv::Store,
    key: &str,
    scope: &ConfigScope,
) -> Result<()> {
    let bytes = serde_json::to_vec(scope).context("encode ConfigScope")?;
    kv.put(key, bytes.into()).await.context("kv put")?;
    info!(key, scope = ?scope, "agent_config row updated");
    Ok(())
}

/// Apply `value` (or `None` for unset) to the named field on
/// `scope`. Lives here rather than as a generic helper because the
/// field names + types are stable enough that an open-coded match
/// is the most readable form.
fn apply_field(scope: &mut ConfigScope, field: &str, value: Option<&str>) -> Result<()> {
    match field {
        "target_version" => scope.target_version = value.map(String::from),
        "target_version_jitter" => scope.target_version_jitter = value.map(String::from),
        "heartbeat_interval" => scope.heartbeat_interval = value.map(String::from),
        "host_perf_interval" => scope.host_perf_interval = value.map(String::from),
        other => bail!(
            "unknown field '{other}' — supported: target_version, target_version_jitter, heartbeat_interval, host_perf_interval"
        ),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_field_sets_string() {
        let mut s = ConfigScope::default();
        apply_field(&mut s, "heartbeat_interval", Some("15s")).unwrap();
        assert_eq!(s.heartbeat_interval.as_deref(), Some("15s"));
    }

    #[test]
    fn apply_field_unset_clears_string() {
        let mut s = ConfigScope {
            heartbeat_interval: Some("15s".into()),
            ..Default::default()
        };
        apply_field(&mut s, "heartbeat_interval", None).unwrap();
        assert!(s.heartbeat_interval.is_none());
    }

    #[test]
    fn apply_field_rejects_unknown() {
        let mut s = ConfigScope::default();
        let err = apply_field(&mut s, "nope", Some("x")).unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }

    #[test]
    fn scope_key_routing() {
        assert_eq!(
            scope_key(&ScopeSel {
                group: None,
                pc: None
            })
            .unwrap(),
            "global",
        );
        assert_eq!(
            scope_key(&ScopeSel {
                group: Some("canary".into()),
                pc: None
            })
            .unwrap(),
            "groups.canary",
        );
        assert_eq!(
            scope_key(&ScopeSel {
                group: None,
                pc: Some("MINIPC".into())
            })
            .unwrap(),
            "pcs.MINIPC",
        );
    }
}

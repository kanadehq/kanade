//! `kanade group …` — fleet-wide group operations on top of the
//! `agent_groups` KV bucket + per-group `agent_config.groups.<name>`
//! overrides.
//!
//! Replaces v0.9-era `kanade agent groups …`. Two layers of state
//! end up rendered as a single "group" abstraction in the CLI:
//!
//!   * `agent_groups.<pc_id>` — `AgentGroups { groups: [...] }`
//!     (membership: which groups a PC is in)
//!   * `agent_config.groups.<name>` — `ConfigScope` (overrides
//!     applied to every PC in the group)
//!
//! A group "exists" in the fleet if either bucket mentions it. The
//! `list` subcommand unions both so an operator can spot
//! mis-targeted config (`agent_config.groups.canray` with no members
//! because of a typo) at a glance.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use futures::StreamExt;
use kanade_shared::kv::{BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, parse_agent_config_group_key};
use kanade_shared::wire::AgentGroups;
use tracing::info;

#[derive(Args, Debug)]
pub struct GroupArgs {
    #[command(subcommand)]
    pub sub: GroupSub,
}

#[derive(Subcommand, Debug)]
pub enum GroupSub {
    /// List every group known to the fleet (union of agent_groups
    /// memberships and agent_config.groups.* overrides). With
    /// `--pc <pc_id>`, list the groups that one PC belongs to
    /// instead.
    List {
        /// Restrict to the membership of a single PC.
        #[arg(long, value_name = "PC_ID")]
        pc: Option<String>,
    },
    /// List the PCs that have <name> in their membership.
    Members {
        /// Group name.
        name: String,
    },
    /// Add <name> to a PC's membership (idempotent).
    Add { pc_id: String, name: String },
    /// Remove <name> from a PC's membership (idempotent).
    Rm { pc_id: String, name: String },
    /// Replace a PC's entire membership list (sorted + deduped on
    /// the server side). Pass zero names to clear.
    Set {
        pc_id: String,
        #[arg(trailing_var_arg = true)]
        names: Vec<String>,
    },
}

pub async fn execute(client: async_nats::Client, args: GroupArgs) -> Result<()> {
    let js = async_nats::jetstream::new(client);
    let groups_kv = js
        .get_key_value(BUCKET_AGENT_GROUPS)
        .await
        .with_context(|| {
            format!("KV '{BUCKET_AGENT_GROUPS}' missing — run `kanade jetstream setup`")
        })?;

    match args.sub {
        GroupSub::List { pc: Some(pc_id) } => list_pc(&groups_kv, &pc_id).await,
        GroupSub::List { pc: None } => list_all(&js, &groups_kv).await,
        GroupSub::Members { name } => members(&groups_kv, &name).await,
        GroupSub::Add { pc_id, name } => add(&groups_kv, &pc_id, &name).await,
        GroupSub::Rm { pc_id, name } => rm(&groups_kv, &pc_id, &name).await,
        GroupSub::Set { pc_id, names } => set(&groups_kv, &pc_id, names).await,
    }
}

async fn list_pc(kv: &async_nats::jetstream::kv::Store, pc_id: &str) -> Result<()> {
    let g = read_groups(kv, pc_id).await?;
    if g.is_empty() {
        println!("{pc_id}: (no groups)");
    } else {
        println!("{pc_id}: {}", g.groups.join(", "));
    }
    Ok(())
}

async fn list_all(
    js: &async_nats::jetstream::Context,
    groups_kv: &async_nats::jetstream::kv::Store,
) -> Result<()> {
    // Pass 1 — membership: walk agent_groups, collect group -> [pc_ids].
    let mut by_group: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut keys = match groups_kv.keys().await {
        Ok(k) => k,
        Err(_) => {
            // Empty bucket on a fresh broker shows up here on some
            // async-nats versions — degrade gracefully.
            println!("(no groups yet)");
            return Ok(());
        }
    };
    while let Some(k) = keys.next().await {
        let pc_id = k.context("kv key entry")?;
        let g = read_groups(groups_kv, &pc_id).await?;
        for name in g.groups {
            by_group.entry(name).or_default().push(pc_id.clone());
        }
    }

    // Pass 2 — config overrides: scan agent_config for `groups.<name>` keys.
    let cfg_kv = js
        .get_key_value(BUCKET_AGENT_CONFIG)
        .await
        .with_context(|| format!("KV '{BUCKET_AGENT_CONFIG}' missing"))?;
    let mut with_config: BTreeSet<String> = BTreeSet::new();
    if let Ok(mut cfg_keys) = cfg_kv.keys().await {
        while let Some(k) = cfg_keys.next().await {
            let k = k.context("kv key entry")?;
            if let Some(name) = parse_agent_config_group_key(&k) {
                with_config.insert(name.to_string());
            }
        }
    }

    // Union: every name that appears in either side gets a row.
    let mut all_names: BTreeSet<String> = by_group.keys().cloned().collect();
    all_names.extend(with_config.iter().cloned());

    if all_names.is_empty() {
        println!("(no groups yet)");
        return Ok(());
    }

    println!(
        "{group:<24} {members:>7}  config",
        group = "group",
        members = "members"
    );
    println!("{}", "-".repeat(48));
    for name in &all_names {
        let members = by_group.get(name).map(|v| v.len()).unwrap_or(0);
        let cfg = if with_config.contains(name) {
            "yes"
        } else {
            "—"
        };
        println!("{name:<24} {members:>7}  {cfg}");
    }
    Ok(())
}

async fn members(kv: &async_nats::jetstream::kv::Store, name: &str) -> Result<()> {
    let mut hits = Vec::new();
    let mut keys = match kv.keys().await {
        Ok(k) => k,
        Err(_) => {
            println!("(no groups yet)");
            return Ok(());
        }
    };
    while let Some(k) = keys.next().await {
        let pc_id = k.context("kv key entry")?;
        let g = read_groups(kv, &pc_id).await?;
        if g.groups.iter().any(|x| x == name) {
            hits.push(pc_id);
        }
    }
    if hits.is_empty() {
        println!("(no PCs in '{name}')");
    } else {
        hits.sort();
        for pc in hits {
            println!("{pc}");
        }
    }
    Ok(())
}

async fn add(kv: &async_nats::jetstream::kv::Store, pc_id: &str, name: &str) -> Result<()> {
    // #505: CAS read-modify-write — a blind get→put raced a
    // concurrent add/remove for the same PC (two operators, or a
    // script fanning out) and silently dropped one side's change.
    let mut changed = false;
    let g = kanade_shared::kv_cas::read_modify_write(kv, pc_id, |g: &mut AgentGroups| {
        changed = g.insert(name);
        changed
    })
    .await?;
    if changed {
        info!(pc_id, groups = ?g.groups, "agent_groups updated");
        println!("{pc_id}: added '{name}' -> [{}]", g.groups.join(", "));
    } else {
        println!("{pc_id}: already has '{name}' (no change)");
    }
    Ok(())
}

async fn rm(kv: &async_nats::jetstream::kv::Store, pc_id: &str, name: &str) -> Result<()> {
    let mut changed = false;
    let g = kanade_shared::kv_cas::read_modify_write(kv, pc_id, |g: &mut AgentGroups| {
        changed = g.remove(name);
        changed
    })
    .await?;
    if changed {
        info!(pc_id, groups = ?g.groups, "agent_groups updated");
        let after = if g.is_empty() {
            "(no groups)".to_string()
        } else {
            g.groups.join(", ")
        };
        println!("{pc_id}: removed '{name}' -> [{after}]");
    } else {
        println!("{pc_id}: not a member of '{name}' (no change)");
    }
    Ok(())
}

async fn set(kv: &async_nats::jetstream::kv::Store, pc_id: &str, names: Vec<String>) -> Result<()> {
    let normalised = AgentGroups::new(names);
    write_groups(kv, pc_id, &normalised).await?;
    if normalised.is_empty() {
        println!("{pc_id}: cleared all groups");
    } else {
        println!(
            "{pc_id}: set membership to [{}]",
            normalised.groups.join(", ")
        );
    }
    Ok(())
}

async fn read_groups(kv: &async_nats::jetstream::kv::Store, pc_id: &str) -> Result<AgentGroups> {
    match kv.get(pc_id).await.context("kv get")? {
        Some(bytes) => serde_json::from_slice(&bytes).context("decode agent_groups"),
        None => Ok(AgentGroups::default()),
    }
}

async fn write_groups(
    kv: &async_nats::jetstream::kv::Store,
    pc_id: &str,
    groups: &AgentGroups,
) -> Result<()> {
    let bytes = serde_json::to_vec(groups).context("encode agent_groups")?;
    kv.put(pc_id, bytes.into()).await.context("kv put")?;
    info!(pc_id, groups = ?groups.groups, "agent_groups updated");
    Ok(())
}

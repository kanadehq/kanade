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
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use futures::StreamExt;
use kanade_shared::kv::{BUCKET_AGENT_CONFIG, BUCKET_AGENT_GROUPS, parse_agent_config_group_key};
use kanade_shared::manifest::{GroupDef, is_valid_resource_id};
use kanade_shared::wire::AgentGroups;
use tracing::{info, warn};

use crate::cmd::provenance::{append_origin_yaml, detect_repo_origin, has_top_level_origin};

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
    /// Manage declarative **group definitions** (#1032) — the `groups/`
    /// manifest kind. Unlike the imperative membership ops above (which
    /// write `agent_groups` KV directly over NATS), these are HTTP manifest
    /// CRUD like `kanade view` / `kanade schedule`: a group is defined by a
    /// static `members:` list or a dynamic `query:` (read-only SQL returning
    /// a `pc_id` column), and a schedule's `target.groups` resolves it.
    #[command(subcommand)]
    Def(GroupDefSub),
}

#[derive(Subcommand, Debug)]
pub enum GroupDefSub {
    /// Upsert one or more group definitions from YAML files.
    ///
    /// Accepts multiple files, a directory (its top-level `*.yaml` / `*.yml`),
    /// and/or glob patterns — e.g. `kanade group def create
    /// configs/groups/*.yaml`. Each file is registered independently
    /// (fail-soft per file); exits non-zero if any fails.
    Create {
        /// Group YAML paths (`id` + `members:` xor `query:`).
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Validate one or more group manifests WITHOUT submitting them.
    ///
    /// Runs the exact client-side checks `create` does — strict YAML parse
    /// (#492) + `GroupDef::validate()` (id charset, members/query exclusivity,
    /// refresh parse). One caveat, shared with `create`: a dynamic `query:` is
    /// only checked read-only at the backend sandbox, so that specific check
    /// isn't run here. Built for CI / pre-commit. Accepts files, directories,
    /// and globs; exits non-zero if any file fails.
    Validate {
        #[arg(required = true, num_args = 1..)]
        paths: Vec<PathBuf>,
    },
    /// Export registered group YAML (the comment-preserving mirror). Same
    /// shape as `kanade view export`: `<id>` to stdout, `--out-dir` to write
    /// files, `--all` to dump every group.
    Export {
        #[arg(required_unless_present = "all")]
        id: Option<String>,
        #[arg(long, conflicts_with = "id")]
        all: bool,
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// List all group definitions currently registered.
    List,
    /// Resolve a group and print the `pc_id`s it currently covers (a dynamic
    /// group runs its query server-side; a static group prints its members).
    /// Handy to preview scope before wiring a group into a schedule `target`.
    Members { id: String },
    /// Delete a group definition by its id.
    Delete { id: String },
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
        // `group def …` is HTTP manifest CRUD, routed to `execute_def` on the
        // no-NATS dispatch path before we ever connect here.
        GroupSub::Def(_) => unreachable!("group def is handled on the HTTP dispatch path"),
    }
}

/// `kanade group def …` — HTTP manifest CRUD for [`GroupDef`] resources
/// (#1032). Same REST shape as `kanade view` (create / validate / list /
/// export / delete), plus a `members` preview that resolves the group
/// server-side. Routed here (not through [`execute`]) because it talks HTTP to
/// the backend, not NATS KV.
pub async fn execute_def(backend_url: &str, sub: GroupDefSub) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match sub {
        GroupDefSub::Create { paths } => def_create_all(base, paths).await,
        // Offline check — no backend round-trip.
        GroupDefSub::Validate { paths } => def_validate_all(paths),
        GroupDefSub::Export { id, all, out_dir } => {
            crate::cmd::bulk::export(base, "group-defs", id, all, out_dir).await
        }
        GroupDefSub::List => def_list(base).await,
        GroupDefSub::Members { id } => def_members(base, &id).await,
        GroupDefSub::Delete { id } => def_delete(base, &id).await,
    }
}

async fn def_create_all(base: &str, paths: Vec<PathBuf>) -> Result<()> {
    let files = crate::cmd::bulk::expand_manifest_paths(&paths)?;
    let mut failures = 0usize;
    for f in &files {
        if let Err(e) = def_create_one(base, f).await {
            eprintln!("✗ {}: {e:#}", f.display());
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures}/{} group manifest(s) failed", files.len());
    }
    Ok(())
}

fn def_validate_all(paths: Vec<PathBuf>) -> Result<()> {
    let files = crate::cmd::bulk::expand_manifest_paths(&paths)?;
    let mut failures = 0usize;
    for f in &files {
        if let Err(e) = def_validate_one(f) {
            eprintln!("✗ {}: {e:#}", f.display());
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!(
            "{failures}/{} group manifest(s) failed validation",
            files.len()
        );
    }
    Ok(())
}

fn def_validate_one(yaml: &std::path::Path) -> Result<()> {
    let raw = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    let docs = crate::cmd::bulk::split_yaml_documents(&raw);
    match docs.as_slice() {
        [] => anyhow::bail!("{yaml:?}: no YAML documents found"),
        [only] => return def_validate_one_doc(yaml, only),
        _ => {}
    }
    let mut failures = 0usize;
    for (i, doc) in docs.iter().enumerate() {
        if let Err(e) = def_validate_one_doc(yaml, doc) {
            eprintln!("✗ {} [doc {}]: {e:#}", yaml.display(), i + 1);
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!(
            "{failures}/{} document(s) in {yaml:?} failed validation",
            docs.len()
        );
    }
    Ok(())
}

fn def_validate_one_doc(yaml: &std::path::Path, raw: &str) -> Result<()> {
    let group: GroupDef = kanade_shared::strict::from_yaml_str(raw)
        .map_err(|e| anyhow::anyhow!("parse {yaml:?}: {e}"))?;
    group
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid group {yaml:?}: {e}"))?;
    let kind = if group.dynamic_query().is_some() {
        "dynamic"
    } else {
        "static"
    };
    println!(
        "✓ {} → group '{}' ({kind}) (valid)",
        yaml.display(),
        group.id,
    );
    Ok(())
}

async fn def_create_one(base: &str, yaml: &std::path::Path) -> Result<()> {
    let raw = std::fs::read_to_string(yaml).with_context(|| format!("read {yaml:?}"))?;
    let docs = crate::cmd::bulk::split_yaml_documents(&raw);
    match docs.as_slice() {
        [] => anyhow::bail!("{yaml:?}: no YAML documents found"),
        [only] => return def_create_one_doc(base, yaml, only).await,
        _ => {}
    }
    let mut failures = 0usize;
    for (i, doc) in docs.iter().enumerate() {
        if let Err(e) = def_create_one_doc(base, yaml, doc).await {
            eprintln!("✗ {} [doc {}]: {e:#}", yaml.display(), i + 1);
            failures += 1;
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures}/{} document(s) in {yaml:?} failed", docs.len());
    }
    Ok(())
}

async fn def_create_one_doc(base: &str, yaml: &std::path::Path, raw: &str) -> Result<()> {
    let mut body = raw.to_string();
    // Parse + validate client-side first so a malformed group errors at the
    // operator's shell rather than as the backend's 400; then ship the raw
    // YAML so the backend's YAML mirror keeps comments. #492: strict parse.
    let group: GroupDef = kanade_shared::strict::from_yaml_str(&body)
        .map_err(|e| anyhow::anyhow!("parse {yaml:?}: {e}"))?;
    group
        .validate()
        .map_err(|e| anyhow::anyhow!("invalid group {yaml:?}: {e}"))?;

    // #678 GitOps provenance — parity with view/job/schedule create. A group
    // carries no script, so the script_file arg is always `None`.
    if let Some(origin) = detect_repo_origin(yaml, None) {
        if has_top_level_origin(&body) {
            warn!(
                group_id = %group.id,
                "origin: already present in source YAML; preserving it",
            );
        } else {
            append_origin_yaml(&mut body, &origin).context("append origin provenance")?;
        }
    }

    let url = format!("{base}/api/group-defs");
    let resp = crate::http_client::authed_client()?
        .post(&url)
        .header(reqwest::header::CONTENT_TYPE, "application/yaml")
        .body(body)
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("create rejected: {status} — {body}");
    }
    let payload: serde_json::Value = resp
        .json()
        .await
        .context("parse JSON response from server")?;
    let id = payload.get("id").and_then(|v| v.as_str()).unwrap_or("?");
    let kind = payload.get("kind").and_then(|v| v.as_str()).unwrap_or("?");
    println!("✓ {} → group '{id}' ({kind})", yaml.display());
    Ok(())
}

async fn def_list(base: &str) -> Result<()> {
    let url = format!("{base}/api/group-defs");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("list failed: {status} — {body}");
    }
    let payload: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

async fn def_members(base: &str, id: &str) -> Result<()> {
    if !is_valid_resource_id(id) {
        anyhow::bail!("invalid group id '{id}' (allowed: [A-Za-z0-9._-])");
    }
    let url = format!("{base}/api/group-defs/{id}/members");
    let resp = crate::http_client::authed_client()?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("resolve failed: {status} — {body}");
    }
    let payload: serde_json::Value = resp.json().await?;
    println!("{}", serde_json::to_string_pretty(&payload)?);
    Ok(())
}

async fn def_delete(base: &str, id: &str) -> Result<()> {
    if !is_valid_resource_id(id) {
        anyhow::bail!("invalid group id '{id}' (allowed: [A-Za-z0-9._-])");
    }
    let url = format!("{base}/api/group-defs/{id}");
    let resp = crate::http_client::authed_client()?
        .delete(&url)
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("delete failed: {status} — {body}");
    }
    println!("deleted: {id}");
    Ok(())
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

//! `kanade group …` — fleet-wide group operations, all through the backend
//! HTTP API (`/api/groups`, `/api/agents/{pc_id}/groups`).
//!
//! Going through the backend (rather than the KV bucket over NATS) means
//! membership changes pass its authentication, role check and audit trail,
//! and the CLI needs only `KANADE_AUTH_TOKEN`, never a broker credential.
//!
//! Two layers of state end up rendered as a single "group" abstraction:
//!
//!   * membership — which groups a PC is in
//!   * `agent_config.groups.<name>` — overrides applied to every PC in the
//!     group
//!
//! `list` / `members` read the backend's fleet overview, which unions both
//! layers (plus dynamic-group and contacts-only groups), so counts can be
//! higher than the manual memberships alone; `list --pc` shows one PC's
//! manual membership only.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use kanade_shared::manifest::{GroupDef, is_valid_resource_id};
use kanade_shared::wire::AgentGroups;
use serde::Deserialize;
use tracing::warn;

use crate::cmd::provenance::{append_origin_yaml, detect_repo_origin, has_top_level_origin};

#[derive(Args, Debug)]
pub struct GroupArgs {
    #[command(subcommand)]
    pub sub: GroupSub,
}

#[derive(Subcommand, Debug)]
pub enum GroupSub {
    /// List every group known to the fleet (the backend's union of
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
    /// edit one PC's membership), these are manifest CRUD like `kanade view` / `kanade schedule`: a group is defined by a
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

/// Entry point for every `kanade group …` subcommand. All of it goes through
/// the backend HTTP API, so the backend's auth, role check and audit apply to
/// membership changes and no NATS connection or broker token is needed.
pub async fn execute(backend_url: &str, args: GroupArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        GroupSub::List { pc: Some(pc_id) } => list_pc(base, &pc_id).await,
        GroupSub::List { pc: None } => list_all(base).await,
        GroupSub::Members { name } => members(base, &name).await,
        GroupSub::Add { pc_id, name } => add(base, &pc_id, &name).await,
        GroupSub::Rm { pc_id, name } => rm(base, &pc_id, &name).await,
        GroupSub::Set { pc_id, names } => set(base, &pc_id, names).await,
        GroupSub::Def(sub) => execute_def(backend_url, sub).await,
    }
}

/// `kanade group def …` — HTTP manifest CRUD for [`GroupDef`] resources
/// (#1032). Same REST shape as `kanade view` (create / validate / list /
/// export / delete), plus a `members` preview that resolves the group
/// server-side.
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

/// `GET /api/groups` row. Only the fields the CLI renders; the backend also
/// sends contact emails, which are ignored here.
#[derive(Deserialize)]
struct GroupSummary {
    name: String,
    members: Vec<String>,
    has_config: bool,
}

#[derive(Deserialize)]
struct GroupsOverview {
    groups: Vec<GroupSummary>,
}

/// Build `{base}/api/agents/{pc_id}/groups[/{group}]` with each dynamic part
/// percent-encoded as one path segment, so a name containing `/` or `?`
/// cannot change which endpoint is hit.
fn agent_groups_url(base: &str, pc_id: &str, group: Option<&str>) -> Result<reqwest::Url> {
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid backend URL '{base}'"))?;
    {
        let mut seg = url
            .path_segments_mut()
            .map_err(|_| anyhow::anyhow!("backend URL '{base}' cannot be a base"))?;
        seg.pop_if_empty()
            .extend(["api", "agents", pc_id, "groups"]);
        if let Some(g) = group {
            seg.push(g);
        }
    }
    Ok(url)
}

/// Send a prepared request and decode the JSON body. Connection failures get
/// the request line as context; non-2xx responses (401 / 403 included) surface
/// the status and body the same way the other HTTP subcommands do.
async fn send_json<T: serde::de::DeserializeOwned>(
    req: reqwest::RequestBuilder,
    op: &str,
    method: &str,
    url: &reqwest::Url,
) -> Result<T> {
    let resp = req
        .send()
        .await
        .with_context(|| format!("{method} {url}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("{op} failed: {status} — {body}");
    }
    resp.json()
        .await
        .with_context(|| format!("parse JSON response from {method} {url}"))
}

async fn fetch_pc_groups(base: &str, pc_id: &str) -> Result<AgentGroups> {
    let url = agent_groups_url(base, pc_id, None)?;
    let req = crate::http_client::authed_client()?.get(url.clone());
    send_json(req, "list", "GET", &url).await
}

async fn fetch_overview(base: &str) -> Result<GroupsOverview> {
    let url = format!("{base}/api/groups");
    let url = reqwest::Url::parse(&url).with_context(|| format!("invalid backend URL '{base}'"))?;
    let req = crate::http_client::authed_client()?.get(url.clone());
    send_json(req, "list", "GET", &url).await
}

fn render_pc_groups(pc_id: &str, g: &AgentGroups) -> String {
    if g.is_empty() {
        format!("{pc_id}: (no groups)")
    } else {
        format!("{pc_id}: {}", g.groups.join(", "))
    }
}

fn render_overview(o: &GroupsOverview) -> String {
    if o.groups.is_empty() {
        return "(no groups yet)".to_string();
    }
    let mut rows: Vec<&GroupSummary> = o.groups.iter().collect();
    rows.sort_by(|a, b| a.name.cmp(&b.name));
    let mut out = format!(
        "{group:<24} {members:>7}  config\n{}",
        "-".repeat(48),
        group = "group",
        members = "members"
    );
    for g in rows {
        let cfg = if g.has_config { "yes" } else { "—" };
        out.push_str(&format!("\n{:<24} {:>7}  {cfg}", g.name, g.members.len()));
    }
    out
}

fn render_members(o: &GroupsOverview, name: &str) -> String {
    let mut hits: Vec<&str> = o
        .groups
        .iter()
        .filter(|g| g.name == name)
        .flat_map(|g| g.members.iter().map(String::as_str))
        .collect();
    if hits.is_empty() {
        return format!("(no PCs in '{name}')");
    }
    hits.sort();
    hits.join("\n")
}

fn render_add(pc_id: &str, name: &str, before: &AgentGroups, after: &AgentGroups) -> String {
    if before.contains(name) {
        format!("{pc_id}: already has '{name}' (no change)")
    } else {
        format!("{pc_id}: added '{name}' -> [{}]", after.groups.join(", "))
    }
}

fn render_rm(pc_id: &str, name: &str, before: &AgentGroups, after: &AgentGroups) -> String {
    if before.contains(name) {
        let rest = if after.is_empty() {
            "(no groups)".to_string()
        } else {
            after.groups.join(", ")
        };
        format!("{pc_id}: removed '{name}' -> [{rest}]")
    } else {
        format!("{pc_id}: not a member of '{name}' (no change)")
    }
}

fn render_set(pc_id: &str, after: &AgentGroups) -> String {
    if after.is_empty() {
        format!("{pc_id}: cleared all groups")
    } else {
        format!("{pc_id}: set membership to [{}]", after.groups.join(", "))
    }
}

async fn list_pc(base: &str, pc_id: &str) -> Result<()> {
    println!(
        "{}",
        render_pc_groups(pc_id, &fetch_pc_groups(base, pc_id).await?)
    );
    Ok(())
}

async fn list_all(base: &str) -> Result<()> {
    println!("{}", render_overview(&fetch_overview(base).await?));
    Ok(())
}

async fn members(base: &str, name: &str) -> Result<()> {
    println!("{}", render_members(&fetch_overview(base).await?, name));
    Ok(())
}

// add / rm read the membership first only to word the result ("added" vs
// "no change"); the POST / DELETE is always sent and the backend does the
// atomic read-modify-write, so a concurrent edit can at worst skew the message.
async fn add(base: &str, pc_id: &str, name: &str) -> Result<()> {
    let before = fetch_pc_groups(base, pc_id).await?;
    let url = agent_groups_url(base, pc_id, None)?;
    let req = crate::http_client::authed_client()?
        .post(url.clone())
        .json(&serde_json::json!({ "group": name }));
    let after: AgentGroups = send_json(req, "add", "POST", &url).await?;
    println!("{}", render_add(pc_id, name, &before, &after));
    Ok(())
}

async fn rm(base: &str, pc_id: &str, name: &str) -> Result<()> {
    let before = fetch_pc_groups(base, pc_id).await?;
    let url = agent_groups_url(base, pc_id, Some(name))?;
    let req = crate::http_client::authed_client()?.delete(url.clone());
    let after: AgentGroups = send_json(req, "remove", "DELETE", &url).await?;
    println!("{}", render_rm(pc_id, name, &before, &after));
    Ok(())
}

async fn set(base: &str, pc_id: &str, names: Vec<String>) -> Result<()> {
    let url = agent_groups_url(base, pc_id, None)?;
    let req = crate::http_client::authed_client()?
        .put(url.clone())
        .json(&AgentGroups { groups: names });
    let after: AgentGroups = send_json(req, "set", "PUT", &url).await?;
    println!("{}", render_set(pc_id, &after));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    /// One request as the fake backend saw it: `(method, path, body)`.
    type Seen = (String, String, String);

    /// Minimal HTTP server on an ephemeral port. Replies to the Nth request
    /// with the Nth `(status, json body)` and records what it received.
    async fn fake_backend(replies: Vec<(u16, &'static str)>) -> (String, Arc<Mutex<Vec<Seen>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            for (status, body) in replies {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let (head_end, content_len) = loop {
                    let mut chunk = [0u8; 4096];
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        return;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..pos]).to_lowercase();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        break (pos + 4, len);
                    }
                };
                while buf.len() < head_end + content_len {
                    let mut chunk = [0u8; 4096];
                    let n = sock.read(&mut chunk).await.unwrap_or(0);
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&chunk[..n]);
                }
                let text = String::from_utf8_lossy(&buf).to_string();
                let mut first = text.lines().next().unwrap_or("").split_whitespace();
                let method = first.next().unwrap_or("").to_string();
                let path = first.next().unwrap_or("").to_string();
                let body_txt = text[head_end.min(text.len())..].to_string();
                log.lock().unwrap().push((method, path, body_txt));
                let resp = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        (base, seen)
    }

    fn args(sub: GroupSub) -> GroupArgs {
        GroupArgs { sub }
    }

    fn seen(s: &Arc<Mutex<Vec<Seen>>>) -> Vec<(String, String, String)> {
        s.lock().unwrap().clone()
    }

    fn s(m: &str, p: &str, b: &str) -> Seen {
        (m.into(), p.into(), b.into())
    }

    #[tokio::test]
    async fn list_pc_sends_get_for_that_pc() {
        let (base, log) = fake_backend(vec![(200, r#"{"groups":["a"]}"#)]).await;
        execute(
            &base,
            args(GroupSub::List {
                pc: Some("pc 1".into()),
            }),
        )
        .await
        .unwrap();
        assert_eq!(seen(&log), vec![s("GET", "/api/agents/pc%201/groups", "")]);
    }

    #[tokio::test]
    async fn list_and_members_read_the_overview() {
        let body = r#"{"groups":[{"name":"g","members":["b","a"],"has_config":true,"emails":[]}]}"#;
        let (base, log) = fake_backend(vec![(200, body), (200, body)]).await;
        // A trailing slash on the backend URL is tolerated.
        let url = format!("{base}/");
        execute(&url, args(GroupSub::List { pc: None }))
            .await
            .unwrap();
        execute(&url, args(GroupSub::Members { name: "g".into() }))
            .await
            .unwrap();
        assert_eq!(
            seen(&log),
            vec![s("GET", "/api/groups", ""), s("GET", "/api/groups", "")]
        );
    }

    #[tokio::test]
    async fn add_reads_then_posts_group_body() {
        let (base, log) = fake_backend(vec![
            (200, r#"{"groups":[]}"#),
            (200, r#"{"groups":["x"]}"#),
        ])
        .await;
        execute(
            &base,
            args(GroupSub::Add {
                pc_id: "pc1".into(),
                name: "x".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            seen(&log),
            vec![
                s("GET", "/api/agents/pc1/groups", ""),
                s("POST", "/api/agents/pc1/groups", r#"{"group":"x"}"#),
            ]
        );
    }

    #[tokio::test]
    async fn add_existing_still_posts_and_is_not_an_error() {
        let g = r#"{"groups":["x"]}"#;
        let (base, log) = fake_backend(vec![(200, g), (200, g)]).await;
        execute(
            &base,
            args(GroupSub::Add {
                pc_id: "pc1".into(),
                name: "x".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(seen(&log).len(), 2);
    }

    #[tokio::test]
    async fn rm_encodes_group_segment_and_absent_is_ok() {
        let g = r#"{"groups":[]}"#;
        let (base, log) = fake_backend(vec![(200, g), (200, g)]).await;
        execute(
            &base,
            args(GroupSub::Rm {
                pc_id: "pc1".into(),
                name: "a/b".into(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            seen(&log),
            vec![
                s("GET", "/api/agents/pc1/groups", ""),
                s("DELETE", "/api/agents/pc1/groups/a%2Fb", ""),
            ]
        );
    }

    #[tokio::test]
    async fn set_puts_names_and_empty_set_sends_empty_list() {
        let (base, log) = fake_backend(vec![
            (200, r#"{"groups":["a","b"]}"#),
            (200, r#"{"groups":[]}"#),
        ])
        .await;
        execute(
            &base,
            args(GroupSub::Set {
                pc_id: "pc1".into(),
                names: vec!["b".into(), "a".into()],
            }),
        )
        .await
        .unwrap();
        execute(
            &base,
            args(GroupSub::Set {
                pc_id: "pc1".into(),
                names: vec![],
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            seen(&log),
            vec![
                s("PUT", "/api/agents/pc1/groups", r#"{"groups":["b","a"]}"#),
                s("PUT", "/api/agents/pc1/groups", r#"{"groups":[]}"#),
            ]
        );
    }

    #[tokio::test]
    async fn unauthorised_and_forbidden_surface_status_and_body() {
        let (base, _) =
            fake_backend(vec![(401, "no token"), (403, "operator role required")]).await;
        let e = execute(&base, args(GroupSub::List { pc: None }))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("401"), "{e:#}");
        let e = execute(
            &base,
            args(GroupSub::Set {
                pc_id: "pc1".into(),
                names: vec![],
            }),
        )
        .await
        .unwrap_err();
        let msg = format!("{e:#}");
        assert!(
            msg.contains("403") && msg.contains("operator role required"),
            "{msg}"
        );
    }

    #[tokio::test]
    async fn rejected_write_after_read_is_an_error() {
        let (base, _) = fake_backend(vec![(200, r#"{"groups":[]}"#), (403, "nope")]).await;
        let e = execute(
            &base,
            args(GroupSub::Add {
                pc_id: "pc1".into(),
                name: "x".into(),
            }),
        )
        .await
        .unwrap_err();
        assert!(format!("{e:#}").contains("403"));
    }

    #[tokio::test]
    async fn connection_failure_names_the_request() {
        // Bind then drop so the port is known-closed.
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        drop(l);
        let e = execute(&base, args(GroupSub::List { pc: None }))
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("GET "), "{e:#}");
    }

    #[tokio::test]
    async fn malformed_response_is_an_error() {
        let (base, _) = fake_backend(vec![(200, "not json")]).await;
        assert!(
            execute(
                &base,
                args(GroupSub::List {
                    pc: Some("p".into())
                })
            )
            .await
            .is_err()
        );
    }

    fn overview() -> GroupsOverview {
        GroupsOverview {
            groups: vec![
                GroupSummary {
                    name: "b".into(),
                    members: vec!["z".into(), "y".into()],
                    has_config: false,
                },
                GroupSummary {
                    name: "a".into(),
                    members: vec![],
                    has_config: true,
                },
            ],
        }
    }

    #[test]
    fn render_overview_matches_legacy_table() {
        let out = render_overview(&overview());
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[0],
            format!("{:<24} {:>7}  config", "group", "members")
        );
        assert_eq!(lines[1], "-".repeat(48));
        assert_eq!(lines[2], format!("{:<24} {:>7}  yes", "a", 0));
        assert_eq!(lines[3], format!("{:<24} {:>7}  —", "b", 2));
        assert_eq!(
            render_overview(&GroupsOverview { groups: vec![] }),
            "(no groups yet)"
        );
    }

    #[test]
    fn render_members_sorted_or_empty_marker() {
        assert_eq!(render_members(&overview(), "b"), "y\nz");
        assert_eq!(render_members(&overview(), "a"), "(no PCs in 'a')");
        assert_eq!(render_members(&overview(), "nope"), "(no PCs in 'nope')");
    }

    #[test]
    fn render_membership_messages() {
        let none = AgentGroups::default();
        let x = AgentGroups::new(["x"]);
        assert_eq!(render_pc_groups("p", &none), "p: (no groups)");
        assert_eq!(
            render_pc_groups("p", &AgentGroups::new(["b", "a"])),
            "p: a, b"
        );
        assert_eq!(render_add("p", "x", &none, &x), "p: added 'x' -> [x]");
        assert_eq!(
            render_add("p", "x", &x, &x),
            "p: already has 'x' (no change)"
        );
        assert_eq!(
            render_rm("p", "x", &x, &none),
            "p: removed 'x' -> [(no groups)]"
        );
        assert_eq!(
            render_rm("p", "x", &none, &none),
            "p: not a member of 'x' (no change)"
        );
        assert_eq!(render_set("p", &none), "p: cleared all groups");
        assert_eq!(render_set("p", &x), "p: set membership to [x]");
    }
}

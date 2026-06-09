//! `jobs.*` method handlers (SPEC §2.12.5 / §2.12.11).
//!
//! This PR ships the read-only catalog half:
//!
//! - `jobs.list` — return every manifest carrying a `client:` block,
//!   optionally narrowed to one [`JobCategory`], mapped into the
//!   [`UserInvokableJob`] wire shape the Client App's three job tabs
//!   (アップデート / 困ったとき / catalog) render.
//!
//! `jobs.execute` / `jobs.progress` / `jobs.kill` (the run half —
//! process spawn + streaming pushes + per-connection kill registry)
//! land in a follow-up PR. Until then the dispatcher answers them
//! with `MethodNotFound`.
//!
//! # Catalog source
//!
//! The agent reads the manifest catalog straight from the
//! `BUCKET_JOBS` KV at call time rather than from a cached snapshot,
//! so adding / removing a manifest's `client:` block (or editing its
//! `name`) takes effect on the client's next `jobs.list` without an
//! agent restart — SPEC §2.1's "Agent 側で manifest を必ず 再 lookup"
//! rule. `jobs.list` is a cold, user-initiated path (a tab tap), so
//! the extra KV round-trip is immaterial.
//!
//! The pure [`build_job_list`] mapping/filtering is split out from
//! the KV fetch so it can be unit-tested without a live NATS — the
//! fetch glue in [`handle_jobs_list`] stays a thin shell.

use futures::TryStreamExt;
use kanade_shared::ipc::error::{ErrorKind, RpcError};
use kanade_shared::ipc::jobs::{JobsListParams, JobsListResult, UserInvokableJob};
use kanade_shared::kv::BUCKET_JOBS;
use kanade_shared::manifest::Manifest;
use tracing::warn;

use super::super::connection::ConnectionState;
use super::system::HandlerResult;

/// `jobs.list` — list the user-invokable job catalog for the Client
/// App, optionally filtered to a single tab's category.
///
/// Reads `BUCKET_JOBS` on demand (see module docs). A connectivity
/// failure opening or scanning the bucket surfaces as
/// [`ErrorKind::InternalError`]; the client retries on the next tab
/// switch.
pub async fn handle_jobs_list(
    conn: &ConnectionState,
    params: JobsListParams,
) -> HandlerResult<JobsListResult> {
    // `nats` is always wired in production (the listener calls
    // `with_nats`); a `None` here only happens in a unit test that
    // forgot to, so treat it as an internal wiring bug, not a client
    // error.
    let client = conn.nats.as_ref().ok_or_else(|| {
        RpcError::new(
            ErrorKind::InternalError,
            "jobs.list: NATS client not wired into the connection",
        )
    })?;

    let js = async_nats::jetstream::new(client.clone());
    let kv = js.get_key_value(BUCKET_JOBS).await.map_err(|e| {
        warn!(error = %e, "jobs.list: failed to open BUCKET_JOBS");
        RpcError::new(
            ErrorKind::InternalError,
            format!("jobs.list: open jobs catalog: {e}"),
        )
    })?;

    // keys() failing is a connectivity-level error (broker hiccup),
    // distinct from "no jobs registered" (an empty key set) — mirror
    // local_scheduler::collect_jobs and surface it rather than
    // returning an empty catalog the client would read as "nothing
    // to run".
    let keys = kv.keys().await.map_err(|e| {
        warn!(error = %e, "jobs.list: BUCKET_JOBS keys() failed");
        RpcError::new(
            ErrorKind::InternalError,
            format!("jobs.list: scan jobs catalog: {e}"),
        )
    })?;
    // A fault mid-iteration (broker hiccup after the cursor opened)
    // is a connectivity error, NOT "no jobs" — propagate it so the
    // client retries instead of rendering an empty catalog. Swallowing
    // it with `unwrap_or_default()` would contradict the keys()
    // handling just above.
    let keys: Vec<String> = keys.try_collect().await.map_err(|e| {
        warn!(error = %e, "jobs.list: BUCKET_JOBS key stream faulted mid-iteration");
        RpcError::new(
            ErrorKind::InternalError,
            format!("jobs.list: stream jobs catalog: {e}"),
        )
    })?;

    // Fetch every manifest concurrently: `jobs.list` has to read the
    // whole BUCKET_JOBS (it can't tell which entries are user-invokable
    // without parsing them), so a fleet with dozens of jobs would pay N
    // sequential round-trips if fetched in a loop. A single corrupt /
    // unreadable entry is skipped (logged) rather than sinking the
    // whole listing — same tolerance the scheduler's catalog walk uses.
    let manifests: Vec<Manifest> = futures::future::join_all(keys.into_iter().map(|k| {
        let kv = kv.clone();
        async move {
            match kv.get(&k).await {
                Ok(Some(bytes)) => match serde_json::from_slice::<Manifest>(&bytes) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        warn!(key = %k, error = %e, "jobs.list: skipping unparseable manifest");
                        None
                    }
                },
                Ok(None) => None,
                Err(e) => {
                    warn!(key = %k, error = %e, "jobs.list: skipping unreadable manifest");
                    None
                }
            }
        }
    }))
    .await
    .into_iter()
    .flatten()
    .collect();

    Ok(build_job_list(&manifests, params.category))
}

/// Pure mapping + filtering: manifests → the `jobs.list` wire result.
///
/// Keeps only manifests carrying a `client:` block, maps each to a
/// [`UserInvokableJob`], applies the optional category filter, and
/// sorts by display name so the catalog renders in a stable order
/// regardless of KV key iteration order.
pub fn build_job_list(
    manifests: &[Manifest],
    filter: Option<kanade_shared::ipc::jobs::JobCategory>,
) -> JobsListResult {
    let mut items: Vec<UserInvokableJob> = manifests
        .iter()
        .filter_map(manifest_to_job)
        .filter(|j| filter.is_none_or(|c| j.category == c))
        .collect();
    // Stable, human-meaningful order: display name, then id as the
    // tiebreaker so two jobs sharing a name don't render
    // nondeterministically.
    items.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then_with(|| a.id.cmp(&b.id))
    });
    JobsListResult { items }
}

/// Map one manifest to its catalog row, or `None` when it carries no
/// `client:` block (i.e. it's an operator-only job).
///
/// The `client:` block's required fields (`name`, `category`) are
/// guaranteed present by serde at parse time, so this is a
/// straight field-for-field projection — no defaulting needed.
fn manifest_to_job(m: &Manifest) -> Option<UserInvokableJob> {
    let client = m.client.as_ref()?;
    Some(UserInvokableJob {
        id: m.id.clone(),
        display_name: client.name.clone(),
        display_description: client.description.clone(),
        icon: client.icon.clone(),
        category: client.category,
        version: m.version.clone(),
        // Per-user run history is minted by `jobs.execute` (a
        // follow-up PR); until then every row is "never run by you".
        last_run: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kanade_shared::ipc::jobs::JobCategory;
    use kanade_shared::manifest::{ClientHint, Execute, ExecuteShell};
    use kanade_shared::wire::{RunAs, Staleness};

    /// Build a manifest fixture. Pass `client: Some((name, category))`
    /// for a user-invokable job, `None` for an operator-only one.
    fn manifest(id: &str, client: Option<(&str, JobCategory)>) -> Manifest {
        Manifest {
            id: id.into(),
            version: "1.0.0".into(),
            description: None,
            execute: Execute {
                shell: ExecuteShell::Powershell,
                script: Some("echo hi".into()),
                script_file: None,
                script_object: None,
                timeout: "30s".into(),
                run_as: RunAs::default(),
                cwd: None,
            },
            require_approval: false,
            inventory: None,
            emit: None,
            check: None,
            staleness: Staleness::default(),
            client: client.map(|(name, category)| ClientHint {
                name: name.into(),
                description: None,
                category,
                icon: None,
            }),
        }
    }

    #[test]
    fn lists_only_client_jobs() {
        let manifests = [
            manifest("inv-hw", None),
            manifest(
                "chrome-update",
                Some(("Chrome を更新", JobCategory::SoftwareUpdate)),
            ),
            manifest("check-bitlocker", None),
        ];
        let result = build_job_list(&manifests, None);
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].id, "chrome-update");
        assert_eq!(result.items[0].display_name, "Chrome を更新");
        assert_eq!(result.items[0].category, JobCategory::SoftwareUpdate);
        assert!(result.items[0].last_run.is_none());
    }

    #[test]
    fn category_filter_narrows_to_one_tab() {
        let manifests = [
            manifest(
                "chrome-update",
                Some(("Chrome", JobCategory::SoftwareUpdate)),
            ),
            manifest("fix-teams", Some(("Teams 修復", JobCategory::Troubleshoot))),
            manifest("install-slack", Some(("Slack", JobCategory::Catalog))),
        ];
        let only_troubleshoot = build_job_list(&manifests, Some(JobCategory::Troubleshoot));
        assert_eq!(only_troubleshoot.items.len(), 1);
        assert_eq!(only_troubleshoot.items[0].id, "fix-teams");
    }

    #[test]
    fn empty_when_no_client_jobs() {
        let manifests = [manifest("inv-hw", None), manifest("inv-sw", None)];
        let result = build_job_list(&manifests, None);
        assert!(result.items.is_empty());
    }

    #[test]
    fn maps_all_client_fields() {
        // Full projection incl. the optional description + icon.
        let mut m = manifest("fix-teams", Some(("Teams 修復", JobCategory::Troubleshoot)));
        if let Some(c) = m.client.as_mut() {
            c.description = Some("重いとき用".into());
            c.icon = Some("brush-cleaning".into());
        }
        let result = build_job_list(std::slice::from_ref(&m), None);
        let row = &result.items[0];
        assert_eq!(row.display_description.as_deref(), Some("重いとき用"));
        assert_eq!(row.icon.as_deref(), Some("brush-cleaning"));
        assert_eq!(row.version, "1.0.0");
    }

    #[test]
    fn items_sorted_by_display_name() {
        let manifests = [
            manifest("z", Some(("Zebra", JobCategory::Catalog))),
            manifest("a", Some(("Apple", JobCategory::Catalog))),
            manifest("m", Some(("Mango", JobCategory::Catalog))),
        ];
        let result = build_job_list(&manifests, None);
        let names: Vec<&str> = result
            .items
            .iter()
            .map(|j| j.display_name.as_str())
            .collect();
        assert_eq!(names, ["Apple", "Mango", "Zebra"]);
    }
}

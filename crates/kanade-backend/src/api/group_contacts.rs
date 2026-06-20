//! Per-group notification email addresses (`group_contacts` KV bucket).
//!
//! Operator-managed contact info, parallel to `agent_groups` membership:
//!
//!   GET /api/groups/{name}/email  (viewer+) -> GroupContacts
//!   PUT /api/groups/{name}/email  (operator) replace the whole list
//!
//! Read backend-side by the compliance-alert projector via
//! [`emails_for_groups`] to fan an alert out to email. The PUT
//! re-normalises (trim / lower-case / dedup / sort) so the stored JSON is
//! bit-identical regardless of operator input.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use futures::StreamExt;
use tracing::{info, warn};

use kanade_shared::kv::BUCKET_GROUP_CONTACTS;
use kanade_shared::wire::GroupContacts;

use super::AppState;

/// `GET /api/groups/{name}/email` — the group's notification addresses
/// (an empty list when none are set).
pub async fn get_contacts(
    State(state): State<AppState>,
    Path(name): Path<String>,
) -> Result<Json<GroupContacts>, (StatusCode, String)> {
    let kv = open_bucket(&state).await?;
    Ok(Json(read_or_default(&kv, &name).await?))
}

/// `PUT /api/groups/{name}/email` — replace the group's address list.
/// Normalises and validates each address; rejects any that don't look
/// like an email so a typo surfaces at save time, not silently at send.
pub async fn put_contacts(
    State(state): State<AppState>,
    Path(name): Path<String>,
    Json(payload): Json<GroupContacts>,
) -> Result<Json<GroupContacts>, (StatusCode, String)> {
    let normalised = GroupContacts::new(payload.emails);
    if let Some(bad) = normalised.emails.iter().find(|e| !looks_like_email(e)) {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("not a valid email address: {bad:?}"),
        ));
    }

    let kv = open_bucket(&state).await?;
    let bytes = serde_json::to_vec(&normalised).map_err(|e| {
        warn!(error = %e, group = %name, "encode group_contacts");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("encode group_contacts for {name}: {e}"),
        )
    })?;
    kv.put(name.as_str(), bytes.into()).await.map_err(|e| {
        warn!(error = %e, group = %name, "write group_contacts");
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("write group_contacts for {name}: {e}"),
        )
    })?;
    info!(group = %name, emails = ?normalised.emails, "group_contacts replaced");
    Ok(Json(normalised))
}

/// Resolve every group's notification addresses into one de-duplicated
/// list, for the compliance-alert email fan-out. Best-effort: a missing
/// bucket or a group with no contacts contributes nothing (an unmapped
/// group is `warn`-logged, not an error) — the caller already published
/// the in-app notification, email is the additive channel.
pub(crate) async fn emails_for_groups(
    js: &async_nats::jetstream::Context,
    groups: &[String],
) -> Vec<String> {
    let Ok(kv) = js.get_key_value(BUCKET_GROUP_CONTACTS).await else {
        warn!(
            bucket = BUCKET_GROUP_CONTACTS,
            "open group_contacts KV for alert email"
        );
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for g in groups {
        match read_or_default(&kv, g).await {
            Ok(c) if !c.is_empty() => out.extend(c.emails),
            Ok(_) => warn!(group = %g, "compliance alert email: group has no contacts — skipping"),
            Err(_) => {
                warn!(group = %g, "compliance alert email: failed to read contacts — skipping")
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// Validate with the SAME parser `lettre` uses at send time
/// ([`lettre::Address`]), so anything accepted here is guaranteed
/// parseable when the alert actually mails it — and single-label domains
/// (`ops@localhost`, a local mail catcher under `encryption = none`) are
/// accepted rather than rejected by a hand-rolled "must contain a dot"
/// rule. Input is already trimmed + lower-cased by [`GroupContacts::new`].
fn looks_like_email(s: &str) -> bool {
    s.parse::<lettre::Address>().is_ok()
}

async fn open_bucket(
    state: &AppState,
) -> Result<async_nats::jetstream::kv::Store, (StatusCode, String)> {
    state
        .jetstream
        .get_key_value(BUCKET_GROUP_CONTACTS)
        .await
        .map_err(|e| {
            warn!(error = %e, bucket = BUCKET_GROUP_CONTACTS, "open group_contacts KV bucket");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("group_contacts KV bucket unavailable: {e}"),
            )
        })
}

async fn read_or_default(
    kv: &async_nats::jetstream::kv::Store,
    name: &str,
) -> Result<GroupContacts, (StatusCode, String)> {
    match kv.get(name).await {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).map_err(|e| {
            warn!(error = %e, group = name, "decode group_contacts");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("decode group_contacts for {name}: {e}"),
            )
        }),
        Ok(None) => Ok(GroupContacts::default()),
        Err(e) => {
            warn!(error = %e, group = name, "read group_contacts");
            Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("read group_contacts for {name}: {e}"),
            ))
        }
    }
}

/// Read the contact lists for many groups at once, returning a
/// `name -> emails` map for the groups that have any. Used to decorate
/// the `GET /api/groups` overview without an extra round-trip per row.
pub(crate) async fn contacts_map(
    state: &AppState,
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    let Ok(kv) = state.jetstream.get_key_value(BUCKET_GROUP_CONTACTS).await else {
        return map;
    };
    let Ok(mut keys) = kv.keys().await else {
        // Empty bucket on a fresh broker can fail keys() — treat as none.
        return map;
    };
    let mut names: Vec<String> = Vec::new();
    while let Some(k) = keys.next().await {
        match k {
            Ok(k) => names.push(k),
            Err(e) => warn!(error = %e, "group_contacts keys()"),
        }
    }
    // Fetch the per-group rows concurrently (bounded) rather than one
    // round-trip at a time — `contacts_map` runs on every `GET /api/groups`,
    // so on a large fleet the serial await dominated. Mirrors the
    // `buffer_unordered(16)` walk in `list_all_groups`.
    const READ_CONCURRENCY: usize = 16;
    map = futures::stream::iter(names)
        .map(|name| {
            let kv = kv.clone();
            async move {
                match read_or_default(&kv, &name).await {
                    Ok(c) if !c.is_empty() => Some((name, c.emails)),
                    _ => None,
                }
            }
        })
        .buffer_unordered(READ_CONCURRENCY)
        .filter_map(|x| async move { x })
        .collect()
        .await;
    map
}

#[cfg(test)]
mod tests {
    use super::looks_like_email;

    #[test]
    fn accepts_plausible_addresses() {
        assert!(looks_like_email("ops@example.com"));
        assert!(looks_like_email("a.b+tag@sub.example.co.jp"));
        // Single-label domain: a local mail catcher under `encryption =
        // none`. Accepted because `lettre::Address` accepts it at send.
        assert!(looks_like_email("ops@localhost"));
    }

    #[test]
    fn rejects_obvious_non_addresses() {
        assert!(!looks_like_email("alice")); // no '@'
        assert!(!looks_like_email("@example.com")); // empty local part
        assert!(!looks_like_email("two @spaces.com")); // space in local part
        assert!(!looks_like_email("ops@")); // empty domain
    }
}

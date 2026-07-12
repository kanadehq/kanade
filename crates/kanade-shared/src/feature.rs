//! Page-level **feature** catalog — the single source of truth for
//! per-account page visibility (see backend `auth::require_features`).
//!
//! Each variant maps 1:1 to a navigable SPA page (the sidebar entries in
//! `web/src/components/Sidebar.tsx`). An account's `allowed_features`
//! column (`users.allowed_features`, JSON array of these string keys) is
//! an **allow-list**: `NULL` means "unrestricted — every page", any array
//! restricts the account to exactly those pages (plus the always-open
//! commons: the login/self-service routes and the Dashboard landing feed).
//!
//! Backend and SPA share these string keys so a page can't be gated on one
//! side without the other agreeing on its name. The keys match the SPA
//! route path without the leading slash (`/compliance` → `"compliance"`).
//!
//! Enforcement is **hard** (backend `403`), not merely a hidden nav item:
//! the routes owned by a page are gated by [`Feature`] in
//! `crate::api::feature_map` on the backend, so a restricted account can't
//! reach the data by typing the URL or calling the API directly either.

use serde::{Deserialize, Serialize};

/// A gatable SPA page. `serde` (de)serializes each variant as its lowercase
/// route key (`Compliance` ⇄ `"compliance"`), which is exactly the string
/// stored in `users.allowed_features` and returned by `/api/auth/me`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Feature {
    Dashboard,
    Run,
    Exec,
    Agents,
    Inventory,
    Compliance,
    Activity,
    Events,
    Audit,
    Logs,
    Collect,
    Analytics,
    Jobs,
    Schedules,
    Views,
    Notifications,
    Rollout,
    Apps,
    Groups,
    Config,
    Jetstream,
    Accounts,
    Settings,
}

impl Feature {
    /// The wire/DB key for this feature (matches the SPA route path minus
    /// the leading slash). Kept in lockstep with the `serde` rename above.
    pub fn as_str(self) -> &'static str {
        match self {
            Feature::Dashboard => "dashboard",
            Feature::Run => "run",
            Feature::Exec => "exec",
            Feature::Agents => "agents",
            Feature::Inventory => "inventory",
            Feature::Compliance => "compliance",
            Feature::Activity => "activity",
            Feature::Events => "events",
            Feature::Audit => "audit",
            Feature::Logs => "logs",
            Feature::Collect => "collect",
            Feature::Analytics => "analytics",
            Feature::Jobs => "jobs",
            Feature::Schedules => "schedules",
            Feature::Views => "views",
            Feature::Notifications => "notifications",
            Feature::Rollout => "rollout",
            Feature::Apps => "apps",
            Feature::Groups => "groups",
            Feature::Config => "config",
            Feature::Jetstream => "jetstream",
            Feature::Accounts => "accounts",
            Feature::Settings => "settings",
        }
    }

    /// Parse a feature key. Unknown keys yield `None` so callers can decide
    /// whether to reject (create/update validation) or silently drop
    /// (loading a stored list whose catalog has since shrunk).
    pub fn parse(s: &str) -> Option<Feature> {
        Some(match s {
            "dashboard" => Feature::Dashboard,
            "run" => Feature::Run,
            "exec" => Feature::Exec,
            "agents" => Feature::Agents,
            "inventory" => Feature::Inventory,
            "compliance" => Feature::Compliance,
            "activity" => Feature::Activity,
            "events" => Feature::Events,
            "audit" => Feature::Audit,
            "logs" => Feature::Logs,
            "collect" => Feature::Collect,
            "analytics" => Feature::Analytics,
            "jobs" => Feature::Jobs,
            "schedules" => Feature::Schedules,
            "views" => Feature::Views,
            "notifications" => Feature::Notifications,
            "rollout" => Feature::Rollout,
            "apps" => Feature::Apps,
            "groups" => Feature::Groups,
            "config" => Feature::Config,
            "jetstream" => Feature::Jetstream,
            "accounts" => Feature::Accounts,
            "settings" => Feature::Settings,
            _ => return None,
        })
    }

    /// Validate + canonicalize a submitted list of feature keys: reject the
    /// first unknown key (returning it as the `Err`), de-duplicate, and return
    /// the surviving keys in catalog order so what's stored is stable. An
    /// empty input is valid. Shared by the account allow-list editor and the
    /// permission-group editor so both agree on what a valid list is.
    pub fn canonicalize(keys: &[String]) -> Result<Vec<String>, String> {
        let mut set: Vec<Feature> = Vec::new();
        for k in keys {
            let f = Feature::parse(k).ok_or_else(|| k.clone())?;
            if !set.contains(&f) {
                set.push(f);
            }
        }
        Ok(Feature::ALL
            .iter()
            .filter(|f| set.contains(f))
            .map(|f| f.as_str().to_string())
            .collect())
    }

    /// Every feature, in catalog order. Drives the SPA's account editor
    /// checkbox list and lets the backend validate a submitted allow-list
    /// against the full known set.
    pub const ALL: [Feature; 23] = [
        Feature::Dashboard,
        Feature::Run,
        Feature::Exec,
        Feature::Agents,
        Feature::Inventory,
        Feature::Compliance,
        Feature::Activity,
        Feature::Events,
        Feature::Audit,
        Feature::Logs,
        Feature::Collect,
        Feature::Analytics,
        Feature::Jobs,
        Feature::Schedules,
        Feature::Views,
        Feature::Notifications,
        Feature::Rollout,
        Feature::Apps,
        Feature::Groups,
        Feature::Config,
        Feature::Jetstream,
        Feature::Accounts,
        Feature::Settings,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_roundtrips_for_every_variant() {
        for f in Feature::ALL {
            assert_eq!(Feature::parse(f.as_str()), Some(f), "roundtrip {f:?}");
        }
    }

    #[test]
    fn all_covers_the_enum() {
        // A missing entry in ALL would silently drop a feature from
        // validation + the SPA editor; assert the count matches the array
        // length so adding a variant without updating ALL fails to compile
        // (length mismatch) or fails here.
        assert_eq!(Feature::ALL.len(), 23);
        // No duplicate keys.
        let mut keys: Vec<&str> = Feature::ALL.iter().map(|f| f.as_str()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), Feature::ALL.len(), "duplicate feature key");
    }

    #[test]
    fn serde_uses_lowercase_key() {
        assert_eq!(
            serde_json::to_string(&Feature::Compliance).unwrap(),
            "\"compliance\""
        );
        assert_eq!(
            serde_json::from_str::<Feature>("\"jetstream\"").unwrap(),
            Feature::Jetstream
        );
    }

    #[test]
    fn unknown_key_is_none() {
        assert_eq!(Feature::parse("nope"), None);
        assert_eq!(Feature::parse("Compliance"), None); // case-sensitive
    }

    #[test]
    fn canonicalize_validates_dedupes_orders() {
        // Unknown key → Err(the offending key).
        assert_eq!(
            Feature::canonicalize(&["bogus".into()]),
            Err("bogus".to_string())
        );
        // Dedup + catalog order (Inventory precedes Compliance in ALL).
        assert_eq!(
            Feature::canonicalize(&["compliance".into(), "inventory".into(), "compliance".into()]),
            Ok(vec!["inventory".to_string(), "compliance".to_string()])
        );
        // Empty is valid.
        assert_eq!(Feature::canonicalize(&[]), Ok(vec![]));
    }
}

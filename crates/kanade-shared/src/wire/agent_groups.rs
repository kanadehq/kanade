use serde::{Deserialize, Serialize};

/// Value stored in the `agent_groups` KV bucket, keyed by `pc_id`. The
/// wrapper struct (instead of a bare `Vec<String>`) leaves room for
/// future per-PC metadata (membership timestamps, who-set-it audit, …)
/// without breaking the wire format.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentGroups {
    /// Sorted, de-duplicated group names. Producers should call
    /// [`AgentGroups::new`] / [`AgentGroups::insert`] / [`AgentGroups::remove`]
    /// to maintain those invariants; consumers can rely on them when
    /// diffing two snapshots for "what changed since last KV update".
    pub groups: Vec<String>,
}

impl AgentGroups {
    /// Construct from any iterator. Sorts + dedups so two callers that
    /// produce the same logical set get bit-identical JSON.
    pub fn new<I, S>(groups: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut v: Vec<String> = groups.into_iter().map(Into::into).collect();
        v.sort();
        v.dedup();
        Self { groups: v }
    }

    /// Insert a group. Returns `true` if the membership actually
    /// changed (i.e. the group wasn't already present). Keeps the
    /// inner Vec sorted.
    pub fn insert(&mut self, group: impl Into<String>) -> bool {
        let group = group.into();
        match self.groups.binary_search(&group) {
            Ok(_) => false,
            Err(idx) => {
                self.groups.insert(idx, group);
                true
            }
        }
    }

    /// Remove a group. Returns `true` if the membership actually
    /// changed (i.e. the group was present).
    pub fn remove(&mut self, group: &str) -> bool {
        match self.groups.binary_search_by(|g| g.as_str().cmp(group)) {
            Ok(idx) => {
                self.groups.remove(idx);
                true
            }
            Err(_) => false,
        }
    }

    pub fn contains(&self, group: &str) -> bool {
        self.groups.binary_search_by(|g| g.as_str().cmp(group)).is_ok()
    }

    pub fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, String> {
        self.groups.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sorts_and_dedups() {
        let g = AgentGroups::new(["wave2", "canary", "wave1", "canary"]);
        assert_eq!(g.groups, vec!["canary", "wave1", "wave2"]);
    }

    #[test]
    fn round_trips_through_json() {
        let g = AgentGroups::new(["wave1", "dept-eng"]);
        let json = serde_json::to_string(&g).unwrap();
        assert_eq!(json, r#"{"groups":["dept-eng","wave1"]}"#);
        let back: AgentGroups = serde_json::from_str(&json).unwrap();
        assert_eq!(back, g);
    }

    #[test]
    fn empty_round_trips() {
        let g = AgentGroups::default();
        let json = serde_json::to_string(&g).unwrap();
        assert_eq!(json, r#"{"groups":[]}"#);
        let back: AgentGroups = serde_json::from_str(&json).unwrap();
        assert!(back.is_empty());
    }

    #[test]
    fn insert_returns_true_on_change_false_on_noop() {
        let mut g = AgentGroups::new(["wave1"]);
        assert!(g.insert("canary"));
        assert!(!g.insert("canary")); // already present
        assert_eq!(g.groups, vec!["canary", "wave1"]);
    }

    #[test]
    fn remove_returns_true_on_change_false_on_noop() {
        let mut g = AgentGroups::new(["wave1", "canary"]);
        assert!(g.remove("wave1"));
        assert!(!g.remove("wave1"));
        assert_eq!(g.groups, vec!["canary"]);
    }

    #[test]
    fn contains_matches_after_mutations() {
        let mut g = AgentGroups::new(["wave1"]);
        assert!(g.contains("wave1"));
        assert!(!g.contains("canary"));
        g.insert("canary");
        assert!(g.contains("canary"));
        g.remove("wave1");
        assert!(!g.contains("wave1"));
    }

    #[test]
    fn accepts_unknown_fields_for_forward_compat() {
        // Future versions may add per-PC metadata next to `groups`.
        // Old clients should not break on the new fields — serde's
        // default is to ignore unknowns, but lock that property down.
        let json = r#"{"groups":["canary"],"set_by":"alice","set_at":"2026-05-16T01:00:00Z"}"#;
        let g: AgentGroups = serde_json::from_str(json).unwrap();
        assert_eq!(g.groups, vec!["canary"]);
    }
}

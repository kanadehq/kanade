//! Per-PC operator metadata stored in the `agent_meta` KV bucket, keyed
//! by `pc_id`.
//!
//! Free-form key/value annotations an operator attaches to a machine —
//! the primary user's name / email / department, or an ad-hoc note. Keys
//! are not fixed; the operator invents them. Distinct from:
//!
//!   * the `agents` heartbeat projection — that table is *volatile*
//!     (overwritten every heartbeat, removed by dead-agent prune), so it
//!     is the wrong home for durable operator-entered data;
//!   * `agent_groups` (per-PC membership) — same "per-PC operator-managed
//!     KV" pattern, but tags, not key/value.
//!
//! A wrapper struct (rather than a bare map) leaves room for future
//! per-PC metadata without a wire break, and a `Vec<MetaEntry>` (not a
//! map) preserves the operator's field order.

use serde::{Deserialize, Serialize};

#[derive(Serialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentMeta {
    /// Operator-entered key/value rows, in display order. Producers
    /// should go through [`AgentMeta::new`] / [`AgentMeta::upsert`] /
    /// [`AgentMeta::remove`] so the invariants below hold; consumers can
    /// then rely on them: unique non-empty keys, each trimmed.
    pub entries: Vec<MetaEntry>,
}

// Deserialize routes through [`AgentMeta::new`] so the invariants
// (trimmed, unique non-empty keys) hold no matter where the JSON came
// from — a raw KV write, an older binary, or a hand-edited value. Without
// this a duplicate/blank/untrimmed key on the wire would slip straight
// past `upsert`/`remove` (which assume unique keys) into the process.
impl<'de> Deserialize<'de> for AgentMeta {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            #[serde(default)]
            entries: Vec<MetaEntry>,
        }
        Ok(AgentMeta::new(Raw::deserialize(deserializer)?.entries))
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct MetaEntry {
    pub key: String,
    pub value: String,
}

impl MetaEntry {
    pub fn new(key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            value: value.into(),
        }
    }
}

impl AgentMeta {
    /// Construct from any iterator of entries, normalising: trim key and
    /// value, drop rows whose key is empty, and de-dup by key keeping the
    /// **last** occurrence's value while preserving each key's first-seen
    /// position. So two operators who enter the same logical set (any
    /// trailing whitespace / duplicate key / re-ordered) converge on the
    /// same stored JSON for a given field order.
    pub fn new<I: IntoIterator<Item = MetaEntry>>(entries: I) -> Self {
        let mut order: Vec<String> = Vec::new();
        let mut values: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for e in entries {
            let key = e.key.trim().to_string();
            if key.is_empty() {
                continue;
            }
            let value = e.value.trim().to_string();
            if !values.contains_key(&key) {
                order.push(key.clone());
            }
            values.insert(key, value); // last wins
        }
        let entries = order
            .into_iter()
            .map(|key| {
                let value = values.remove(&key).unwrap_or_default();
                MetaEntry { key, value }
            })
            .collect();
        Self { entries }
    }

    /// Set a key to a value (insert or overwrite). Trims both. Returns
    /// `true` if the stored set changed (new key, or a different value for
    /// an existing key), `false` on a no-op. An empty (post-trim) key is
    /// rejected as a no-op — use [`AgentMeta::remove`] to drop a key.
    pub fn upsert(&mut self, key: impl Into<String>, value: impl Into<String>) -> bool {
        let key = key.into().trim().to_string();
        if key.is_empty() {
            return false;
        }
        let value = value.into().trim().to_string();
        match self.entries.iter_mut().find(|e| e.key == key) {
            Some(e) if e.value == value => false,
            Some(e) => {
                e.value = value;
                true
            }
            None => {
                self.entries.push(MetaEntry { key, value });
                true
            }
        }
    }

    /// Remove a key. Returns `true` if it was present.
    pub fn remove(&mut self, key: &str) -> bool {
        let key = key.trim();
        let before = self.entries.len();
        self.entries.retain(|e| e.key != key);
        self.entries.len() != before
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        let key = key.trim();
        self.entries
            .iter()
            .find(|e| e.key == key)
            .map(|e| e.value.as_str())
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(key: &str, value: &str) -> MetaEntry {
        MetaEntry::new(key, value)
    }

    #[test]
    fn new_trims_drops_empty_keys_and_dedups_last_wins() {
        let m = AgentMeta::new([
            e("  Name ", "  Alice  "),
            e("Email", "alice@example.com"),
            e("   ", "orphan value"), // empty key -> dropped
            e("Name", "Alice Smith"), // dup key -> last wins, keeps position
        ]);
        assert_eq!(
            m.entries,
            vec![e("Name", "Alice Smith"), e("Email", "alice@example.com"),]
        );
    }

    #[test]
    fn round_trips_through_json() {
        let m = AgentMeta::new([e("Dept", "Finance")]);
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(json, r#"{"entries":[{"key":"Dept","value":"Finance"}]}"#);
        let back: AgentMeta = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn empty_round_trips() {
        let m = AgentMeta::default();
        assert_eq!(serde_json::to_string(&m).unwrap(), r#"{"entries":[]}"#);
        assert!(m.is_empty());
    }

    #[test]
    fn upsert_inserts_overwrites_and_noops() {
        let mut m = AgentMeta::default();
        assert!(m.upsert("Name", "Alice")); // insert
        assert!(!m.upsert("Name", "Alice")); // no-op
        assert!(m.upsert("Name", "Bob")); // overwrite
        assert!(!m.upsert("  ", "x")); // empty key rejected
        assert!(m.upsert("  Email ", " a@b.com ")); // trims key + value
        assert_eq!(m.get("Name"), Some("Bob"));
        assert_eq!(m.get("Email"), Some("a@b.com"));
        assert_eq!(m.entries.len(), 2);
    }

    #[test]
    fn remove_reports_change() {
        let mut m = AgentMeta::new([e("Name", "Alice"), e("Dept", "IT")]);
        assert!(m.remove("Name"));
        assert!(!m.remove("Name"));
        assert_eq!(m.entries, vec![e("Dept", "IT")]);
    }

    #[test]
    fn accepts_unknown_fields_for_forward_compat() {
        // Future versions may add per-PC metadata (set_by / set_at)
        // alongside `entries`; old clients must not break on them.
        let json = r#"{"entries":[{"key":"Name","value":"Alice"}],"set_by":"admin"}"#;
        let m: AgentMeta = serde_json::from_str(json).unwrap();
        assert_eq!(m.entries, vec![e("Name", "Alice")]);
    }

    #[test]
    fn deserialize_normalises_untrusted_input() {
        // A raw KV write / older binary could store un-normalised rows
        // (untrimmed, blank key, duplicate key). Deserialize must clean
        // them so upsert/remove — which assume unique keys — stay correct.
        let json = r#"{"entries":[
            {"key":"  Name ","value":" Alice "},
            {"key":"","value":"orphan"},
            {"key":"Name","value":"Bob"}
        ]}"#;
        let m: AgentMeta = serde_json::from_str(json).unwrap();
        assert_eq!(m.entries, vec![e("Name", "Bob")]);
    }

    #[test]
    fn deserialize_defaults_missing_entries() {
        let m: AgentMeta = serde_json::from_str("{}").unwrap();
        assert!(m.is_empty());
    }
}

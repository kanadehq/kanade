use serde::{Deserialize, Serialize};

/// Value stored in the `group_contacts` KV bucket, keyed by group name.
/// Holds the email addresses that should receive notifications targeted
/// at the group — e.g. a compliance alert's `notify_groups`. Operator-
/// managed via the SPA Groups page, parallel to (but separate from)
/// `agent_groups` membership: membership is per-PC, contact is per-group.
///
/// A wrapper struct (rather than a bare `Vec<String>`) leaves room for
/// future per-group contact metadata (a display name, a phone/IM hook)
/// without breaking the wire format.
#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupContacts {
    /// Normalised email addresses: trimmed, lower-cased, de-duplicated,
    /// sorted, with blanks dropped. Use [`GroupContacts::new`] so two
    /// callers that enter the same set (any case/order) store identical
    /// JSON.
    pub emails: Vec<String>,
}

impl GroupContacts {
    /// Construct from any iterator, normalising the addresses: trim,
    /// lower-case, drop empties, sort, dedup.
    pub fn new<I, S>(emails: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut v: Vec<String> = emails
            .into_iter()
            .map(|e| e.into().trim().to_lowercase())
            .filter(|e| !e.is_empty())
            .collect();
        v.sort();
        v.dedup();
        Self { emails: v }
    }

    pub fn is_empty(&self) -> bool {
        self.emails.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_trims_lowercases_sorts_dedups_and_drops_blanks() {
        let c = GroupContacts::new([
            "  Ops@Example.com ",
            "it@example.com",
            "OPS@example.com",
            "   ",
            "",
        ]);
        assert_eq!(c.emails, vec!["it@example.com", "ops@example.com"]);
    }

    #[test]
    fn round_trips_through_json() {
        let c = GroupContacts::new(["sec@example.com"]);
        let json = serde_json::to_string(&c).unwrap();
        assert_eq!(json, r#"{"emails":["sec@example.com"]}"#);
        let back: GroupContacts = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn empty_round_trips() {
        let c = GroupContacts::default();
        assert_eq!(serde_json::to_string(&c).unwrap(), r#"{"emails":[]}"#);
        assert!(c.is_empty());
    }

    #[test]
    fn accepts_unknown_fields_for_forward_compat() {
        let json = r#"{"emails":["a@b.com"],"display_name":"IT","set_by":"alice"}"#;
        let c: GroupContacts = serde_json::from_str(json).unwrap();
        assert_eq!(c.emails, vec!["a@b.com"]);
    }
}

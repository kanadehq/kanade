//! #492: strict create-boundary parsing.
//!
//! The manifest / schedule types used to carry
//! `#[serde(deny_unknown_fields)]` as an operator typo guard — but
//! the same types are READ fleet-wide (agents decode them from
//! BUCKET_JOBS / BUCKET_SCHEDULES and inside live `Command`s), so
//! any field a newer backend added made every older agent reject the
//! whole object during a gradual fleet upgrade. CheckHint's `fleet`
//! field (#290 PR-E) did exactly that to pre-PR-E agents.
//!
//! The split: read paths are tolerant (plain serde, unknown fields
//! ignored — the wire rule is "new fields always have defaults"),
//! and the WRITE boundaries (`kanade job/schedule create`, the
//! backend's POST body extractor) call these helpers, which collect
//! every ignored path via [`serde_ignored`] and reject the payload
//! with the offending key paths spelled out — strictly better
//! diagnostics than `deny_unknown_fields`' single-key error.

use serde::de::DeserializeOwned;

/// Parse YAML, rejecting any key the target type doesn't know.
/// Errors are human-readable strings ready for CLI / HTTP 400 use.
pub fn from_yaml_str<T: DeserializeOwned>(raw: &str) -> Result<T, String> {
    let de = serde_yaml::Deserializer::from_str(raw);
    let mut ignored: Vec<String> = Vec::new();
    let value: T = serde_ignored::deserialize(de, |path| ignored.push(path.to_string()))
        .map_err(|e| e.to_string())?;
    reject_ignored(ignored)?;
    Ok(value)
}

/// Parse JSON bytes, rejecting any key the target type doesn't know.
pub fn from_json_slice<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    let mut de = serde_json::Deserializer::from_slice(bytes);
    let mut ignored: Vec<String> = Vec::new();
    let value: T = serde_ignored::deserialize(&mut de, |path| ignored.push(path.to_string()))
        .map_err(|e| e.to_string())?;
    // serde_json::from_slice calls de.end() internally to reject
    // trailing bytes; this open-coded path must do the same or
    // `{...}{"extra":true}` would pass the strict boundary
    // (PR #558 review, claude).
    de.end().map_err(|e| e.to_string())?;
    reject_ignored(ignored)?;
    Ok(value)
}

fn reject_ignored(mut ignored: Vec<String>) -> Result<(), String> {
    if ignored.is_empty() {
        return Ok(());
    }
    ignored.sort();
    ignored.dedup();
    Err(format!(
        "unknown field(s): {} — check for typos; fields must match the current schema",
        ignored.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use crate::manifest::Manifest;

    const MINIMAL: &str = r#"
id: echo-test
version: 0.0.1
execute:
  shell: powershell
  script: "echo 'kanade'"
  timeout: 30s
"#;

    #[test]
    fn strict_accepts_clean_manifest() {
        let m: Manifest = super::from_yaml_str(MINIMAL).expect("clean yaml parses");
        assert_eq!(m.id, "echo-test");
    }

    #[test]
    fn strict_rejects_top_level_typo_with_path() {
        let yaml = format!("{MINIMAL}staleness_polcy: warn\n");
        let err = super::from_yaml_str::<Manifest>(&yaml).unwrap_err();
        assert!(err.contains("staleness_polcy"), "{err}");
    }

    #[test]
    fn strict_rejects_nested_typo_with_path() {
        let yaml = r#"
id: echo-test
version: 0.0.1
execute:
  shell: powershell
  script_objectt: foo
  timeout: 30s
"#;
        let err = super::from_yaml_str::<Manifest>(yaml).unwrap_err();
        // serde_ignored reports the full path, e.g. `execute.script_objectt`.
        assert!(err.contains("execute.script_objectt"), "{err}");
    }

    #[test]
    fn tolerant_read_accepts_future_fields() {
        // #492: the READ path (plain serde) must accept payloads from
        // a newer writer — this is the gradual-upgrade contract that
        // deny_unknown_fields used to break fleet-wide.
        let yaml = format!("{MINIMAL}field_from_the_future: 42\n");
        let m: Manifest = serde_yaml::from_str(&yaml).expect("tolerant read");
        assert_eq!(m.id, "echo-test");
    }

    #[test]
    fn strict_json_rejects_typo() {
        let json = serde_json::json!({
            "id": "echo-test",
            "version": "0.0.1",
            "execute": { "shell": "powershell", "script": "echo", "timeout": "30s" },
            "tyypo": true,
        });
        let err =
            super::from_json_slice::<Manifest>(&serde_json::to_vec(&json).unwrap()).unwrap_err();
        assert!(err.contains("tyypo"), "{err}");
    }
}

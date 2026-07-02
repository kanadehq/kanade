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

/// Types parsed at a strict create boundary implement this so the
/// boundary can guard fields that [`serde_ignored`] can't see.
///
/// The blanket-ish default returns `None` ("serde_ignored catches every
/// typo, nothing extra to check"), which is correct for any type
/// WITHOUT a `#[serde(flatten)]` field. A type that DOES flatten a
/// sub-struct must override it: serde deserialises a flattened parent
/// by buffering all fields into an internal map and handing the
/// leftovers to the flattened field's deserialiser, and that buffering
/// is opaque to serde_ignored — so an unknown TOP-LEVEL key on a
/// flattening type is silently dropped instead of rejected (#924).
/// Overriding with the complete top-level key set (the type's own
/// fields AND the flattened struct's fields) lets the boundary reject
/// those keys explicitly. Nested typos are still caught by
/// serde_ignored as usual.
pub trait StrictSchema {
    /// The complete set of valid top-level keys, or `None` when the
    /// type has no flattened field. See the trait docs.
    fn strict_top_level_keys() -> Option<&'static [&'static str]> {
        None
    }
}

/// Parse YAML, rejecting any key the target type doesn't know.
/// Errors are human-readable strings ready for CLI / HTTP 400 use.
pub fn from_yaml_str<T: DeserializeOwned + StrictSchema>(raw: &str) -> Result<T, String> {
    let de = serde_yaml::Deserializer::from_str(raw);
    let mut ignored: Vec<String> = Vec::new();
    let value: T = serde_ignored::deserialize(de, |path| ignored.push(path.to_string()))
        .map_err(|e| e.to_string())?;
    reject_ignored(ignored)?;
    if let Some(known) = T::strict_top_level_keys() {
        // #924: guard the flatten-hidden top-level keys. Re-parse to a
        // Value only to enumerate the mapping's keys — cheap, and only
        // for flattening types (Schedule).
        let value: serde_yaml::Value = serde_yaml::from_str(raw).map_err(|e| e.to_string())?;
        if let Some(map) = value.as_mapping() {
            // A YAML key isn't necessarily a string: `on:` parses as the
            // boolean `true`, `123:` as a number. `yaml_key_label` renders
            // those to their scalar text so a non-string key can't slip
            // past the guard by not being a `&str` (gemini #945).
            reject_unknown_top_level(map.keys().map(yaml_key_label), known)?;
        }
    }
    Ok(value)
}

/// Render a YAML mapping key as the string the strict guard compares
/// against the known-key set. A string key borrows; a scalar key
/// (bool / number / null — e.g. the `on:` → `true` footgun) is rendered
/// owned so it's still checked rather than silently dropped.
fn yaml_key_label(k: &serde_yaml::Value) -> std::borrow::Cow<'_, str> {
    use std::borrow::Cow;
    match k {
        serde_yaml::Value::String(s) => Cow::Borrowed(s),
        serde_yaml::Value::Bool(b) => Cow::Owned(b.to_string()),
        serde_yaml::Value::Number(n) => Cow::Owned(n.to_string()),
        serde_yaml::Value::Null => Cow::Owned("null".to_string()),
        other => Cow::Owned(format!("{other:?}")),
    }
}

/// Parse JSON bytes, rejecting any key the target type doesn't know.
pub fn from_json_slice<T: DeserializeOwned + StrictSchema>(bytes: &[u8]) -> Result<T, String> {
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
    if let Some(known) = T::strict_top_level_keys() {
        // #924: same flatten-hidden top-level guard as the YAML path.
        let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
        if let Some(map) = value.as_object() {
            // JSON object keys are always strings — borrow them.
            reject_unknown_top_level(
                map.keys().map(|k| std::borrow::Cow::Borrowed(k.as_str())),
                known,
            )?;
        }
    }
    Ok(value)
}

/// Reject any present top-level key not in `known`. Takes an iterator of
/// `Cow<str>` so the common string-key case borrows (no per-key alloc on
/// the happy path, gemini #945) while a rendered non-string YAML key can
/// still be passed owned; only the `unknown` set — empty in the happy
/// path — ever allocates.
fn reject_unknown_top_level<'a, I>(present: I, known: &[&str]) -> Result<(), String>
where
    I: IntoIterator<Item = std::borrow::Cow<'a, str>>,
{
    let mut unknown: Vec<String> = present
        .into_iter()
        .filter(|k| !known.contains(&k.as_ref()))
        .map(|k| k.into_owned())
        .collect();
    if unknown.is_empty() {
        return Ok(());
    }
    unknown.sort();
    unknown.dedup();
    Err(format!(
        "unknown field(s): {} — check for typos; fields must match the current schema",
        unknown.join(", ")
    ))
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

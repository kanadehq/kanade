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

use serde::Deserialize;
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

/// Buffer a discriminator without losing duplicate keys, including nested
/// identities/specs. Never deserialize into Value before this visitor runs.
pub(crate) struct UniqueValue(pub serde_json::Value);
impl<'de> Deserialize<'de> for UniqueValue {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueValue;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a value without duplicate object keys")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut result = serde_json::Map::new();
                while let Some(key) = map.next_key::<String>()? {
                    if result.contains_key(&key) {
                        return Err(serde::de::Error::custom(format!("duplicate field {key}")));
                    }
                    result.insert(key, map.next_value::<UniqueValue>()?.0);
                }
                Ok(UniqueValue(result.into()))
            }
            fn visit_seq<S: serde::de::SeqAccess<'de>>(
                self,
                mut seq: S,
            ) -> Result<Self::Value, S::Error> {
                let mut result = Vec::new();
                while let Some(value) = seq.next_element::<UniqueValue>()? {
                    result.push(value.0);
                }
                Ok(UniqueValue(result.into()))
            }
            fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
                Ok(UniqueValue(v.into()))
            }
            fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
                serde_json::Number::from_f64(v)
                    .map(|n| UniqueValue(n.into()))
                    .ok_or_else(|| E::custom("nonfinite number"))
            }
            fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                Ok(UniqueValue(serde_json::Value::Null))
            }
            fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
                self.visit_unit()
            }
        }
        de.deserialize_any(Visitor)
    }
}

/// Preserve the source format's scalar conversions and serde_ignored callbacks
/// while detecting repeated keys at every map, before any tagged-enum buffer.
pub(crate) struct UniqueDeserializer<D>(pub D);
struct UniqueVisitor<V>(V);
struct UniqueMap<M> {
    inner: M,
    seen: std::collections::HashSet<String>,
}
struct UniqueSeq<S>(S);
struct UniqueSeed<S>(S);
impl<'de, S: serde::de::DeserializeSeed<'de>> serde::de::DeserializeSeed<'de> for UniqueSeed<S> {
    type Value = S::Value;
    fn deserialize<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        self.0.deserialize(UniqueDeserializer(de))
    }
}
impl<'de, M: serde::de::MapAccess<'de>> serde::de::MapAccess<'de> for UniqueMap<M> {
    type Error = M::Error;
    fn next_key_seed<K: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: K,
    ) -> Result<Option<K::Value>, Self::Error> {
        let Some(key) = self.inner.next_key::<UniqueValue>()? else {
            return Ok(None);
        };
        let label = key.0.to_string();
        if !self.seen.insert(label.clone()) {
            return Err(serde::de::Error::custom(format!("duplicate field {label}")));
        }
        let value = match key.0 {
            serde_json::Value::String(s) => {
                seed.deserialize(serde::de::value::StringDeserializer::<Self::Error>::new(s))?
            }
            other => seed.deserialize(other).map_err(serde::de::Error::custom)?,
        };
        Ok(Some(value))
    }
    fn next_value_seed<V: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner.next_value_seed(UniqueSeed(seed))
    }
    fn size_hint(&self) -> Option<usize> {
        self.inner.size_hint()
    }
}
impl<'de, S: serde::de::SeqAccess<'de>> serde::de::SeqAccess<'de> for UniqueSeq<S> {
    type Error = S::Error;
    fn next_element_seed<T: serde::de::DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, Self::Error> {
        self.0.next_element_seed(UniqueSeed(seed))
    }
    fn size_hint(&self) -> Option<usize> {
        self.0.size_hint()
    }
}
impl<'de, V: serde::de::Visitor<'de>> serde::de::Visitor<'de> for UniqueVisitor<V> {
    type Value = V::Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        self.0.expecting(f)
    }
    fn visit_map<M: serde::de::MapAccess<'de>>(self, map: M) -> Result<Self::Value, M::Error> {
        self.0.visit_map(UniqueMap {
            inner: map,
            seen: Default::default(),
        })
    }
    fn visit_seq<S: serde::de::SeqAccess<'de>>(self, seq: S) -> Result<Self::Value, S::Error> {
        self.0.visit_seq(UniqueSeq(seq))
    }
    fn visit_some<D: serde::Deserializer<'de>>(self, de: D) -> Result<Self::Value, D::Error> {
        self.0.visit_some(UniqueDeserializer(de))
    }
    fn visit_newtype_struct<D: serde::Deserializer<'de>>(
        self,
        de: D,
    ) -> Result<Self::Value, D::Error> {
        self.0.visit_newtype_struct(UniqueDeserializer(de))
    }
    fn visit_none<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        self.0.visit_none()
    }
    fn visit_unit<E: serde::de::Error>(self) -> Result<Self::Value, E> {
        self.0.visit_unit()
    }
    fn visit_enum<A: serde::de::EnumAccess<'de>>(self, data: A) -> Result<Self::Value, A::Error> {
        self.0.visit_enum(UniqueEnum(data))
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> Result<Self::Value, E> {
        self.0.visit_bool(v)
    }
    fn visit_i8<E: serde::de::Error>(self, v: i8) -> Result<Self::Value, E> {
        self.0.visit_i8(v)
    }
    fn visit_i16<E: serde::de::Error>(self, v: i16) -> Result<Self::Value, E> {
        self.0.visit_i16(v)
    }
    fn visit_i32<E: serde::de::Error>(self, v: i32) -> Result<Self::Value, E> {
        self.0.visit_i32(v)
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
        self.0.visit_i64(v)
    }
    fn visit_i128<E: serde::de::Error>(self, v: i128) -> Result<Self::Value, E> {
        self.0.visit_i128(v)
    }
    fn visit_u8<E: serde::de::Error>(self, v: u8) -> Result<Self::Value, E> {
        self.0.visit_u8(v)
    }
    fn visit_u16<E: serde::de::Error>(self, v: u16) -> Result<Self::Value, E> {
        self.0.visit_u16(v)
    }
    fn visit_u32<E: serde::de::Error>(self, v: u32) -> Result<Self::Value, E> {
        self.0.visit_u32(v)
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
        self.0.visit_u64(v)
    }
    fn visit_u128<E: serde::de::Error>(self, v: u128) -> Result<Self::Value, E> {
        self.0.visit_u128(v)
    }
    fn visit_f32<E: serde::de::Error>(self, v: f32) -> Result<Self::Value, E> {
        self.0.visit_f32(v)
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> Result<Self::Value, E> {
        self.0.visit_f64(v)
    }
    fn visit_char<E: serde::de::Error>(self, v: char) -> Result<Self::Value, E> {
        self.0.visit_char(v)
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
        self.0.visit_str(v)
    }
    fn visit_borrowed_str<E: serde::de::Error>(self, v: &'de str) -> Result<Self::Value, E> {
        self.0.visit_borrowed_str(v)
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
        self.0.visit_string(v)
    }
    fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
        self.0.visit_bytes(v)
    }
    fn visit_borrowed_bytes<E: serde::de::Error>(self, v: &'de [u8]) -> Result<Self::Value, E> {
        self.0.visit_borrowed_bytes(v)
    }
    fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<Self::Value, E> {
        self.0.visit_byte_buf(v)
    }
}
impl<'de, D: serde::Deserializer<'de>> serde::Deserializer<'de> for UniqueDeserializer<D> {
    type Error = D::Error;
    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
    fn deserialize_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_any(UniqueVisitor(visitor))
    }
    fn deserialize_bool<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_bool(UniqueVisitor(visitor))
    }
    fn deserialize_i8<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_i8(UniqueVisitor(visitor))
    }
    fn deserialize_i16<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_i16(UniqueVisitor(visitor))
    }
    fn deserialize_i32<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_i32(UniqueVisitor(visitor))
    }
    fn deserialize_i64<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_i64(UniqueVisitor(visitor))
    }
    fn deserialize_i128<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_i128(UniqueVisitor(visitor))
    }
    fn deserialize_u8<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_u8(UniqueVisitor(visitor))
    }
    fn deserialize_u16<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_u16(UniqueVisitor(visitor))
    }
    fn deserialize_u32<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_u32(UniqueVisitor(visitor))
    }
    fn deserialize_u64<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_u64(UniqueVisitor(visitor))
    }
    fn deserialize_u128<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_u128(UniqueVisitor(visitor))
    }
    fn deserialize_f32<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_f32(UniqueVisitor(visitor))
    }
    fn deserialize_f64<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_f64(UniqueVisitor(visitor))
    }
    fn deserialize_char<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_char(UniqueVisitor(visitor))
    }
    fn deserialize_str<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_str(UniqueVisitor(visitor))
    }
    fn deserialize_string<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_string(UniqueVisitor(visitor))
    }
    fn deserialize_bytes<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_bytes(UniqueVisitor(visitor))
    }
    fn deserialize_byte_buf<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_byte_buf(UniqueVisitor(visitor))
    }
    fn deserialize_option<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_option(UniqueVisitor(visitor))
    }
    fn deserialize_unit<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_unit(UniqueVisitor(visitor))
    }
    fn deserialize_seq<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_seq(UniqueVisitor(visitor))
    }
    fn deserialize_map<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_map(UniqueVisitor(visitor))
    }
    fn deserialize_identifier<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_identifier(UniqueVisitor(visitor))
    }
    fn deserialize_ignored_any<V: serde::de::Visitor<'de>>(
        self,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_ignored_any(UniqueVisitor(visitor))
    }
    fn deserialize_unit_struct<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_unit_struct(name, UniqueVisitor(visitor))
    }
    fn deserialize_newtype_struct<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0
            .deserialize_newtype_struct(name, UniqueVisitor(visitor))
    }
    fn deserialize_tuple<V: serde::de::Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.deserialize_tuple(len, UniqueVisitor(visitor))
    }
    fn deserialize_tuple_struct<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0
            .deserialize_tuple_struct(name, len, UniqueVisitor(visitor))
    }
    fn deserialize_struct<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0
            .deserialize_struct(name, fields, UniqueVisitor(visitor))
    }
    fn deserialize_enum<V: serde::de::Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0
            .deserialize_enum(name, variants, UniqueVisitor(visitor))
    }
}

struct UniqueEnum<A>(A);
struct UniqueVariant<A>(A);
impl<'de, A: serde::de::EnumAccess<'de>> serde::de::EnumAccess<'de> for UniqueEnum<A> {
    type Error = A::Error;
    type Variant = UniqueVariant<A::Variant>;
    fn variant_seed<V: serde::de::DeserializeSeed<'de>>(
        self,
        seed: V,
    ) -> Result<(V::Value, Self::Variant), Self::Error> {
        self.0
            .variant_seed(seed)
            .map(|(v, a)| (v, UniqueVariant(a)))
    }
}
impl<'de, A: serde::de::VariantAccess<'de>> serde::de::VariantAccess<'de> for UniqueVariant<A> {
    type Error = A::Error;
    fn unit_variant(self) -> Result<(), Self::Error> {
        self.0.unit_variant()
    }
    fn newtype_variant_seed<T: serde::de::DeserializeSeed<'de>>(
        self,
        seed: T,
    ) -> Result<T::Value, Self::Error> {
        self.0.newtype_variant_seed(UniqueSeed(seed))
    }
    fn tuple_variant<V: serde::de::Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.tuple_variant(len, UniqueVisitor(visitor))
    }
    fn struct_variant<V: serde::de::Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.0.struct_variant(fields, UniqueVisitor(visitor))
    }
}

//! `scripts/ops/jetstream-resources.json` is the single list of names the
//! operator recovery scripts (`jetstream-delete.ps1`, `jetstream-reset.ps1`)
//! delete. It must not drift from the canonical lists in `kv.rs`, or a reset
//! would silently leave resources behind (the old CLI copy did exactly that).

use std::collections::BTreeSet;
use std::path::PathBuf;

use kanade_shared::kv::{ALL_KV_BUCKETS, ALL_OBJECT_STORES, ALL_STREAMS};

/// Buckets kanade uses that `ALL_KV_BUCKETS` deliberately omits. Most are
/// created lazily on first use (`views*`, `group_defs*`, `scheduler_dispatch`).
/// `server_settings` is different: bootstrap does create it, but it was never
/// added to `ALL_KV_BUCKETS`, so the health probe doesn't count it. The reset
/// script must still wipe it, hence it is listed here explicitly. Adding a
/// bucket constant in `kv.rs` that is in neither list fails
/// `every_bucket_constant_is_listed`.
const LAZY_KV: &[&str] = &[
    "group_defs",
    "group_defs_yaml",
    "scheduler_dispatch",
    "server_settings",
    "views",
    "views_yaml",
];

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn load() -> serde_json::Map<String, serde_json::Value> {
    // Read at run time rather than `include_str!`: the file lives outside the
    // crate, which would break `cargo package`.
    let path = repo_root().join("scripts/ops/jetstream-resources.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
    serde_json::from_str::<serde_json::Value>(&text)
        .expect("valid JSON")
        .as_object()
        .expect("top-level object")
        .clone()
}

fn names(map: &serde_json::Map<String, serde_json::Value>, key: &str) -> Vec<String> {
    map.get(key)
        .unwrap_or_else(|| panic!("missing key {key}"))
        .as_array()
        .unwrap_or_else(|| panic!("{key} is not an array"))
        .iter()
        .map(|v| v.as_str().expect("string entry").to_string())
        .collect()
}

fn set(v: &[String]) -> BTreeSet<String> {
    v.iter().cloned().collect()
}

fn canon(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| s.to_string()).collect()
}

#[test]
fn resource_file_shape_is_strict() {
    let map = load();
    let keys: BTreeSet<_> = map.keys().cloned().collect();
    assert_eq!(
        keys,
        ["streams", "kv", "object", "lazy_kv"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        "unexpected or missing keys"
    );
    for key in ["streams", "kv", "object", "lazy_kv"] {
        let list = names(&map, key);
        assert_eq!(set(&list).len(), list.len(), "{key} has duplicates");
        for n in &list {
            assert!(
                !n.is_empty()
                    && n.chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{key} entry {n:?} is empty or not broker-safe"
            );
        }
    }
}

#[test]
fn resource_file_matches_canonical_lists() {
    let map = load();
    assert_eq!(set(&names(&map, "streams")), canon(ALL_STREAMS), "streams");
    assert_eq!(set(&names(&map, "kv")), canon(ALL_KV_BUCKETS), "kv");
    assert_eq!(
        set(&names(&map, "object")),
        canon(ALL_OBJECT_STORES),
        "object"
    );
}

#[test]
fn resource_file_lazy_buckets_are_explicit() {
    let map = load();
    let lazy = names(&map, "lazy_kv");
    assert_eq!(set(&lazy), canon(LAZY_KV), "lazy_kv");
    assert!(
        set(&lazy).is_disjoint(&canon(ALL_KV_BUCKETS)),
        "lazy_kv overlaps ALL_KV_BUCKETS"
    );
}

/// Every `pub const BUCKET_*` in `kv.rs` must be known to the reset script,
/// so a new bucket cannot be added without updating the data file.
#[test]
fn every_bucket_constant_is_listed() {
    let src = std::fs::read_to_string(repo_root().join("crates/kanade-shared/src/kv.rs"))
        .expect("read kv.rs");
    let map = load();
    let mut known = set(&names(&map, "kv"));
    known.extend(names(&map, "lazy_kv"));
    let mut found = 0;
    for line in src.lines() {
        let Some(rest) = line.strip_prefix("pub const BUCKET_") else {
            continue;
        };
        let value = rest
            .split('"')
            .nth(1)
            .unwrap_or_else(|| panic!("no string value on {line:?}"));
        found += 1;
        assert!(
            known.contains(value),
            "bucket {value:?} is defined in kv.rs but missing from scripts/ops/jetstream-resources.json"
        );
    }
    assert!(found > 0, "no BUCKET_ constants found; parser out of date");
}

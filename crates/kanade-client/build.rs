// Tauri 2.x build helper: reads `tauri.conf.json` and emits the
// generated code consumed by `tauri::generate_context!()` in
// `src/app.rs`. cfg-gated to Windows hosts to match the
// build-dependency gate in `Cargo.toml` — on non-Windows CI
// runners we compile the crate as the exit-fast shim in
// `src/main.rs` and `tauri-build` is never pulled in.
fn main() {
    #[cfg(target_os = "windows")]
    {
        // #260: keep `tauri.conf.json` version in lockstep with the
        // workspace (CARGO_PKG_VERSION). Tauri 2.x's CLI does this
        // when invoked via `tauri build`, but `cargo build` doesn't —
        // so historically the file drifted (e.g. workspace at 0.43.1
        // / tauri.conf.json frozen at 0.41.0, leaving the binary's
        // embedded VERSIONINFO ProductVersion stale and the SPA's
        // inventory page reporting the wrong client version).
        //
        // Done BEFORE `tauri_build::build()` so the latter reads the
        // freshly-synced version and embeds it in the codegen + the
        // PE VERSIONINFO resource. The write is idempotent (compare
        // first, only rewrite when different), so day-to-day
        // `cargo build` doesn't dirty the working tree.
        sync_tauri_version();
        tauri_build::build();
    }
}

#[cfg(target_os = "windows")]
fn sync_tauri_version() {
    use std::fs;
    use std::path::PathBuf;

    let manifest_dir = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR")
            .expect("CARGO_MANIFEST_DIR not set — cargo invokes build scripts with it"),
    );
    let conf_path = manifest_dir.join("tauri.conf.json");
    let cargo_version = std::env::var("CARGO_PKG_VERSION")
        .expect("CARGO_PKG_VERSION not set — cargo invokes build scripts with it");

    println!("cargo:rerun-if-changed=tauri.conf.json");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");

    let original = fs::read_to_string(&conf_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", conf_path.display()));

    // Targeted string-replace on the `"version": "X.Y.Z"` field
    // rather than a full JSON round-trip — `serde_json::Value` uses
    // `BTreeMap` for objects (unless `preserve_order` is enabled
    // workspace-wide) and silently re-sorts every key alphabetically
    // on re-serialize, producing a huge spurious diff on the bump
    // PR. Slicing keeps the rest of the file byte-identical (Gemini
    // #266 HIGH). Also lets us drop `serde_json` from
    // build-dependencies entirely (Gemini #266 MEDIUM).
    const PATTERN: &str = "\"version\": \"";
    let Some(start_idx) = original.find(PATTERN) else {
        panic!(
            "could not find {PATTERN:?} field in {} — has the file been hand-edited into a shape build.rs can't parse?",
            conf_path.display()
        );
    };
    let val_start = start_idx + PATTERN.len();
    let Some(end_offset) = original[val_start..].find('"') else {
        panic!(
            "malformed \"version\" field in {} (unterminated string)",
            conf_path.display()
        );
    };
    let val_end = val_start + end_offset;
    let current_version = &original[val_start..val_end];

    if current_version == cargo_version {
        return; // already in sync — skip the rewrite
    }

    let mut updated = String::with_capacity(original.len() + cargo_version.len());
    updated.push_str(&original[..val_start]);
    updated.push_str(&cargo_version);
    updated.push_str(&original[val_end..]);
    fs::write(&conf_path, updated).unwrap_or_else(|e| panic!("write {}: {e}", conf_path.display()));

    println!(
        "cargo:warning=tauri.conf.json version synced: {current_version:?} -> {cargo_version} (was drifting; #260)"
    );
}

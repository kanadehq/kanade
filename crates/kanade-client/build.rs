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
    let mut conf: serde_json::Value = serde_json::from_str(&original)
        .unwrap_or_else(|e| panic!("parse {}: {e}", conf_path.display()));

    let current = conf
        .get("version")
        .and_then(|v| v.as_str())
        .map(String::from);
    if current.as_deref() == Some(cargo_version.as_str()) {
        return; // already in sync — skip the rewrite
    }

    conf["version"] = serde_json::Value::String(cargo_version.clone());
    // `to_string_pretty` matches Tauri's own formatting (2-space
    // indent, trailing newline) so the diff against a hand-edited
    // file stays minimal.
    let updated = serde_json::to_string_pretty(&conf).expect("re-serialize tauri.conf.json") + "\n";
    fs::write(&conf_path, updated).unwrap_or_else(|e| panic!("write {}: {e}", conf_path.display()));

    println!(
        "cargo:warning=tauri.conf.json version synced: {:?} -> {} (was drifting; #260)",
        current.as_deref().unwrap_or("(missing)"),
        cargo_version
    );
}

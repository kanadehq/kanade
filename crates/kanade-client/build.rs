// Tauri 2.x build helper: reads `tauri.conf.json` and emits the
// generated code consumed by `tauri::generate_context!()` in
// `src/app.rs`. cfg-gated to Windows hosts to match the
// build-dependency gate in `Cargo.toml` — on non-Windows CI
// runners we compile the crate as the exit-fast shim in
// `src/main.rs` and `tauri-build` is never pulled in.
fn main() {
    #[cfg(target_os = "windows")]
    tauri_build::build();
}

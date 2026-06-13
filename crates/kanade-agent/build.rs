// Embed a Windows VERSIONINFO resource so the published binary
// carries its semver as a PE resource. Backend's
// POST /api/agents/publish (v0.13.1+) reads ProductVersion from
// here via `pelite` to auto-derive the Object-Store key — no
// operator-typed label, no chance of a "label vs binary" drift
// loop like the v0.11.1 → "1.0.0" incident.

// `#[cfg(target_os = "windows")]` evaluates against the build.rs
// HOST target — which equals the build TARGET when CI is doing a
// native compile (the only mode the release pipeline uses).
// Cross-compiling Linux → Windows would skip the resource compile,
// but kanade doesn't do that today and rc.exe / windres isn't
// available there anyway.
//
// The cfg gate (instead of a runtime `if target != "windows"`) is
// what lets non-Windows hosts compile build.rs at all: `winres`
// is a Windows-only build-dependency, so on Linux / macOS the
// symbol isn't even in scope.

#[cfg(target_os = "windows")]
fn main() {
    let mut res = winres::WindowsResource::new();
    res.set("ProductName", "kanade-agent");
    res.set("FileDescription", "Kanade endpoint management agent");
    res.set("OriginalFilename", "kanade-agent.exe");
    // Embed the shared kanade brand icon (奏 + the タクト/baton mark) so
    // the .exe carries an icon in Explorer / the taskbar / service
    // listings. The canonical .ico lives at the repo root (next to the
    // SVG sources the SPA + README use). It's outside this crate, so on a
    // crates.io / sparse-checkout build the file isn't packaged — guard on
    // existence so we don't emit a `rerun-if-changed` for a missing path
    // (which would dirty the build-script fingerprint every build and
    // defeat incremental compilation — Gemini #628). The GitHub-Release
    // binaries are built from a full checkout, so they get the icon.
    let icon = "../../assets/icon.ico";
    if std::path::Path::new(icon).exists() {
        println!("cargo:rerun-if-changed={icon}");
        res.set_icon(icon);
    }
    if let Ok(v) = std::env::var("CARGO_PKG_VERSION") {
        res.set("ProductVersion", &v);
        res.set("FileVersion", &v);
    }
    if let Err(e) = res.compile() {
        println!("cargo:warning=winres compile failed: {e}");
    }
}

#[cfg(not(target_os = "windows"))]
fn main() {}

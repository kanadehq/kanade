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

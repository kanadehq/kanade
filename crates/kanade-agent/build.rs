// Embed a Windows VERSIONINFO resource so the published binary
// carries its semver as a PE resource. Backend's
// POST /api/agents/publish (v0.13.1+) reads ProductVersion from
// here via `pelite` to auto-derive the Object-Store key — no
// operator-typed label, no chance of a "label vs binary" drift
// loop like the v0.11.1 → "1.0.0" incident.

fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target != "windows" {
        return;
    }
    let mut res = winres::WindowsResource::new();
    res.set("ProductName", "kanade-agent");
    res.set("FileDescription", "Kanade endpoint management agent");
    res.set("OriginalFilename", "kanade-agent.exe");
    if let Ok(v) = std::env::var("CARGO_PKG_VERSION") {
        res.set("ProductVersion", &v);
        res.set("FileVersion", &v);
    }
    if let Err(e) = res.compile() {
        // CI's Linux + macOS runners reach here only when cross-
        // compiling for Windows without an rc.exe in PATH; we keep
        // the build non-fatal so the agent binary still ships
        // (just without the embedded version resource).
        println!("cargo:warning=winres compile failed: {e}");
    }
}

// See crates/kanade-agent/build.rs for the rationale. Backend
// reads its own VERSIONINFO at /api/health/fleet (potentially) and
// at upload-validation time.

fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target != "windows" {
        return;
    }
    let mut res = winres::WindowsResource::new();
    res.set("ProductName", "kanade-backend");
    res.set("FileDescription", "Kanade endpoint management backend");
    res.set("OriginalFilename", "kanade-backend.exe");
    if let Ok(v) = std::env::var("CARGO_PKG_VERSION") {
        res.set("ProductVersion", &v);
        res.set("FileVersion", &v);
    }
    if let Err(e) = res.compile() {
        println!("cargo:warning=winres compile failed: {e}");
    }
}

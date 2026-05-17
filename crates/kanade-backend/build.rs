// See crates/kanade-agent/build.rs for the rationale. Backend
// reads its own VERSIONINFO at /api/health/fleet (potentially) and
// at upload-validation time.

// See crates/kanade-agent/build.rs for the cfg-gate rationale.

#[cfg(target_os = "windows")]
fn main() {
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

#[cfg(not(target_os = "windows"))]
fn main() {}

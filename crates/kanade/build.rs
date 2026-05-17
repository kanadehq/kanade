// See crates/kanade-agent/build.rs for the rationale.

fn main() {
    let target = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if target != "windows" {
        return;
    }
    let mut res = winres::WindowsResource::new();
    res.set("ProductName", "kanade");
    res.set("FileDescription", "Kanade admin CLI");
    res.set("OriginalFilename", "kanade.exe");
    if let Ok(v) = std::env::var("CARGO_PKG_VERSION") {
        res.set("ProductVersion", &v);
        res.set("FileVersion", &v);
    }
    if let Err(e) = res.compile() {
        println!("cargo:warning=winres compile failed: {e}");
    }
}

// See crates/kanade-agent/build.rs for the rationale. Backend
// reads its own VERSIONINFO at /api/health/fleet (potentially) and
// at upload-validation time.

// See crates/kanade-agent/build.rs for the cfg-gate rationale.

use std::fs;
use std::path::Path;

// `rust-embed` reads `web/dist/` at compile time, but the SPA is built
// only by `cargo make web-build` (release.yml + local dev), not by the
// kata-managed ci.yml or a fresh `cargo check`. Seed a placeholder so
// those builds compile; `bun run build` / release overwrites it with the
// real bundle, so tagged release binaries never embed the placeholder.
const PLACEHOLDER_HTML: &str = r#"<!doctype html>
<html lang="ja">
  <head><meta charset="utf-8" /><title>kanade</title></head>
  <body style="font-family: sans-serif; padding: 2rem; max-width: 40rem; margin: 0 auto;">
    <h1>kanade — SPA not built</h1>
    <p>This binary was compiled without the frontend bundle. Run
      <code>cargo make web-build</code> from the workspace root and rebuild.</p>
  </body>
</html>
"#;

fn seed_web_dist_placeholder() {
    println!("cargo:rerun-if-changed=web/dist");
    let dist = Path::new("web/dist");
    let index = dist.join("index.html");
    if !index.exists() {
        fs::create_dir_all(dist).expect("create web/dist");
        fs::write(&index, PLACEHOLDER_HTML).expect("write placeholder index.html");
        println!(
            "cargo:warning=web/dist/ was empty — wrote a placeholder index.html. Run `cargo make web-build` to embed the real SPA."
        );
    }
}

fn main() {
    seed_web_dist_placeholder();

    #[cfg(target_os = "windows")]
    {
        let mut res = winres::WindowsResource::new();
        res.set("ProductName", "kanade-backend");
        res.set("FileDescription", "Kanade endpoint management backend");
        res.set("OriginalFilename", "kanade-backend.exe");
        // Shared kanade brand icon — see crates/kanade-agent/build.rs for
        // the repo-root path + the exists()-guard rationale (Gemini #628).
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
}

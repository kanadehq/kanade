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

/// Write the release-metadata envelope (`OUT_DIR/agent_metadata.bin`) that
/// `src/release_metadata.rs` links into a dedicated section. The layout is
/// defined by `kanade_shared::agent_metadata`; keep the two in step (the
/// agent's `embedded_metadata_is_admitted` test fails if they drift).
fn generate_agent_metadata() {
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR");
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let os = std::env::var("CARGO_CFG_TARGET_OS").expect("CARGO_CFG_TARGET_OS");
    let arch = std::env::var("CARGO_CFG_TARGET_ARCH").expect("CARGO_CFG_TARGET_ARCH");
    let platform = match (os.as_str(), arch.as_str()) {
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "aarch64") => "macos-aarch64",
        other => panic!("kanade-agent has no release platform for target {other:?}"),
    };

    let payload =
        format!(r#"{{"product":"kanade-agent","version":"{version}","platform":"{platform}"}}"#);
    let mut encoded = Vec::new();
    encoded.extend_from_slice(b"KNAGMETA");
    encoded.extend_from_slice(&1u16.to_le_bytes()); // schema version
    encoded.extend_from_slice(&0u16.to_le_bytes()); // flags
    encoded.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    encoded.extend_from_slice(payload.as_bytes());

    let dest = std::path::Path::new(&out_dir).join("agent_metadata.bin");
    std::fs::write(dest, encoded).expect("write agent_metadata.bin");
}

#[cfg(target_os = "windows")]
fn main() {
    generate_agent_metadata();
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

#[cfg(target_os = "macos")]
fn main() {
    generate_agent_metadata();
    use std::{env, path::PathBuf, process::Command};
    println!("cargo:rerun-if-changed=src/capture_macos.m");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let target = env::var("TARGET").expect("TARGET");
    let arch = if target.starts_with("aarch64") {
        "arm64"
    } else {
        "x86_64"
    };
    let object = out.join("capture_macos.o");
    assert!(
        Command::new("xcrun")
            .args([
                "clang",
                "-arch",
                arch,
                "-mmacosx-version-min=11.0",
                "-fobjc-arc",
                "-fblocks",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-c",
                "src/capture_macos.m",
                "-o",
            ])
            .arg(&object)
            .status()
            .expect("compile ScreenCaptureKit bridge")
            .success()
    );
    assert!(
        Command::new("xcrun")
            .args(["ar", "crs"])
            .arg(out.join("libkanade_capture.a"))
            .arg(object)
            .status()
            .expect("archive ScreenCaptureKit bridge")
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=kanade_capture");
    for framework in [
        "Foundation",
        "CoreGraphics",
        "SystemConfiguration",
        "Security",
    ] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
    // Keep startup usable on older macOS; availability is checked before use.
    println!("cargo:rustc-link-arg=-Wl,-weak_framework,ScreenCaptureKit");
    println!("cargo:rustc-link-lib=objc");
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn main() {
    generate_agent_metadata();
}

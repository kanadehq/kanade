fn main() {
    println!("cargo:rerun-if-changed=src/client_lifecycle/native.m");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let arch = match std::env::var("CARGO_CFG_TARGET_ARCH").unwrap().as_str() {
        "aarch64" => "arm64",
        "x86_64" => "x86_64",
        other => panic!("unsupported macOS architecture: {other}"),
    };
    let status = std::process::Command::new("/usr/bin/xcrun")
        .args([
            "clang",
            "-arch",
            arch,
            "-mmacosx-version-min=14.0",
            "-fobjc-arc",
            "-Wall",
            "-Wextra",
            "-Werror",
            "-c",
            "src/client_lifecycle/native.m",
            "-o",
        ])
        .arg(out.join("client_lifecycle.o"))
        .status()
        .expect("compile native Client coordination");
    assert!(
        status.success(),
        "native Client coordination compilation failed"
    );
    assert!(
        std::process::Command::new("/usr/bin/ar")
            .arg("crs")
            .arg(out.join("libclient_lifecycle.a"))
            .arg(out.join("client_lifecycle.o"))
            .status()
            .unwrap()
            .success()
    );
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=client_lifecycle");
    for framework in ["Foundation", "AppKit", "CoreGraphics", "Security"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }
}

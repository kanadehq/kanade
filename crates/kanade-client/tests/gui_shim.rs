//! Unsupported targets must keep the exit-fast shim instead of starting a GUI.
#![cfg(not(any(
    target_os = "windows",
    all(target_os = "macos", target_arch = "aarch64")
)))]

#[test]
fn unsupported_gui_exits_with_a_platform_diagnostic() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_kanade-client"))
        .output()
        .expect("run the Client shim");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 diagnostic");
    assert!(
        stderr.contains("GUI requires Windows or Apple Silicon macOS 14+"),
        "unexpected diagnostic: {stderr}"
    );
}

//! Release signing pipeline regression tests.
//!
//! Asserts that:
//! 1. `deploy/macos/signing-cert.sha1` exists and contains a valid 40-character hex SHA-1 pin.
//! 2. `.github/workflows/release.yml` invokes both `release-post-build.sh sign` and
//!    `release-post-build.sh verify-dist` in the required order.
//! 3. `verify-dist` runs secretless (does not pass `MACOS_SIGN_CERT_*` secrets).
//! 4. `scripts/ci/macos-codesign.sh` enforces the pinned identity and explicitly rejects
//!    unstable cdhash-only designated requirements.

use std::fs;
use std::path::{Path, PathBuf};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/kanade-shared always has two ancestors")
        .to_path_buf()
}

#[test]
fn signing_cert_pin_exists_and_valid() {
    let root = workspace_root();
    let pin_path = root.join("deploy/macos/signing-cert.sha1");
    assert!(
        pin_path.is_file(),
        "deploy/macos/signing-cert.sha1 must exist to pin the macOS signing identity"
    );

    let raw = fs::read_to_string(&pin_path)
        .unwrap_or_else(|e| panic!("failed to read {pin_path:?}: {e}"));
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_whitespace() && *c != ':')
        .collect();

    assert_eq!(
        cleaned.len(),
        40,
        "signing-cert.sha1 must be a 40-character SHA-1 hex string, found: {cleaned:?}"
    );
    assert!(
        cleaned.chars().all(|c| c.is_ascii_hexdigit()),
        "signing-cert.sha1 contains non-hex characters: {cleaned:?}"
    );
    assert_eq!(
        cleaned,
        cleaned.to_ascii_lowercase(),
        "signing-cert.sha1 must be lowercase hex"
    );
}

#[test]
fn release_workflow_contains_signing_hooks_in_order() {
    let root = workspace_root();
    let workflow_path = root.join(".github/workflows/release.yml");
    assert!(
        workflow_path.is_file(),
        ".github/workflows/release.yml must exist"
    );

    let content = fs::read_to_string(&workflow_path)
        .unwrap_or_else(|e| panic!("failed to read {workflow_path:?}: {e}"));

    let sign_idx = content
        .find("scripts/ci/release-post-build.sh sign")
        .expect("release.yml must invoke `release-post-build.sh sign`");
    let stage_idx = content
        .find("Stage release artifacts")
        .expect("release.yml must have a `Stage release artifacts` step");
    let verify_idx = content
        .find("scripts/ci/release-post-build.sh verify-dist")
        .expect("release.yml must invoke `release-post-build.sh verify-dist`");
    let upload_idx = content
        .find("actions/upload-artifact")
        .expect("release.yml must have `actions/upload-artifact`");

    assert!(
        sign_idx < stage_idx,
        "post-build sign hook must run BEFORE artifact staging (sign_idx={sign_idx}, stage_idx={stage_idx})"
    );
    assert!(
        stage_idx < verify_idx,
        "post-package verify-dist hook must run AFTER artifact staging (stage_idx={stage_idx}, verify_idx={verify_idx})"
    );
    assert!(
        verify_idx < upload_idx,
        "post-package verify-dist hook must run BEFORE artifact upload (verify_idx={verify_idx}, upload_idx={upload_idx})"
    );

    // Verify verify-dist hook does not receive signing secrets (least privilege).
    let verify_block = &content[verify_idx..upload_idx];
    assert!(
        !verify_block.contains("MACOS_SIGN_CERT_P12_BASE64"),
        "verify-dist hook must not receive MACOS_SIGN_CERT_P12_BASE64 secret"
    );
    assert!(
        !verify_block.contains("MACOS_SIGN_CERT_PASSWORD"),
        "verify-dist hook must not receive MACOS_SIGN_CERT_PASSWORD secret"
    );
}

#[test]
fn macos_codesign_script_contract() {
    let root = workspace_root();
    let script_path = root.join("scripts/ci/macos-codesign.sh");
    assert!(
        script_path.is_file(),
        "scripts/ci/macos-codesign.sh must exist"
    );

    let script = fs::read_to_string(&script_path)
        .unwrap_or_else(|e| panic!("failed to read {script_path:?}: {e}"));

    assert!(
        script.contains("deploy/macos/signing-cert.sha1"),
        "script must reference deploy/macos/signing-cert.sha1"
    );
    assert!(
        script.contains("com.kanade.agent"),
        "script must target com.kanade.agent identifier"
    );
    assert!(
        script.contains("cdhash"),
        "script must explicitly check and reject cdhash constraints"
    );
    assert!(
        script.contains("verify_bin"),
        "script must verify binaries after signing and in verify-dist"
    );
}

//! Process-level tests for the offline `kanade app` manifest operations: real
//! temporary keys, real files, real exit codes. No backend or NATS is
//! reachable (the URLs point at a closed port), proving the commands are local.

use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use base64::Engine;

const IDENTITY: &str = r#"{"role":"cli","version":"1.2.3","os":"linux","arch":"aarch64","distro_abi":"musl","format":"tar_gz"}"#;
const OTHER_IDENTITY: &str = r#"{"role":"cli","version":"1.2.4","os":"linux","arch":"aarch64","distro_abi":"musl","format":"tar_gz"}"#;

fn seed() -> String {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).unwrap();
    base64::engine::general_purpose::STANDARD.encode(seed)
}

fn run(args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kanade"))
        .env("KANADE_NO_AUTOUPDATE", "1")
        .env_remove("KANADE_AUTH_TOKEN")
        .args([
            "--server",
            "nats://127.0.0.1:1",
            "--backend-url",
            "http://127.0.0.1:1",
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let mut si = child.stdin.take().unwrap();
        if let Some(s) = stdin {
            si.write_all(s.as_bytes()).unwrap();
        }
    }
    child.wait_with_output().unwrap()
}

struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("identity.json"), IDENTITY).unwrap();
        std::fs::write(dir.path().join("other.json"), OTHER_IDENTITY).unwrap();
        std::fs::write(dir.path().join("pkg.tar.gz"), b"artifact bytes").unwrap();
        Self { dir }
    }
    fn p(&self, name: &str) -> String {
        self.dir.path().join(name).to_string_lossy().into_owned()
    }
    fn trust(&self, name: &str, secret: &str) {
        let o = run(
            &["app", "publisher-trust", "--secret-stdin"],
            Some(&format!("{secret}\n")),
        );
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        std::fs::write(self.p(name), o.stdout).unwrap();
    }
    fn sign(&self, secret: &str, out: &str) -> Output {
        run(
            &[
                "app",
                "sign-artifact",
                "--identity",
                &self.p("identity.json"),
                "--package-id",
                "com.kanade.cli",
                "--min-os-version",
                "1.0",
                "--capability",
                "b_cap",
                "--capability",
                "a_cap",
                "--out",
                &self.p(out),
                "--secret-stdin",
                &self.p("pkg.tar.gz"),
            ],
            Some(&format!("{secret}\n")),
        )
    }
    fn verify(&self, trust: &str, identity: &str, manifest: &str, artifact: &str) -> Output {
        run(
            &[
                "app",
                "verify-artifact",
                "--trusted-key",
                &self.p(trust),
                "--identity",
                &self.p(identity),
                &self.p(manifest),
                &self.p(artifact),
            ],
            None,
        )
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

#[test]
fn sign_then_verify_round_trip_is_offline_only_and_leaks_no_secret() {
    let f = Fixture::new();
    let secret = seed();
    f.trust("trust.json", &secret);
    let s = f.sign(&secret, "m.json");
    assert!(s.status.success(), "{}", text(&s.stderr));
    assert!(!text(&s.stdout).contains(&secret) && !text(&s.stderr).contains(&secret));
    let manifest = std::fs::read_to_string(f.p("m.json")).unwrap();
    assert!(!manifest.contains(&secret));
    assert!(manifest.contains(r#""required_agent_capabilities":["a_cap","b_cap"]"#));
    assert!(manifest.contains(r#""min_os_version":"1.0""#));

    let v = f.verify("trust.json", "identity.json", "m.json", "pkg.tar.gz");
    assert!(v.status.success(), "{}", text(&v.stderr));
    let out = text(&v.stdout);
    assert!(out.contains("OFFLINE VERIFIED (signature + integrity only)"));
    assert!(out.contains("NOT CHECKED: current publisher authorization, freshness, revocation"));
    assert!(!out.to_lowercase().contains("deploy-ready:"));
}

#[test]
fn sign_refuses_to_overwrite_and_requires_min_os_version() {
    let f = Fixture::new();
    assert!(f.sign(&seed(), "m.json").status.success());
    let before = std::fs::read(f.p("m.json")).unwrap();
    let again = f.sign(&seed(), "m.json");
    assert_eq!(again.status.code(), Some(1));
    assert_eq!(std::fs::read(f.p("m.json")).unwrap(), before);

    let o = run(
        &[
            "app",
            "sign-artifact",
            "--identity",
            &f.p("identity.json"),
            "--package-id",
            "x",
            "--out",
            &f.p("n.json"),
            "--secret-stdin",
            &f.p("pkg.tar.gz"),
        ],
        Some(&format!("{}\n", seed())),
    );
    assert_eq!(o.status.code(), Some(2));
    assert!(!Path::new(&f.p("n.json")).exists());
}

#[test]
fn verification_failures_exit_nonzero_with_no_success_banner() {
    let f = Fixture::new();
    let secret = seed();
    f.trust("trust.json", &secret);
    f.trust("wrong.json", &seed());
    assert!(f.sign(&secret, "m.json").status.success());

    // wrong trusted key, other expected identity, tampered artifact
    std::fs::write(f.p("bad.bin"), b"artifact bytez").unwrap();
    std::fs::write(f.p("short.bin"), b"artifact").unwrap();
    for (trust, id, art) in [
        ("wrong.json", "identity.json", "pkg.tar.gz"),
        ("trust.json", "other.json", "pkg.tar.gz"),
        ("trust.json", "identity.json", "bad.bin"),
        ("trust.json", "identity.json", "short.bin"),
    ] {
        let o = f.verify(trust, id, "m.json", art);
        assert_eq!(o.status.code(), Some(1), "{trust} {id} {art}");
        assert!(!text(&o.stdout).contains("OFFLINE VERIFIED"));
    }

    // truncated and tampered manifest bytes
    let good = std::fs::read(f.p("m.json")).unwrap();
    std::fs::write(f.p("trunc.json"), &good[..good.len() / 2]).unwrap();
    let mut tampered = good.clone();
    let i = tampered.windows(5).position(|w| w == b"1.2.3").unwrap();
    tampered[i + 4] = b'4';
    std::fs::write(f.p("tamper.json"), tampered).unwrap();
    let good_text = std::str::from_utf8(&good).unwrap();
    for (name, replacement) in [
        ("null.json", "\"min_os_version\":null,"),
        ("missing.json", ""),
    ] {
        std::fs::write(
            f.p(name),
            good_text.replace("\"min_os_version\":\"1.0\",", replacement),
        )
        .unwrap();
    }
    for m in ["trunc.json", "tamper.json", "null.json", "missing.json"] {
        let o = f.verify("trust.json", "identity.json", m, "pkg.tar.gz");
        assert_eq!(o.status.code(), Some(1), "{m}");
    }
}

#[test]
fn a_trust_record_embedded_in_the_manifest_is_never_used_and_bad_keys_are_rejected() {
    let f = Fixture::new();
    // Not a trust record at all: a manifest passed as --trusted-key.
    assert!(f.sign(&seed(), "m.json").status.success());
    let o = f.verify("m.json", "identity.json", "m.json", "pkg.tar.gz");
    assert_eq!(o.status.code(), Some(1));

    let o = run(
        &["app", "publisher-trust", "--secret-stdin"],
        Some("hunter2-not-a-key\n"),
    );
    assert_eq!(o.status.code(), Some(1));
    assert!(!text(&o.stderr).contains("hunter2") && !text(&o.stdout).contains("hunter2"));
}

#[test]
fn weak_trusted_public_key_fails_before_signature_verification() {
    use sha2::{Digest, Sha256};

    let f = Fixture::new();
    let mut raw = [0u8; 32];
    raw[0] = 1;
    let mut hash = Sha256::new();
    hash.update(b"kanade/package-publisher-key-id/v1\0");
    hash.update(raw);
    let fingerprint: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
    let trust = serde_json::json!({
        "purpose": "kanade-package-publisher-key-v1",
        "algorithm": "ed25519",
        "key_id": format!("ed25519:{fingerprint}"),
        "public_key": base64::engine::general_purpose::STANDARD.encode(raw),
    });
    std::fs::write(f.p("weak.json"), serde_json::to_vec(&trust).unwrap()).unwrap();
    assert!(f.sign(&seed(), "m.json").status.success());
    let o = f.verify("weak.json", "identity.json", "m.json", "pkg.tar.gz");
    assert_eq!(o.status.code(), Some(1));
    assert!(text(&o.stderr).contains("weak publisher public key"));
    assert!(!text(&o.stdout).contains("OFFLINE VERIFIED"));
}

#[test]
fn clap_argument_errors_exit_two() {
    let o = run(&["app", "verify-artifact"], None);
    assert_eq!(o.status.code(), Some(2));
    let o = run(
        &[
            "app",
            "sign-artifact",
            "--min-os-version",
            "1",
            "--no-min-os-version",
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(2));
}

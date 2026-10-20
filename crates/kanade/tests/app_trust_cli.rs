//! Process-level tests for the managed publisher-trust CLI: real temporary
//! keys, a real temporary trust directory, real exit codes. No backend or NATS
//! is reachable (closed ports), proving the commands are local.

use std::io::Write;
use std::process::{Command, Output, Stdio};

use base64::Engine;

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

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

struct Fx {
    dir: tempfile::TempDir,
    root_secret: String,
}

impl Fx {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
            root_secret: seed(),
        }
    }
    fn p(&self, name: &str) -> String {
        self.dir
            .path()
            .canonicalize()
            .unwrap()
            .join(name)
            .to_string_lossy()
            .into_owned()
    }
    fn store(&self) -> String {
        self.p("store")
    }
    fn root_record(&self) {
        let o = run(
            &["app", "trust-root-record", "--secret-stdin"],
            Some(&format!("{}\n", self.root_secret)),
        );
        assert!(o.status.success(), "{}", text(&o.stderr));
        std::fs::write(self.p("root.json"), o.stdout).unwrap();
    }
    /// Returns the publisher's JSON entry for a draft.
    fn publisher(&self, secret: &str) -> serde_json::Value {
        let o = run(
            &["app", "publisher-trust", "--secret-stdin"],
            Some(&format!("{secret}\n")),
        );
        assert!(o.status.success());
        let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
        serde_json::json!({
            "key_id": v["key_id"], "public_key": v["public_key"],
            "not_before": now() - 3600, "not_after": now() + 86400 * 10
        })
    }
    fn sign_state(
        &self,
        name: &str,
        seq: u64,
        pubs: &[serde_json::Value],
        revoked: &[&str],
    ) -> Output {
        let draft = serde_json::json!({
            "trust_set_id": "fleet", "sequence": seq,
            "issued_at": now(), "not_after": now() + 86400 * 20,
            "publishers": pubs, "revoked": revoked
        });
        std::fs::write(self.p(&format!("{name}.draft")), draft.to_string()).unwrap();
        run(
            &[
                "app",
                "trust-state-sign",
                "--draft",
                &self.p(&format!("{name}.draft")),
                "--out",
                &self.p(name),
                "--secret-stdin",
            ],
            Some(&format!("{}\n", self.root_secret)),
        )
    }
    #[cfg(unix)]
    fn apply(&self, name: &str) -> Output {
        run(
            &[
                "app",
                "trust-store-apply",
                "--dir",
                &self.store(),
                "--state",
                &self.p(name),
            ],
            None,
        )
    }
    #[cfg(unix)]
    fn status(&self) -> String {
        let o = run(&["app", "trust-store-status", "--dir", &self.store()], None);
        assert!(o.status.success());
        text(&o.stdout)
    }
}

#[cfg(not(unix))]
#[test]
fn unsupported_store_provision_fails_without_creating_storage() {
    let fx = Fx::new();
    fx.root_record();
    let output = run(
        &[
            "app",
            "trust-store-provision",
            "--dir",
            &fx.store(),
            "--root-record",
            &fx.p("root.json"),
        ],
        None,
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(text(&output.stderr).contains("unsupported on this platform"));
    assert!(!std::path::Path::new(&fx.store()).exists());
}

#[cfg(unix)]
#[test]
fn provision_apply_status_and_rollback_refusal() {
    let fx = Fx::new();
    let (pa, pb) = (seed(), seed());
    let (ea, eb) = (fx.publisher(&pa), fx.publisher(&pb));

    // Nothing pinned yet: apply must refuse, not bootstrap from the state.
    assert!(
        fx.sign_state("s1", 1, std::slice::from_ref(&ea), &[])
            .status
            .success()
    );
    let o = fx.apply("s1");
    assert_eq!(o.status.code(), Some(1));
    assert!(fx.status().contains("health:  no-root"));

    fx.root_record();
    let prov = |dir: &str| {
        run(
            &[
                "app",
                "trust-store-provision",
                "--dir",
                dir,
                "--root-record",
                &fx.p("root.json"),
            ],
            None,
        )
    };
    let o = prov(&fx.store());
    assert!(o.status.success(), "{}", text(&o.stderr));
    assert!(
        prov(&fx.store()).status.success(),
        "identical re-pin is idempotent"
    );

    // A different root cannot replace the pinned one.
    let other = Fx::new();
    other.root_record();
    let o = run(
        &[
            "app",
            "trust-store-provision",
            "--dir",
            &fx.store(),
            "--root-record",
            &other.p("root.json"),
        ],
        None,
    );
    assert_eq!(o.status.code(), Some(1));

    assert!(fx.apply("s1").status.success());
    let st = fx.status();
    assert!(st.contains("health:  ok"), "{st}");
    assert!(st.contains(ea["key_id"].as_str().unwrap()));

    // Rotate: both active, then revoke the old key.
    assert!(
        fx.sign_state("s2", 2, &[ea.clone(), eb.clone()], &[])
            .status
            .success()
    );
    assert!(fx.apply("s2").status.success());
    let revoked = ea["key_id"].as_str().unwrap().to_owned();
    assert!(
        fx.sign_state("s3", 3, std::slice::from_ref(&eb), &[&revoked])
            .status
            .success()
    );
    assert!(fx.apply("s3").status.success());
    assert!(fx.status().contains(&format!("revoked: {revoked}")));

    // Replaying older states is rejected and changes nothing.
    assert_eq!(fx.apply("s2").status.code(), Some(1));
    assert_eq!(fx.apply("s1").status.code(), Some(1));
    assert!(fx.status().contains("sequence 3"));

    // A state signed by a different root key is not accepted.
    let forger = Fx::new();
    let draft = serde_json::json!({
        "trust_set_id":"fleet","sequence":9,"issued_at":now(),"not_after":now()+86400,
        "publishers":[ea],"revoked":[]
    });
    std::fs::write(fx.p("f.draft"), draft.to_string()).unwrap();
    let o = run(
        &[
            "app",
            "trust-state-sign",
            "--draft",
            &fx.p("f.draft"),
            "--out",
            &fx.p("forged"),
            "--secret-stdin",
        ],
        Some(&format!("{}\n", forger.root_secret)),
    );
    assert!(o.status.success());
    assert_eq!(fx.apply("forged").status.code(), Some(1));
    assert!(fx.status().contains("sequence 3"));

    // No secret ever shows up in any output.
    for s in [&pa, &pb, &fx.root_secret] {
        assert!(!fx.status().contains(s.as_str()));
    }
}

#[test]
fn sign_refuses_overwrite_and_bad_drafts() {
    let fx = Fx::new();
    let e = fx.publisher(&seed());
    assert!(
        fx.sign_state("s1", 1, std::slice::from_ref(&e), &[])
            .status
            .success()
    );
    assert_eq!(
        fx.sign_state("s1", 1, std::slice::from_ref(&e), &[])
            .status
            .code(),
        Some(1)
    );
    // Duplicate publisher and sequence 0 are rejected before any key is read.
    assert_eq!(
        fx.sign_state("d", 1, &[e.clone(), e.clone()], &[])
            .status
            .code(),
        Some(1)
    );
    assert_eq!(fx.sign_state("z", 0, &[e], &[]).status.code(), Some(1));
}

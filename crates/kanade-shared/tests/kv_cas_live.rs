//! Live-broker integration tests for [`kanade_shared::kv_cas`].
//!
//! Ignored by default — like the agent's offline_boot suite, each
//! test spawns a throwaway `nats-server -js` (must be in PATH) on a
//! random port with tempdir storage, so no external broker or auth
//! is needed:
//!
//! ```text
//! cargo test -p kanade-shared --test kv_cas_live -- --ignored
//! ```

use std::process::Stdio;
use std::time::Duration;

use kanade_shared::kv_cas::read_modify_write;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Default, Clone, PartialEq, Eq, Debug)]
struct Members {
    items: Vec<String>,
}

impl Members {
    fn insert(&mut self, s: &str) -> bool {
        if self.items.iter().any(|x| x == s) {
            return false;
        }
        self.items.push(s.to_string());
        self.items.sort();
        true
    }
}

/// Throwaway broker + bucket. The child is `kill_on_drop`, the
/// storage dir a `TempDir` — keep both alive for the test's
/// duration by holding the returned harness.
struct Harness {
    js: async_nats::jetstream::Context,
    _server: tokio::process::Child,
    _storage: tempfile::TempDir,
}

async fn throwaway_bucket(bucket: &str) -> Harness {
    let port = portpicker::pick_unused_port().expect("pick port");
    let storage = tempfile::TempDir::new().expect("storage tempdir");
    let server = tokio::process::Command::new("nats-server")
        .arg("-js")
        .arg("-p")
        .arg(port.to_string())
        .arg("-sd")
        .arg(storage.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("spawn nats-server (is it in PATH?)");

    // Poll-connect until the broker accepts (a couple hundred ms).
    let url = format!("nats://127.0.0.1:{port}");
    let mut client = None;
    for _ in 0..50 {
        match async_nats::connect(&url).await {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
    let js = async_nats::jetstream::new(client.expect("nats-server did not come up in 5s"));
    js.create_key_value(async_nats::jetstream::kv::Config {
        bucket: bucket.to_string(),
        ..Default::default()
    })
    .await
    .expect("create throwaway bucket");
    Harness {
        js,
        _server: server,
        _storage: storage,
    }
}

/// The headline race from #505: N concurrent writers each adding a
/// distinct element to the same key. A blind get→put loses updates;
/// CAS must land all of them.
#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn concurrent_adds_all_survive() {
    let h = throwaway_bucket("race").await;
    let kv = h.js.get_key_value("race").await.unwrap();

    const WRITERS: usize = 20;
    let mut handles = Vec::new();
    for i in 0..WRITERS {
        let kv = kv.clone();
        handles.push(tokio::spawn(async move {
            read_modify_write(&kv, "pc1", |m: &mut Members| m.insert(&format!("g{i:02}")))
                .await
                .expect("rmw");
        }));
    }
    for h in handles {
        h.await.unwrap();
    }

    let bytes = kv.get("pc1").await.unwrap().expect("key exists");
    let m: Members = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        m.items.len(),
        WRITERS,
        "every concurrent add must survive; got {:?}",
        m.items
    );
}

/// `mutate` returning false must not write — the key stays absent
/// and an existing key's revision doesn't bump.
#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn noop_skips_the_write() {
    let h = throwaway_bucket("noop").await;
    let kv = h.js.get_key_value("noop").await.unwrap();

    // Absent key + no-op mutate → still absent.
    let v = read_modify_write(&kv, "k", |_: &mut Members| false)
        .await
        .unwrap();
    assert_eq!(v, Members::default());
    assert!(
        kv.get("k").await.unwrap().is_none(),
        "no-op must not create"
    );

    // Existing key + no-op mutate → revision unchanged.
    read_modify_write(&kv, "k", |m: &mut Members| m.insert("a"))
        .await
        .unwrap();
    let rev_before = kv.entry("k").await.unwrap().unwrap().revision;
    read_modify_write(&kv, "k", |m: &mut Members| m.insert("a"))
        .await
        .unwrap();
    let rev_after = kv.entry("k").await.unwrap().unwrap().revision;
    assert_eq!(rev_before, rev_after, "no-op must not bump the revision");
}

/// A deleted key reads as default but its delete marker still guards
/// the CAS — the write must succeed (update over the marker) and the
/// value must contain only the new element.
#[tokio::test]
#[ignore = "requires nats-server in PATH; cargo test -- --ignored"]
async fn write_over_delete_marker_starts_from_default() {
    let h = throwaway_bucket("del").await;
    let kv = h.js.get_key_value("del").await.unwrap();

    read_modify_write(&kv, "k", |m: &mut Members| m.insert("old"))
        .await
        .unwrap();
    kv.delete("k").await.unwrap();

    let v = read_modify_write(&kv, "k", |m: &mut Members| m.insert("new"))
        .await
        .unwrap();
    assert_eq!(v.items, vec!["new"], "delete wipes prior state");
    let bytes = kv.get("k").await.unwrap().expect("rewritten");
    let m: Members = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(m.items, vec!["new"]);
}

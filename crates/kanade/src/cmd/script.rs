//! `kanade script` — manage the manifest-script Object Store
//! (`OBJECT_SCRIPTS`, #211).
//!
//! Sibling of `kanade app` — same backend HTTP API shape
//! (`/api/script-objects`, authenticated with `KANADE_AUTH_TOKEN`,
//! audited backend-side), different bucket. This one holds the PowerShell / shell / etc. bodies
//! that manifests reference via `execute.script_object` (#213
//! schema, #214 agent fetch). Bodies are bounded at 4 MB
//! (vs `app_packages`'s 256 MB) — scripts are KB-to-MB text, not
//! installer binaries.
//!
//! Object key shape is `<name>/<version>` — same as `app`. For
//! manifest-driven scripts `<name>` is conventionally the manifest
//! id and `<version>` matches the manifest version, but the bucket
//! imposes no policy (operator-uploaded ad-hoc scripts can use any
//! pair they like).

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use kanade_shared::kv::OBJECT_SCRIPTS;
use tracing::info;

use super::object_http::{self, Bucket};
use super::validate_segment;

/// The backend's body limit for `POST /api/script-objects/...`. It applies
/// to the whole multipart body, so a file just under this can still be
/// refused (boundary overhead) — only a file over it is rejected locally.
const MAX_SCRIPT_BYTES: u64 = 4 * 1024 * 1024;

const BUCKET: Bucket = Bucket {
    route: "api/script-objects",
    store: OBJECT_SCRIPTS,
    empty_msg: "(no script objects)",
    limit_note: "script bodies are limited to 4 MB for the whole upload request \
                 (multipart framing included)",
};

#[derive(Args, Debug)]
pub struct ScriptArgs {
    #[command(subcommand)]
    pub sub: ScriptSub,
}

#[derive(Subcommand, Debug)]
pub enum ScriptSub {
    /// Upload a script body to the scripts Object Store under
    /// `<name>/<version>`. Use this for bodies referenced by a
    /// manifest's `execute.script_object` field — agents fetch +
    /// sha-verify at exec time (#214). Goes through the backend API
    /// (needs KANADE_AUTH_TOKEN with the operator role; no NATS access).
    Publish {
        /// Script "name" — typically the referencing manifest's id.
        name: String,
        /// Script "version" — typically the referencing manifest's
        /// version. Operator picks any scheme; the bucket just stores
        /// the pair as the key.
        version: String,
        /// Path to the script file (.ps1 / .sh / .py / …).
        file: PathBuf,
    },
    /// List every `<name>/<version>` row in the bucket — size +
    /// digest + last-modified. Goes through the backend API.
    List,
    /// Delete a single script version via the backend API. No-op + clear
    /// message when the key isn't present.
    Delete { name: String, version: String },
}

pub async fn execute(backend_url: &str, args: ScriptArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        ScriptSub::Publish {
            name,
            version,
            file,
        } => publish(base, name, version, file).await,
        ScriptSub::List => {
            object_http::list(&crate::http_client::authed_client()?, base, &BUCKET).await
        }
        ScriptSub::Delete { name, version } => {
            validate_segment("name", &name)?;
            validate_segment("version", &version)?;
            let client = crate::http_client::authed_client()?;
            object_http::delete(&client, base, &BUCKET, &name, &version).await
        }
    }
}

async fn publish(base: &str, name: String, version: String, file: PathBuf) -> Result<()> {
    validate_segment("name", &name)?;
    validate_segment("version", &version)?;

    let len = tokio::fs::metadata(&file)
        .await
        .with_context(|| format!("stat {file:?}"))?
        .len();
    if len > MAX_SCRIPT_BYTES {
        bail!(
            "{file:?} is {len} bytes, over the {MAX_SCRIPT_BYTES}-byte (4 MB) script object limit"
        );
    }

    info!(name, version, "uploading script object");
    let client = crate::http_client::authed_client()?;
    let published = object_http::upload(&client, base, &BUCKET, &name, &version, &file).await?;
    let key = format!("{name}/{version}");
    info!(name, version, size = published.size, digest = ?published.digest, "script object uploaded");

    object_http::print_published(&BUCKET, &key, &published);
    println!();
    println!("Reference from a manifest with:");
    println!("  execute:");
    println!("    shell: powershell");
    println!("    script_object: {key}");
    println!("    timeout: 600s");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{TEST_TOKEN, export_test_token, fake_backend, seen};

    fn pubargs(path: &std::path::Path) -> ScriptArgs {
        ScriptArgs {
            sub: ScriptSub::Publish {
                name: "inv".into(),
                version: "2".into(),
                file: path.to_path_buf(),
            },
        }
    }

    fn file_with(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("run.ps1");
        std::fs::write(&p, bytes).unwrap();
        (dir, p)
    }

    #[tokio::test]
    async fn publish_posts_the_file_as_a_multipart_field() {
        export_test_token();
        let body = b"Write-Host '\xe3\x81\x82'\r\n\xff\xfe";
        let (base, log) = fake_backend(vec![(
            200,
            r#"{"name":"inv","version":"2","size":20,"digest":"SHA-256=x"}"#,
        )])
        .await;
        let (_dir, path) = file_with(body);
        execute(&format!("{base}/"), pubargs(&path)).await.unwrap();
        let got = seen(&log);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].target, "/api/script-objects/inv/2");
        assert!(
            got[0]
                .headers
                .contains(&format!("authorization: bearer {TEST_TOKEN}"))
        );
        assert!(got[0].headers.contains("x-kanade-source: cli"));
        assert!(got[0].headers.contains("content-type: multipart/form-data"));
        assert!(!got[0].headers.contains("transfer-encoding"));
        assert!(
            String::from_utf8_lossy(&got[0].raw).contains(r#"name="file""#),
            "{}",
            got[0].body
        );
        assert!(got[0].raw.windows(body.len()).any(|w| w == body));
    }

    #[tokio::test]
    async fn publish_rejections_carry_status_body_and_hints() {
        export_test_token();
        for (code, body, hint) in [
            (413, "too large", "4 MB"),
            (403, "operator role required", "KANADE_AUTH_TOKEN"),
            (500, "boom", ""),
        ] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let (_dir, path) = file_with(b"x");
            let err = format!("{:#}", execute(&base, pubargs(&path)).await.unwrap_err());
            assert!(err.contains("publish failed"), "{err}");
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(body), "{err}");
            assert!(err.contains(hint), "{err}");
        }
    }

    #[tokio::test]
    async fn publish_with_an_unparseable_reply_errors() {
        let (base, _log) = fake_backend(vec![(200, "<html>")]).await;
        let (_dir, path) = file_with(b"x");
        assert!(execute(&base, pubargs(&path)).await.is_err());
    }

    #[tokio::test]
    async fn a_file_over_the_limit_is_refused_locally() {
        let (base, log) = fake_backend(vec![]).await;
        let (_dir, path) = file_with(b"");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SCRIPT_BYTES + 1)
            .unwrap();
        let err = execute(&base, pubargs(&path))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("4 MB"), "{err}");
        assert!(seen(&log).is_empty());
    }

    #[tokio::test]
    async fn a_file_at_the_limit_is_left_to_the_backend() {
        let (base, log) = fake_backend(vec![(413, "limit")]).await;
        let (_dir, path) = file_with(b"");
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_SCRIPT_BYTES)
            .unwrap();
        let err = format!("{:#}", execute(&base, pubargs(&path)).await.unwrap_err());
        assert!(err.contains("413"), "{err}");
        assert_eq!(seen(&log).len(), 1);
    }

    #[tokio::test]
    async fn invalid_segments_are_rejected_before_any_request() {
        let (base, log) = fake_backend(vec![]).await;
        let (_dir, path) = file_with(b"x");
        for (name, version) in [("a/b", "1"), ("a", ""), ("..", "1")] {
            let args = ScriptArgs {
                sub: ScriptSub::Publish {
                    name: name.into(),
                    version: version.into(),
                    file: path.clone(),
                },
            };
            assert!(execute(&base, args).await.is_err(), "{name} {version}");
        }
        assert!(seen(&log).is_empty());
    }

    #[tokio::test]
    async fn list_gets_the_collection_with_auth() {
        export_test_token();
        let (base, log) = fake_backend(vec![(200, "[]")]).await;
        execute(
            &base,
            ScriptArgs {
                sub: ScriptSub::List,
            },
        )
        .await
        .unwrap();
        let got = seen(&log);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].target, "/api/script-objects");
        assert!(got[0].headers.contains("authorization: bearer"));
        assert!(got[0].headers.contains("x-kanade-source: cli"));
    }

    #[tokio::test]
    async fn list_errors_are_reported() {
        for (code, body) in [(403, "nope"), (500, "boom"), (200, "{")] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            assert!(
                execute(
                    &base,
                    ScriptArgs {
                        sub: ScriptSub::List
                    }
                )
                .await
                .is_err(),
                "{code}"
            );
        }
    }

    fn delargs() -> ScriptArgs {
        ScriptArgs {
            sub: ScriptSub::Delete {
                name: "inv".into(),
                version: "2".into(),
            },
        }
    }

    #[tokio::test]
    async fn delete_sends_delete_and_treats_404_as_a_no_op() {
        export_test_token();
        for code in [204, 404] {
            let (base, log) = fake_backend(vec![(code, "")]).await;
            execute(&base, delargs()).await.unwrap();
            let got = seen(&log);
            assert_eq!(got[0].method, "DELETE");
            assert_eq!(got[0].target, "/api/script-objects/inv/2");
            assert!(got[0].headers.contains("authorization: bearer"));
            assert!(got[0].headers.contains("x-kanade-source: cli"));
        }
    }

    #[tokio::test]
    async fn delete_rejections_are_errors() {
        for (code, body) in [(403, "operator role required"), (500, "boom")] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let err = format!("{:#}", execute(&base, delargs()).await.unwrap_err());
            assert!(err.contains("delete failed"), "{err}");
            assert!(err.contains(body), "{err}");
        }
    }
}

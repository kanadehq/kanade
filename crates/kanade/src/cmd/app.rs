//! `kanade app` — manage the generic app-package Object Store
//! (`OBJECT_APP_PACKAGES`, #207).
//!
//! Talks to the backend's `/api/app-packages` endpoints (authenticated with
//! `KANADE_AUTH_TOKEN`, role-checked and audited there) instead of writing
//! the object store over NATS. `agent publish` covers the agent's own
//! self-update binary, this covers everything else operators install on
//! endpoints — kanade-client, kanade-backend, Webex / Teams / vendor MSIs,
//! etc.
//!
//! See `kanade-shared::kv::OBJECT_APP_PACKAGES` for the bucket-
//! level design notes. Object key shape is `<name>/<version>`;
//! operator picks `<name>` once per package family and `<version>`
//! per release.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Subcommand};
use kanade_shared::kv::OBJECT_APP_PACKAGES;
use tracing::info;

use super::object_http::{self, Bucket};
use super::validate_segment;

const BUCKET: Bucket = Bucket {
    route: "api/app-packages",
    store: OBJECT_APP_PACKAGES,
    empty_msg: "(no app packages)",
    limit_note: "the backend refused the upload size; app packages are capped at the bucket's \
                 256 MB ceiling",
};

#[derive(Args, Debug)]
pub struct AppArgs {
    #[command(subcommand)]
    pub sub: AppSub,
}

#[derive(Subcommand, Debug)]
pub enum AppSub {
    /// Upload a binary / installer to the app_packages Object Store
    /// under `<name>/<version>`, through the backend API (needs
    /// KANADE_AUTH_TOKEN with the operator role; no NATS access).
    ///
    /// Operators pick `<name>` once per package family
    /// (e.g. `kanade-client`, `kanade-backend`, `webex-meetings`).
    /// `<version>` defaults to the binary's embedded VERSIONINFO
    /// (same pelite extraction as `kanade agent publish`) — pass
    /// `--version` to override (vendor MSIs / non-PE binaries need
    /// the explicit label).
    Publish {
        /// Package family name. Slash-free, ASCII-printable; see
        /// `kanade-backend::api::app_packages::validate_segment`
        /// for the full set of restrictions the HTTP side enforces.
        name: String,
        /// Path to the binary to upload.
        binary: PathBuf,
        /// Version label. When omitted, extracted from the binary's
        /// embedded VERSIONINFO (Windows PE built with `winres` —
        /// every kanade-* binary qualifies). Required for binaries
        /// without VERSIONINFO (most vendor installers) — fails fast
        /// rather than silently uploading under an empty version.
        #[arg(long)]
        version: Option<String>,
    },
    /// List every `<name>/<version>` row in the bucket — size +
    /// digest + last-modified. Goes through the backend API.
    List,
    /// Delete a single package version via the backend API. No-op + clear message when
    /// the key isn't present (idempotent re-runs are fine).
    Delete { name: String, version: String },
}

pub async fn execute(backend_url: &str, args: AppArgs) -> Result<()> {
    let base = backend_url.trim_end_matches('/');
    match args.sub {
        AppSub::Publish {
            name,
            binary,
            version,
        } => publish(base, name, binary, version).await,
        AppSub::List => {
            object_http::list(&crate::http_client::authed_client()?, base, &BUCKET).await
        }
        AppSub::Delete { name, version } => {
            validate_segment("name", &name)?;
            validate_segment("version", &version)?;
            let client = crate::http_client::authed_client()?;
            object_http::delete(&client, base, &BUCKET, &name, &version).await
        }
    }
}

async fn publish(base: &str, name: String, binary: PathBuf, version: Option<String>) -> Result<()> {
    validate_segment("name", &name)?;

    // #261: default version to the binary's embedded VERSIONINFO —
    // same pelite extractor that `kanade agent publish` uses. Avoids
    // the operator typing the version twice (once in the build, once
    // here) for kanade-* binaries. Explicit `--version` overrides for
    // non-PE inputs (MSIs, scripts) where extraction returns None.
    //
    // The slurp below buffers the whole binary in RAM (pelite needs a
    // contiguous `&[u8]`, and we read the full file rather than just
    // the PE header for the same reason `kanade agent publish` does).
    // The streaming upload path below benefits separately — it doesn't
    // help here, so the RSS spike during extraction is real.
    //
    // In practice this is bounded: every binary that actually carries
    // a VERSIONINFO (i.e. takes the slurp path at all) is one of the
    // `winres`-built kanade-* binaries — those are tens of MB. Vendor
    // MSIs that hit the 256 MB ceiling never reach the slurp branch
    // because extraction would return None — the operator MUST pass
    // `--version` for them, and the explicit-flag path skips the read.
    // If a future package family lands that's both large AND
    // VERSIONINFO-tagged, switch this to memmap2 (Gemini #263 MED).
    let resolved_version = match version {
        Some(v) => v,
        None => {
            let bytes = tokio::fs::read(&binary)
                .await
                .with_context(|| format!("read {binary:?}"))?;
            match kanade_shared::exe_version::extract_pe_version(&bytes) {
                Some(v) => v,
                // #270: extraction failed. Interactive shell → prompt for
                // the label inline; pipe / CI → fail fast with the same
                // guidance the original path emitted.
                None => match super::prompt_version_if_interactive(binary.clone()).await? {
                    Some(v) => v,
                    None => bail!(
                        "no --version given and couldn't extract VERSIONINFO from {binary:?} \
                         (Windows PE built with `winres`? otherwise pass `--version <label>`)"
                    ),
                },
            }
        }
    };
    validate_segment("version", &resolved_version)?;
    let version = resolved_version;

    // The upload streams from disk (see `object_http::upload`): app packages
    // can reach the bucket's 256 MB ceiling, so the whole binary is never
    // buffered for the request.
    info!(name, version, "uploading app package");
    let client = crate::http_client::authed_client()?;
    let published = object_http::upload(&client, base, &BUCKET, &name, &version, &binary).await?;
    let key = format!("{name}/{version}");
    info!(name, version, size = published.size, digest = ?published.digest, "app package uploaded");

    // #277: a downstream `store.get(key)` (e.g. backend serving
    // /api/app-packages/...) can read stale / partial bytes for ~30-
    // 180 s after `put` returns on at least single-node JetStream, and the
    // backend's publish does not read back. Block on a download hash check
    // here — through the same endpoint agents fetch from — so the operator
    // only sees "published" once the bytes are actually consumable; without
    // it, a `kanade exec install-kanade-backend --pcs ...` fired immediately
    // after publish 50/50 fails with a sha-mismatch on the agent side.
    let url = object_http::object_url(base, BUCKET.route, &name, &version)?;
    super::publish_verify::verify_http_readback(
        &client,
        &url,
        &key,
        published.digest.as_deref(),
        published.size,
    )
    .await
    .context("publish read-back verify")?;

    object_http::print_published(&BUCKET, &key, &published);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{TEST_TOKEN, export_test_token, fake_backend, fake_backend_bytes, seen};
    use base64::Engine;
    use sha2::{Digest, Sha256};

    fn digest_of(bytes: &[u8]) -> String {
        let b64 = base64::engine::general_purpose::URL_SAFE.encode(Sha256::digest(bytes));
        format!("SHA-256={b64}")
    }

    fn payload() -> Vec<u8> {
        // Non-UTF-8, larger than one read chunk.
        (0..=255u8).cycle().take(200_000).collect()
    }

    fn published_json(size: usize, digest: Option<&str>) -> Vec<u8> {
        serde_json::json!({"name":"webex","version":"1.0","size":size,"digest":digest})
            .to_string()
            .into_bytes()
    }

    fn pubargs(path: &std::path::Path) -> AppArgs {
        AppArgs {
            sub: AppSub::Publish {
                name: "webex".into(),
                binary: path.to_path_buf(),
                version: Some("1.0".into()),
            },
        }
    }

    fn file_with(bytes: &[u8]) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("installer.msi");
        std::fs::write(&p, bytes).unwrap();
        (dir, p)
    }

    #[tokio::test]
    async fn publish_streams_a_multipart_file_field_and_reads_it_back() {
        export_test_token();
        let data = payload();
        let d = digest_of(&data);
        let (base, log) = fake_backend_bytes(vec![
            (200, published_json(data.len(), Some(&d))),
            (200, data.clone()),
        ])
        .await;
        let (_dir, path) = file_with(&data);
        execute(&format!("{base}/"), pubargs(&path)).await.unwrap();

        let got = seen(&log);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].method, "POST");
        assert_eq!(got[0].target, "/api/app-packages/webex/1.0");
        assert!(
            got[0]
                .headers
                .contains(&format!("authorization: bearer {TEST_TOKEN}")),
            "{}",
            got[0].headers
        );
        assert!(got[0].headers.contains("x-kanade-source: cli"));
        assert!(got[0].headers.contains("content-type: multipart/form-data"));
        assert!(
            !got[0].headers.contains("transfer-encoding"),
            "length must be declared, not chunked"
        );
        let head = String::from_utf8_lossy(&got[0].raw[..300.min(got[0].raw.len())]).to_string();
        assert!(head.contains(r#"name="file""#), "{head}");
        let needle = &data[..];
        assert!(
            got[0].raw.windows(needle.len()).any(|w| w == needle),
            "exact file bytes must be in the multipart body"
        );

        assert_eq!(got[1].method, "GET");
        assert_eq!(got[1].target, "/api/app-packages/webex/1.0");
        assert!(got[1].headers.contains("authorization: bearer"));
    }

    #[tokio::test]
    async fn publish_without_a_reported_digest_checks_size_only() {
        export_test_token();
        let data = payload();
        let (base, _log) = fake_backend_bytes(vec![
            (200, published_json(data.len(), None)),
            (200, data.clone()),
        ])
        .await;
        let (_dir, path) = file_with(&data);
        execute(&base, pubargs(&path)).await.unwrap();
    }

    #[tokio::test]
    async fn publish_retries_a_stale_readback() {
        export_test_token();
        let data = payload();
        let d = digest_of(&data);
        let (base, log) = fake_backend_bytes(vec![
            (200, published_json(data.len(), Some(&d))),
            (200, data[..10].to_vec()),
            (404, b"not yet".to_vec()),
            (200, data.clone()),
        ])
        .await;
        let (_dir, path) = file_with(&data);
        execute(&base, pubargs(&path)).await.unwrap();
        assert_eq!(seen(&log).len(), 4);
    }

    #[tokio::test]
    async fn publish_fails_when_the_readback_never_matches() {
        export_test_token();
        let data = payload();
        let d = digest_of(&data);
        let mut replies = vec![(200, published_json(data.len(), Some(&d)))];
        replies.extend((0..5).map(|_| (200, vec![0u8; data.len()])));
        let (base, _log) = fake_backend_bytes(replies).await;
        let (_dir, path) = file_with(&data);
        let err = format!("{:#}", execute(&base, pubargs(&path)).await.unwrap_err());
        assert!(err.contains("read-back"), "{err}");
    }

    #[tokio::test]
    async fn publish_rejections_carry_status_body_and_hints() {
        export_test_token();
        for (code, body, hint) in [
            (413, "too big", "256 MB"),
            (403, "operator role required", "KANADE_AUTH_TOKEN"),
            (401, "no token", "KANADE_AUTH_TOKEN"),
            (400, "'file' field is empty", ""),
            (500, "boom", ""),
        ] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            let (_dir, path) = file_with(b"abc");
            let err = format!("{:#}", execute(&base, pubargs(&path)).await.unwrap_err());
            assert!(err.contains("publish failed"), "{err}");
            assert!(err.contains(&code.to_string()), "{err}");
            assert!(err.contains(body), "{err}");
            assert!(err.contains(hint), "{err}");
        }
    }

    #[tokio::test]
    async fn publish_with_an_unparseable_reply_errors() {
        export_test_token();
        let (base, _log) = fake_backend(vec![(200, "not json")]).await;
        let (_dir, path) = file_with(b"abc");
        assert!(execute(&base, pubargs(&path)).await.is_err());
    }

    #[tokio::test]
    async fn publish_validates_name_and_version_before_any_request() {
        let (base, log) = fake_backend(vec![]).await;
        let (_dir, path) = file_with(b"abc");
        for (name, version) in [("bad/name", "1.0"), ("webex", "a/b"), ("webex", "..")] {
            let args = AppArgs {
                sub: AppSub::Publish {
                    name: name.into(),
                    binary: path.clone(),
                    version: Some(version.into()),
                },
            };
            assert!(execute(&base, args).await.is_err(), "{name} {version}");
        }
        assert!(seen(&log).is_empty());
    }

    #[tokio::test]
    async fn publish_of_a_missing_file_errors_without_a_request() {
        let (base, log) = fake_backend(vec![]).await;
        let err = execute(&base, pubargs(std::path::Path::new("/nonexistent/x.msi")))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("open"), "{err:#}");
        assert!(seen(&log).is_empty());
    }

    #[tokio::test]
    async fn list_gets_the_collection_with_auth() {
        export_test_token();
        let (base, log) = fake_backend(vec![(
            200,
            r#"[{"name":"b","version":"1","size":3,"digest":null,"modified":null}]"#,
        )])
        .await;
        execute(&base, AppArgs { sub: AppSub::List }).await.unwrap();
        let got = seen(&log);
        assert_eq!(got[0].method, "GET");
        assert_eq!(got[0].target, "/api/app-packages");
        assert!(got[0].headers.contains("authorization: bearer"));
        assert!(got[0].headers.contains("x-kanade-source: cli"));
    }

    #[tokio::test]
    async fn list_errors_report_status_and_body() {
        for (code, body) in [(403, "nope"), (500, "boom"), (200, "{")] {
            let (base, _log) = fake_backend(vec![(code, body)]).await;
            assert!(
                execute(&base, AppArgs { sub: AppSub::List }).await.is_err(),
                "{code}"
            );
        }
    }

    fn delargs() -> AppArgs {
        AppArgs {
            sub: AppSub::Delete {
                name: "webex".into(),
                version: "1.0".into(),
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
            assert_eq!(got[0].target, "/api/app-packages/webex/1.0");
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

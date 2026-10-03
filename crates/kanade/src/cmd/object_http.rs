//! HTTP client side of `kanade app` / `kanade script`.
//!
//! Both subcommands drive the same backend surface (`/api/app-packages` and
//! `/api/script-objects`: list, multipart publish, delete) and differ only in
//! route, bucket label and size ceiling, so the request building, the
//! streaming upload and the error mapping live here once.
//!
//! Going through the backend (rather than the object store over NATS) means
//! the publish and delete are authenticated, role-checked and audited
//! against the operator's account there; the CLI writes no audit record of
//! its own.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use reqwest::multipart::{Form, Part};
use reqwest::{Body, Client, StatusCode, Url};
use serde::Deserialize;
use tokio_util::io::ReaderStream;

/// One object-store family served by the backend.
pub struct Bucket {
    /// Backend route prefix, without the leading slash.
    pub route: &'static str,
    /// Object store name, for the `published:` output.
    pub store: &'static str,
    /// What the `list` prints for an empty bucket.
    pub empty_msg: &'static str,
    /// Human description of the backend's upload ceiling, used to explain a
    /// 413.
    pub limit_note: &'static str,
}

/// What the backend reports back for a successful publish.
#[derive(Debug, Deserialize)]
pub struct Published {
    pub size: u64,
    pub digest: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Row {
    name: String,
    version: String,
    size: u64,
    digest: Option<String>,
    modified: Option<String>,
}

/// Build `<base>/<route>/<name>/<version>` with `name` and `version`
/// percent-encoded as single path segments. The names the CLI accepts may
/// carry URL-reserved characters (`?`, `#`, `%`, spaces), which plain string
/// concatenation would turn into a query / fragment or a different key.
/// `.` and `..` are rejected: URL normalisation would resolve them away and
/// silently address another object.
pub fn object_url(base: &str, route: &str, name: &str, version: &str) -> Result<Url> {
    for (label, value) in [("name", name), ("version", version)] {
        if value == "." || value == ".." {
            bail!(
                "{label} must not be '.' or '..' (it would be normalised out of the request path)"
            );
        }
    }
    let mut url = Url::parse(base).with_context(|| format!("invalid backend URL {base:?}"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("backend URL {base:?} cannot carry a path"))?
        .pop_if_empty()
        .extend(route.split('/'))
        .push(name)
        .push(version);
    Ok(url)
}

pub(super) fn collection_url(base: &str, route: &str) -> Result<Url> {
    let mut url = Url::parse(base).with_context(|| format!("invalid backend URL {base:?}"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow!("backend URL {base:?} cannot carry a path"))?
        .pop_if_empty()
        .extend(route.split('/'));
    Ok(url)
}

/// Turn a non-success response into the operator-facing error. The backend's
/// own reason is always kept; 401 / 403 and 413 get a hint because those are
/// the two rejections an operator can act on.
pub(super) async fn rejected(
    op: &str,
    resp: reqwest::Response,
    limit_note: Option<&str>,
) -> anyhow::Error {
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    let mut msg = format!("{op} failed: {status} — {body}");
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => msg.push_str(
            "\nhint: this command goes through the backend API — set KANADE_AUTH_TOKEN \
             (`kanade login`) to an account with the operator role",
        ),
        StatusCode::PAYLOAD_TOO_LARGE => {
            if let Some(note) = limit_note {
                msg.push_str(&format!("\nhint: {note}"));
            }
        }
        _ => {}
    }
    anyhow!(msg)
}

/// Open `file` as a multipart part that is read from disk as it is sent, with
/// its length declared up front so the request carries a `Content-Length`
/// rather than being chunked.
pub(super) async fn file_part(file: &Path) -> Result<Part> {
    let handle = tokio::fs::File::open(file)
        .await
        .with_context(|| format!("open {file:?}"))?;
    let len = handle
        .metadata()
        .await
        .with_context(|| format!("stat {file:?}"))?
        .len();
    Part::stream_with_length(Body::wrap_stream(ReaderStream::new(handle)), len)
        .file_name(
            file.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".into()),
        )
        .mime_str("application/octet-stream")
        .context("build multipart part")
}

/// Stream `file` to `POST <route>/<name>/<version>` as the multipart `file`
/// field. The file is read from disk as it is sent and its length is declared
/// up front, so a package near the bucket ceiling never sits in memory and
/// the request carries a `Content-Length` rather than being chunked.
pub async fn upload(
    client: &Client,
    base: &str,
    bucket: &Bucket,
    name: &str,
    version: &str,
    file: &Path,
) -> Result<Published> {
    let url = object_url(base, bucket.route, name, version)?;
    let part = file_part(file).await?;
    let resp = client
        .post(url.clone())
        .multipart(Form::new().part("file", part))
        .send()
        .await
        .with_context(|| {
            format!(
                "POST {url} (if the backend closed the connection mid-upload it rejected the \
                 body early — check the size limit and KANADE_AUTH_TOKEN)"
            )
        })?;
    if !resp.status().is_success() {
        return Err(rejected("publish", resp, Some(bucket.limit_note)).await);
    }
    resp.json()
        .await
        .context("parse publish response from server")
}

/// `GET <route>` and print one `<key>\t<size>\t<modified>\t<digest>` line per
/// object, sorted by key (the backend returns newest first).
pub async fn list(client: &Client, base: &str, bucket: &Bucket) -> Result<()> {
    let url = collection_url(base, bucket.route)?;
    let resp = client
        .get(url.clone())
        .send()
        .await
        .with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        return Err(rejected("list", resp, None).await);
    }
    let rows: Vec<Row> = resp
        .json()
        .await
        .context("parse list response from server")?;
    for line in render_rows(rows, bucket.empty_msg) {
        println!("{line}");
    }
    Ok(())
}

/// Rebuild each `<name>/<version>` key and order by it; missing digest /
/// timestamp print as `—`.
fn render_rows(rows: Vec<Row>, empty_msg: &str) -> Vec<String> {
    let mut rows: Vec<(String, Row)> = rows
        .into_iter()
        .map(|r| (format!("{}/{}", r.name, r.version), r))
        .collect();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if rows.is_empty() {
        return vec![empty_msg.to_string()];
    }
    rows.into_iter()
        .map(|(key, row)| {
            let dgst = row.digest.as_deref().unwrap_or("—");
            let modt = row.modified.as_deref().unwrap_or("—");
            format!("{key}\t{}\t{modt}\t{dgst}", row.size)
        })
        .collect()
}

/// `DELETE <route>/<name>/<version>`. 404 is the idempotent no-op; every
/// other rejection (403, 500, …) is an error.
pub async fn delete(
    client: &Client,
    base: &str,
    bucket: &Bucket,
    name: &str,
    version: &str,
) -> Result<()> {
    let url = object_url(base, bucket.route, name, version)?;
    let key = format!("{name}/{version}");
    let resp = client
        .delete(url.clone())
        .send()
        .await
        .with_context(|| format!("DELETE {url}"))?;
    match resp.status() {
        s if s.is_success() => println!("deleted: {key}"),
        StatusCode::NOT_FOUND => println!("not present: {key} (idempotent no-op)"),
        _ => return Err(rejected("delete", resp, None).await),
    }
    Ok(())
}

/// Print the `published:` block shared by both publish commands.
pub fn print_published(bucket: &Bucket, key: &str, published: &Published) {
    println!("published: {key}");
    println!("  object_store : {}/{key}", bucket.store);
    println!("  size         : {} bytes", published.size);
    if let Some(d) = published.digest.as_deref() {
        println!("  digest       : {d}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{fake_backend, seen};

    fn row(name: &str, version: &str, digest: Option<&str>) -> Row {
        Row {
            name: name.into(),
            version: version.into(),
            size: 7,
            digest: digest.map(String::from),
            modified: None,
        }
    }

    #[test]
    fn list_is_sorted_by_key_not_by_backend_order() {
        let lines = render_rows(
            vec![
                row("b", "1", Some("SHA-256=x")),
                row("a", "2", None),
                row("a", "10", None),
            ],
            "(none)",
        );
        assert_eq!(
            lines,
            vec![
                "a/10\t7\t—\t—".to_string(),
                "a/2\t7\t—\t—".to_string(),
                "b/1\t7\t—\tSHA-256=x".to_string(),
            ]
        );
    }

    #[test]
    fn empty_list_prints_the_bucket_message() {
        assert_eq!(
            render_rows(vec![], "(no app packages)"),
            ["(no app packages)"]
        );
    }

    #[test]
    fn url_encodes_each_segment_and_keeps_a_base_path_prefix() {
        let u = object_url("http://h:1/prefix", "api/app-packages", "a b?#%x", "1.0").unwrap();
        assert_eq!(
            u.as_str(),
            "http://h:1/prefix/api/app-packages/a%20b%3F%23%25x/1.0"
        );
        assert_eq!(u.query(), None);
        assert_eq!(u.fragment(), None);
    }

    #[test]
    fn dot_segments_are_rejected_rather_than_normalised() {
        for (n, v) in [(".", "1"), ("..", "1"), ("a", "."), ("a", "..")] {
            assert!(object_url("http://h", "api/app-packages", n, v).is_err());
        }
        // Only the exact dot segments are special.
        assert!(object_url("http://h", "api/app-packages", "a..", "...").is_ok());
    }

    #[tokio::test]
    async fn encoded_name_reaches_the_backend_as_one_segment() {
        let (base, log) = fake_backend(vec![(204, "")]).await;
        let bucket = Bucket {
            route: "api/app-packages",
            store: "S",
            empty_msg: "",
            limit_note: "",
        };
        let client = crate::http_client::authed_client().unwrap();
        delete(&client, &base, &bucket, "we b?x", "v#1")
            .await
            .unwrap();
        let got = seen(&log);
        assert_eq!(got[0].target, "/api/app-packages/we%20b%3Fx/v%231");
    }
}

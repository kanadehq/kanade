//! Integrity-only immutable app artifacts. Object Store wire compatibility is
//! isolated here: one conditional ObjectInfo publication commits the bytes and
//! identity together. No signature, alias, or deployment admission is implied.
use async_nats::jetstream::{
    Context,
    object_store::{ObjectInfo, ObjectStore},
};
use axum::{
    Json,
    body::Body,
    extract::{Multipart, Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine, engine::general_purpose::URL_SAFE};
use futures::TryStreamExt;
use kanade_shared::{
    artifact::{ArtifactIdentity, ArtifactIntegrity, ArtifactSha256},
    kv::OBJECT_APP_PACKAGES,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::io::StreamReader;

use super::AppState;
use crate::audit::{self, Caller};

type Error = (StatusCode, String);
const RECORD: &str = "kanade.artifact.v1";
const ENABLE: &str = "KANADE_APP_ARTIFACT_PUBLICATION";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Publication {
    pub identity: ArtifactIdentity,
    pub integrity: ArtifactIntegrity,
}

fn internal(e: impl std::fmt::Display) -> Error {
    (StatusCode::SERVICE_UNAVAILABLE, e.to_string())
}
fn invalid(e: impl std::fmt::Display) -> Error {
    (StatusCode::BAD_REQUEST, e.to_string())
}
fn subject(key: &str) -> String {
    format!("$O.{OBJECT_APP_PACKAGES}.M.{}", URL_SAFE.encode(key))
}
fn record(info: &ObjectInfo) -> Result<Publication, Error> {
    let publication: Publication = serde_json::from_str(
        info.metadata
            .get(RECORD)
            .ok_or_else(|| internal("artifact record missing"))?,
    )
    .map_err(internal)?;
    if info.deleted
        || info.bucket != OBJECT_APP_PACKAGES
        || publication.identity.app_package_key().map_err(internal)? != info.name
        || info.size as u64 != publication.integrity.size_bytes
        || info.digest.as_deref()
            != Some(
                publication
                    .integrity
                    .sha256
                    .to_object_store_digest()
                    .as_str(),
            )
        || info.options.as_ref().is_some_and(|o| o.link.is_some())
    {
        return Err(internal("artifact metadata integrity mismatch"));
    }
    Ok(publication)
}
async fn verify(
    reader: &mut (impl AsyncRead + Unpin),
    expected: &ArtifactIntegrity,
) -> Result<(), Error> {
    let mut hash = Sha256::new();
    let mut size = 0u64;
    let mut buf = [0; 128 * 1024];
    loop {
        let n = read_chunk(reader, &mut buf).await.map_err(internal)?;
        if n == 0 {
            break;
        }
        size = size
            .checked_add(n as u64)
            .ok_or_else(|| invalid("size overflow"))?;
        if size > expected.size_bytes {
            return Err(invalid("artifact size mismatch"));
        }
        hash.update(&buf[..n]);
    }
    if size != expected.size_bytes
        || ArtifactSha256::from_bytes(hash.finalize().into()) != expected.sha256
    {
        return Err(invalid("artifact size or SHA-256 mismatch"));
    }
    Ok(())
}

// Missing chunk subjects otherwise leave an ordered Object Store consumer
// waiting forever. Bound inactivity, rather than total multi-GB transfer time.
async fn read_chunk(
    reader: &mut (impl AsyncRead + Unpin),
    buf: &mut [u8],
) -> std::io::Result<usize> {
    tokio::time::timeout(std::time::Duration::from_secs(30), reader.read(buf))
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "artifact chunk read timed out",
            )
        })?
}
fn checked_stream(
    reader: impl AsyncRead + Unpin + Send + 'static,
    expected: ArtifactIntegrity,
) -> impl futures::Stream<Item = std::io::Result<Vec<u8>>> {
    futures::stream::try_unfold(
        (reader, Sha256::new(), 0u64, expected),
        |(mut reader, mut hash, size, expected)| async move {
            let mut buf = vec![0; 128 * 1024];
            let n = read_chunk(&mut reader, &mut buf).await?;
            let size = size
                .checked_add(n as u64)
                .ok_or_else(|| std::io::Error::other("artifact size overflow"))?;
            if size > expected.size_bytes {
                return Err(std::io::Error::other("artifact size mismatch"));
            }
            if n == 0 {
                if size != expected.size_bytes
                    || ArtifactSha256::from_bytes(hash.finalize().into()) != expected.sha256
                {
                    return Err(std::io::Error::other("artifact integrity mismatch"));
                }
                return Ok(None);
            }
            buf.truncate(n);
            hash.update(&buf);
            Ok(Some((buf, (reader, hash, size, expected))))
        },
    )
}

/// Check the retention guarantees we can observe. Broker authorization is a
/// deployment prerequisite, explicitly attested by the opt-in, not inferred
/// from a successful hash check or the operator's HTTP role.
async fn store(js: &Context) -> Result<ObjectStore, Error> {
    let mut stream = js
        .get_stream(format!("OBJ_{OBJECT_APP_PACKAGES}"))
        .await
        .map_err(internal)?;
    let config = &stream.info().await.map_err(internal)?.config;
    if !config.max_age.is_zero()
        || config.discard != async_nats::jetstream::stream::DiscardPolicy::New
        || config.max_messages > 0
        || config.max_messages_per_subject > 0
    {
        return Err(internal(
            "artifact storage requires no expiry/eviction and discard-new",
        ));
    }
    js.get_object_store(OBJECT_APP_PACKAGES)
        .await
        .map_err(internal)
}

// Before CAS, staging chunks are private to this upload. After CAS starts an
// uncertain acknowledgement MUST retain them: the server may still commit.
// No online age-based sweeper is safe. See docs/app-artifacts.md.
async fn discard_stage(store: &ObjectStore, stage: &str) {
    if let Err(error) = store.delete(stage).await {
        tracing::warn!(%error, "artifact staging cleanup failed; offline reclamation required");
    }
}
async fn forget_stage(js: &Context, stage: &str) {
    // Purge only the staging metadata, never the chunks now referenced by the
    // canonical ObjectInfo. ObjectStore::delete would delete those chunks.
    let result = async {
        js.get_stream(format!("OBJ_{OBJECT_APP_PACKAGES}"))
            .await?
            .purge()
            .filter(subject(stage))
            .await?;
        Ok::<(), Box<dyn std::error::Error + Send + Sync>>(())
    }
    .await;
    if let Err(error) = result {
        tracing::warn!(%error, "artifact staging metadata cleanup failed");
    }
}

// 保存の条件付き確定と照会を分離し、不明な応答を障害注入できるようにする。
trait Backing: Sync {
    fn create(&self, info: &ObjectInfo) -> impl Future<Output = Result<(), Error>> + Send;
    fn read_info(
        &self,
        key: &str,
    ) -> impl Future<Output = Result<Option<ObjectInfo>, Error>> + Send;
    fn discard(&self, stage: &str) -> impl Future<Output = ()> + Send;
    fn forget(&self, stage: &str) -> impl Future<Output = ()> + Send;
}

struct NatsBacking<'a> {
    js: &'a Context,
    store: &'a ObjectStore,
}

impl Backing for NatsBacking<'_> {
    async fn create(&self, info: &ObjectInfo) -> Result<(), Error> {
        let mut headers = async_nats::HeaderMap::new();
        headers.insert("Nats-Expected-Last-Subject-Sequence", "0");
        self.js
            .publish_with_headers(
                subject(&info.name),
                headers,
                serde_json::to_vec(info).map_err(internal)?.into(),
            )
            .await
            .map_err(internal)?
            .await
            .map_err(internal)?;
        Ok(())
    }

    async fn read_info(&self, key: &str) -> Result<Option<ObjectInfo>, Error> {
        match self.store.info(key).await {
            Ok(info) => Ok(Some(info)),
            Err(e) if e.kind() == async_nats::jetstream::object_store::InfoErrorKind::NotFound => {
                Ok(None)
            }
            Err(e) => Err(internal(e)),
        }
    }

    async fn discard(&self, stage: &str) {
        discard_stage(self.store, stage).await;
    }
    async fn forget(&self, stage: &str) {
        forget_stage(self.js, stage).await;
    }
}

fn matching(info: &ObjectInfo, publication: &Publication) -> Result<Publication, Error> {
    let accepted = record(info)?;
    if accepted != *publication {
        return Err((
            StatusCode::CONFLICT,
            "identity already has different bytes or metadata".into(),
        ));
    }
    Ok(accepted)
}

async fn verify_existing(
    backing: &impl Backing,
    publication: &Publication,
    reader: &mut (impl AsyncRead + Unpin),
) -> Result<Option<Publication>, Error> {
    let key = publication.identity.app_package_key().map_err(invalid)?;
    let Some(info) = backing.read_info(&key).await? else {
        return Ok(None);
    };
    let accepted = matching(&info, publication)?;
    verify(reader, &publication.integrity).await?;
    Ok(Some(accepted))
}

async fn commit_backed(
    backing: &impl Backing,
    stage: &str,
    mut info: ObjectInfo,
    publication: &Publication,
) -> Result<Publication, Error> {
    info.name = publication.identity.app_package_key().map_err(invalid)?;
    info.metadata.insert(
        RECORD.into(),
        serde_json::to_string(publication).map_err(internal)?,
    );
    for _ in 0..2 {
        if backing.create(&info).await.is_ok() {
            backing.forget(stage).await;
            return Ok(publication.clone());
        }
        // 応答喪失と競合を照会で解決する。未確定なら同じ内容で一度だけ再試行する。
        // 照会失敗や継続的不明では、遅れて確定する可能性があるため bytes を残す。
        if let Some(existing) = backing.read_info(&info.name).await? {
            // 不正なレコードは参照を信用できないため回収しない。
            record(&existing)?;
            if existing.nuid == info.nuid {
                backing.forget(stage).await;
            } else {
                backing.discard(stage).await;
            }
            return matching(&existing, publication);
        }
    }
    Err(internal(
        "publication outcome unknown; staging retained; retry the same publication",
    ))
}

async fn commit(
    js: &Context,
    store: &ObjectStore,
    stage: &str,
    info: ObjectInfo,
    publication: &Publication,
) -> Result<Publication, Error> {
    commit_backed(&NatsBacking { js, store }, stage, info, publication).await
}

pub async fn publish(
    State(state): State<AppState>,
    caller: Caller,
    mut multipart: Multipart,
) -> Result<Json<Publication>, Error> {
    if std::env::var(ENABLE).as_deref() != Ok("backend-exclusive") {
        return Err(internal(
            "identity publication disabled; backend-exclusive broker authorization must be provisioned first",
        ));
    }
    let store = store(&state.jetstream).await?;
    let mut metadata = multipart
        .next_field()
        .await
        .map_err(invalid)?
        .ok_or_else(|| invalid("metadata must precede file"))?;
    if metadata.name() != Some("metadata") {
        return Err(invalid("metadata must precede file"));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = metadata.chunk().await.map_err(invalid)? {
        if bytes.len() + chunk.len() > 4096 {
            return Err(invalid("metadata exceeds 4096 bytes"));
        }
        bytes.extend_from_slice(&chunk);
    }
    let publication: Publication = serde_json::from_slice(&bytes).map_err(invalid)?;
    let key = publication.identity.app_package_key().map_err(invalid)?;
    if publication.integrity.size_bytes == 0
        || publication.integrity.size_bytes > super::APP_PACKAGE_BODY_LIMIT as u64
    {
        return Err(invalid("artifact size outside upload limit"));
    }
    let field = multipart
        .next_field()
        .await
        .map_err(invalid)?
        .ok_or_else(|| invalid("missing file"))?;
    if field.name() != Some("file") {
        return Err(invalid("expected file after metadata"));
    }
    let backing = NatsBacking {
        js: &state.jetstream,
        store: &store,
    };
    let mut reader = StreamReader::new(field.map_err(std::io::Error::other))
        .take(publication.integrity.size_bytes + 1);
    let existing = verify_existing(&backing, &publication, &mut reader).await?;
    // Four segments cannot be addressed by the two-segment legacy API.
    let stage = format!("__uploads/app/{}/bytes", uuid::Uuid::new_v4());
    let staged = if existing.is_some() {
        None
    } else {
        let uploaded = async {
            let info = store
                .put(stage.as_str(), &mut reader)
                .await
                .map_err(internal)?;
            if info.size as u64 != publication.integrity.size_bytes
                || info.digest != Some(publication.integrity.sha256.to_object_store_digest())
            {
                return Err(invalid("artifact size or SHA-256 mismatch"));
            }
            let mut readback = store.get(&stage).await.map_err(internal)?;
            verify(&mut readback, &publication.integrity).await?;
            Ok(info)
        }
        .await;
        match uploaded {
            Ok(info) => Some(info),
            Err(e) => {
                discard_stage(&store, &stage).await;
                return Err(e);
            }
        }
    };
    drop(reader);
    let trailing = multipart
        .next_field()
        .await
        .map(|field| field.is_some())
        .map_err(invalid);
    if trailing != Ok(false) {
        if staged.is_some() {
            discard_stage(&store, &stage).await;
        }
        return Err(trailing
            .err()
            .unwrap_or_else(|| invalid("unexpected extra multipart field")));
    }
    let accepted = match staged {
        Some(info) => commit(&state.jetstream, &store, &stage, info, &publication).await?,
        None => existing.expect("既存成果物の本文検証済み"),
    };
    audit::record(
        &state.nats,
        "operator",
        "app_artifact_publish",
        Some(&key),
        Some(&caller),
        serde_json::to_value(&accepted).map_err(internal)?,
    )
    .await;
    Ok(Json(accepted))
}
fn key(path: (String, String, String)) -> Result<String, Error> {
    let raw = format!("{}/{}/{}", path.0, path.1, path.2);
    ArtifactIdentity::from_app_package_key(&raw)
        .map_err(invalid)?
        .app_package_key()
        .map_err(invalid)
}
async fn lookup(state: &AppState, key: &str) -> Result<(ObjectStore, Publication), Error> {
    let store = state
        .jetstream
        .get_object_store(OBJECT_APP_PACKAGES)
        .await
        .map_err(internal)?;
    let info = store.info(key).await.map_err(|e| {
        if e.kind() == async_nats::jetstream::object_store::InfoErrorKind::NotFound {
            (StatusCode::NOT_FOUND, "artifact not found".into())
        } else {
            internal(e)
        }
    })?;
    let publication = record(&info)?;
    Ok((store, publication))
}
pub async fn metadata(
    State(state): State<AppState>,
    Path(path): Path<(String, String, String)>,
) -> Result<Json<Publication>, Error> {
    Ok(Json(lookup(&state, &key(path)?).await?.1))
}
pub async fn download(
    State(state): State<AppState>,
    Path(path): Path<(String, String, String)>,
) -> Result<Response, Error> {
    let key = key(path)?;
    let (store, publication) = lookup(&state, &key).await?;
    // Fail before sending HTTP success on corruption. Re-open for bounded-memory
    // streaming; async-nats also verifies the digest while consuming that read.
    let mut check = store.get(&key).await.map_err(internal)?;
    verify(&mut check, &publication.integrity)
        .await
        .map_err(|e| internal(e.1))?;
    let object = store.get(&key).await.map_err(internal)?;
    if record(object.info())? != publication {
        return Err(internal("artifact metadata changed"));
    }
    Ok((
        [
            (header::CONTENT_TYPE, "application/octet-stream"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        Body::from_stream(checked_stream(object, publication.integrity)),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::super::agent_releases::tests as fixture;
    use super::*;
    use axum::http::{Method, Request};
    use tower::ServiceExt;

    fn publication(package: &str, bytes: &[u8]) -> Publication {
        Publication {
            identity: ArtifactIdentity::from_app_package_key(&format!("cli/1.2.3/{package}"))
                .unwrap(),
            integrity: ArtifactIntegrity::of(bytes),
        }
    }
    async fn stage(js: &Context, publication: &Publication, bytes: &[u8]) -> (String, ObjectInfo) {
        let store = store(js).await.unwrap();
        let name = format!("__uploads/app/{}/bytes", uuid::Uuid::new_v4());
        let info = store.put(name.as_str(), &mut &bytes[..]).await.unwrap();
        let mut object = store.get(&name).await.unwrap();
        verify(&mut object, &publication.integrity).await.unwrap();
        (name, info)
    }
    async fn publish_bytes(
        js: &Context,
        publication: &Publication,
        bytes: &[u8],
    ) -> Result<Publication, Error> {
        let store = store(js).await?;
        let (name, info) = stage(js, publication, bytes).await;
        commit(js, &store, &name, info, publication).await
    }
    async fn request(
        app: &fixture::App,
        method: Method,
        path: &str,
        user: Option<&str>,
        content_type: &str,
        body: Vec<u8>,
    ) -> Response {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::CONTENT_TYPE, content_type);
        if let Some(user) = user {
            req = req.header(
                header::AUTHORIZATION,
                format!("Bearer {}", fixture::mint(user)),
            );
        }
        app.router
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap()
    }
    async fn http_publish(app: &fixture::App, publication: &Publication, bytes: &[u8]) -> Response {
        let json = serde_json::to_vec(publication).unwrap();
        let (ct, body) = fixture::multipart(&[
            ("metadata", None, &json),
            ("file", Some("irrelevant"), bytes),
        ]);
        request(
            app,
            Method::POST,
            "/api/app-artifacts",
            Some("op"),
            &ct,
            body,
        )
        .await
    }

    struct FakeBacking(std::sync::Mutex<FakeState>);

    struct FakeState {
        committed: Option<ObjectInfo>,
        unapplied: usize,
        lose_ack: bool,
        fail_lookup: bool,
        creates: usize,
        stage_metadata: bool,
        chunks: bool,
    }

    impl FakeBacking {
        fn new(unapplied: usize, lose_ack: bool) -> Self {
            Self(std::sync::Mutex::new(FakeState {
                committed: None,
                unapplied,
                lose_ack,
                fail_lookup: false,
                creates: 0,
                stage_metadata: true,
                chunks: true,
            }))
        }
    }

    impl Backing for FakeBacking {
        async fn create(&self, info: &ObjectInfo) -> Result<(), Error> {
            let mut state = self.0.lock().unwrap();
            state.creates += 1;
            if state.unapplied > 0 {
                state.unapplied -= 1;
                return Err(internal("injected unapplied commit"));
            }
            if state.committed.is_some() {
                return Err(internal("CAS conflict"));
            }
            state.committed = Some(info.clone());
            if state.lose_ack {
                return Err(internal("injected lost acknowledgement"));
            }
            Ok(())
        }
        async fn read_info(&self, _: &str) -> Result<Option<ObjectInfo>, Error> {
            let state = self.0.lock().unwrap();
            if state.fail_lookup {
                return Err(internal("injected lookup failure"));
            }
            Ok(state.committed.clone())
        }
        async fn discard(&self, _: &str) {
            let mut state = self.0.lock().unwrap();
            state.stage_metadata = false;
            state.chunks = false;
        }
        async fn forget(&self, _: &str) {
            self.0.lock().unwrap().stage_metadata = false;
        }
    }

    fn fake_info(p: &Publication, nuid: &str) -> ObjectInfo {
        serde_json::from_value(serde_json::json!({
            "name": p.identity.app_package_key().unwrap(),
            "bucket": OBJECT_APP_PACKAGES,
            "nuid": nuid,
            "size": p.integrity.size_bytes,
            "chunks": 1,
            "digest": p.integrity.sha256.to_object_store_digest(),
            "metadata": { RECORD: serde_json::to_string(p).unwrap() }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn lost_ack_applied_reconciles_without_deleting_committed_chunks() {
        let backing = FakeBacking::new(0, true);
        let p = publication("linux-x86_64-musl.tar_gz", b"payload");
        assert_eq!(
            commit_backed(&backing, "stage", fake_info(&p, "ours"), &p)
                .await
                .unwrap(),
            p
        );
        let state = backing.0.lock().unwrap();
        assert_eq!(state.creates, 1);
        assert_eq!(record(state.committed.as_ref().unwrap()).unwrap(), p);
        assert!(!state.stage_metadata);
        assert!(state.chunks);
    }

    #[tokio::test]
    async fn lost_ack_unapplied_retries_the_same_conditional_record() {
        let backing = FakeBacking::new(1, false);
        let p = publication("linux-x86_64-musl.tar_gz", b"payload");
        assert_eq!(
            commit_backed(&backing, "stage", fake_info(&p, "ours"), &p)
                .await
                .unwrap(),
            p
        );
        let state = backing.0.lock().unwrap();
        assert_eq!(state.creates, 2);
        assert_eq!(state.committed.as_ref().unwrap().nuid, "ours");
        assert!(!state.stage_metadata);
        assert!(state.chunks);
    }

    #[tokio::test]
    async fn persistently_unknown_commit_preserves_staging_and_allows_retry() {
        let backing = FakeBacking::new(2, false);
        let p = publication("linux-x86_64-musl.tar_gz", b"payload");
        let info = fake_info(&p, "ours");
        assert_eq!(
            commit_backed(&backing, "stage", info.clone(), &p)
                .await
                .unwrap_err()
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
        {
            let state = backing.0.lock().unwrap();
            assert_eq!(state.creates, 2);
            assert!(state.committed.is_none());
            assert!(state.stage_metadata && state.chunks);
        }
        assert_eq!(commit_backed(&backing, "stage", info, &p).await.unwrap(), p);
    }

    #[tokio::test]
    async fn lost_ack_and_failed_lookup_keep_potentially_committed_bytes() {
        let backing = FakeBacking::new(0, true);
        backing.0.lock().unwrap().fail_lookup = true;
        let p = publication("linux-x86_64-musl.tar_gz", b"payload");
        let info = fake_info(&p, "ours");
        assert!(
            commit_backed(&backing, "stage", info.clone(), &p)
                .await
                .is_err()
        );
        {
            let mut state = backing.0.lock().unwrap();
            assert!(state.stage_metadata && state.chunks);
            assert!(state.committed.is_some());
            state.fail_lookup = false;
        }
        assert_eq!(commit_backed(&backing, "stage", info, &p).await.unwrap(), p);
        assert!(backing.0.lock().unwrap().chunks);
    }

    #[tokio::test]
    async fn competing_record_cleans_only_loser_for_identical_and_conflicting_publications() {
        let ours = publication("linux-x86_64-musl.tar_gz", b"ours");
        for bytes in [b"ours".as_slice(), b"theirs"] {
            let theirs = publication("linux-x86_64-musl.tar_gz", bytes);
            let backing = FakeBacking::new(0, false);
            let winner = fake_info(&theirs, "theirs");
            backing.0.lock().unwrap().committed = Some(winner.clone());
            let result = commit_backed(&backing, "stage", fake_info(&ours, "ours"), &ours).await;
            if theirs == ours {
                assert_eq!(result.unwrap(), ours);
            } else {
                assert_eq!(result.unwrap_err().0, StatusCode::CONFLICT);
            }
            let state = backing.0.lock().unwrap();
            assert!(!state.stage_metadata && !state.chunks);
            assert_eq!(state.committed.as_ref().unwrap(), &winner);
        }
    }

    #[tokio::test]
    async fn existing_publication_checks_body_without_creating_or_cleaning_objects() {
        let backing = FakeBacking::new(0, false);
        let p = publication("linux-x86_64-musl.tar_gz", b"payload");
        let info = fake_info(&p, "existing");
        backing.0.lock().unwrap().committed = Some(info.clone());
        assert_eq!(
            verify_existing(&backing, &p, &mut &b"payload"[..])
                .await
                .unwrap(),
            Some(p.clone())
        );
        for bytes in [b"payloa".as_slice(), b"payload!", b"corrupt"] {
            assert_eq!(
                verify_existing(&backing, &p, &mut &bytes[..])
                    .await
                    .unwrap_err()
                    .0,
                StatusCode::BAD_REQUEST
            );
        }
        let different = Publication {
            integrity: ArtifactIntegrity::of(b"different"),
            ..p
        };
        assert_eq!(
            verify_existing(&backing, &different, &mut &b"different"[..])
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT
        );
        let state = backing.0.lock().unwrap();
        assert_eq!(state.creates, 0);
        assert!(state.stage_metadata && state.chunks);
        assert_eq!(state.committed.as_ref().unwrap(), &info);
    }

    #[tokio::test]
    #[ignore = "requires nats-server and KANADE_APP_ARTIFACT_PUBLICATION=backend-exclusive"]
    async fn identical_http_republication_does_not_write_to_object_store() {
        let app = fixture::app().await;
        let p = publication("linux-x86_64-musl.tar_gz", b"payload");
        assert_eq!(
            http_publish(&app, &p, b"payload").await.status(),
            StatusCode::OK
        );
        let mut stream = app
            .js
            .get_stream(format!("OBJ_{OBJECT_APP_PACKAGES}"))
            .await
            .unwrap();
        let before = stream.info().await.unwrap().state.clone();
        assert_eq!(
            http_publish(&app, &p, b"payload").await.status(),
            StatusCode::OK
        );
        for bytes in [b"payloa".as_slice(), b"payload!", b"corrupt"] {
            assert_eq!(
                http_publish(&app, &p, bytes).await.status(),
                StatusCode::BAD_REQUEST
            );
        }
        let json = serde_json::to_vec(&p).unwrap();
        let (ct, body) = fixture::multipart(&[
            ("metadata", None, &json),
            ("file", Some("bytes"), b"payload"),
            ("extra", None, b"unexpected"),
        ]);
        assert_eq!(
            request(
                &app,
                Method::POST,
                "/api/app-artifacts",
                Some("op"),
                &ct,
                body
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
        let after = &stream.info().await.unwrap().state;
        assert_eq!(after.last_sequence, before.last_sequence);
        assert_eq!(after.messages, before.messages);
        assert_eq!(after.bytes, before.bytes);
    }

    #[tokio::test]
    async fn streamed_integrity_rejects_size_and_digest_mismatch() {
        let expected = ArtifactIntegrity::of(b"abc");
        for bytes in [b"ab".as_slice(), b"abcd", b"xyz"] {
            assert!(verify(&mut &bytes[..], &expected).await.is_err());
        }
        verify(&mut &b"abc"[..], &expected).await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH"]
    async fn platforms_formats_and_standard_object_store_reads_are_isolated() {
        use futures::StreamExt;
        let app = fixture::app().await;
        let store = store(&app.js).await.unwrap();
        for platform in [
            "windows-x86_64-msvc.exe",
            "windows-x86_64-msvc.zip",
            "linux-x86_64-musl.tar_gz",
            "linux-aarch64-musl.tar_gz",
            "macos-aarch64-darwin.tar_gz",
        ] {
            let p = publication(platform, platform.as_bytes());
            publish_bytes(&app.js, &p, platform.as_bytes())
                .await
                .unwrap();
            let key = p.identity.app_package_key().unwrap();
            let mut object = store.get(&key).await.unwrap();
            assert_eq!(record(object.info()).unwrap(), p);
            verify(&mut object, &p.integrity).await.unwrap();
            crate::projector::object_meta::apply(&app.pool, OBJECT_APP_PACKAGES, object.info())
                .await
                .unwrap();
        }
        let mut list = store.list().await.unwrap();
        let mut count = 0;
        while let Some(info) = list.next().await {
            record(&info.unwrap()).unwrap();
            count += 1;
        }
        assert_eq!(count, 5);
        assert_eq!(
            crate::projector::object_meta::list_bucket(&app.pool, OBJECT_APP_PACKAGES)
                .await
                .unwrap()
                .len(),
            5
        );
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH"]
    async fn concurrent_publishers_idempotence_conflict_and_lost_ack_reconciliation() {
        let app = fixture::app().await;
        let a = publication("windows-x86_64-msvc.exe", b"a");
        let (x, y) = tokio::join!(
            publish_bytes(&app.js, &a, b"a"),
            publish_bytes(&app.js, &a, b"a")
        );
        assert_eq!(x.unwrap(), a);
        assert_eq!(y.unwrap(), a);
        let b = publication("windows-x86_64-msvc.zip", b"b");
        let c = publication("windows-x86_64-msvc.zip", b"c");
        let (x, y) = tokio::join!(
            publish_bytes(&app.js, &b, b"b"),
            publish_bytes(&app.js, &c, b"c")
        );
        assert!(matches!(
            (&x, &y),
            (Ok(_), Err((StatusCode::CONFLICT, _))) | (Err((StatusCode::CONFLICT, _)), Ok(_))
        ));
        let store = store(&app.js).await.unwrap();
        let p = publication("linux-x86_64-musl.tar_gz", b"payload");
        let (name, info) = stage(&app.js, &p, b"payload").await;
        commit(&app.js, &store, &name, info.clone(), &p)
            .await
            .unwrap();
        // Re-enter with the same upload nuid, as on an uncertain acknowledgement.
        assert_eq!(commit(&app.js, &store, &name, info, &p).await.unwrap(), p);
    }

    #[tokio::test]
    #[ignore = "requires nats-server in PATH"]
    async fn interrupted_upload_is_not_discoverable_and_corruption_fails_closed() {
        let app = fixture::app().await;
        let p = publication("linux-x86_64-musl.tar_gz", b"data");
        // A reader failure after the first chunk must not publish any metadata.
        let partial = format!("__uploads/app/{}/bytes", uuid::Uuid::new_v4());
        let chunks = futures::stream::iter([
            Ok(bytes::Bytes::from_static(b"prefix")),
            Err(std::io::Error::other("interrupted upload")),
        ]);
        let mut reader = StreamReader::new(chunks);
        let store_before = store(&app.js).await.unwrap();
        assert!(
            store_before
                .put(partial.as_str(), &mut reader)
                .await
                .is_err()
        );
        assert!(store_before.info(&partial).await.is_err());
        let (stage_name, _) = stage(&app.js, &p, b"data").await;
        let store = store(&app.js).await.unwrap();
        let key = p.identity.app_package_key().unwrap();
        assert!(store.info(&key).await.is_err());
        discard_stage(&store, &stage_name).await;
        publish_bytes(&app.js, &p, b"data").await.unwrap();
        let info = store.info(&key).await.unwrap();
        for malformed in [
            ObjectInfo {
                size: info.size + 1,
                ..info.clone()
            },
            ObjectInfo {
                digest: Some(ArtifactSha256::of(b"wrong").to_object_store_digest()),
                ..info.clone()
            },
            ObjectInfo {
                name: "cli/1.2.3/windows-x86_64-msvc.zip".into(),
                ..info.clone()
            },
            ObjectInfo {
                deleted: true,
                ..info.clone()
            },
        ] {
            assert!(record(&malformed).is_err());
        }
        let mut stream = app
            .js
            .get_stream(format!("OBJ_{OBJECT_APP_PACKAGES}"))
            .await
            .unwrap();
        stream
            .purge()
            .filter(format!("$O.{OBJECT_APP_PACKAGES}.C.{}", info.nuid))
            .await
            .unwrap();
        app.js
            .publish(
                format!("$O.{OBJECT_APP_PACKAGES}.C.{}", info.nuid),
                b"evil".as_slice().into(),
            )
            .await
            .unwrap()
            .await
            .unwrap();
        let response = request(
            &app,
            Method::GET,
            &format!("/api/app-artifacts/{key}"),
            Some("op"),
            "",
            vec![],
        )
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        // Metadata alone still exists, but never yields a successful download.
        assert_eq!(record(&store.info(&key).await.unwrap()).unwrap(), p);
        let _ = stream.info().await.unwrap();
    }

    #[tokio::test]
    #[ignore = "requires nats-server and KANADE_APP_ARTIFACT_PUBLICATION=backend-exclusive"]
    async fn authenticated_api_integrity_immutability_and_legacy_regression() {
        assert_eq!(std::env::var(ENABLE).as_deref(), Ok("backend-exclusive"));
        let app = fixture::app().await;
        sqlx::query("INSERT INTO users (username, password_hash, role, allowed_features) VALUES ('viewer', 'x', 'viewer', NULL), ('restricted', 'x', 'operator', '[]')").execute(&app.pool).await.unwrap();
        let p = publication("windows-x86_64-msvc.exe", b"data");
        let json = serde_json::to_vec(&p).unwrap();
        let (ct, body) =
            fixture::multipart(&[("metadata", None, &json), ("file", Some("bytes"), b"data")]);
        for user in ["viewer", "restricted"] {
            assert_eq!(
                request(
                    &app,
                    Method::POST,
                    "/api/app-artifacts",
                    Some(user),
                    &ct,
                    body.clone()
                )
                .await
                .status(),
                StatusCode::FORBIDDEN
            );
        }
        for method in [Method::POST, Method::GET] {
            let path = if method == Method::POST {
                "/api/app-artifacts".into()
            } else {
                format!(
                    "/api/app-artifacts/{}",
                    p.identity.app_package_key().unwrap()
                )
            };
            assert_eq!(
                request(&app, method, &path, None, "", vec![])
                    .await
                    .status(),
                StatusCode::UNAUTHORIZED
            );
        }
        assert_eq!(
            http_publish(&app, &p, b"wrong").await.status(),
            StatusCode::BAD_REQUEST
        );
        assert!(
            store(&app.js)
                .await
                .unwrap()
                .info(p.identity.app_package_key().unwrap())
                .await
                .is_err()
        );
        assert_eq!(
            http_publish(&app, &p, b"data").await.status(),
            StatusCode::OK
        );
        assert_eq!(
            http_publish(&app, &p, b"data").await.status(),
            StatusCode::OK
        );
        let conflict = Publication {
            integrity: ArtifactIntegrity::of(b"other"),
            ..p.clone()
        };
        assert_eq!(
            http_publish(&app, &conflict, b"other").await.status(),
            StatusCode::CONFLICT
        );
        let key = p.identity.app_package_key().unwrap();
        assert_eq!(
            request(
                &app,
                Method::GET,
                &format!("/api/app-artifacts/{key}/metadata"),
                Some("viewer"),
                "",
                vec![]
            )
            .await
            .status(),
            StatusCode::OK
        );
        assert_eq!(
            request(
                &app,
                Method::GET,
                &format!("/api/app-artifacts/{key}"),
                Some("restricted"),
                "",
                vec![]
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        for method in [Method::DELETE, Method::POST, Method::PUT] {
            assert_eq!(
                request(
                    &app,
                    method,
                    &format!("/api/app-artifacts/{key}"),
                    Some("op"),
                    "",
                    vec![]
                )
                .await
                .status(),
                StatusCode::METHOD_NOT_ALLOWED
            );
        }
        for method in [Method::POST, Method::DELETE] {
            let response = request(
                &app,
                method,
                "/api/app-packages/cli%2F1.2.3/windows-x86_64-msvc.exe",
                Some("op"),
                "",
                vec![],
            )
            .await;
            assert!(!response.status().is_success());
        }
        let metadata_bytes = serde_json::to_vec(&p).unwrap();
        for (suffix, expected) in [
            ("", b"data".as_slice()),
            ("/metadata", metadata_bytes.as_slice()),
        ] {
            let response = request(
                &app,
                Method::GET,
                &format!("/api/app-artifacts/{key}{suffix}"),
                Some("op"),
                "",
                vec![],
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                &axum::body::to_bytes(response.into_body(), 8192)
                    .await
                    .unwrap()[..],
                expected
            );
        }
        for bytes in [b"old".as_slice(), b"replacement"] {
            let (ct, body) = fixture::multipart(&[("file", Some("legacy.exe"), bytes)]);
            assert_eq!(
                request(
                    &app,
                    Method::POST,
                    "/api/app-packages/tool/1",
                    Some("op"),
                    &ct,
                    body
                )
                .await
                .status(),
                StatusCode::OK
            );
            let response = request(
                &app,
                Method::GET,
                "/api/app-packages/tool/1",
                Some("op"),
                "",
                vec![],
            )
            .await;
            assert_eq!(
                &axum::body::to_bytes(response.into_body(), 8192)
                    .await
                    .unwrap()[..],
                bytes
            );
        }
        let indexed_store = store(&app.js).await.unwrap();
        let canonical = indexed_store.info(&key).await.unwrap();
        crate::projector::object_meta::apply(&app.pool, OBJECT_APP_PACKAGES, &canonical)
            .await
            .unwrap();
        let (staging_name, staging_info) = stage(&app.js, &p, b"data").await;
        crate::projector::object_meta::apply(&app.pool, OBJECT_APP_PACKAGES, &staging_info)
            .await
            .unwrap();
        discard_stage(&indexed_store, &staging_name).await;
        let response = request(
            &app,
            Method::GET,
            "/api/app-packages",
            Some("op"),
            "",
            vec![],
        )
        .await;
        let rows: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 8192)
                .await
                .unwrap(),
        )
        .unwrap();
        assert_eq!(rows.as_array().unwrap().len(), 1);
        assert_eq!(rows[0]["name"], "tool");
        assert_eq!(
            request(
                &app,
                Method::DELETE,
                "/api/app-packages/tool/1",
                Some("op"),
                "",
                vec![]
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
        assert!(store(&app.js).await.unwrap().info(key).await.is_ok());
    }
}

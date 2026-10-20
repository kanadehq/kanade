# Identity-addressed app artifacts (stage 1b)

This API stores integrity-checked bytes using `kanade_shared::artifact` identities
and canonical keys in the existing `app_packages` Object Store. It does **not**
authenticate a publisher, verify a package's embedded identity, or admit an artifact
for deployment. All identity fields and version are explicit declarations. No host
platform inference, legacy-key promotion, or PE version fallback is performed.
Publisher trust provisioning, alias snapshots, CI producer
transition, and deploy plan/apply/status remain unimplemented. An offline signed
manifest contract and CLI exist (SPEC §2.13.4.3, stage 2a) but are **not** integrated
into this API: it neither stores nor returns signed manifests, and signing must
never be added as mutable metadata beside the immutable record. Binding a signature
into the committed metadata and idempotency comparison is a separate, narrowly
scoped integration.

## Authorization and enabling publication

All routes require normal backend HTTP authentication and the Apps feature.
Publication additionally requires operator (or admin). Metadata and downloads
allow viewers with Apps. Publication is disabled by default (503).

Before setting backend environment `KANADE_APP_ARTIFACT_PUBLICATION=backend-exclusive`,
the operator must provision and verify broker authorization: only the backend
service principal and trusted administrators may publish `$O.app_packages.>` or
mutate/purge/delete/update `OBJ_app_packages`. No other service or operator client
may hold those credentials or equivalent wildcard JetStream administration grants.
The existing role-based `configs/nats-server.users.conf` gives those operations to
the backend; the shared-token configuration cannot enforce this boundary and must
not enable publication. This environment value is an explicit deployment
attestation, **not** automatic discovery or proof of broker permissions. Credential
isolation/provisioning is external to this slice. Compromised backend/admin
credentials remain outside the immutability guarantee. CAS alone cannot prevent
an unconditional Object Store put, rollup, stream purge, or broker administrator.

Publication also checks that the backing stream has no expiry/message eviction
and uses discard-new. Capacity exhaustion fails the upload; it never evicts a
committed artifact. Existing storage capacity settings still apply. Do not change
retention or broker grants while publication is enabled.

## HTTP contract

`POST /api/app-artifacts` accepts multipart with exactly two fields in order:

1. `metadata`: JSON, at most 4096 bytes, containing exactly `identity` and
   `integrity` as below. Shared validators reject unknown fields, unsupported
   combinations, noncanonical versions/digests, and agent app-package identities.
2. `file`: raw bytes (filename has no identity meaning). Nonempty, within the
   existing app-package body limit, with the declared size and SHA-256.

```json
{"identity":{"role":"cli","version":"1.2.3","os":"linux","arch":"aarch64","distro_abi":"musl","format":"tar_gz"},"integrity":{"sha256":"ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad","size_bytes":3}}
```

CLI opt-in: `kanade app publish-artifact --identity identity.json package.tar.gz`
reads the explicit shared `ArtifactIdentity` JSON, computes size/SHA-256 by streaming
the file, uploads metadata first, and verifies both metadata and downloaded bytes.
It does not infer a version/platform or change `kanade app publish` behavior.

Example using a metadata file and the existing API authentication token:

```sh
curl --fail-with-body -H "Authorization: Bearer $TOKEN" \
  -F metadata=@metadata.json -F file=@package.tar.gz "$BACKEND/api/app-artifacts"
curl --fail-with-body -H "Authorization: Bearer $TOKEN" \
  "$BACKEND/api/app-artifacts/cli/1.2.3/linux-aarch64-musl.tar_gz/metadata"
curl --fail-with-body -H "Authorization: Bearer $TOKEN" \
  "$BACKEND/api/app-artifacts/cli/1.2.3/linux-aarch64-musl.tar_gz" -o retrieved.tar.gz
```

Success returns 200 with the accepted metadata. Repeating the same identity and
integrity is idempotent **after consuming and validating the new body**.
既存レコードが一致する場合も受信本文のサイズと SHA-256 を検証するが、
追加のステージング保存や Object Store 書込みは行わない。
Different
bytes/accepted metadata for an existing identity return 409; mismatched declared
size/digest or malformed multipart return 400. Storage/verification failures return
503. Unknown identity keys return 404. No force overwrite or delete API exists;
POST/PUT/DELETE to an individual identity return 405.

`GET /api/app-artifacts/{role}/{version}/{package}/metadata` reads the committed
record directly, validates the canonical key, identity, Object Store digest, size,
and non-link/nondeleted metadata. It does not read package bytes or claim trust.
`GET /api/app-artifacts/{role}/{version}/{package}` additionally performs a bounded
memory full read-back verification before streaming a second read. Corruption
fails before HTTP success; the streamed read verifies size/digest too, with a 30-second per-chunk inactivity timeout.
These routes do not support Range. Clients should still validate downloaded size
and SHA-256 before use. Backend traffic is twice the object size per download.

## Commit, interruption, retry, and maintenance

Each upload uses a unique four-segment staging object
`__uploads/app/<uuid>/bytes`; legacy two-segment API routes cannot address it.
The backend compares Object Store's streamed size/digest with the declaration and
reads the staged bytes back through a SHA-256/size verifier. It then publishes one
standard Object Store `ObjectInfo`, embedding the accepted metadata in
`metadata["kanade.artifact.v1"]`, to the canonical metadata subject using
`Nats-Expected-Last-Subject-Sequence: 0`. That single acknowledged message is the
only discovery/commit point. Its nuid references only that upload's chunks.
Standard Object Store get/list and the metadata projector remain compatible.
Readers cannot discover an unfinished upload through the identity API.

Concurrent losers reconcile against the winning record: identical publications
succeed and conflicting publications fail. Only a loser's known unreferenced
chunks are deleted. Success purges only the private staging metadata, never its
now-committed chunks. A lost/uncertain commit acknowledgement triggers direct
reconciliation; unresolved outcomes retain staging/chunks and return an error.
照会で未確定と分かった場合、同じ ObjectInfo の条件付き確定を一度だけ再試行する。
応答喪失が続く場合や照会に失敗した場合はステージングと chunks を保持して 503 を返す。
Retry the entire request with the same metadata/bytes: a committed result becomes
an idempotent success. Validation failures clean up staged objects best effort.
Cancellation/crash during upload can leave chunks without metadata; cancellation
at commit can leave staging metadata sharing chunks with a committed object.

There is deliberately **no live sweeper**: age is not proof that an upload cannot
still commit. For reclamation, disable publication and stop **all** backend writers,
ensure outstanding broker requests have completed, snapshot the stream, enumerate
all committed canonical ObjectInfo nuid references, and remove only unreferenced
staging/chunk subjects. Never use ObjectStore delete on a staging object whose
nuid is referenced by a committed record. Clean stale staging metadata separately.
No reclamation command is supplied here. Restore capacity before retrying uploads.

All HTTP app-package mutations remain limited to validated two-segment legacy
keys. The legacy list hides canonical and staging records; legacy publish still
overwrites and legacy download/list/delete semantics are preserved. Other backend
Object Store APIs operate on different buckets. Agent release/rollout keys,
fleet scripts, bootstrap flows, and shell jobs are unchanged.

Plan persistence/reference tracking is absent. Consequently **all** new-namespace
mutation/deletion is disabled, including replacement of an existing identity.
This is conservative retention, not implemented referenced-plan GC protection.
No signed publisher metadata or additional metadata update interface is accepted.

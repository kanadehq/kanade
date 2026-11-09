# v0.104.0 backend deployment status — 2026-11-09

Deployment remains blocked pending established host access and the supported
backend-only backup, update and rollback procedure. Release binaries are now
published, and the parent graph has retired the competing candidate. The
configured backend still reports **0.102.1**;
v0.104.0 has not been installed by this attempt. No service, binary,
database, credential, configuration, passkey setting, or rollout was changed.
The initial observations below were collected around 15:00–15:04 UTC;
the review follow-up records the changed conditions at 15:08 UTC.

## Source and release evidence

- The launcher's successful fetch at `2026-11-09T14:58:42.44544Z` pinned
  `21e2182dce825cd600df53f62b2b940c57d8bf2e`. `git rev-parse HEAD`
  returned that exact commit. No additional fetch or base movement occurred.
- `gh pr view 1691 --repo kanadehq/kanade --json state,mergedAt,mergeCommit`
  confirmed the feature was merged as
  `86ae611eca06071bc608e6357608e80fa1fd1ca4`.
  `git merge-base --is-ancestor` confirmed its inclusion in the pinned base.
- The equivalent query for PR #1696 confirmed the release merge at the
  pinned base. The GitHub tag API resolved annotated tag object
  `0215ebee4ddb2879016a4921855b2ecdb19f4b66` to that same commit.
- `gh release view v0.104.0 --repo kanadehq/kanade --json
  tagName,publishedAt,isDraft,isPrerelease,targetCommitish,assets` confirmed
  publication at `2026-11-09T14:47:45Z`, neither draft nor prerelease.
  A subsequent release API query still listed only four `.cdx.json` SBOMs.
  **No downloadable backend binary was available in the release.**
- `gh run view 42638068454 --repo kanadehq/kanade --json
  status,conclusion,jobs` reported Release in progress. Linux builds had
  succeeded; Windows and macOS builds were still in progress. Build success
  does not establish that a release asset is published. No artifact was
  substituted, locally built, downloaded, or hash-verified.

## Read-only environment verification

The existing private OCI environment file supplied the backend URL and
authentication token. Neither values nor identifying endpoint information
are recorded here. Python `urllib.request` used default TLS certificate
verification and 15-second request timeouts; no insecure TLS option was used.

| Request | Observed result |
| --- | --- |
| `GET /api/version` | HTTP 200, JSON `version: 0.102.1` |
| `GET /health` | HTTP 200, plain-text `ok` |
| Authenticated `GET /api/auth/me` | HTTP 200 JSON, role `admin` |
| Authenticated `GET /api/server-settings` | HTTP 200 JSON; no top-level `passkeys` field |
| `GET /` | HTTP 200 HTML |
| First three asset references from the delivered HTML | Each HTTP 200 JavaScript, nonempty bodies |
| Authenticated `GET /api/server-settings/passkeys/status` | HTTP 200 HTML, not the expected JSON API response |

The last response does not prove passkey availability: it is consistent with
an unknown API route reaching the SPA fallback on the older deployment.
An initial `/api/health` probe likewise returned HTML when authenticated;
the source in `crates/kanade-backend/src/api/mod.rs` establishes `/health`
as the actual health route. HTTP success alone was not treated as health
or feature verification. No browser-rendered Server Settings Passkeys card
was verified. Service logs and installed binary hash remain unverified.

## Deployment path and blockers

`magi show bba8 --json` reported two implementation candidates and an
implementing run. No exclusive production execution designation was
established. A coordination message was posted through `magi talk post`
to the originating conversation requesting one execution owner and
read-only behavior for the other seat. This attempt remained read-only.

The repository procedures were inspected, including
`docs/operations-deployment-status-2026-10-17.md`,
`deploy/oci/README.md`, `deploy/linux/README.md`,
`deploy/linux/fleet-deploy.sh`, `deploy/linux/setup.sh`, and
`book/src/operations/deploy.md`.

The documented Linux command shape is `deploy/linux/fleet-deploy.sh
--role backend --mode install --version 0.104.0 --host <established-host>
--domain <established-domain> --identity <established-key>`. The script's
`--mode update` is agent-only. However, backend install invokes `setup.sh`,
which installs NATS and Caddy binaries, overwrites backend/broker/proxy
configuration and service units, and enables services. It is not a
backend-only upgrade and supplies no stopped-database backup transaction.
It was not executed or presented as a safe dry run. The Windows fleet
procedure was not selected without evidence of a Windows deployment target.

The private environment file establishes application connectivity, not an
SSH login, key selection, current service layout, backup destination, or
supported backend-only replacement command. Those operational details and
exclusive ownership must be established before any production mutation.
No host, login, identity, or destination was inferred from DNS, examples,
or obsolete provisioning instructions.

`git diff --name-only v0.102.1
21e2182dce825cd600df53f62b2b940c57d8bf2e --
crates/kanade-backend/migrations` lists
`20261030000000_index_execution_results_identifiers_started_at.sql`.
The local baseline tag resolves to
`4bf1a40a23401e1c1e6c3d15bcf2c768b6f591a3`; the running binary's commit
and applied migration state have not been independently established.
Rollback compatibility therefore remains unverified. Do not wipe the DB.

## Required continuation

Confirm the existing host access and supported update/backup procedure.
Release assets and competing-seat status were rechecked below. Before
mutation, recheck the running version and shared execution result; if the
requested version is already deployed, verify it without redeploying.
Preserve the old binary, settings, credentials, SQLite database and WAL
using the established stopped-backend backup and rollback practice.

After the authorized backend update, verify the release binary's identity,
`/api/version`, `/health`, service logs, root HTML and referenced assets.
Use existing operator authentication to verify JSON from
`/api/server-settings/passkeys/status` and render Settings → Server Settings
to confirm the Passkeys card. No enablement, settings save, RP ID, or
origin change is authorized by this deployment request. Agent, client,
CLI, NATS and proxy updates are outside this attempt's scope.

## Review follow-up — 15:08 UTC

Finding `R1-1-1` correctly identifies that deployment has not happened;
it remains unresolved. The initial missing-asset and competing-candidate
conditions have changed and must not be used as current blockers:

- `gh release view v0.104.0 --repo kanadehq/kanade --json assets` now
  lists the backend tarballs for Linux x86_64/aarch64 and macOS aarch64,
  plus the Windows x86_64 zip. The Release workflow's `github release`
  job and all four build jobs succeeded; crates.io publishing was still
  in progress. Release asset availability no longer blocks deployment.
- The reported Linux aarch64 asset digest is
  `sha256:9a7edcb426323c2257babca91048474427510ecd54711cc496ca01c65837edd2`;
  Linux x86_64 is
  `sha256:3e473d334064d5554bbe100d2f98fabeeddc02467c88e6fcfca7fe957829f1ba`.
  These are API metadata, not downloaded-byte verification. Target
  architecture still needs confirmation before selecting an asset.
- `magi show bba8 --json` records the other candidate folded at
  `15:06:25.119859Z`, the surviving branch selected, and only its fix seat
  active. The previous competing implementation condition has ended.
- Repeated TLS-verified requests returned `/api/version` HTTP 200 JSON
  with `0.102.1`, `/health` HTTP 200 with `ok`, and the passkey status
  path HTTP 200 HTML. This is still the older deployment; feature
  acceptance remains unverified.

The unresolved decision is the established SSH access and backend-only
update/backup/rollback path requested through the task chat. The private
application environment file contains backend and NATS connection/auth
settings, not an SSH destination, login or identity. Targeted inspection
of the local shell histories found no Kanade SSH, SCP, fleet deployment,
service or backup command. Repository examples do not establish those
operator-specific values. No alternate deployment mechanism was invented
to make the review finding appear resolved. No production mutation occurred.

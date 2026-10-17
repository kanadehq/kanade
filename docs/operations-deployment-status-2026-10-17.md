# Backend and agent deployment status — 2026-10-17

Deployment is blocked. No remote mutation was attempted: no backend stop or
replacement, database change, release upload, agent target-version update,
service restart, or macOS permission change. Desktop capture is unverified.

## Locally verified release provenance

- Cached `origin/main`: `823c2ba5b6f4a89bfdb3595c483fc75cd1794789`.
- Annotated `v0.79.0` tag object: `a9e48df2b5396398c40cdbc33c39673999ce2fc3`.
- Tag's release commit: `0213e96d8f19f77684ffaa7971f76f6be87391dd`.
- Read-only macOS viewing implementation:
  `f788fb737cec0a91e7eee6bde46c8b43739d0d0d`.
  `git merge-base --is-ancestor f788fb73 v0.79.0` exited 0.
  Online confirmation of its association with PR #1611 was unavailable.
- `git diff --stat v0.79.0..origin/main` lists only
  `.github/workflows/release.yml` and `.kata/applied.toml`.
- `git diff --name-only v0.78.0..v0.79.0 -- crates/kanade-backend/migrations`
  produced no output. The actual deployed baseline remains unknown, so this
  does not establish whether the environment needs migrations.

These are cached references, not confirmation of current upstream main or
available release assets. No asset was downloaded or hash-verified.

## Observed blockers

- `git fetch origin` failed opening the worktree's `FETCH_HEAD` with
  `Operation not permitted`; Git metadata is outside the writable workspace.
- `git ls-remote origin refs/heads/main refs/tags/v0.79.0
  'refs/tags/v0.79.0^{}'` failed with `Could not resolve host: github.com`.
- `gh release view --repo kanadehq/kanade --json
  tagName,targetCommitish,publishedAt,assets` and
  `gh pr view 1611 --repo kanadehq/kanade --json state,mergedAt,mergeCommit`
  failed connecting to the GitHub API.
- Existing local OCI environment configuration contains backend and NATS URLs
  and authentication settings. Secret values were not printed or modified;
  their validity was not established. The OCI profile references existing key
  and session-token files; this does not establish a valid session or SSH access.
- DNS lookup of both configured endpoints failed. A read-only backend
  `/api/health` request failed during name resolution. TCP connection attempts
  to ports 22 and 443 at the IP encoded in the configured endpoint both failed
  with `Operation not permitted`.
- `magi show 5cf2 --json` reported two active implementation seats and status
  `implementing`. No execution-owner designation was established. Preparation
  remained read-only with respect to the environment.
- The installed CLI reports `kanade 0.63.4`; compatibility with the intended
  release's publish and rollout API has not been established.

Backend health, installed backend version/commit, fleet inventory, existing
scope overrides/jitter, and actual agent versions remain unknown. No machine
can be classified as successfully updated, failed, or offline from this
evidence. The requested focus Mac's presence and version are unverified. The
entire requested deployment and fleet rollout remain deferred.

## Resume prerequisites and sequence

Restore upstream/API and environment network access and writable Git metadata,
then designate one execution owner across all competing seats. Other seats
must remain read-only. Re-fetch and pin the current intended release and its
actual artifacts; confirm inclusion of PR #1611 for both backend and macOS
agent. Do not treat this cached release as an automatically selected target.

Follow `deploy/oci/README.md`, `deploy/linux/README.md`,
`scripts/fleet-deploy.ps1`, and
`book/src/operations/agent-mediated-updates/agent-self.md`, checking CLI help
against documentation before execution. Inspect the established host service,
running binary, database location, and fleet policy first. Preserve environment
configuration, secrets, NATS and Caddy. Save the old binary and obtain a
consistent database backup while the backend is stopped before a binary-only
replacement. Compare migrations against the actual installed baseline.

Require backend health, service logs, deployed version and binary hash to
verify the backend before proceeding. Publish the matching release artifacts
for every OS/architecture actually present in the fleet before setting rollout
targets. Preserve registered identifier casing, scope overrides, staging and
jitter. Verify reconnection and reported agent versions, rather than only
target-version acceptance. Report per-machine results and offline/deferred
machines, with particular attention to the requested Mac.

## Operator manual viewing check

After verified deployment, use macOS 14 or later with a logged-in visible
desktop. In System Settings → Privacy & Security → Screen Recording (or
Screen & System Audio Recording), enable the actual process macOS identifies.
If necessary, select the installed agent executable identified in the refusal
message. A Terminal grant alone does not verify the installed launch route.

For the standard documented LaunchDaemon, restart with
`sudo launchctl kickstart -k system/com.kanade.agent` after granting permission.
Stop and reconnect the SPA Remote Screen viewer using an authorized operator
account and the existing audit consent flow. Confirm primary-display updates
using a changing clock or moving window. Permission and capability reporting
alone are not evidence of successful capture. No Accessibility grant or remote
input implementation is needed. See
`book/src/operations/remote-viewing-macos.md` for the acceptance procedure.

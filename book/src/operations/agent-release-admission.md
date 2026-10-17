# Agent release admission

`POST /api/agents/publish` (what `kanade agent publish`, both fleet-deploy
scripts and any direct HTTP caller use) only registers a file in the
`agent_releases` Object Store if the file proves it is the `kanade-agent`
binary. The backend decides; the CLI repeats the same check only to fail
fast. `POST /api/app-packages/{name}/{version}` and `app_packages` are
unaffected and still accept any file.

## What is checked

Before anything is written to the Object Store, the backend parses the
upload (it is never executed, so foreign-platform files are safe):

1. **OS / CPU from the executable itself**: Windows PE x86_64 / aarch64,
   Linux ELF x86_64 / aarch64, thin arm64 macOS Mach-O. File names are
   ignored. The container must be a linked executable: PE executable (no
   DLL), Mach-O `MH_EXECUTE`, or ELF `ET_EXEC` / `ET_DYN` with a nonzero
   entry point for `ET_DYN` (PIE). Intermediate objects (`ET_REL`,
   `MH_OBJECT`), shared libraries and core dumps are rejected even if they
   carry valid agent metadata.
2. **A release-metadata section**, found through the container's section
   table: `.kanade` (PE, ELF) or `__DATA,__kanade` (Mach-O). Missing,
   duplicated, truncated, wrongly framed, unknown-schema or non-JSON
   sections are rejected. Identifying bytes elsewhere in the file (appended
   or in other sections) do not count.
3. **Product** is exactly `kanade-agent`. The `kanade` CLI, backend, client
   and any renamed copy of them are refused.
4. **Platform** in the metadata equals the platform of the container.
5. **Version**: the embedded version is the release version and the key is
   derived from it. A `version` form field / `--version` is optional; if
   given (ignoring surrounding whitespace and one leading `v`) it must equal
   the embedded version. On Windows a VERSIONINFO that contradicts the
   embedded version is refused too.

A refusal is HTTP 400 with the reason and what to do; nothing is stored, and
an existing release under the same key is left byte-for-byte untouched.

This identifies the product the build system stamped; it is not a signature
and does not authenticate a deliberately forged binary (publishing already
requires an authenticated operator).

## Legacy binaries

Agents built before this metadata existed (up to v0.80.0) carry no section
and are **refused**, whether or not `--version` is passed; there is no
bypass flag. Releases already in the Object Store are not re-checked or
changed: being stored does not mean they were verified. To re-publish an old
version, rebuild it from a source tree that embeds the metadata.

## How the metadata gets into the agent

`crates/kanade-agent/build.rs` renders the envelope (`KNAGMETA`, schema 1,
JSON `{product, version, platform}` from `CARGO_PKG_VERSION` and the Cargo
target) and `src/release_metadata.rs` links it into the dedicated section;
`main` reads it so LTO, `--gc-sections`/`-dead_strip` and `strip` keep it.
Only the agent crate embeds it. `scripts/ci/release-post-build.sh` re-reads
the built (and, on macOS, signed) binary after the build and the packaged
archive before upload with `scripts/ci/verify-agent-dist.py`, read-only, and
fails the release if the metadata is missing or wrong. Installed binaries are
never modified or re-signed.

## Upgrade / release ordering

The first release containing this change (the one after v0.80.0, call it
`N`) must be rolled out in this order:

1. Release `N` (CI verifies the agent artifacts carry the metadata).
2. Update the backend to `N`. From now on only metadata-bearing agents can be
   published; old agents already in the store keep working.
3. Publish the `N` agents with `kanade agent publish` (or the fleet-deploy
   scripts) for each platform.
4. Roll out with `kanade agent rollout N …` as usual. Running agents older
   than `N` need no change: they self-update from the store exactly as
   before, and the store now only receives real agents.

Publishing before step 2 would still work but would be unprotected; do not
publish a legacy-built agent after step 2 — it is refused.

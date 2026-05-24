# `jobs/` — operator manifest catalog

This directory holds the `kanade Job` manifests operators register
into the backend via `kanade job create <yaml>` (SPEC §2.4.1).

## Layout

```
jobs/
├── README.md                       — this file
├── echo-test.yaml                  — smoke-test manifest
├── schedule-test.yaml              — schedule-side smoke test
├── wave-test.yaml                  — rollout-wave smoke test
├── install-kanade-client.yaml      — client-app install/upgrade (#210)
└── scripts/
    └── install-kanade-client.ps1   — inlined via `script_file:` (#215)
```

A manifest's `execute.script_file:` is resolved relative to the
manifest's own directory, so the `jobs/<name>.yaml` +
`jobs/scripts/<name>.ps1` pairing is the conventional layout.

## install-kanade-client — end-to-end flow

Pulls the kanade-client Tauri app binary from `OBJECT_APP_PACKAGES`
(#207) onto endpoints. Three steps from a fresh release to a
deployed client:

1. **Upload the binary.** From an operator host that can reach the
   backend HTTP port:

   ```bash
   curl -F "file=@target/release/kanade-client.exe" \
     http://<backend>/api/app-packages/kanade-client/0.42.0
   ```

   The bucket is keyed by `<name>/<version>` — pick the version
   string per release (semver / calendar / git sha all work; see
   `kanade-shared::kv::OBJECT_APP_PACKAGES` for the constraints).

2. **Pin the version + sha in the script.** Edit three knobs at
   the top of `scripts/install-kanade-client.ps1`
   (`--- Configurable knobs ---`):

   - `$Version` — string you uploaded under in step 1.
   - `$BackendBase` — only if your backend isn't at the default
     URL.
   - `$ExpectedSha256` — operator-computed digest of the binary,
     pasted in hex. Get it with:

     ```powershell
     Get-FileHash target\release\kanade-client.exe -Algorithm SHA256
     ```

   The script refuses to install unless the downloaded bytes
   match `$ExpectedSha256` — a poisoned upload / MITM-substituted
   binary fails fast instead of being silently promoted under
   `LocalSystem`. Leaving the field blank is also a hard error
   (`fail fast at the verification step` per the script doc).

3. **Register + deploy.**

   ```bash
   kanade job create jobs/install-kanade-client.yaml
   kanade exec install-kanade-client --target groups=canary
   ```

   The agent runs the script under `LocalSystem`, downloads the
   binary, and reports `{ version, path }` JSON that the
   inventory projector renders on the SPA's Inventory page —
   operators see "which kanade-client version is on each PC" at a
   glance.

   `require_approval: true` on the manifest means an operator with
   the SPA's approver role must ack the exec before the fan-out
   actually publishes (SPEC §2.5).

## Why `script_file:` instead of inlining the PowerShell

Inlining a multi-line `.ps1` into YAML is read-hostile — backslash
escapes, here-string quoting, and PowerShell's `$(...)` clashing
with YAML's flow-mapping syntax all fight the operator. `script_file:`
keeps the script editable as a normal `.ps1` (linters, syntax
highlighting, `Invoke-Pester` for tests) while still landing a
single fully-resolved Manifest in `BUCKET_JOBS`.

The CLI's resolver (#215) reads the file at `kanade job create`
time and inlines it into `execute.script` before POSTing — the
backend never sees `script_file:`. The script text is what the
agent executes; the file is just the authoring format.

## Why not `script_object:` for the install script

`script_object:` (OBJECT_SCRIPTS, #211) is for scripts whose body
is too large to round-trip through `STREAM_EXEC` on every fire, or
that the operator wants under explicit version control in the
bucket separate from the manifest. The install script is small
(~60 lines, < 4 KB) and version-coupled to the manifest, so
`script_file:` is the lighter choice — agents fetch the manifest
once and have everything they need. `script_object:` shines for
distributing third-party PowerShell modules / longer remediation
scripts to many manifests.

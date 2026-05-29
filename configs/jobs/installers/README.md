# `configs/jobs/installers/` — kanade-component install manifests

This directory holds the `kanade Job` manifests that install /
update the kanade components themselves (backend, client). They
sit under `configs/jobs/` alongside the example operational
manifests but in a dedicated `installers/` subdir so the two
intents read separately at a glance: the sibling `inventory-*`,
`collect-winlog-events`, `kitting-setup`, `urgent-patch` are
**examples**; the manifests here are **first-class infrastructure**
that the project itself depends on for upgrades.

Operators register them via `kanade job create <yaml>` (SPEC §2.4.1).

## Layout

```
configs/jobs/installers/
├── README.md                       — this file
├── install-kanade-client.yaml      — client-app install/upgrade (#210, script_file)
├── install-kanade-backend.yaml     — backend self-update (#210, script_object)
└── scripts/
    └── install-kanade-client.ps1   — inlined into the client manifest via `script_file:` (#215)
```

The backend self-update doesn't ship a co-located `.ps1` here —
its install script is the existing `scripts/deploy/backend.ps1`
at the repo root (used for manual installs too). The manifest
references it via `script_object:` after the operator publishes
an edited copy to `OBJECT_SCRIPTS` (see the section below).

## install-kanade-client — end-to-end flow

Pulls the kanade-client Tauri app binary from `OBJECT_APP_PACKAGES`
(#207) onto endpoints. Three steps from a fresh release to a
deployed client:

1. **Upload the binary.**

   ```bash
   kanade app publish kanade-client 0.42.0 \
     target/release/kanade-client.exe
   ```

   The bucket is keyed by `<name>/<version>` — pick the version
   string per release (semver / calendar / git sha all work; see
   `kanade-shared::kv::OBJECT_APP_PACKAGES` for the constraints).
   `kanade app` (#222) talks straight to NATS — no backend HTTP
   round-trip — so this works even when the backend itself is
   restarting.

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
   `LocalSystem`. Leaving the field blank is also a hard error.

3. **Register + deploy.**

   ```bash
   kanade job create configs/jobs/installers/install-kanade-client.yaml
   kanade exec install-kanade-client --target groups=canary
   ```

   The agent runs the script under `LocalSystem`, downloads the
   binary, and reports `{ version, path }` JSON that the
   inventory projector renders on the SPA's Inventory page —
   operators see "which kanade-client version is on each PC" at a
   glance.

## install-kanade-backend — end-to-end flow

Self-update path for the backend itself, riding on the agent that
runs on the backend host. The backend's brief restart window is
safe because the agent → NATS → backend pipeline has no synchronous
HTTP dependency: agent publishes the script result to NATS while
the backend is down, JetStream persists it, and the backend
projects the missed row when it comes back up.

Five steps per backend release:

1. **Upload the binary.**

   ```bash
   kanade app publish kanade-backend 0.43.0 \
     target/release/kanade-backend.exe
   ```

2. **Stamp the install script with this release's coordinates.**
   Copy `scripts/deploy/backend.ps1` locally and set the three
   `$Agent*` knobs near the top of the file:

   ```powershell
   $AgentSourceUrl     = 'http://kanade-backend.local:8080'
   $AgentSourceVersion = '0.43.0'
   $AgentSourceSha256  = (Get-FileHash target\release\kanade-backend.exe -Algorithm SHA256).Hash
   ```

   Leave them blank in the canonical `scripts/deploy/backend.ps1`
   (the manual `-SourceDir` install flow depends on the
   blank-default branch). The edits only live in the per-release
   copy you publish in step 3.

3. **Publish the edited script to `OBJECT_SCRIPTS`.**

   ```bash
   kanade script publish deploy-backend 0.43.0 ./deploy-backend.0.43.0.ps1
   ```

   `kanade script publish` (#222) returns the sha256 the manifest
   resolver will pin against — agents reject any mid-rollout
   re-upload that changes the bytes (#214).

4. **Update the manifest's `version:` + `script_object:` pair to
   match, then register.**

   ```bash
   kanade job create configs/jobs/installers/install-kanade-backend.yaml
   ```

5. **Exec against the backend host.**

   ```bash
   kanade exec install-kanade-backend --target pcs=<backend-host>
   ```

   The agent (running on the backend host as LocalSystem) fetches
   the deploy script from `OBJECT_SCRIPTS`, sha-verifies it,
   runs it — which in turn pulls `kanade-backend.exe` from
   `OBJECT_APP_PACKAGES`, sha-verifies that, reuses the existing
   `%ProgramData%\Kanade\config\backend.toml`, stops the
   `KanadeBackend` service, swaps the binary, starts the service.
   Stdout (the human-readable install log + `kanade-backend
   --version`) lands in the Activity page on the next backend
   projector tick — usually within a few seconds of the new
   backend coming up.

### Bootstrap note

Agent-mode is strictly an **upgrade** path. The first install of
the backend has to happen manually (`scripts/deploy/backend.ps1`
with `-SourceDir <folder containing exe + toml>`) — agent-mode
errors out hard when no existing `backend.toml` is present at the
canonical destination, because guessing a fresh config from the
update path would silently overwrite operator settings.

## script_file vs script_object

The two install manifests pick different transports for their
script bodies — both are first-class in SPEC §2.4.1, and the
pick is per-script:

- **`script_file:` (install-kanade-client)** — small (~60 lines,
  < 4 KB), version-coupled to the manifest, lives in the same
  repo. CLI's resolver (#215) reads the file at `kanade job
  create` time and inlines it into `execute.script` before
  POSTing. Agent fetches the manifest once and has the script.

- **`script_object:` (install-kanade-backend)** — the install
  logic is `scripts/deploy/backend.ps1` which the operator also
  runs manually for fresh installs. Keeping it under explicit
  version control in `OBJECT_SCRIPTS` (separate lifecycle from
  the manifest) means we publish per-release copies with the
  release's coordinates baked in, while the canonical repo copy
  stays neutral for the manual path. Agent fetches the script
  body at exec time, sha-verifies, executes (#214).

Inlining a multi-line `.ps1` into YAML is read-hostile either
way; both `script_file:` and `script_object:` keep the script
editable as a normal `.ps1` (linters, syntax highlighting,
`Invoke-Pester` for tests).

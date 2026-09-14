# `configs/jobs/installers/` — kanade-component install manifests

This directory holds the `kanade Job` manifests that install /
update the kanade components themselves (backend, client, CLI). They
sit under `configs/jobs/` in a dedicated `installers/` subdir so the
intent reads unambiguously at a glance: everything here is
**first-class infrastructure** the project itself depends on for
upgrades. Operator-authored example manifests (health checks,
inventory, patching, troubleshooting, …) live in a separate showcase
repo, [`kanadehq/kanade-manifests`](https://github.com/kanadehq/kanade-manifests),
not here.

Operators register them via `kanade job create <yaml>` (SPEC §2.4.1).

## Layout

```
configs/jobs/installers/
├── README.md                       — this file
├── install-kanade-client.yaml      — client-app install/upgrade (#210, script_file)
├── install-kanade-backend.yaml     — backend self-update (#210, script_object)
├── install-kanade-agent.yaml       — agent emergency out-of-band swap (#566, script_object)
├── install-kanade-cli.yaml         — admin-CLI install/upgrade (script_file)
└── scripts/
    ├── install-kanade-client.ps1   — inlined into the client manifest via `script_file:` (#215)
    └── install-kanade-cli.ps1      — inlined into the CLI manifest via `script_file:`
```

The backend self-update and the agent emergency swap don't ship a
co-located `.ps1` here — their scripts are `scripts/deploy/backend.ps1`
and `scripts/deploy/agent-emergency-swap.ps1` at the repo root (used
for manual installs too). The manifests reference them via
`script_object:` after the operator publishes an edited copy to
`OBJECT_SCRIPTS` (see the per-component sections below).

## Quick path: `scripts/fleet-deploy.ps1`

The per-role end-to-end flows below are the *manual* breakdown. For a
**repeat deploy** (2nd onwards), `scripts/fleet-deploy.ps1` does the whole
sequence in one command — it's the agent-route companion to
`build-release.ps1` (which only *stages* the binary into `dist/<role>/`):

```powershell
# fetch the newest release + deploy it in one shot (auto-stages):
.\scripts\fleet-deploy.ps1 -Role backend -Version latest -Pc <pc-id>
# or stage a specific version, then publish + roll it out
# (version auto-read from the staged exe when omitted):
.\scripts\build-release.ps1 -Roles backend -Version 0.43.17
.\scripts\fleet-deploy.ps1 -Role backend -Pc <pc-id>     # or set $env:KANADE_TARGET_PC
.\scripts\fleet-deploy.ps1 -Role backend -Pc <pc-id> -WipeDb -JwtSecret dev -BootstrapAdminPassword dev
.\scripts\fleet-deploy.ps1 -Role client -Groups canary -SourceUrl http://<backend-host>:8080
.\scripts\fleet-deploy.ps1 -Role agent -Version latest -Pc <pc-id>   # try on one box
.\scripts\fleet-deploy.ps1 -Role agent -All -Jitter 30m              # then fleet-wide rollout
.\scripts\fleet-deploy.ps1 -Role cli -Version latest -Pc <pc-id>     # admin CLI onto an operator host
.\scripts\fleet-deploy.ps1 -Role backend -Pc <pc-id> -DryRun         # print every command, change nothing
```

Auto-computed so you don't pass them: the SHA-256 (`Get-FileHash`), the
version (PE VERSIONINFO of the staged exe, or `-Version latest`), the exe
path (`dist/<role>/`), the tokens (`$env:KANADE_*_TOKEN` → `dev`), and the
SourceUrl port (read from the staged `backend.toml`).

**From an ops-management terminal** (not the broker host) point the CLI at
the broker — `-Server` (or `$env:KANADE_NATS_URL`) carries the
publish/job/exec traffic:

```powershell
.\scripts\fleet-deploy.ps1 -Role backend -Version latest `
  -Server nats://broker.corp:4222 -NatsToken $tok -Pc some-host
```

Three endpoints, all localhost by default, all overridable: `-Server`
(this terminal → NATS broker), `-BackendUrl` (this terminal → backend
HTTP), and `-SourceUrl` (the *target host's* agent → its co-located
backend — usually leave at `127.0.0.1`). The local-install verify step is
auto-skipped when the target isn't this machine; confirm the rollout on
the SPA Inventory page instead.

What it automates per role:

- **backend / client / cli** — `kanade app publish` → inject the deploy script's
  download knobs (`SourceUrl`/`Version`/`Sha256`/`AuthToken`, plus
  backend-only `-WipeDb` / `-JwtSecret` / `-StaticToken` /
  `-BootstrapAdminPassword`) into a **temp** copy → publish it
  (backend `script_object`, client inlined `script_file`) → render a
  version-pinned **temp** manifest → `kanade job create` → `kanade exec
  --pcs <pc>` (verbatim casing) / `--groups <g>` → poll the installed exe
  to verify. Nothing under version control is mutated.
- **agent** — `kanade agent publish` → `kanade agent rollout <ver>` with
  the scope mapped from the same flags (`-Pc` → `--pc`, one `-Groups` →
  `--group`, `-All` → `--global`, plus optional `-Jitter`). Verification:
  `kanade agent current` reports the **global** target_version only, so
  `-All` rollouts are verified there; pc/group rollouts are confirmed on
  the SPA Agents page (agent version column). The agent updates itself —
  there is no install job for it.

Tokens default to the `dev` literals (or `$env:KANADE_AUTH_TOKEN` /
`$env:KANADE_NATS_TOKEN`). Skip a step's wipe/secrets to leave the existing
registry values untouched. Use the manual flows below when you need to
deviate (custom knobs, partial steps, debugging).

## install-kanade-client — end-to-end flow

Pulls the kanade-client Tauri app binary from `OBJECT_APP_PACKAGES`
(#207) onto endpoints. Three steps from a fresh release to a
deployed client:

1. **Upload the binary.**

   ```bash
   kanade app publish kanade-client \
     target/release/kanade-client.exe
   ```

   The bucket is keyed by `<name>/<version>`. The version is read
   from the binary's embedded VERSIONINFO (#261) so it can't drift
   from what you built; pass `--version <label>` to override, or for
   inputs without PE metadata (see
   `kanade-shared::kv::OBJECT_APP_PACKAGES` for what a version
   string may contain — semver / calendar / git sha all work).
   `kanade app` (#222) talks straight to NATS — no backend HTTP
   round-trip — so this works even when the backend itself is
   restarting.

2. **Pin the version + sha + token in the script.** Edit the
   knobs at the top of `scripts/install-kanade-client.ps1`
   (`--- Configurable knobs ---`):

   - `$Version` — string you uploaded under in step 1.
   - `$BackendBase` — only if your backend isn't at the default
     URL.
   - `$ExpectedSha256` — operator-computed digest of the binary,
     pasted in hex. Get it with:

     ```powershell
     Get-FileHash target\release\kanade-client.exe -Algorithm SHA256
     ```

   - `$ClientSourceAuthToken` — bearer for the backend's
     `/api/app-packages/<name>/<version>` route. Required when
     backend auth is enabled (production posture); leave empty
     for dev / smoke-test setups where the route is open. Same
     token the agent uses against `/api/*` from
     `KANADE_AUTH_TOKEN`.

   The script refuses to install unless the downloaded bytes
   match `$ExpectedSha256` — a poisoned upload / MITM-substituted
   binary fails fast instead of being silently promoted under
   `LocalSystem`. Leaving the field blank is also a hard error.

3. **Register + deploy.**

   ```bash
   kanade job create configs/jobs/installers/install-kanade-client.yaml
   kanade exec install-kanade-client --groups canary
   ```

   The agent runs the script under `LocalSystem`, downloads the
   binary, and reports `{ version, path }` JSON that the
   inventory projector renders on the SPA's Inventory page —
   operators see "which kanade-client version is on each PC" at a
   glance.

## install-kanade-cli — end-to-end flow

Puts the `kanade` admin CLI on an operator host and keeps it current.

**Why this job exists.** The CLI is the one kanade component with no
rollout path of its own — agents self-update via `kanade agent rollout`,
the backend has `install-kanade-backend`, the client has
`install-kanade-client`, and the CLI got hand-copied onto whichever box
needed to run operator commands. That is how a host ends up driving a
newer backend with a CLI old enough that `job validate` and `job create`
disagree about the manifest schema. With this job the CLI rides the same
publish → job → exec route as everything else, and the `inventory:` block
answers "which CLI is on which host" from the SPA.

**Target operator hosts, not the fleet.** `--pcs <id>`, not `--all`. The
binary carries no credentials of its own (it reads `$env:KANADE_*_TOKEN`
or a `kanade login` session), so a stray copy is not a privilege grant,
but an admin CLI on end-user endpoints is noise at best.

The one-command path:

```powershell
.\scripts\fleet-deploy.ps1 -Role cli -Version latest -Pc <operator-host>
```

That stages `kanade-x86_64-pc-windows-msvc.zip` into `dist\cli\kanade.exe`
(note: **not** `kanade-cli.exe` — the crate is plain `kanade`), publishes
it as the `kanade-cli` app package, injects `BackendBase` / `Version` /
`ExpectedSha256` / `CliSourceAuthToken` into a temp copy of
`configs/jobs/installers/scripts/install-kanade-cli.ps1`, renders a
version-pinned temp manifest, `job create`s it and execs at the target.

Manual breakdown, if you need to deviate:

1. **Upload the binary.**

   ```bash
   kanade app publish kanade-cli dist/cli/kanade.exe
   ```

   The version is read from the binary's embedded VERSIONINFO (#261), so
   it can't drift from what you built. Pass `--version <X.Y.Z>` only for
   inputs without PE metadata.

2. **Pin the knobs** at the top of
   `configs/jobs/installers/scripts/install-kanade-cli.ps1` —
   `$BackendBase`, `$Version`,
   `$ExpectedSha256` (`Get-FileHash dist\cli\kanade.exe -Algorithm
   SHA256`), `$CliSourceAuthToken`. A blank `$ExpectedSha256` is a hard
   error, same posture as the client installer.

3. **Register + deploy.**

   ```bash
   kanade job create configs/jobs/installers/install-kanade-cli.yaml
   kanade exec install-kanade-cli --pcs <operator-host>
   ```

The script installs to `%ProgramFiles%\Kanade\kanade.exe` with the same
stage → verify sha256 → atomic swap → rollback-on-failure shape as the
client installer, and adds `%ProgramFiles%\Kanade` to the **machine**
PATH (knob `$AddToMachinePath`, default on) — a CLI that isn't on PATH is
half-installed, since scheduled jobs and service-hosted scripts run with
no interactive profile. Two notes on that:

- The PATH write goes through the registry with
  `DoNotExpandEnvironmentNames` and re-writes with the original value
  kind, **not** `[Environment]::SetEnvironmentVariable(..., 'Machine')` —
  that API reads the value expanded and writes it back as plain `REG_SZ`,
  permanently baking this machine's `%SystemRoot%` / `%ProgramFiles%`
  into every other entry.
- No `WM_SETTINGCHANGE` broadcast: the job runs as LocalSystem in session
  0 and `HWND_BROADCAST` doesn't cross sessions. Already-running
  processes keep the environment they inherited; a new logon (or service
  restart) picks the entry up.

Renaming a running image is legal on Windows, so the swap succeeds even
if an operator has a `kanade` running at the time — the running process
keeps its now-renamed file open and exits normally. That renamed file is
`kanade.exe.old.<8 hex>`, unique per run rather than a fixed
`kanade.exe.old`, precisely because it is the file most likely to still
be **locked**: with a fixed name the *next* install's rename would target
it and die with a sharing violation, i.e. an install refused by the
debris of the last one. Each run sweeps whatever earlier rollback files
have since been released, so they don't accumulate.

`-Role cli` rejects `-All` outright (`-Pc` / `-Groups` only) — a
forgotten flag must not be how an admin CLI reaches every endpoint.

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
   kanade app publish kanade-backend \
     target/release/kanade-backend.exe
   ```

   Version from the binary's VERSIONINFO (#261); `--version` to
   override.

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
   kanade exec install-kanade-backend --pcs <backend-host>
   ```

   > **Target flag gotcha.** It's `--pcs <id>` / `--groups <grp>` —
   > NOT `--target pcs=<id>`. And the agent registers its `pc_id` as
   > its `$env:COMPUTERNAME` **verbatim** — casing is NOT uniform
   > across the fleet (some boxes upper-, some lower-case), so pass
   > the **exact registered casing** (check the SPA Inventory /
   > `kanade ping`); do NOT case-fold it. NATS subjects are
   > case-sensitive, so a mis-cased name publishes to a subject no
   > agent is subscribed to and the exec sticks at `pending` with no
   > error.

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

## install-kanade-agent — emergency out-of-band swap

The normal agent upgrade path is **`kanade agent rollout`**
(`fleet-deploy.ps1 -Role agent`), which sets a `target_version` and
lets the running agent **self-update**. That path is useless when the
bug is *in self-update itself* — e.g. #566, where a base64-padding
mismatch in the staged-binary digest check made the agent reject every
download and stay pinned to 0.43.46 with no way to pull the fix that
would un-wedge it (a "can't update the updater" bootstrap).

`install-kanade-agent` is that bootstrap. It rides on the **same wedged
agent** (which is still running, just refusing self-updates) and swaps
its binary out-of-band. The twist vs. the backend job: the agent can't
stop *its own* service inline — that would kill the very script doing
the work (the script is a child of `KanadeAgent`). So
`scripts/deploy/agent-emergency-swap.ps1` instead **stages the verified
binary and registers a one-shot SYSTEM Scheduled Task** that performs
the `stop → back up → copy → start` a short delay later, fully detached
from the agent's process tree. The detached runner rolls back to the
saved binary if the new one won't start, logs to
`%ProgramData%\Kanade\logs\agent-emergency-swap.log`, and self-cleans
(task + staging + backup) on success.

Five steps, same shape as the backend flow:

1. **Upload the target binary to app-packages.**

   ```bash
   kanade app publish kanade-agent dist/agent/kanade-agent.exe
   ```

2. **Stamp the swap script with this release's coordinates.** Copy
   `scripts/deploy/agent-emergency-swap.ps1` locally and set the
   `$Agent*` knobs near the top:

   ```powershell
   $AgentSourceUrl       = 'http://127.0.0.1:8080'
   $AgentSourceVersion   = '0.43.48'
   $AgentSourceSha256    = (Get-FileHash dist\agent\kanade-agent.exe -Algorithm SHA256).Hash.ToLower()
   $AgentSourceAuthToken = 'dev'   # bearer for /api/app-packages
   ```

3. **Publish the edited script to `OBJECT_SCRIPTS`.**

   ```bash
   kanade script publish deploy-agent-emergency 0.43.48 ./deploy-agent-emergency.0.43.48.ps1
   ```

4. **Match the manifest's `version:` + `script_object:` pair, register.**

   ```bash
   kanade job create configs/jobs/installers/install-kanade-agent.yaml
   ```

5. **Exec against the wedged agent's host** (`--pcs <id>`, verbatim
   casing — same gotcha as above).

   ```bash
   kanade exec install-kanade-agent --pcs <wedged-agent-host>
   ```

   The job result reports "swap scheduled in 60s" and publishes while
   the agent is still up; the detached task then bounces the service
   onto the new binary. Confirm the flip on the SPA Agents page (or
   `/api/agents` → `agent_version`) ~60–90s after exec, and check the
   swap log if it doesn't move.

> **Why a separate `kanade-agent` app-package?** The rollout path ships
> the binary to `OBJECT_AGENT_RELEASES` (what self-update reads); this
> job downloads over plain HTTP from `OBJECT_APP_PACKAGES` like the
> backend script does, so step 1 publishes it there explicitly. A
> future `fleet-deploy.ps1 -Role agent -Emergency` one-liner can fold
> steps 1–5 together (tracked as follow-up).

## script_file vs script_object

The two install manifests pick different transports for their
script bodies — both are first-class in SPEC §2.4.1, and the
pick is per-script:

- **`script_file:` (install-kanade-client, install-kanade-cli)** — small (~60 lines,
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

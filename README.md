<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/kanade/main/assets/logo-dark.svg">
  <img src="https://raw.githubusercontent.com/yukimemi/kanade/main/assets/logo.svg" alt="kanade — orchestrate fleets of Windows endpoints" width="540">
</picture>

[![CI](https://github.com/yukimemi/kanade/actions/workflows/ci.yml/badge.svg)](https://github.com/yukimemi/kanade/actions/workflows/ci.yml)
[![codecov](https://codecov.io/gh/yukimemi/kanade/graph/badge.svg)](https://codecov.io/gh/yukimemi/kanade)
[![crates.io](https://img.shields.io/crates/v/kanade.svg)](https://crates.io/crates/kanade)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/yukimemi/kanade/blob/main/LICENSE)

</div>

> 奏 — *orchestrate*. A self-hosted Rust pub/sub backbone for managing
> thousands of Windows endpoints without Active Directory. NATS / JetStream
> carries inventory polling, fleet-wide rollouts, and ad-hoc emergency
> commands on a single channel.

**Status: 0.1.0 — Sprint 4 shipped.** Agent + backend (axum + SQLite
projector + JetStream KV watcher + cron scheduler) + admin CLI + an
embedded SPA dashboard + JWT-gated `/api/*` + agent self-update via the
JetStream Object Store. Full design lives in
[docs/SPEC.md](https://github.com/yukimemi/kanade/blob/main/docs/SPEC.md) (Japanese, ~1150 lines covering Part 1
overview and Part 2 detailed design).

## Why

The off-the-shelf endpoint managers (Intune, Tanium, Workspace ONE, …)
either require Active Directory, lock you into a vendor cloud, or both.
For shops that want AD-independent, on-prem, scriptable fleet control
the answer has historically been "build something on top of a message
broker" — which everyone reinvents from scratch.

`kanade` aims to be the reusable shape of that build:

- **NATS + JetStream as the only moving part.** Agents speak to the
  broker over outbound TLS; the broker fans out commands, fans in
  inventory and results. No AD, no client-pull-from-server, no opening
  inbound ports on user PCs.
- **Declarative job manifests in Git.** Review, history, rollback all
  come for free; the YAML schema (`jobs/*.yaml`) is the same input
  whether you `kanade exec` ad-hoc or wire it onto a cron `kanade
  schedule`.
- **Three layers of stop-the-bleed.** Stream max-msgs-per-subject
  replaces stale rollouts in the broker; consumer-side version checks
  guard execution; `kanade kill <job_id>` terminates running children.
  The emergency-stop path is wired from MVP, not bolted on later (see
  [SPEC.md §2.6](https://github.com/yukimemi/kanade/blob/main/docs/SPEC.md)).
- **Phased build-out.** One server is enough for a few hundred
  endpoints; the same code scales to a 3-node NATS cluster + replicated
  backend + Postgres for several thousand.

## Crates

| crate            | kind | role |
|------------------|------|------|
| `kanade-shared`  | lib  | wire types (`Command` / `ExecResult` / `Heartbeat` / `HwInventory`), NATS subject + KV helpers, YAML manifest schema, [teravars]-backed config loader |
| `kanade-agent`   | bin  | Windows-side resident daemon: subscribes to `commands.*`, runs child processes, publishes results + heartbeats + WMI inventory; watches the layered `agent_config` + `agent_groups` KV buckets and reacts live to cadence / membership / target_version changes |
| `kanade-backend` | bin  | axum HTTP server: `/health`, `/api/{agents,results,audit,deploy,schedules,config,…}`, embedded SPA at `/`. Auto-bootstraps every required JetStream resource at startup, runs durable projectors (INVENTORY/RESULTS/AUDIT → SQLite) and a `tokio-cron-scheduler` driven by the schedules KV |
| `kanade`         | bin  | operator-side admin CLI (`kubectl`-style single entry point); subcommands talk to NATS directly for `run`/`ping`/`kill`/`revoke`/`jetstream`/`agent`/`config` and to the backend over HTTP for `deploy`/`schedule` |

## Install

You'll need:

- Rust 1.85+ (the workspace pins `edition = "2024"`)
- A NATS server (Go binary, ~15 MB)

```powershell
# 1. NATS server
scoop install nats-server         # or: winget install nats-io.nats-server

# 2. The three kanade binaries — straight from crates.io.
cargo install kanade kanade-agent kanade-backend
```

`kanade`, `kanade-agent`, and `kanade-backend` are now on your PATH
(under `~/.cargo/bin/`).

You'll also want the sample configs (`configs/agent.toml` / `configs/backend.toml`) and
the example manifests (`jobs/*.yaml`). The fastest way is a shallow
clone of this repo:

```powershell
git clone --depth=1 https://github.com/yukimemi/kanade.git
cd kanade
```

(or `curl` the individual files from
`https://raw.githubusercontent.com/yukimemi/kanade/main/...` into your
own working dir if you'd rather not clone).

> **Build it yourself from source.** Skip the `cargo install` step,
> `git clone` the full repo, and run `cargo install --path crates/kanade
> --path crates/kanade-agent --path crates/kanade-backend` (one
> `--path` at a time, or repeat the command three times). That path
> matters if you're hacking on the crates.

## Quick start (5 terminals, ~2 minutes)

Run each step in its own PowerShell window so the daemons stay up. All
of them assume `cd` into the repo root (which holds `configs/agent.toml`
/ `configs/backend.toml` / `jobs/`).

### 1 — start NATS

```powershell
nats-server -js -p 4222
```

### 2 — provision JetStream (optional)

```powershell
kanade jetstream setup
```

Creates every stream (`INVENTORY` / `RESULTS` / `DEPLOY` / `EVENTS` /
`AUDIT`), KV bucket (`script_current` / `script_status` / `agents_state`
/ `agent_config` / `agent_groups` / `schedules`), and the
`agent_releases` Object Store. This step is **optional** as of v0.3.1:
`kanade-backend` auto-bootstraps the same set at startup, so a fresh
NATS server + `kanade-backend` is enough to get a working fleet. The
CLI command is still useful for re-running setup against a different
broker, or for inspecting what would be created (`kanade jetstream
status`).

### 3 — start the backend

```powershell
$env:KANADE_AUTH_DISABLE = "1"   # JWT off for development
kanade-backend
```

Serves the dashboard at <http://127.0.0.1:8080> and the JSON API at
`/api/*`. SQLite is created at `./backend.db`. Both projectors and the
cron scheduler start in the background.

### 4 — start the agent

```powershell
kanade-agent
```

Loads `./configs/agent.toml`, picks `$env:COMPUTERNAME` as `pc_id`,
subscribes to `commands.all` + `commands.pc.{pc_id}`, then spawns the
config_supervisor (watches `agent_config` + `agent_groups` KV) plus
the heartbeat / inventory / self-update / groups-manager loops. Group
membership and cadence settings are read from the KV buckets — see
`kanade group` and `kanade config` to drive them.

### 5 — drive it

```powershell
# Round-trip a script via NATS, request/reply.
kanade run $env:COMPUTERNAME -- 'echo hello from kanade'

# Or via the backend's YAML deploy path (writes a row to deployments,
# emits an audit event, broadcasts the Command).
kanade exec jobs/echo-test.yaml

# Heartbeat probe.
kanade ping $env:COMPUTERNAME

# Inspect via curl…
curl http://127.0.0.1:8080/api/agents
curl http://127.0.0.1:8080/api/results
curl http://127.0.0.1:8080/api/audit

# …or open the dashboard.
start http://127.0.0.1:8080
```

## CLI cheat sheet

```text
kanade run    <pc_id> -- <script>                # request/reply via NATS
kanade ping   <pc_id>                            # wait for one heartbeat
kanade kill   <job_id>                           # publish kill.{job_id}
kanade revoke <cmd_id>                           # script_status = REVOKED
kanade unrevoke <cmd_id>                         # → ACTIVE

kanade jetstream setup                           # create streams + KV + Object Store (optional; backend auto-bootstraps on startup)
kanade jetstream status                          # health snapshot

kanade job create   <manifest.yaml>              # upsert into the jobs catalog (BUCKET_JOBS)
kanade job list                                  # every registered job
kanade job delete <id>                           # refuses if any schedule references it

kanade exec     <job-id>                         # fire a registered job ad-hoc (POST /api/exec/<id>)

kanade schedule create <schedule.yaml>           # cron yaml: { id, cron, job_id, enabled }
kanade schedule list
kanade schedule delete <id>

kanade agent publish <binary> [--version <v>]    # upload binary to Object Store (no KV touch)
kanade agent rollout <v> --global  [--jitter <d>]            # fleet-wide
kanade agent rollout <v> --group <name> [--jitter <d>]       # canary / wave
kanade agent rollout <v> --pc    <pc_id> [--jitter <d>]      # single-host pin
kanade agent current                             # read agent_config.global.target_version

kanade group list                                # fleet-wide: every known group + member count + config flag
kanade group list --pc <pc_id>                   # one PC's memberships
kanade group members <name>                      # PCs in this group
kanade group add  <pc_id> <name>                 # add membership (idempotent)
kanade group rm   <pc_id> <name>                 # drop membership
kanade group set  <pc_id> <name> ...             # replace whole list

kanade config get  [--group <name>|--pc <pc_id>] # ConfigScope at this scope (default: global)
kanade config set  <field>=<value> [...]         # set one field (target_version / inventory_* / heartbeat_*)
kanade config unset <field> [...]                # clear one field
kanade config clear [--group <name>|--pc <pc_id>] # delete the whole scope row
kanade config effective <pc_id>                  # resolved view for a PC (built-in -> global -> groups -> pc)
```

`kanade <subcommand> --help` for argument details.

## Authoring jobs

YAML manifests in `jobs/*.yaml` (see [spec §2.4.1](https://github.com/yukimemi/kanade/blob/main/docs/SPEC.md)).
Sample manifests in the repo cover:

- `jobs/echo-test.yaml` — minimal ad-hoc command
- `jobs/wave-test.yaml` — `rollout.waves` rollout (canary → wave1 with delay)
- `jobs/schedule-test.yaml` — cron-driven echo every 10 s

A wave manifest sketch:

```yaml
id: cleanup-disk-temp
version: 1.0.1
target:
  pcs: [PC1234]
execute:
  shell: powershell
  script: |
    $temp = [System.IO.Path]::GetTempPath()
    Remove-Item "$temp\*" -Recurse -Force -ErrorAction SilentlyContinue
  timeout: 600s
  jitter: 5m
rollout:
  strategy: wave
  waves:
    - { group: canary, delay: 0s  }
    - { group: wave1,  delay: 30m }
```

## Config files

Both use [teravars] templating — `{{ system.host }}`, `{{ env(name="X", default="Y") }}`, `{% if is_windows() %}…{% endif %}` are all available.

`agent.toml` (intentionally minimal — fleet policy lives in the
`agent_config` + `agent_groups` KV buckets, edited via
`kanade config` / `kanade agent groups`):

```toml
[agent]
id = '{{ system.host }}'
nats_url = 'nats://127.0.0.1:4222'

[log]
path = 'logs/agent.log'
level = 'info'
```

Older agent.toml files that still carry `[agent] groups = […]` or an
`[inventory]` section keep loading — both fields are parsed via
`#[serde(default)]` — but the values are logged-and-ignored at
startup. Removal is scheduled for v0.4.0.

`backend.toml`:

```toml
[server]
bind = '0.0.0.0:8080'

[nats]
url = 'nats://127.0.0.1:4222'

[db]
sqlite_path = './backend.db'

[log]
path = 'logs/backend.log'
level = 'info'
```

## Authentication

`/api/*` is protected by a single middleware (`crates/kanade-backend/src/auth.rs`).
Three modes:

| Mode | Selector | Use for |
|---|---|---|
| open | `KANADE_AUTH_DISABLE=1` | local dev, `cargo run` |
| static bearer | `StaticToken` registry value or `$KANADE_AUTH_STATIC_TOKEN` | single-operator fleets — paste the same secret on the SPA login + `kanade` CLI |
| HS256 JWT | `JwtSecret` registry value or `$KANADE_JWT_SECRET` | full multi-user setup; sign tokens out-of-band with `aud=kanade` |

Precedence: `DISABLE` > static bearer > JWT. Backend with none of the three
set falls back to a hard-coded dev secret and logs a loud warning — fine for
one-shot debugging, **never** for production.

Each secret resolves registry-first, env-second:

```text
StaticToken:  HKLM\SOFTWARE\kanade\backend\StaticToken  →  $KANADE_AUTH_STATIC_TOKEN
JwtSecret:    HKLM\SOFTWARE\kanade\backend\JwtSecret    →  $KANADE_JWT_SECRET
```

Provision the registry values with `deploy-backend.ps1` so the script can
strip non-admin ACEs from the key (SYSTEM + Administrators read only). The
env vars stay for `cargo run` / `cargo make dev` / non-Windows hosts.
`KANADE_AUTH_DISABLE` stays env-only — it's a presence flag, not a secret.

Clients send `Authorization: Bearer <token>` on every `/api/*` request:

- **SPA**: stores the token in `localStorage`; click `login` in the top-right
  nav to paste, `logout` to clear. A 401 from the backend auto-clears the
  stored token and re-prompts.
- **CLI**: reads `$env:KANADE_AUTH_TOKEN`. Set it once per shell session
  (or export it from a shell profile). The CLI sends the same header
  regardless of which auth mode the backend is running.

```powershell
# Backend side — production (registry, hardened ACL)
.\deploy-backend.ps1 -StaticToken 'kanade-fleet-secret-2026'

# Backend side — dev (env, current shell only)
$env:KANADE_AUTH_STATIC_TOKEN = "kanade-fleet-secret-2026"
.\deploy-backend.ps1

# Operator side (CLI)
$env:KANADE_AUTH_TOKEN = "kanade-fleet-secret-2026"
kanade exec jobs\echo-test.yaml
```

### NATS authentication

Separate from the backend HTTP layer above. By default `nats-server -js`
listens on `:4222` without auth — anyone on the LAN who can reach the broker
can publish `commands.pc.<host>` and execute scripts on every agent. Lock
it down for production with token auth:

1. Start nats-server with the bundled config:

   ```powershell
   nats-server -c configs/nats-server.conf
   ```

   The shipped `configs/nats-server.conf` enables JetStream + an
   `authorization.token` block. Pick your own secret. For
   production, run as a Windows service via `deploy-nats.ps1`
   (the script applies a SYSTEM + Administrators-only ACL on the
   installed config so the token isn't readable by other users).

2. Provision the token on every kanade host. The shared
   `kanade_shared::nats_client::connect()` helper resolves it in this
   order:

   **(1) `HKLM\SOFTWARE\kanade\agent\NatsToken` (REG_SZ) — production.**
   `deploy-agent.ps1` / `deploy-backend.ps1` accept `-NatsToken` and
   write the value with a hardened ACL (SYSTEM + Administrators only).
   Low-privilege users on the host cannot read it back, which a
   Machine-scope environment variable cannot prevent.

   ```powershell
   # On agent + backend hosts
   .\deploy-agent.ps1   -NatsToken 'kanade-fleet-secret-2026'
   .\deploy-backend.ps1 -NatsToken 'kanade-fleet-secret-2026'
   ```

   **(2) `$KANADE_NATS_TOKEN` environment variable — dev / fallback.**
   Used only when the registry value is absent. Service binaries run
   as LocalSystem and never see user-session env vars, so this branch
   fires for `cargo run`, the operator CLI, and `cargo make dev`:

   ```powershell
   $env:KANADE_NATS_TOKEN = 'kanade-fleet-secret-2026'
   kanade jetstream status
   ```

   **(3) No token → unauthenticated connect.** Works against a broker
   started without `authorization { ... }` — fine for local dev.

`nats_url` in `agent.toml` / `backend.toml` stays plain. The secret
never lands in config files or process listings.

For multi-tenant / per-agent identity (NKeys, NATS JWT, mTLS), see
[spec §2.7.1](https://github.com/yukimemi/kanade/blob/main/docs/SPEC.md).
Stick with the shared token while operating ≤ ~1000 hosts.

## Dev workflow

```powershell
cargo make check       # fmt-check + clippy + test + lock-check (same as CI)
cargo make fmt         # apply formatting
cargo make on-add      # renri post_create hook (apm install + vcs fetch)
```

The workspace pins `[profile.dev] debug = "line-tables-only"` because
Windows MSVC `link.exe` hits `LNK1318` (PDB record limit) once axum +
sqlx + reqwest + tokio-cron-scheduler + jsonwebtoken all sit in one
workspace; line-tables-only keeps backtraces useful without exploding
the PDB.

## Sprint history

- **Sprint 1** — workspace scaffolding, NATS plumbing, agent + CLI echo round-trip
- **Sprint 2** — §2.6 kill switch (subscribe + flush race fix), version-pin KV, WMI HW inventory
- **Sprint 3** — backend skeleton, SQLite projectors, YAML deploy API, audit log, `tokio-cron-scheduler` with dynamic KV watch
- **Sprint 4** — wave rollout + agent-side jitter, embedded SPA dashboard, HS256 JWT middleware, agent self-update via the JetStream Object Store (atomic exe swap + SCM failure-action restart in v0.1.5)
- **Sprint 5** (v0.2.0) — server-managed group membership: `agent_groups` KV bucket, dynamic agent-side subscribe/unsubscribe, admin API + `kanade agent groups` CLI. `[agent] groups` field in agent.toml deprecated
- **Sprint 6** (v0.3.0) — layered `agent_config` KV bucket: `ConfigScope` per global / per-group / per-pc, resolver with deterministic precedence + multi-group conflict warnings, dynamic cadence reconciliation for heartbeat / inventory / self_update, admin API + `kanade config` CLI. `[inventory]` section in agent.toml deprecated
- **v0.3.1** — `kanade-backend` auto-bootstraps every JetStream resource at startup; the operator-side `kanade jetstream setup` is now optional
- **Sprint 10** (v0.7.0) — Audit / Results page filters (actor / action / pc_id / status / since presets) and `/api/health/fleet` rollup endpoint (agents · JetStream · recent failures, 200/503 by `status`); Dashboard surfaces the server-computed banner
- **v0.7.1** — agent file logging via `tracing-appender` (daily rotation, `[log] keep_days` retention), `kanade agent publish` auto-detects `--version` from the binary via `<exe> --version` probe
- **v0.8.0** — staged self-update rollout: `kanade agent publish` is now upload-only (no KV touch); new `kanade agent rollout <ver> --global|--group <name>|--pc <pc_id> [--jitter <dur>]` flips `target_version` on one scope and (optionally) `target_version_jitter`. Agent-side `self_update` sleeps `random(0..jitter)` before downloading, defusing the "3000 agents hammer the Object Store at the same instant" failure mode. **Breaking change**: any operator scripts that relied on `publish` doing the rollout in one step need to chain a `rollout` call
- **v0.9.0** — on-demand agent log fetch and a Web UI for rollout. New `logs.fetch.<pc_id>` NATS request/reply on the agent (`kanade agent logs <pc_id> [--tail N]` from the CLI, or the new **Logs** page in the SPA). New backend endpoints `/api/agents/<pc_id>/logs`, `/api/agents/releases`, `POST /api/agents/rollout`, plus a **Rollout** SPA page with a version picker / scope select / jitter input
- **v0.10.0** — fleet-wide group ops + Web UI binary upload + logo viewBox fix. New `kanade group` top-level subcommand (`list` fleet-wide, `list --pc <id>` per-PC, `members <name>` reverse lookup, `add` / `rm` / `set` membership) replaces the old `kanade agent groups …`. SPA Rollout page gains an upload card backed by a new `POST /api/agents/publish` multipart endpoint (64 MB body limit) — the CLI is no longer required to publish a new binary
- **v0.11.0** — fleet bootstrap from a clean Windows box. `build-release.ps1` defaults to `Invoke-WebRequest` from GitHub Releases (no cargo / bun / git on the build host); new `nats` role downloads from `nats-io/nats-server`; sample configs moved from repo root to `configs/`. New `deploy-nats.ps1` registers nats-server as a Windows service (`KanadeNats`), opens TCP 4222 / 8222, and hardens the ACL on `nats-server.conf` (token plaintext)
- **v0.11.2** — drop the up-to-10-minute initial inventory pause (dev / fresh-deploy UX); switch agent inventory error logging from `%e` to `?e` so the anyhow chain (WMI HRESULTs etc.) actually surfaces. New `kanade inventory <pc_id>` CLI + `inventory.request.<pc_id>` NATS request/reply on the agent triggers a one-shot collection on demand. ACL hardening in `deploy-{agent,backend,nats}.ps1` rewritten to use `icacls` / pure `Microsoft.Win32.Registry` APIs instead of `Get-Acl` / `Set-Acl` cmdlets, fixing `CouldNotAutoloadMatchingModule` on hosts with broken pwsh module loaders
- **v0.11.5** — SPA login page + protected-route redirect; 401 responses route the UI to `/login` instead of rendering an ErrorCard on every page
- **v0.12.0** — heartbeat-baseline projector + PowerShell-driven inventory. `Heartbeat` now carries hostname + os_family, and a new backend projector upserts a minimal agents row from every heartbeat — fresh deploys show up in `/api/agents` within ~30 s, no inventory required. The inventory path swapped from the `wmi` crate to `powershell.exe -Command 'Get-CimInstance … | ConvertTo-Json'` to get past LocalSystem-context `WBEM_E_INVALID_CLASS` failures (same hosts where user-shell `Get-CimInstance` works fine). Dashboard + `/api/health/fleet` "active" rollup switched from a 25 h inventory cutoff to a 2 min heartbeat cutoff. Inventory request subject moved out of the `inventory.>` stream filter (`request.inventory.<pc_id>`) so JetStream's publish-ack stops clobbering the agent's `"ok"` reply. release.yml passes `--allow-dirty` on the kanade-backend publish step so the fresh CI-built `web/dist/` doesn't trip cargo's clean-tree check
- **v0.13.0** — operator-defined inventory probes via the existing schedule + deploy + ExecResult pipeline. New `inventory:` section on the job manifest (`display: [{ field, label, type? }]`) tags a scheduled PowerShell job as an inventory probe. The agent forwards `Command.id` as `ExecResult.manifest_id`; the backend's results projector spots inventory-tagged manifests, parses the script's `ConvertTo-Json` stdout, and upserts an `inventory_facts(pc_id, job_id, facts_json, display_json, …)` row. New `/api/inventory/<pc_id>` + `/api/inventory/jobs` endpoints. New SPA **Inventory** page renders facts per the display config (`bytes` / `number` / plain). The hardcoded WMI inventory loop in `kanade-agent` stays for now so existing fleets don't lose their agents table fields; v0.14 removes it once a default probe yaml is the documented path. Sample probe shipped at `configs/jobs/inventory-hw.yaml` — install with `kanade schedule create configs/jobs/inventory-hw.yaml`
- **v0.13.1** — kill the operator-typed version label entirely (root cause of the `1.0.0`-as-label / agent-baked-as-`0.11.1` self-update loop). Build-time: `winres` embeds VERSIONINFO into every kanade .exe so the binary CARRIES its version. Backend `POST /api/agents/publish` drops the `version` form field and extracts ProductVersion from the uploaded bytes via pelite (pure-Rust, no spawn). CLI `kanade agent publish <bin>` likewise loses `--version`. SPA Rollout page replaces its free-text version field with an auto-extracting upload, and the Object Store releases table grows per-row **Roll out** + **Delete** buttons. New `DELETE /api/agents/releases/<v>` (refuses with 409 when any scope still targets the release). Agent gains a `<data_dir>/last_swap.json` based loop guard: if the same (target, running_before) pair is seen across an SCM-restart cycle, the agent refuses to swap to that target again until the operator changes it
- **v0.14.0** — retire the hardcoded WMI inventory loop. The agent no longer has an inventory.rs (cadence + WMI / PowerShell shell-out, `inventory_loop` + `serve_requests`, the `request.inventory.<pc_id>` subject, and the `kanade inventory <pc_id>` CLI are all gone). All inventory now flows through operator-defined probes: `configs/jobs/inventory-*.yaml` with an `inventory:` hint, registered via `kanade schedule create`, fanned out by the existing deploy + ExecResult path, projected into `inventory_facts` by the results projector. Backend's `projector/inventory.rs` is gone too — facts upserts happen inline with result projection. Migration `0005` drops the rich columns (`os_name` / `cpu_model` / `ram_bytes` / `disks_json` / ...) from `agents`, so the table holds only the baseline heartbeat-derived fields. The SPA **Agents** page is now a baseline liveness list with a per-row "facts" link to the **Inventory** page
- **v0.14.1** — split inventory display config into `display` (per-PC detail) + `summary` (fleet list, 3-5 columns). `InventoryHint` grows an `Option<Vec<DisplayField>>` `summary` field; migration `0006` adds `summary_json` to `inventory_facts`; results projector snapshots both. New `GET /api/inventory/by-job/<manifest_id>` returns one row per PC for a probe, fronting the SPA **Inventory** page's new fleet-list view (row-per-PC, columns = `summary`, click a row → existing per-PC detail). Sample `configs/jobs/inventory-hw.yaml` + `configs/schedules/hourly-inventory.yaml` updated to demonstrate trimming the wide `display` set down to four summary columns
- **v0.15.0** — kill the redundant inline-manifest body inside Schedule yaml. Jobs are now first-class catalog rows in a new `BUCKET_JOBS` KV; `Schedule` shrinks to `{ id, cron, job_id, enabled }`. New `kanade job {create,list,delete}` subcommand + `GET/POST /api/jobs` + `DELETE /api/jobs/{id}` (refuses with 409 when any schedule references the job). Scheduler resolves `job_id → Manifest` from KV at every fire, so editing a job takes effect on the next tick without re-creating schedules. The results projector + `/api/inventory/jobs` listing now read from `BUCKET_JOBS` directly instead of scanning the schedules bucket — faster, and ad-hoc deploys of registered jobs (not just scheduled ones) get inventory-fact projection. Schedule yaml on disk shrinks from ~50 lines to 4
- **v0.16.0** — `kanade deploy` → `kanade exec`. The "deploy" name implied long-lived rollout, but the operation is just a one-shot fanout — so the user-facing surface (CLI subcommand, HTTP route `/api/exec`, SPA Exec page + nav) all rename. Wire scope: NATS subject prefix `commands.deploy.>` → `commands.exec.>`, stream `DEPLOY` → `EXEC`. DB scope: `deployment_results` → `execution_results`, `deployments` → `executions`, `deploy_id` column → `exec_id`. Audit event `"deploy"` → `"exec"`. Migrations 0001-0006 are squashed into a fresh `0001_baseline.sql` — operators upgrading must wipe their sqlite db and JetStream `DEPLOY` stream first
- **v0.16.1** — fix `nats-server.conf`'s `store_dir: "./jetstream"` writing JetStream data under `C:\Windows\System32\jetstream\` because the Windows service starts with cwd = System32. Switched to absolute `C:/ProgramData/Kanade/nats/jetstream` (which `deploy-nats.ps1` already provisions). Operators upgrading should re-run `deploy-nats.ps1 -ForceConfig` (or edit the conf in place) and migrate any pre-existing data from `C:\Windows\System32\jetstream\` — though if you're upgrading from v0.16.0 the wipe-and-recreate path already covers it
- **v0.16.2** — SPA: per-row enable/disable toggle on the Schedules table (reuses the existing upsert; scheduler's KV watcher picks up the flip on the next put). New **Jobs** page + nav entry that lists `BUCKET_JOBS` entries with per-row delete (backend already refuses with 409 when a schedule still references the job)
- **v0.17.0** — `kanade exec` stops accepting inline Manifest yaml. The catalog (`kanade job create`) is now the single authoritative path for Manifests, and `kanade exec <job-id>` just fires a registered one — yaml input would have bypassed the catalog. Route changes to `POST /api/exec/{job_id}` (no body); backend resolves the Manifest from `BUCKET_JOBS` and 404s when the id isn't registered. SPA Exec page replaces its Manifest JSON textbox with a dropdown of registered jobs. Three-tier operator surface now reads cleanly: `kanade run <pc> -- <script>` for inline one-PC sync, `kanade exec <job-id>` for catalog-registered fanout, `kanade schedule create` for cron-wrapped catalog-registered. `--version` override flag dropped — re-run `kanade job create` to bump
- **v0.17.1** — SPA: mobile-responsive Nav. The 12-link horizontal bar overflowed on phone screens; mobile (< md breakpoint) now collapses to a hamburger button that opens a vertical dropdown drawer below the header. Desktop keeps the inline list (wraps if it ever needs to). Dev convenience: new `cargo make nats-dev` task spins up a workspace-local unauthenticated nats-server on :4223 so `cargo make dev` can run end-to-end without touching the prod KanadeNats service (`backend.dev.toml` now points at :4223); Vite config picks up `allowedHosts: ['.ts.net']` so phones reach the dev server via Tailscale MagicDNS
- **v0.18.0** — separate "what / who / when" cleanly. Manifest carries only the script (id, version, description, execute, inventory hint, require_approval); `target`, `rollout`, and `jitter` move to the new `FanoutPlan` struct that Schedules embed (`#[serde(flatten)]`) and the `POST /api/exec/{job_id}` body deserialises directly. Same `kanade job create` can now be wrapped in two schedules targeting different groups on different cadences without copying the script body. CLI grows `kanade exec <job-id> --all|--groups a,b|--pcs pc1,pc2 [--jitter 30s]`; SPA Exec page replaces single-button form with target selector. Manifest grows `#[serde(deny_unknown_fields)]` so a yaml that still has `target:` / `rollout:` errors clearly at `kanade job create` time. Configs sample updated: `configs/jobs/inventory-hw.yaml` drops `target: { all: true }`, `configs/schedules/hourly-inventory.yaml` carries it (with commented `jitter` + `rollout` examples)
- **v0.18.1** — SPA Table wrapper switched from `overflow-hidden` to `overflow-x-auto` (+ `min-w-max` on the inner `<table>`), so multi-column tables (Schedules, Results, etc.) horizontally scroll on phones instead of clipping the right edge. Closes the immediate gap on the mobile-responsive backlog item
- **v0.19.0** — schedule `mode`s: `EveryTick` (default, historical) / `OncePerPc` / `OncePerTarget`. With `OncePerPc`, the scheduler resolves the target to a concrete alive-pc set at each tick, subtracts the pcs that have already exit_code=0'd for this job, and fires only at the remainder — naturally catching kitting / first-boot scenarios (new PCs join later → next tick picks them up). `OncePerTarget` gates the whole target on "anyone in scope has succeeded". Optional `cooldown` (humantime) re-arms each pc / the target after that interval — `cooldown: 1d` turns a `*/5min` cron into "fire at most once per day per pc", great for compliance checks. `auto_disable_when_done: true` flips the schedule's `enabled = false` once the lifecycle is permanently terminated (only when `cooldown` is unset; cooldown'd schedules always re-arm). Implementation: pure `scheduler::policy::decide_fire(...)` with 24 boundary tests covering empty target, cooldown ≥ vs <, multiple completions per pc, decommissioned pc handling, etc.; new `execution_results.job_id` column (migration 0002) + projector wire-through; new `BUCKET_AGENT_GROUPS` KV scan for `target.groups` resolution. Sample `configs/schedules/kitting-once.yaml`
- **v0.20.0** — drop the dead `inventory_interval` / `inventory_jitter` / `inventory_enabled` fields from `ConfigScope` + `EffectiveConfig` (leftover from the v0.14-retired hardcoded WMI inventory loop — they sat in the wire types as schema noise for five releases). CLI `kanade config set` drops the inventory-* field handlers; SPA types + Config page reflect the smaller schema; agent.toml `[inventory]` section + the `InventorySection` struct are removed entirely (deprecation warning was on the books for "v0.4.0", we're way past). Existing KV rows that still carry an `inventory_*` key parse fine (serde tolerates unknown fields on `ConfigScope`) — the values just dissolve into "unknown, ignored". Filed #26 for a follow-up debate on `target_version_jitter` defaulting to `"0s"`
- **v0.21.0** — job execution identity (spec §2.4.1). New `Execute.run_as` field on the Manifest + matching `Command.run_as` on the wire, with three variants: `system` (default, Session 0 / LocalSystem / no GUI — historical behavior), `user` (current console user's session + their UAC-filtered token — HKCU / %APPDATA% / Slack-notify use cases), `system_gui` (user's session + LocalSystem privs — admin installer with visible UI, PsExec `-i -s` pattern). Pre-v0.21 wire payloads omitting `run_as` default to `system` so existing fleets are unaffected. Agent grows a Windows-only `process_as_user` module using the `windows` crate: `WTSQueryUserToken` for `user`, `DuplicateTokenEx` + `SetTokenInformation(TokenSessionId)` for `system_gui`, anonymous pipes for stdio capture, `CreateProcessAsUserW` for the spawn. Kill subject + timeout still work via a oneshot bridge that calls `TerminateProcess`. Non-Windows agents emit a sentinel skip result with a `run_as: <variant> is Windows-only` stderr line. CLI / SPA Jobs page expose the new column; sample `configs/jobs/kitting-setup.yaml` documents the choice
- **v0.21.1** — optional `Execute.cwd` (working directory for the spawned shell). Without it the agent service's cwd (`%SystemRoot%\System32` on Windows for prod) gets inherited — almost never what scripts assume. Absolute paths; no agent-side env-var expansion (use teravars in yaml or shell-side `$env:USERPROFILE`). Wire backwards-compat via `#[serde(default)]`. Threaded through both spawn paths (`tokio::process::Command::current_dir` for `run_as: system`; `CreateProcessAsUserW`'s `lpCurrentDirectory` for user / system_gui). SPA Jobs page surfaces the column
- **v0.21.2** — `Execute.cwd` learns to expand `~` (leading) and `%FOO%` env vars on the agent side. New `cwd_expand` module: `GetUserProfileDirectoryW` for the tilde, `ExpandEnvironmentStringsForUserW` for env vars — both keyed by the spawning token, so `~` and `%USERPROFILE%` resolve to the *target* user's profile under `run_as: user` / `system_gui`. PowerShell's `$env:FOO` syntax intentionally unsupported (would need a PS parser; `%FOO%` is the Windows-native form and covers the same use cases). Expansion failures fall back to the raw value with a warn-log rather than refusing to spawn
- **v0.22.0** — schedule `starting_deadline` (humantime; k8s CronJob's `startingDeadlineSeconds` semantics). Scheduler stamps `now + starting_deadline` onto each emitted Command as the absolute `deadline_at` field; agents receiving a Command past that instant publish a synthetic skipped-result (exit_code 125 + explanatory stderr) instead of running the script. `None` (default) = no deadline / catch up whenever delivered — keeps current behavior for kitting / inventory / cleanup. Set this on time-of-day schedules (lunch announce, morning greeting) where firing 3 hours late is wrong. Wire is back-compat via `#[serde(default)]`. Sample `configs/schedules/morning-greeting.yaml` demonstrates the `30m` deadline + skipped-result UX. Issue #20 (STREAM_EXEC actually used) tracks the next step: durable consumer + retention so reconnecting agents catch up to deadline-bound or deadline-free commands
- **v0.22.1** — close the agent-reconnect catch-up gap (closes #20). `STREAM_EXEC` filter changes from the orphan `commands.exec.>` to `commands.>`, so a single backend publish lands in BOTH the agent's live core subscription AND the stream's `max_messages_per_subject=1` retention store. New `command_replay` agent module: stable-named durable consumer (`agent_replay_<pc_id>`) with `DeliverPolicy::LastPerSubject` replays the latest retained Command per subject on reconnect. Shared `DedupCache` (FIFO-bounded by `request_id`) between the core sub and the replay path keeps an online agent from running the same Command twice. Combined with v0.22.0's `deadline_at`: reconnecting agents catch up to all in-window Commands, automatically skip ones that became stale, and emit synthetic skipped-results so the operator sees the miss
- **v0.23.0** — `Schedule.runs_on: backend | agent` (default `backend`). When set to `agent`, the backend's scheduler steps out and leaves the definition in `BUCKET_SCHEDULES` for each targeted agent to read; new `local_scheduler` agent module watches both `BUCKET_SCHEDULES` + `BUCKET_JOBS`, picks up the schedules whose target matches itself + `runs_on: agent`, and runs an internal `tokio_cron_scheduler` for them. On a local tick the agent fires the cached Manifest through the same `handle_command` path as live-NATS Commands (so kill / cooldown / inventory projection all behave identically). `mode: once_per_pc` dedup state persists to `<data_dir>/local_completions.json` so an agent restart doesn't re-fire kitting scripts. Use case: laptops that roam off the WAN — hourly inventory, kitting, compliance checks keep ticking through broker outages. Sample `configs/schedules/offline-inventory.yaml` demonstrates. Caveats (v0.24): result publish still depends on async-nats client buffering rather than a real outbox; group-membership reflection refreshes only on schedule edits, not on its own watch
- **v0.24.0** — three durability + observability fixes closing the v0.23 caveats. **(1) File-based outbox for ExecResult.** Every result the agent produces is persisted under `<data_dir>\outbox\<request_id>.json` (atomic tmp + rename) before any NATS publish; a 1 s background drain task publishes via JetStream and only deletes on `PubAck`. Survives agent crashes mid-publish, broker outages longer than the async-nats client buffer, and partial-send races — results queue on disk and ship when the broker returns. **(2) Live group-membership watch.** `groups::manage` now exposes a `watch::Receiver<Vec<String>>`; `local_scheduler` subscribes to it and re-reconciles every cached schedule when membership changes — so adding a laptop to a group while it's offline triggers its `runs_on: agent` schedules on the next watch event instead of waiting for someone to re-save the schedule. **(3) Backend file-appender parity.** `kanade-backend` was emitting `tracing_subscriber::fmt()` to stdout only — under the Windows service (no console) that meant zero log lines anywhere on disk and silent crashes. `init_tracing` now mirrors the agent: daily-rotated `[log] path` + stdout, honoring `RUST_LOG` and `keep_days = 0` opt-out. Backend startup info! now also logs `log_path` / `log_keep_days` so the operator can confirm wiring from the first line
- **v0.25.0** — three operator-facing recovery + ergonomics improvements. **(1) Bootstrap auto-migrate.** `ensure_jetstream_resources` switched from `create_stream` / `create_key_value` to `create_or_update_stream` / `create_or_update_key_value`. The old form returned error 10058 ("name already in use with a different configuration") when a release widened a stream's subjects or changed retention — e.g. v0.22.1 broadening `STREAM_EXEC` from `commands.exec.>` to `commands.>` would silently kill the backend at startup on any broker still holding the v0.22.0-era config (observed in the wild after the v0.24.0 deploy). The new form reconciles the broker's definition to this file, so release-to-release config drift no longer requires manual `Stop-Service KanadeNats` + JetStream data wipes. **(2) `kanade jetstream delete` / `reset` CLI.** Surgical and nuclear operator recovery paths that don't require shutting NATS down. `kanade jetstream delete stream EXEC --yes` removes one resource; `kanade jetstream reset --yes` wipes every stream / bucket / store the fleet uses and re-bootstraps. Both refuse to run without `--yes`. Backed by `delete_stream` / `delete_key_value` / `delete_object_store` from async-nats 0.48. **(3) SPA scrollbar theme.** Tables with overflow on desktop were rendering the OS default chunky white scrollbar against the dark backdrop. `index.css` now sets `scrollbar-width: thin` + `scrollbar-color` to theme borders, with matching `::-webkit-scrollbar` rules for older WebKit. Also fixed `kanade jetstream status`'s bucket list which had stale enumeration (was missing `agent_groups` / `schedules` / `jobs` for several releases)
- **v0.26.0** — Layer 2 of the §2.6 3-layer defense gets *staleness policy* — closing the offline gap that v0.23.0's `runs_on: agent` opened. The hole: when an agent fires a `runs_on: agent` schedule from its cached `BUCKET_JOBS` entry while disconnected from the broker, the existing version-pin + revoke checks (`script_current` / `script_status` KV gets) silently no-op'd on `Err` — letting potentially revoked or stale commands run because we "couldn't ask". New per-Manifest `staleness:` block lets operators classify scripts: `mode: strict` requires a recent confirmed broker connection (configurable via `max_cache_age`) or the agent skips with synthetic exit 127; `mode: cached` (default, back-compat) runs from cache without an age limit; `mode: unchecked` skips Layer 2 entirely. Agent tracking is push-based via async-nats `event_callback` — the `Tracker` captures `Event::Connected` and stamps a shared `Instant`, with `Client::connection_state() == Connected` as the fast-path "right now we know we're fresh" answer. Zero polling overhead. Wire-compatible: pre-v0.26 Commands and Manifests omitting `staleness` parse as `Cached`. Sample `configs/jobs/urgent-patch.yaml` shows the strict pattern. Full operator semantics (including how `max_cache_age` interacts with the NATS push-based watch contract) live in [SPEC.md §2.6.2](https://github.com/yukimemi/kanade/blob/main/docs/SPEC.md#262-第2層-実行直前の-version-照合--staleness-policy). The §2.6 spec itself got a substantial rewrite — it now matches the v0.22.1+ `STREAM_EXEC` reality (instead of the long-retired `DEPLOY` stream), documents the offline considerations, and lays out the 3 operator stop-actions (exec / job-delete / schedule-disable) along with their cascade rules. v0.27 will implement those cascade ops + SPA Revoke / Kill UI

Backlog: Prometheus metrics, 3000-agent simulation, NATS cluster + replicated backend, Postgres migration.

## Production install layout

`cargo install` drops the binaries under `~/.cargo/bin/` (user-local).
For a real deployment, copy them into the spec §2.11 layout and register
a service so they survive reboots.

### Path layout

```text
Windows                                    Linux
C:\Program Files\Kanade\                   /usr/local/bin/
  ├── kanade-agent.exe                       ├── kanade-agent
  ├── kanade-backend.exe                     ├── kanade-backend
  ├── kanade.exe                             ├── kanade
  └── nats-server.exe                        └── nats-server

C:\ProgramData\Kanade\config\              /etc/kanade/
  ├── agent.toml                             ├── agent.toml
  ├── backend.toml                           ├── backend.toml
  └── nats-server.conf  (hardened ACL)       └── nats-server.conf

C:\ProgramData\Kanade\data\                /var/lib/kanade/
  ├── state.db        (agent)                ├── state.db
  ├── outbox\         (agent)                ├── outbox/
  ├── staging\        (self-update)          ├── staging/
  ├── backend.db      (backend)              ├── backend.db
  ├── certs\                                 ├── certs/
  └── nats\           (JetStream data)       └── nats/

C:\ProgramData\Kanade\logs\                /var/log/kanade/
  ├── agent.log                              ├── agent.log
  ├── backend.log                            ├── backend.log
  └── nats-server.log                        └── nats-server.log
```

### Config discovery

Every binary looks up its config file in this exact order (no cwd
fallback — too easy to load the wrong file by accident):

1. `--config <path>` CLI flag (always honored, even if the file
   doesn't exist — that's the caller's choice).
2. Environment variable: `KANADE_AGENT_CONFIG` for `kanade-agent`,
   `KANADE_BACKEND_CONFIG` for `kanade-backend`. Non-empty value
   wins.
3. `<config_dir>/<basename>`:
   - Windows: `%ProgramData%\Kanade\config\agent.toml`
   - Linux: `/etc/kanade/agent.toml`

If none of the three is reachable, the binary exits with a message
listing every option an operator can use to fix it.

### Install scripts (Windows, recommended)

PowerShell scripts under
[`scripts/`](https://github.com/yukimemi/kanade/blob/main/scripts/)
handle the whole "drop a folder onto the target, run as Admin" path
— no Rust toolchain, no bun, no git required on the deploy host:

```powershell
# 1. On any Windows box (no dev tooling needed): pull pre-built
#    binaries straight from GitHub Releases via Invoke-WebRequest
#    and assemble one stage folder per role under .\dist\.
PS> .\scripts\build-release.ps1
# → dist\agent\, dist\backend\, dist\nats\
#   each contains: <role>.exe, <role>.{toml,conf}, deploy-<role>.ps1
#
# Variants:
#   -Roles agent,backend           # skip nats
#   -NatsVersion 2.11.10           # pin a specific NATS broker tag
#   -FromSource                    # compile from this checkout (cargo + bun required)
#   -FromCrates                    # install from crates.io (cargo required)
#   -Zip                           # also produce dist\<role>.zip

# 2. Copy each stage folder onto the target host (xcopy, robocopy,
#    scp, USB stick — whatever fits your environment).

# 3. On the target host, run the matching script as Administrator:
PS> .\deploy-nats.ps1     -NatsToken 'kanade-fleet-secret-2026'    # broker host (run once)
PS> .\deploy-agent.ps1    -NatsToken 'kanade-fleet-secret-2026'    # every endpoint
PS> .\deploy-backend.ps1  -NatsToken 'kanade-fleet-secret-2026' `
                          -StaticToken '<api-token>'                # admin box
```

Re-running the script upgrades the binary in place and preserves
the edited config. Pass `-ForceConfig` to overwrite the installed
config from the source folder, or `-NoStart` to skip the
post-install service start.

### Windows Service registration (sc.exe)

If you'd rather not use the deploy scripts (or want to understand
exactly what they do), here are the equivalent manual commands:

```powershell
# Stage the binaries
New-Item -ItemType Directory -Force 'C:\Program Files\Kanade'
Copy-Item "$env:USERPROFILE\.cargo\bin\kanade-agent.exe"   'C:\Program Files\Kanade\'
Copy-Item "$env:USERPROFILE\.cargo\bin\kanade-backend.exe" 'C:\Program Files\Kanade\'

# Stage the config (review + edit first)
New-Item -ItemType Directory -Force 'C:\ProgramData\Kanade\config'
Copy-Item .\configs\agent.toml   'C:\ProgramData\Kanade\config\'
Copy-Item .\configs\backend.toml 'C:\ProgramData\Kanade\config\'

# Register the agent as a service running under LocalSystem.
sc.exe create KanadeAgent `
  binPath= '"C:\Program Files\Kanade\kanade-agent.exe"' `
  start= auto `
  obj= LocalSystem `
  DisplayName= "Kanade Endpoint Agent"
sc.exe failure KanadeAgent reset= 86400 actions= restart/60000/restart/60000/restart/60000

# Register the backend the same way.
sc.exe create KanadeBackend `
  binPath= '"C:\Program Files\Kanade\kanade-backend.exe"' `
  start= auto `
  obj= LocalSystem `
  DisplayName= "Kanade Backend"

sc.exe start KanadeAgent
sc.exe start KanadeBackend
```

### Linux systemd units

```ini
# /etc/systemd/system/kanade-backend.service
[Unit]
Description=Kanade Backend
After=network.target nats.service

[Service]
ExecStart=/usr/local/bin/kanade-backend
Restart=always
User=kanade
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now kanade-backend.service
```

The agent unit is symmetric (`kanade-agent.service`, `ExecStart=/usr/local/bin/kanade-agent`).

## Scaffolded with kata

The skeleton (`AGENTS.md` / `Makefile.toml` / `clippy.toml` /
`rustfmt.toml` / `.github/workflows/*` / etc.) was applied via
[`github.com/yukimemi/pj-presets:rust-cli`](https://github.com/yukimemi/pj-presets)
through `kata init`. The Cargo workspace layout under `crates/` is
hand-written because the preset is single-crate by default; a
`pj-rust-workspace` layer is on the future TODO once the multi-crate
patterns stabilise.

## License

MIT — see [LICENSE](https://github.com/yukimemi/kanade/blob/main/LICENSE).

[teravars]: https://github.com/yukimemi/teravars

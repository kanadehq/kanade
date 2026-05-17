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
  whether you `kanade deploy` ad-hoc or wire it onto a cron `kanade
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
kanade deploy jobs/echo-test.yaml

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

kanade deploy   <manifest.yaml> [--version <v>]  # POST /api/deploy
kanade schedule create <schedule.yaml>           # POST /api/schedules (cron + manifest)
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
kanade deploy jobs\echo-test.yaml
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

Backlog: v0.12.0 — generalise inventory as scheduled `jobs/inventory-*.yaml` (PowerShell + `ConvertTo-Json` → projector → display-config-driven UI), enrich `Heartbeat` with hostname / os_name so the UI gets a baseline row 30s after agent start. Then Prometheus metrics, 3000-agent simulation, NATS cluster + replicated backend, Postgres migration.

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

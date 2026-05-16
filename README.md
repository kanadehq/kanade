<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/kanade/main/assets/logo-dark.svg">
  <img src="https://raw.githubusercontent.com/yukimemi/kanade/main/assets/logo.svg" alt="kanade — orchestrate fleets of Windows endpoints" width="540">
</picture>

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
| `kanade-agent`   | bin  | Windows-side resident daemon: subscribes to `commands.*`, runs child processes, publishes results + heartbeats + WMI inventory, watches `agent_config.target_version` for self-update |
| `kanade-backend` | bin  | axum HTTP server: `/health`, `/api/{agents,results,audit,deploy,schedules}`, embedded SPA at `/`. Runs 3 durable JetStream projectors (INVENTORY/RESULTS/AUDIT → SQLite) and a `tokio-cron-scheduler` driven by the schedules KV |
| `kanade`         | bin  | operator-side admin CLI (`kubectl`-style single entry point); subcommands talk to NATS directly for `run`/`ping`/`kill`/`revoke`/`jetstream` and to the backend over HTTP for `deploy`/`schedule`/`agent` |

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

You'll also want the sample configs (`agent.toml` / `backend.toml`) and
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
of them assume `cd` into the directory that holds `agent.toml` /
`backend.toml` / `jobs/`, which is the repo root if you cloned it.

### 1 — start NATS

```powershell
nats-server -js -p 4222
```

### 2 — provision JetStream (one-time)

```powershell
kanade jetstream setup
```

Creates the `INVENTORY` / `RESULTS` / `DEPLOY` / `AUDIT` streams, the
`script_current` / `script_status` / `agents_state` / `agent_config` KV
buckets, and the `agent_releases` Object Store.

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

Loads `./agent.toml`, picks `$env:COMPUTERNAME` as `pc_id`, subscribes
to `commands.all` + `commands.pc.{pc_id}` + every group declared in
`agent.toml` (`canary` + `wave1` in the bundled sample), starts the
heartbeat / inventory / self-update loops.

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

kanade jetstream setup                           # create streams + KV + Object Store
kanade jetstream status                          # health snapshot

kanade deploy   <manifest.yaml> [--version <v>]  # POST /api/deploy
kanade schedule create <schedule.yaml>           # POST /api/schedules (cron + manifest)
kanade schedule list
kanade schedule delete <id>

kanade agent publish <binary> --version <v>      # upload to Object Store + flip target_version
kanade agent current                             # read agent_config.target_version
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

`agent.toml`:

```toml
[agent]
id = '{{ system.host }}'
nats_url = 'nats://127.0.0.1:4222'
groups = ['canary', 'wave1']

[inventory]
hw_interval = '24h'
jitter = '10m'
enabled = true

[log]
path = 'logs/agent.log'
level = 'info'
```

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
- **Sprint 4** — wave rollout + agent-side jitter, embedded SPA dashboard, HS256 JWT middleware, agent self-update via the JetStream Object Store

Sprint 5 (Prometheus metrics, 3000-agent simulation, backups) and
Sprint 6 (NATS cluster + replicated backend + Postgres migration) are
open backlog items.

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
  └── backend.toml                           └── backend.toml

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

### Windows Service registration (sc.exe)

```powershell
# Stage the binaries
New-Item -ItemType Directory -Force 'C:\Program Files\Kanade'
Copy-Item "$env:USERPROFILE\.cargo\bin\kanade-agent.exe"   'C:\Program Files\Kanade\'
Copy-Item "$env:USERPROFILE\.cargo\bin\kanade-backend.exe" 'C:\Program Files\Kanade\'

# Stage the config (review + edit first)
New-Item -ItemType Directory -Force 'C:\ProgramData\Kanade\config'
Copy-Item .\agent.toml   'C:\ProgramData\Kanade\config\'
Copy-Item .\backend.toml 'C:\ProgramData\Kanade\config\'

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

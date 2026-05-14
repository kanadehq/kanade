<picture>
  <source media="(prefers-color-scheme: dark)" srcset="https://raw.githubusercontent.com/yukimemi/kanade/main/assets/logo-dark.svg">
  <img src="https://raw.githubusercontent.com/yukimemi/kanade/main/assets/logo.svg" alt="kanade — orchestrate fleets of Windows endpoints" width="540">
</picture>

> 奏 — *orchestrate*. A self-hosted Rust pub/sub backbone for managing
> thousands of Windows endpoints without Active Directory. NATS / JetStream
> carries inventory polling, fleet-wide rollouts, and ad-hoc emergency
> commands on a single channel.

**Status: 0.1.0 — Sprint 1 (PoC) shipping.** Agent + admin CLI + local
single-node NATS, end-to-end echo roundtrip verified. The full design
lives in [docs/SPEC.md](./docs/SPEC.md) (Japanese, ~1150 lines covering
Part 1 overview and Part 2 detailed design across Sprints 1-6).

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
- **Declarative job manifests in Git** (Sprint 2+) — review, history,
  rollback all come for free.
- **Three layers of stop-the-bleed.** Stream max-msgs-per-subject
  replaces stale rollouts in the broker; consumer-side version checks
  guard execution; `kill.{job_id}` terminates running children. The
  emergency-stop path is wired from MVP, not bolted on later (see
  [SPEC.md §2.6](./docs/SPEC.md)).
- **Phased build-out.** One server is enough for a few hundred
  endpoints; the same code scales to a 3-node NATS cluster + replicated
  backend + Postgres for several thousand.

## Crates

| crate            | kind | role |
|------------------|------|------|
| `kanade-shared`  | lib  | wire types (`Command` / `ExecResult` / `Heartbeat`), NATS subject helpers, and a [teravars]-backed config loader |
| `kanade-agent`   | bin  | Windows-side resident daemon. Connects to NATS, subscribes to `commands.*`, spawns child processes, publishes `ExecResult`, heartbeats every 30 s |
| `kanade`         | bin  | operator-side admin CLI. Sends commands via NATS request/reply (`kubectl`-style single-name entry point) |

A future `kanade-backend` crate lands in Sprint 3 (axum API + SQLite
projector + scheduler, fronting the CLI / Web UI).

## Quick start

Install a single-node NATS server and start JetStream:

```powershell
scoop install nats-server   # or: winget install nats-io.nats-server
nats-server -js -p 4222
```

Run the agent in a separate terminal (reads `agent.toml` from the repo
root, picks up the local hostname as `pc_id`):

```powershell
cargo run -p kanade-agent
```

Round-trip a script through NATS — `$env:COMPUTERNAME` doubles as the
agent's `pc_id` for the local-loopback case:

```powershell
cargo run -p kanade -- run $env:COMPUTERNAME -- 'echo hello from kanade'
```

The CLI prints `exit_code`, `stdout`, and `stderr` of the remote
execution. Liveness probe via heartbeat:

```powershell
cargo run -p kanade -- ping $env:COMPUTERNAME
```

## Config (`agent.toml`)

Tera-templated TOML loaded via the [teravars] crate. The intent is for
a single file to drive both Windows and Linux agents through
`system.host` cross-platform hostname resolution and `is_windows()`
branches (see [SPEC.md §2.4.4](./docs/SPEC.md)). Sprint 1 ships a
minimal subset — full self-referencing `[vars]` blocks land once
[teravars#21](https://github.com/yukimemi/teravars/issues/21) is
resolved.

## Dev workflow

```powershell
cargo make check       # fmt-check + clippy + test + lock-check
cargo make fmt         # apply formatting
cargo make on-add      # renri post_create hook (apm install + vcs fetch)
```

## Sprint 1 scope

- [x] Cargo workspace + three crates (`shared` / `agent` / CLI)
- [x] Agent: NATS connection, `commands.all` + `commands.pc.{pc_id}` + `kill.>` subscribers
- [x] Agent: child-process execution, `ExecResult` published to `results.{request_id}`
- [x] Agent: heartbeat published to `heartbeat.{pc_id}` every 30 s
- [x] CLI: `run` (request/reply) + `ping` (heartbeat wait)
- [x] `cargo make check` passes workspace-wide
- [x] Local NATS round-trip verified

Sprint 2 onwards — Windows Service hosting (`windows-service`),
inventory collection (WMI), kill-signal child-process termination, YAML
job manifest parser, wave / jitter delivery, mTLS, self-update, backend
(axum + SQLite + projector). See [docs/SPEC.md](./docs/SPEC.md)
Sprints 2-6 for the full roadmap.

## Scaffolded with kata

The skeleton (`AGENTS.md` / `Makefile.toml` / `clippy.toml` /
`rustfmt.toml` / `.github/workflows/*` / etc.) was applied via
[`github.com/yukimemi/pj-presets:rust-cli`](https://github.com/yukimemi/pj-presets)
through `kata init`. The Cargo workspace layout under `crates/` is
hand-written because the preset is single-crate by default; a
`pj-rust-workspace` layer is on the future TODO once the multi-crate
patterns stabilise.

## License

MIT — see [LICENSE](./LICENSE).

[teravars]: https://github.com/yukimemi/teravars

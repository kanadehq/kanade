# Introduction

**kanade — 奏** is an endpoint management system for Windows, Linux and Apple Silicon macOS fleets. It
gives an operator a single CLI / SPA to run scripts, install
software, gather inventory, and stream live perf data from hundreds
of PCs at once. The pieces:

| Component | What it is |
|-----------|------------|
| **kanade-agent** | Service that runs on each managed PC. Subscribes to NATS, executes commands, ships results. |
| **kanade-backend** | HTTP API + projector. Persists state, serves the SPA, exposes operator endpoints. |
| **kanade-client** | Optional end-user Tauri app on Windows and Apple Silicon macOS 14+; macOS native notification/startup integration is deferred. |
| **NATS server** | Message broker for command fan-out + result aggregation. Agents use NATS for commands and telemetry; installer jobs can also fetch artifacts over backend HTTP. |
| **kanade CLI** | Operator-facing command line: publish binaries, fire jobs, query state. |

This site covers two audiences:

- **Operators** running a kanade fleet — how to update each
  component without ssh-ing into endpoints (see
  [Agent-mediated updates](./operations/agent-mediated-updates/index.md)).
- **Developers** writing PowerShell jobs the agent will execute —
  what works, what doesn't, what changed in recent agent versions
  (see [Writing scripts for the agent](./developing/writing-agent-scripts.md)).

The detailed protocol / on-wire spec lives at
[Spec](./reference/spec.md) (the legacy single-page document; will
be split into chapters as the docs site fills out).

# Operations overview

Day-2 operations in kanade fall into two flows:

1. **Direct install** — drop binaries + config on a fresh host and
   register the Windows service. Used to bootstrap the first agent,
   the initial backend, and the NATS server. Scripts:
   `scripts/deploy-agent.ps1`, `scripts/deploy-backend.ps1`,
   `scripts/deploy-nats.ps1`. Run **manually** on the target host.

2. **Agent-mediated update** — once an agent is running, the agent
   itself can install / update other components on its own host
   without ssh / RDP. The operator publishes binaries + script
   bodies to the broker, then fires a job; the agent fetches,
   verifies, swaps, and restarts services. This is the bulk of
   day-2 operations.

The agent-mediated flow has the same shape regardless of what
you're updating:

```text
operator host ─► kanade CLI ─► NATS broker ─► agent (on target host)
                    │                              │
                    ├── publish binary ────────────► fetches from
                    │   to OBJECT_APP_PACKAGES        OBJECT_APP_PACKAGES
                    ├── publish script ────────────► fetches from
                    │   to OBJECT_SCRIPTS             OBJECT_SCRIPTS
                    ├── register / update job ─────► reads job manifest
                    │                                 from `jobs` KV
                    └── exec job ──────────────────► PowerShell child
                                                      runs the script
```

Component-specific guides:

- [kanade-backend](./agent-mediated-updates/backend.md) — the
  HTTP / projector binary
- [kanade-client](./agent-mediated-updates/client.md) — the Tauri
  end-user app
- [NATS server](./agent-mediated-updates/nats.md) — the broker
  itself (yes, you can update the broker over the broker)
- [kanade-agent itself](./agent-mediated-updates/agent-self.md) —
  the agent self-update path (different from the other three; it
  uses a dedicated rollout bucket, not the generic
  OBJECT_APP_PACKAGES + script pair)

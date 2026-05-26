# Removing kanade from a host (undeploy)

Production rollback path. When a host needs to come off kanade —
because a rollout broke something, the host is being
decommissioned, or you just want a clean slate to re-install
from — there's one undeploy script per component, mirroring the
deploy script that put it there.

| Component | Deploy | Undeploy |
|-----------|--------|----------|
| Agent | `scripts/deploy-agent.ps1` | `scripts/undeploy-agent.ps1` |
| Backend | `scripts/deploy-backend.ps1` | `scripts/undeploy-backend.ps1` |
| NATS server | `scripts/deploy-nats.ps1` | `scripts/undeploy-nats.ps1` |
| Client (Tauri) | `jobs/scripts/install-kanade-client.ps1` (agent-driven) | `scripts/undeploy-client.ps1` |

All four are admin-only and idempotent — safe to re-run after a
partial uninstall, safe to run when the component is already
gone (each step logs "not present, skipping" and moves on).

## Default posture: safe

Run with no flags and the script:

- Stops the Windows service.
- Unregisters it from SCM (waits for the entry to actually
  disappear, so a re-deploy doesn't race a pending removal).
- Removes the installed binary from `%ProgramFiles%\Kanade\`,
  including any half-completed `<exe>.new` / `<exe>.old` swap
  artefacts.
- Removes any inbound firewall rule the deploy script created
  (pass `-KeepFirewall` to skip this — useful when an external
  WAF / Group Policy owns the rule).
- **Keeps** everything under `%ProgramData%\Kanade\` (config,
  logs, JetStream data, SQLite DB, …) so forensics / rollback /
  re-deploy can proceed without losing state.
- **Keeps** registry-stored secrets at
  `HKLM:\SOFTWARE\kanade\<role>\*`.

That's enough for the common case: "this host's kanade is
misbehaving, get it off without destroying state".

## `-Purge`: destructive cleanup

Adds:

- Removes the component's exclusive entries under
  `%ProgramData%\Kanade\`. Crucially, **only the component's own
  files** — agent / backend / NATS share the same root and each
  script avoids touching the others' files.
- Removes the matching `HKLM:\SOFTWARE\kanade\<role>\*` key
  (unless `-KeepSecrets` is also passed — useful when multiple
  components share the same bearer).

| Component | What `-Purge` removes |
|-----------|------------------------|
| Agent | `config\agent.toml`, `logs\agent.*.log`, `outbox\`, `HKLM:\SOFTWARE\kanade\agent\` |
| Backend | `config\backend.toml`, `data\*.db*` (**SQLite — historical results / inventory wiped**), `logs\backend.*.log`, `HKLM:\SOFTWARE\kanade\backend\` |
| NATS | `config\nats-server.conf`, `nats\` (**JetStream — all KV / Object Store / streams wiped**), `logs\nats*.log` |
| Client | Nothing extra (no per-user state yet) |

> ⚠️  `undeploy-nats.ps1 -Purge` and `undeploy-backend.ps1
> -Purge` are the dangerous ones. The first wipes the fleet's
> entire JetStream state (agent_releases, app_packages, scripts,
> jobs, agent_config, results stream); the second wipes the
> projector's historical SQLite. **Both are unrecoverable
> without out-of-band backups.** The scripts print a loud banner
> before running.

## Rollback recipes

### Bad rollout on one canary host

```pwsh
# On the canary, as Admin:
.\scripts\undeploy-agent.ps1            # safe default
# kanade is now off the host. Re-deploy when ready:
.\scripts\deploy-agent.ps1 -SourceDir C:\path\to\prev-version
```

### Decommission a host permanently

```pwsh
.\scripts\undeploy-agent.ps1 -Purge
```

### Wipe a dev box for a clean re-install

```pwsh
.\scripts\undeploy-agent.ps1 -Purge
.\scripts\undeploy-backend.ps1 -Purge   # ⚠️ SQLite gone
.\scripts\undeploy-nats.ps1 -Purge      # ⚠️ JetStream gone
.\scripts\undeploy-client.ps1
# Now nothing about kanade exists on the box.
```

### Rebuild a single bad service without touching state

```pwsh
.\scripts\undeploy-backend.ps1          # safe default: SQLite intact
.\scripts\deploy-backend.ps1 -Recreate  # fresh service registration, same data
```

## What undeploy does NOT do

- It doesn't notify the rest of the fleet that this host has
  gone away — the backend will keep listing it under "agents"
  until its heartbeat ages out (`/api/agents` staleness
  threshold). If you want it removed from the SPA immediately,
  delete the row via the backend API after undeploy.
- It doesn't roll back the deployed binary to a previous
  version. "Roll back" in this script's vocabulary means
  "remove entirely"; if you want to swap to an older version,
  re-run the matching `deploy-*.ps1` against a folder containing
  the older binary.
- It doesn't touch NATS-side state when you remove the agent —
  the agent's `target_version` entry under `agent_config.pcs.<pc>`
  stays in the KV. Clean those up server-side with
  `kanade jetstream kv del agent_config pcs.<pc>.target_version`
  if needed.

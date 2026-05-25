# Updating NATS server

Updating the broker on a managed host is the most interesting
case because the agent **talks to the broker over the broker** —
stopping NATS means losing the agent's connection mid-job. The
machinery handles this with two mechanisms working together:

1. **Reconnect.** The agent's NATS client reconnects automatically
   on broker restart. No human intervention needed.
2. **Outbox.** Job results produced while the broker is down are
   queued under `%ProgramData%\Kanade\outbox\` and replayed once
   the connection comes back. The result row reaches the backend
   as soon as the new NATS server is up.

So the flow looks the same as
[backend updates](./backend.md) — the script Stops the service,
swaps the binary, Starts it — and the agent transparently rides
out the broker gap.

## Caveats specific to NATS updates

| Concern | Reality |
|---------|---------|
| Will the result row be lost? | No — outbox persists it across the broker outage and drains on reconnect. |
| Can I update from the SPA? | Yes, same as any job — `kanade exec install-kanade-nats --pcs <broker-host>`. |
| What if NATS doesn't come back up? | The result will sit in the outbox indefinitely. Operators should monitor `outbox/` on the broker host as a leading indicator. |
| What if the new NATS version is incompatible (JetStream upgrade etc.)? | Roll a single canary first (`--pcs <one-broker>`), watch outbox + backend health, then roll out fleet-wide. The 5-min cache TTL for SPA queries means you'll see the canary's state within a few minutes. |

## Manual install (bootstrap)

For the very first install — when there's no agent on the broker
host yet — use the direct workflow:

```pwsh
.\scripts\build-release.ps1 -Roles nats       # fetches nats-server.exe
                                              # from github.com/nats-io/nats-server/releases
.\scripts\deploy-nats.ps1 -NatsToken '<token>'
```

This installs `nats-server.exe` to `%ProgramFiles%\Kanade\` and
`nats-server.conf` to `%ProgramData%\Kanade\config\` (with ACL
hardened to SYSTEM + Administrators because the bearer token
lives in plaintext), registers the `KanadeNats` Windows service,
opens TCP 4222 (broker) + 8222 (monitoring HTTP), and starts the
service.

## Agent-mediated update (steady state)

> **Status:** template-only. `scripts/deploy-nats.ps1` doesn't
> ship `$AgentSource*` knobs yet — agent-mode is on the backlog.
> The shape below is what it WILL look like once the knobs land.

### 1. Build / fetch nats-server.exe

Either:

```pwsh
.\scripts\build-release.ps1 -Roles nats   # fetches the binary
```

…or download it directly from
[github.com/nats-io/nats-server/releases](https://github.com/nats-io/nats-server/releases).

### 2. Publish the binary

```pwsh
kanade app publish nats-server 2.10.20 .\nats-server.exe
```

### 3. Edit deploy-nats.ps1

Once the agent-mode knobs ship, the pattern matches
[deploy-backend.ps1](./backend.md#3-edit-scriptsdeploy-backendps1):

```powershell
$AgentSourceUrl       = 'http://kanade-backend.example.com:8080'
$AgentSourceVersion   = '2.10.20'
$AgentSourceSha256    = '<lowercase hex of nats-server.exe>'
$AgentSourceAuthToken = '<bearer for the backend HTTP API>'
```

### 4. Publish + register + exec

```pwsh
kanade script publish deploy-nats 2.10.20 .\deploy-nats.edited.ps1
kanade job create jobs\install-kanade-nats.yaml
kanade exec install-kanade-nats --pcs <broker-host>
```

The job manifest will look like:

```yaml
id: install-kanade-nats
version: 2.10.20
execute:
  shell: powershell
  script_object: deploy-nats/2.10.20
  timeout: 300s
  run_as: system
require_approval: true
```

### 5. Verify

After the broker comes back, the outbox drains and you'll see the
result row in `/api/results`. Confirm the new NATS version via
the broker's monitoring endpoint:

```pwsh
curl http://<broker>:8222/varz | python -m json.tool | rg version
```

## Why we don't need a separate "broker update" mechanism

Earlier designs considered a dedicated bootstrap channel
(parallel NATS link the agent uses just for broker updates) to
avoid the self-update-over-broker chicken-and-egg. The outbox +
reconnect pair makes that unnecessary: the result is "merely
delayed", not "lost". One transport, one mental model.

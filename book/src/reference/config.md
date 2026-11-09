# Configuration Reference

`kanade` services rely on structured configurations loaded from TOML files, environment variables, or registry paths.

---

## 1. Agent Configuration

Both services resolve config in this order: `--config <path>`, a nonempty
`KANADE_AGENT_CONFIG` / `KANADE_BACKEND_CONFIG`, then the existing native file
(`%ProgramData%\Kanade\config\agent.toml` / `backend.toml` on Windows,
`/etc/kanade/agent.toml` / `backend.toml` on Unix). There is no current-directory
fallback or synthesized default config; missing configuration is an error.
TOML templates use teravars, so an environment variable changes a field only
when the template explicitly reads it.

### Dev Configuration (`configs/agent.dev.toml`)

```toml
[vars]
hostname = '{{ system.host }}'
pc_id = '{{ env(name="KANADE_DEV_AGENT_ID", default=vars.hostname) }}'

[agent]
id = '{{ vars.pc_id }}'
nats_url = 'nats://127.0.0.1:4223'

[log]
path = './target/dev-data/agents/{{ vars.pc_id }}/agent.log'
level = 'info'
```

### Configuration Parameters

| Field | Type | Description |
|---|---|---|
| `agent.id` | String | Exact, case-sensitive host identity (`pc_id`), not a hardware UUID. The dev template accepts `KANADE_DEV_AGENT_ID`. |
| `agent.nats_url` | String | Broker address. `KANADE_NATS_URL` controls the CLI's `--server`; it does not override this service field. |
| `log.level` | String | Log filter; `RUST_LOG` overrides it. |
| `log.path` | Path | Destination for daily rotated logs. |
| `log.keep_days` | Integer | Retained daily files, including today; default 14, 0 disables on-disk logging. |

The data directory is **not** a TOML field. `KANADE_AGENT_DATA_DIR` overrides
`%ProgramData%\Kanade\data` on Windows or `/var/lib/kanade` on Unix.
The Linux and macOS setup scripts explicitly use `/var/lib/kanade-agent`.

### Per-PC job concurrency

`max_local_concurrent` lives in the layered `agent_config` store, not the
agent TOML file. It applies to backend schedules, agent-local schedules,
and operator runs from the CLI or administration SPA. One agent shares a
single budget across these paths. **User-triggered kanade-client actions
are exempt**: they start without waiting for or consuming a slot. They do
not interrupt jobs already running.

When no scope sets the limit, the agent uses its locally available logical
CPU count (falling back to 1 if detection fails). The backend leaves this
automatic value as `null`; it never substitutes the backend host's CPU count.
An explicit limit must be an integer of at least 1. Scopes apply in order:
built-in automatic default → global → groups → PC.

```sh
kanade config set max_local_concurrent=4
kanade config set --group low-power max_local_concurrent=2
kanade config set --pc EXACT-HOSTNAME max_local_concurrent=1
kanade config unset --pc EXACT-HOSTNAME max_local_concurrent
```

`kanade config` (`get`, `set`, `unset`, `clear`, `effective`) talks to the
backend HTTP API, not to NATS: it needs `KANADE_AUTH_TOKEN` (an operator-role
token to change anything, like the other HTTP subcommands) and no broker
token. Automation that calls `kanade config` must therefore authenticate with
an auth token. The backend does each `set` / `unset` as a compare-and-swap on
that one field, so a concurrent rollout writing `target_version` on the same
scope is not overwritten, and a request that would change nothing writes
nothing. Each change is recorded in the audit log under the caller's account;
the audit entry is published after the write and is best-effort, not atomic
with it. `effective` prints the backend's resolved view; its warnings are the
backend's rendered text.

`unset` restores inheritance; when all applicable scopes omit the field,
CPU-based sizing resumes. Updates apply without restarting the agent.
Reducing the limit lets existing jobs finish and holds new jobs until enough
slots are free. A job keeps its slot through retries, collection and finalize.
Jitter happens before admission. Queued jobs can be killed and are skipped if
their starting deadline expires. Waiting does not
consume the script timeout or emit a running lifecycle event. Execution gates
(version pin, revocation, staleness and deadline) are checked before jitter
and again after admission.

Compatibility note: `runs_on: agent` schedules previously omitted their
starting deadline from local commands. They now enforce `starting_deadline`
from the local fire time, including jitter and slot waiting. Existing schedules
whose jitter exceeds that deadline can therefore report a deadline skip even
with a free slot. Keep the deadline longer than the maximum jitter plus the
acceptable queue wait, or omit it when late execution is acceptable.
Kill delivery is best effort: if the broker subscription fails, the agent logs
a warning and continues waiting under the same capacity and deadline rules.

A critical job can opt out for every execution of that manifest:

```yaml
execute:
  shell: powershell
  script: 'Write-Output "critical action"'
  timeout: 30s
  bypass_local_limit: true
```

Exempt jobs consume no slots, so total host concurrency can exceed the limit.
The budget is agent-process-wide; it assumes one agent service per PC.
`constraints.max_concurrent` remains a separate backend, fleet-wide per-job
limit. This setting does not fix its jitter accounting issue (#1373).

## 2. Backend Configuration

The backend uses the same discovery rules above. For development, pass
`--config configs/backend.dev.toml` (or use `cargo make backend-dev`).

### Dev Configuration (`configs/backend.dev.toml`)

```toml
[server]
bind = '127.0.0.1:8081'

[nats]
url = 'nats://127.0.0.1:4223'

[db]
sqlite_path = './target/dev-data/backend.db'

[log]
path = './target/dev-data/backend.log'
level = 'info'
```

### Configuration Parameters

| Field | Type | Description |
|---|---|---|
| `server.bind` | String | HTTP/WebSocket bind address. |
| `server.public_url` | Optional String | Public SPA base URL for account emails; set it behind a proxy and to avoid deriving links from request headers. |
| `nats.url` | String | Broker address. |
| `nats.monitor_url` | Optional String | Monitoring base URL; defaults to HTTP on the broker host, port 8222. |
| `db.sqlite_path` | Path | SQLite file path. |
| `log.path`, `log.level`, `log.keep_days` | | Same logging fields as the agent. |

`KANADE_BIND_ADDR`, `DATABASE_URL` and `KANADE_NATS_URL` do not override these
service fields. Authentication is not an `[auth]` TOML section:
`KANADE_AUTH_DISABLE` disables authentication whenever present (even `0`),
for development only. Production uses backend secrets such as `JwtSecret` /
`KANADE_JWT_SECRET`, `StaticToken` / `KANADE_AUTH_STATIC_TOKEN`, and
`BootstrapAdminPassword` / `KANADE_BOOTSTRAP_ADMIN_PASSWORD`.

SMTP connection settings live in `server_settings.mail` (Settings → Server),
not a `[mail]` TOML section. They take effect after backend restart. The SMTP
password remains a separate `MailPassword` / `KANADE_MAIL_PASSWORD` secret.

---

## 3. Windows Registry Integration

In production environments, security-sensitive tokens (like NATS client tokens and administrative API bearer tokens) are stored in the Windows Registry values protected by ACLs. These are plaintext REG_SZ values,
not encrypted storage; non-Windows hosts use the corresponding environment variables.

### Key Paths
- **Agent Settings**: `HKLM:\SOFTWARE\Kanade\agent`
- **Backend Settings**: `HKLM:\SOFTWARE\Kanade\backend`

These registry paths are protected with local ACL configurations, allowing read permissions strictly to `SYSTEM` and Administrators in the deployment scripts.

### Passkeys (WebAuthn)

Passkeys are configured **only** in **Operator Settings → Server Settings →
Passkeys** (the persisted `server_settings` KV document). There is no TOML
setting. New installations default to disabled. An operator sets the RP ID,
allowed origins (one per line), optional display name (default: Kanade), and
explicitly enables passkeys. The update API uses nested per-field PATCH
semantics: omission preserves a field, null clears it, and an origins array
replaces the whole array. Invalid configurations are rejected before saving.

The RP ID must be a lowercase ASCII domain name without scheme, port or path.
Origins must be HTTPS bare origins matching that domain or a subdomain; HTTP
is accepted only for localhost. Values are never derived from request headers.
Saving valid settings takes effect immediately, without restarting, on all
backend instances. The settings card reports the effective enabled/disabled
state and configuration/read failures. Each change invalidates outstanding
registration and sign-in challenges, including a change back to an earlier RP.
A ceremony's final authoritative settings read is its commit boundary: a settings
write after that read applies to subsequent ceremonies.

Credential listing, removal and administrator reset remain available when
passkey sign-in is disabled. Enrollment/removal still require the current
password and, when enabled, TOTP. Password/TOTP sign-in and account recovery
remain available. RP changes can make credentials registered under another RP
unusable; register new credentials for the configured RP.

Configuration is shared across backends, but in-flight challenges remain in
process memory. Route both steps of each ceremony to the same backend (sticky
sessions); restarting that backend discards its outstanding challenges. Keep
the backend behind a trusted proxy that appends the real client IP for throttling.

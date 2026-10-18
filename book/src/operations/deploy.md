# Installation and Deployment

The cross-platform `kanade deploy` design and script retirement gates are in [SPEC §2.13](https://github.com/kanadehq/kanade/blob/main/docs/SPEC.md#213-クロスプラットフォーム-デプロイ設計-kanade-deploy-cli--typed-operations). This is an unimplemented design; keep using the current procedures below during migration. It targets enrolled agents only and leaves the Agent Installation screen one-liners unchanged.

This section details how to bootstrap `kanade` components as native Windows services in production or staging environments.

## Deployment Model

Production hosts and target endpoints run `kanade` components as background Windows services. This ensures high availability and automatic startup.

| Service Name | Triple / Binary | Config Source | Typical Target |
|---|---|---|---|
| **KanadeNats** | `nats-server.exe` | Hardened Registry / Registry-baked CLI flags | Central server |
| **KanadeBackend** | `kanade-backend.exe` | Hardened Registry / Config file | Central server |
| **KanadeAgent** | `kanade-agent.exe` | Hardened Registry / Local state DB | Managed endpoints |

---

## 1. Prerequisites

- **Host OS**: Windows 10/11 or Windows Server 2016+.
- **gsudo**: Required to perform elevated installations from standard user shells (or run commands from an Administrator-level PowerShell prompt).
- **Network Routing**: Managed endpoints must be able to reach the NATS server port (default `4222`) over TCP.

---

## 2. Setting Up the NATS Server (Broker)

The NATS server acts as the messaging core.

1. Stage the deployment bundle using `scripts/build-release.ps1 -Roles nats`.
2. Deploy the service with elevation:
   ```powershell
   # Elevated PowerShell prompt
   & "dist\nats\deploy-nats.ps1" -NatsToken "your-secure-nats-token" -Recreate
   ```
This installs the **KanadeNats** service, configures it to run under the local system account, sets up JetStream data directories, and locks down the secure authorization token in the Windows registry.

### Optional: per-role users instead of the shared token

By default the broker runs on the single shared token above. To run the three
role users (`agent`, `backend`, `breakglass`) instead, deliberately:

```powershell
# Default cost 8 requires a long random password (32 random alphanumeric characters recommended).
# Hashes only, minted with scripts/ops/nats-password-hash.ps1 (or .sh); never plaintext.
$env:KANADE_NATS_AGENT_PASSWORD_HASH      = '<hash>'
$env:KANADE_NATS_BACKEND_PASSWORD_HASH    = '<hash>'
$env:KANADE_NATS_BREAKGLASS_PASSWORD_HASH = '<hash>'
& "dist\nats\deploy-nats.ps1" -UseNatsUsers      # switch
& "dist\nats\deploy-nats.ps1" -UseNatsToken -NatsToken "your-secure-nats-token"   # revert
```

`-UseNatsUsers` replaces the `authorization` block of the installed config with
an include of `nats-server.users.conf` (the template with the hashes
substituted) and gives both files the SYSTEM + Administrators ACL;
`-UseNatsToken` restores the shipped token block (hand edits inside it are not
restored) and removes the users file. The service is stopped and started by
the script as always (`-NoStart` defers the start). On Linux the equivalent is
`KANADE_NATS_AUTH_MODE=users|token` for `setup.sh`, recorded so re-runs keep it,
with a manual `systemctl restart nats-server` to apply it (see
`deploy/linux/README.md`).

For the recommended bcrypt cost 8 and broker burst measurements, see
[Choosing the bcrypt cost](nats-user-switch.md#choosing-the-bcrypt-cost).

**The switch is atomic:** a config cannot hold both a token and users, and a
client presenting a token is rejected once users exist. Every agent, backend
and CLI host must already hold a user pair before the broker flips; follow the
separately documented readiness procedure first.

### Expected authentication mode and connection findings

The backend watches how every connection to the broker authenticated and
raises a warning when that is not what you intended, so that a switch to
per-role NATS users that failed or silently reverted, or a broker that
authenticates nobody, does not go unnoticed. The broker's own report is the
source (its monitoring endpoint, polled every minute), not anything an agent
says about itself.

**Setting.** *Settings → Server → Expected NATS authentication* (the
`nats_auth_mode` server setting) is either `token` (the default, today's
shared token) or `users` (per-role users). It is an expectation only: it
changes nothing about how any process connects, it just defines what counts as
unexpected. Set it to `users` when you switch the broker to the `users` block,
and back to `token` if you switch back.

**Findings.** Each connection is judged by the mode you expect, the role it
announces in its connection name (`kanade-<role>[/identity]`) and the user the
broker reports it authenticated as:

| finding | when | severity |
| --- | --- | --- |
| `broker_open` | the broker authenticated nobody (`no-auth`), in either mode | critical |
| `token_reverted` | `users` expected, but the connection used the shared token | critical |
| `role_user_mismatch` | `users` expected, but the user is not the announced role's (`agent`, `backend`, `breakglass` for the CLI), e.g. the backend user on an agent connection | warning |
| `credential_unnameable` | `users` expected, but the credential could not be named | info |

Under `token`, the shared token is expected and only `broker_open` is
reported. The announced role is chosen by the client, so a mismatch is
evidence of a misconfigured or misused host, while a match proves nothing: it
is only the connection's claim, compared against the broker's word.

Findings are grouped by kind and announced role, so a whole fleet reverting at
once is a handful of entries, each with a connection count and at most five
registered host names. A finding is raised when it first appears, shows as a
banner on the Dashboard, and is marked resolved when it is gone (a later
recurrence is raised again). While the backend's own broker connection
cannot prove the mode, user names read as `unknown`, so expect
`credential_unnameable` entries until it reconnects.

**Grace after a switch.** When you change the expected mode, new findings are
held back for **5 minutes**. Switching re-authenticates every client, and each
reconnects with backoff while the poll only looks once a minute, so for a few
polls the broker's view is a mix of old and new; the window keeps those
stragglers from being reported against the mode you just chose. Re-saving the
same mode does not start it again, and findings already open still update and
resolve during it. The Dashboard says when the window is open.

**Reading it from outside.** `GET /api/health/fleet` carries a `nats_auth`
block: `expected_mode`, `grace_until`, `poll_status`, `open_total` (the
connections covered) and `findings[]` (`kind`, `subject`, `severity`, `count`,
`sample_hosts`, `first_seen_at`, `occurrence_id`). It does **not** change the response's
`status` or HTTP code, so an existing monitor keeps behaving as before; a
monitor that should alert on this must read `nats_auth.findings`. A
`poll_status` other than `ok` means the audit could not see the whole broker
(`incomplete`, `endpoint_unreadable`, `settings_unreadable`, `stale`), and an
empty list is then not a clean result.

**What is never shown.** The reported value of a connection may be a secret on
some broker builds, so findings carry fixed vocabularies (kind, severity,
the role bucket `agent` / `backend` / `cli` / `unnamed` / `other`), counts,
registered host samples and an occurrence reference. Admin-only connection
evidence adds broker-issued identifiers and parsed timestamps, as described
below. Neither the credential, nor the reported user name, nor the raw
connection name is copied into notice evidence.

### Retained NATS authentication evidence

Each new occurrence has its own numeric `occurrence_id` in the authenticated
fleet health finding. The aggregate `backend_notices` row remains compatible;
reopening it does not overwrite earlier occurrences. Dashboard's admin-only
**NATS authentication evidence and resolved history** details and
`GET /api/notices/nats-auth` expose the latest 100 occurrences, including resolved
ones. Pass `?before=<oldest-id>` to page backwards. Viewers and operators cannot
read this endpoint. Existing service-token and auth-disabled administrator
semantics still apply. Identifiers are selectable and copyable for log searches.

A sample records the broker-issued decimal `cid`, `server_id` **and broker
startup timestamp** (`/varz.start`), backend first/last observation times (page
fetch completion, before settings/database work), broker page observation time, and connection start/last activity timestamps when supplied.
Occurrence timestamps describe notice processing; individual sample timestamps
describe page acquisition. A cid alone, or server_id alone, must not be treated
as a restart identity.
The collector reads `/varz` before and after the paginated `/connz` walk and
checks each page's server_id, timestamp and duplicate cids, within the existing
20-second timeout. Missing identity or an inconsistent/restarted broker rejects
the snapshot, reports an unreadable poll and leaves open findings unresolved.
A paging cap reports an incomplete poll and likewise cannot resolve findings.
This is a best-effort walk of live connections, not an atomic broker snapshot.

Evidence includes only a fixed credential class (`user`, `shared-token`,
`no-auth`, `unknown`) and a fixed `claimed_role` bucket. The role is an
**untrusted client claim**, unlike broker-issued connection identifiers.
Registered host samples remain in the existing aggregate notification. Raw
`authorized_user`, tokens, passwords, connection names, arbitrary client metadata
and unrestricted monitoring responses are never copied into connection evidence.
Peer addresses are deliberately omitted: proxies and loopback commonly identify
an intermediary rather than the originating client. The admin boundary limits
exposure of retained connection correlation data.

Per occurrence, at most the first 10 sampled identities and 10 most recently
observed other identities are retained, deduplicated by server_id/start/cid,
within a 32 KiB JSON limit. Each sample and the latest occurrence observation
record whether their poll was complete; partial polls may undercount connections. Broker IDs are limited to 64 alphanumeric bytes;
other context is parsed timestamps or fixed vocabulary. `sampled_out` indicates
sample/size omissions. Evidence updates on each successful poll even if group
counts/host samples have not changed. Ledger changes, occurrence creation,
resolution and evidence updates commit in one SQLite transaction.

Samples expire 30 days after their last observation, including samples in open
findings. Resolved occurrences expire 30 days after resolution and are also
capped at the newest 1000. Cleanup runs when the projector starts and every
poll (including failed polls); an offline backend performs cleanup when restarted.
There are only the classifier's fixed kind/role buckets of open occurrences.
SQLite backups must use the operator's corresponding retention/access policy;
logical expiry does not securely erase old database pages or backups.

Old notices have no recoverable connection evidence; the UI labels absent or
expired evidence accordingly. The identifiers lost in the 2026-10-18 incident
cannot be reconstructed by this migration. Last observed is **not** a disconnect
time. Connections that open and close between minute polls can be missed, and
wiping the projection database loses these snapshots permanently. This change
leaves classification conditions unchanged; authentication/pre-authentication
classification is a separate change. Its integration boundary remains `Observed`,
`Poll.complete` and the shared `evaluate_observed` adapter; the additive migration is named
`20261018000000_nats_auth_evidence.sql`.

---

## 3. Deploying the Backend API & SPA

The backend manages operator connections and processes event logs.

1. Stage the backend binaries and React SPA bundle using `scripts/build-release.ps1 -Roles backend`.
2. Deploy the service:
   ```powershell
   # Elevated PowerShell prompt
   & "dist\backend\deploy-backend.ps1" `
       -NatsToken "your-secure-nats-token" `
       -StaticToken "your-operator-spa-bearer-token" `
       -ForceConfig -Recreate
   ```
- `-NatsToken`: Connects the backend to the local NATS server securely.
- `-StaticToken`: Defines the API bearer token required for operator CLI/SPA logins.

The deployment script registers the **KanadeBackend** Windows service, sets the appropriate ACLs, and verifies the endpoint.

---

## 4. Installing the Agent on Target Endpoints

Install the agent on every endpoint PC that you want to manage.

1. Stage the agent bundle using `scripts/build-release.ps1 -Roles agent`.
2. Copy the contents of the `dist/agent` folder to the target PC.
3. On the target PC, run the installer:
   ```powershell
   # Elevated PowerShell prompt
   & ".\deploy-agent.ps1" -NatsToken "your-secure-nats-token" -ForceConfig -Recreate
   ```
The script:
- Places `kanade-agent.exe` into its destination directory.
- Secures the configuration and NATS token in the Windows registry path (`HKLM:\SOFTWARE\Kanade\agent`).
- Registers and starts the **KanadeAgent** service.

#### Agent-role NATS user in generated installers

`deploy-agent.ps1` also accepts `-NatsUser` / `-NatsPassword` (the shell
scripts take `KANADE_NATS_USER` / `KANADE_NATS_PASSWORD`), both or neither.
The installers the backend generates (Windows ZIP, Linux / macOS tarball)
embed such a pair when the server settings' `agent_install` section holds
both a user and a password, next to the token, so machines installed from now
on already carry the credential the broker may later require.

- The pair is the **agent role's** credential and is **shared by all agents by
  design**. The backend's own credential and the break-glass credential must
  never be entered in these settings.
- It is write-only, like the token: the API returns only `nats_user_set` /
  `nats_password_set`, and a settings update that omits both keeps the stored
  pair. A settings update that sets only one of the two is rejected, so
  replacing the password means sending the user again.
- A generated `README.txt` states whether a user pair is included, without
  revealing it; with no paragraph about it, none was embedded and the
  installer is exactly what it was before this setting existed.

Once the service is active, the agent establishes an outbound NATS connection, subscribes to command streams, and reports its online heartbeat back to the fleet backend.

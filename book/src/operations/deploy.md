# Installation and Deployment

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
`sample_hosts`, `first_seen_at`). It does **not** change the response's
`status` or HTTP code, so an existing monitor keeps behaving as before; a
monitor that should alert on this must read `nats_auth.findings`. A
`poll_status` other than `ok` means the audit could not see the whole broker
(`incomplete`, `endpoint_unreadable`, `settings_unreadable`, `stale`), and an
empty list is then not a clean result.

**What is never shown.** The reported value of a connection may be a secret on
some broker builds, so findings carry only fixed vocabularies (kind, severity,
the role bucket `agent` / `backend` / `cli` / `unnamed` / `other`), counts and
the names of hosts registered in the fleet. Neither the credential, nor the
user name, nor the raw connection name is stored or served.

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

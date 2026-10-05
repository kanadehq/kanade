# Switching the broker to role-level users

This is the operating procedure for moving the NATS broker from the single
shared token to the three role users (`agent`, `backend`, `breakglass`), with
the revert, and for checking readiness before and fallout after.

> **Nothing here is run by any automation.** No installer, scheduled job,
> backend task or agent performs this procedure or any step of it. The broker
> is switched only by a person running the deployment script on the broker
> host, and the checks below are run by that person. The expected-mode setting
> in the backend only changes what is *reported* as unexpected; it changes how
> no process connects.

## Why it needs a procedure

A broker configuration cannot hold both a token and users. The moment it is
reloaded with users, a client that presents only the token is refused. A host
that is not ready is therefore locked out **at that instant**, and because a
locked-out host also disappears from the broker's connection list and stops
heartbeating, it looks exactly like a powered-off one. Two consequences:

* readiness has to be established **beforehand, from outside the broker**;
* the damage has to be measured **afterwards by comparison** with what was
  alive before.

Both are done with `scripts/ops/nats-switch-check.ps1`, which talks only to
the backend HTTP API with an API token. It never connects to the broker and
never prints a credential.

## Prerequisites

1. **Client versions.** User credentials arrived in **0.61.0** (the release
   whose client library presents a configured user and falls back to the
   token). Agents, the backend and the break-glass CLI all need at least that.
   The script's `-MinAgentVersion` defaults to `0.61.0`. The shipped users
   configuration additionally assumes two later fixes: a connection-liveness
   probe that does not publish to `_INBOX`, and an agent that never overwrites
   object-store objects (so it needs no stream purge right). They are not in
   tag `0.62.1`; use the first release that contains them and pass it as
   `-MinAgentVersion`. With an older agent behind the current users file, a
   host can connect but then hit permission violations.
2. **Credentials are distributed.** Every agent holds the agent role's user
   pair, delivered by the credential distribution jobs (or by the generated
   installer carrying the pair). The check job that reports whether the pair
   is present must have run on every host.
   * The check job is not part of this repository, so the script has no
     default for `-CheckName`. The contract it expects: the check reports
     only whether **both** user and password are present (never a value),
     registers fleet-wide, and reports `ok` only when both are. If the name
     is wrong or the check is not fleet-wide, every host simply shows
     `no-check-result`.
   * `Readiness` must list nothing (step 1 below).
3. **Backend and break-glass hosts** are provisioned by their own deployment
   scripts (`deploy-backend.ps1` / the backend setup script with
   `-NatsUser` / `-NatsPassword`; the break-glass credential by hand on the
   operator machines). These are not distributed to agents and are **not
   covered by the script**: use the manual checklist below.
4. **A tested way to reach the broker host** that does not depend on the
   broker, the backend or the fleet: console, RDP/SSH from a management
   network, whatever you have. Test it *before*; the revert needs it.
5. **The old configuration is kept.** Know where the token and the previous
   installed `nats-server.conf` are. The deployment script's revert restores
   the *shipped* token block only; hand edits made inside that block are not
   restored.
6. **The broker service restarts itself** (Windows service recovery,
   `Restart=on-failure` on Linux), and so do the agents (below).
7. An **API token** for the backend, in `KANADE_API_TOKEN` (or `-Token`), and
   the backend URL in `KANADE_BACKEND_URL` (or `-BackendUrl`). Prefer an
   `https://` URL: the token travels in the request header.

### Manual checklist: backend and break-glass

Tick every line before the switch; the script cannot see these.

- [ ] The backend host holds the backend user pair and runs a 0.61.0+ backend
      (check the version it reports and its log for a user connection).
- [ ] The backend's own credential is **not** the agent pair and is not in the
      installer settings.
- [ ] The break-glass CLI on each operator machine is 0.61.0+, holds the
      break-glass pair, and has been exercised once against a broker running
      the users configuration (a rehearsal broker is fine).
- [ ] You know which of these hosts is also an agent host. If the backend host
      runs an agent, a backend lock-out also shows as that agent vanishing.

## The order

All commands are run from a checkout of the repository on a machine that can
reach the backend. Use `pwsh` (PowerShell 7) where you can: it lets the
script take the backend's own clock from the response `Date` header. On
Windows PowerShell 5.1 it falls back to the local clock and says so; keep the
clock correct.

### 1. Readiness

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Readiness -CheckName <credential-check> `
    -MinAgentVersion 0.61.0   # or the first release with the two later fixes
```

It lists every registered agent that is **not** ready, with the reason, and
exits 1 if there is any. A host is ready only if all hold:

| reason | meaning |
| --- | --- |
| `agent-version-too-old(..)` / `agent-version-unknown` | the agent is older than the minimum, or reports no parseable version |
| `no-recent-heartbeat` | not online now and not seen within `-RecentWithinHours` (default 24) |
| `no-check-result` | the check job has never reported for this host |
| `check-not-ok(<status>)` | the check reported something other than `ok` |
| `check-stale` | the check's last result is older than the backend's staleness window |
| `check-older-than-CheckedSince` | with `-CheckedSince <UTC date>`, an `ok` from before the distribution does not count |

Continue only when it prints "every registered agent is ready". Exit code 2
means the backend could not be read and **nothing was determined**.

What it cannot know: an `ok` is the agent's own report that a pair is
present, not proof that the values are correct or that the broker will accept
them; hosts that never registered are invisible; the backend and break-glass
are not covered.

The backend API has no per-host "online" field, so the script uses
`last_heartbeat` against the backend's own two-minute alive threshold, and
joins `/api/agents` with `/api/checks` by `pc_id`.

### 2. Snapshot

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Snapshot -SnapshotPath before.json
```

Records the hosts alive **now** (hostname, agent version, last heartbeat,
authenticated NATS user as the backend reports it). Hosts already offline are
deliberately left out so they are never counted as lock-outs. It will not
overwrite an existing file without `-Force`.

### 3. Switch

On the broker host, using the deployment script's users mode. The exact
options, the hash variables and the Windows / Linux differences are in
[Installation and Deployment](./deploy.md#optional-per-role-users-instead-of-the-shared-token);
do not improvise them here. In short:

* Windows: `deploy-nats.ps1 -UseNatsUsers` (the service is restarted by the
  script).
* Linux: `KANADE_NATS_AUTH_MODE=users` with `setup.sh`, then apply it with a
  restart or reload of `nats-server`.

Write down the **UTC time** at which the broker applied the new configuration;
`Compare` needs it as `-SwitchedAt`. Then set *Settings → Server → Expected
NATS authentication* to `users` so the backend reports anything unexpected.
Its findings are held back for five minutes after that change, which does not
affect the script.

### 4. Compare, about two heartbeat intervals later

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Compare -SnapshotPath before.json `
    -SwitchedAt 2026-01-01T09:00:00Z -WaitSeconds 150 -MaxDisappeared 0
```

A locked-out host still looks alive for up to two minutes after its last
heartbeat, so the script refuses to judge sooner than that plus one heartbeat
interval (`-HeartbeatSeconds`, default 60; set it to your fleet's configured
interval) and exits 3 "too early". `-WaitSeconds` makes it wait before
reading. Run it again after waiting rather than reading a partial answer.

It prints:

* **Disappeared**: alive in the snapshot, not alive now. These are the
  lock-out candidates.
* **Heartbeated after the switch and alive now**: indirect evidence of
  reconnection.
* **New hosts**: alive now and not in the snapshot.
* **Live hosts by authenticated NATS user.** Expected: every agent as
  `agent`. Unexpected: `shared-token` (still on the old token),
  `no-auth` (the broker authenticated nobody), `unknown`, another role's
  user, or *absent* (never correlated, which is unknown and not "old
  credential"). Values last changed before the switch are noted, because the
  backend keeps the last value it saw.

The exit code is 1 when more hosts disappeared than `-MaxDisappeared`. If no
host from the snapshot is alive it says the backend itself is probably locked
out; revert first and look at the backend host.

If the backend cannot be reached it retries (`-Retries`, `-RetryDelaySeconds`)
telling you so, because it may be reconnecting to the broker. If it still
fails it exits 2 saying plainly that this is **not** an empty result.

### 5. Decide

* Disappeared count within what you accepted, and the user table shows only
  `agent`: keep the switch and go to *Afterwards*.
* A handful disappeared: decide per host with the next section while the
  rest of the fleet runs; reverting for a few hosts is usually wrong.
* Many disappeared, a non-`agent` user on live hosts, or the backend looks
  locked out: **revert** (below), re-run `Compare` with the same snapshot to
  confirm the fleet is back, and find out why before trying again.

## Revert

On the broker host (through the way you tested in the prerequisites):
restore the token configuration and reload.

* Windows: `deploy-nats.ps1 -UseNatsToken -NatsToken '<token>'`.
* Linux: `KANADE_NATS_AUTH_MODE=token` with `setup.sh`, then restart or reload
  `nats-server`.

Set the expected authentication mode back to `token`. Nothing on any agent is
touched. Agents retry on their own and rejoin; confirm with:

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Compare -SnapshotPath before.json `
    -SwitchedAt <time of the revert, UTC> -WaitSeconds 150 -MaxDisappeared 0
```

Caveat on "agents retry forever": this is not exactly what the code does. A
client that holds only the token and is refused repeatedly stops waiting and
the agent process exits non-zero, relying on the service manager (SCM
recovery, systemd `Restart=on-failure`) to start it again; a client with a
user pair keeps one connection and retries. Hosts whose service manager does
not restart a failed agent will not return by themselves. This is why the
service-restart settings are a prerequisite.

## Telling a locked-out host from an offline one

From the backend alone you cannot (the script says so every time). Use
evidence outside the broker:

1. **Was it alive just before?** It is in the snapshot, with a last heartbeat
   seconds before the switch: consistent with either lock-out or power loss,
   but a host that had been dark for hours before is not a lock-out.
2. **What did the backend last record for it?** `shared-token` as its last
   NATS user means it was on the old token and never reported the new user.
   A missing user means never correlated, not "old credential".
3. **Reach the machine without the broker**: ping or power state from your
   management tooling, then the agent service state and its log. A lock-out
   shows a running (or restart-looping) agent and authorization errors in the
   log; a powered-off host shows nothing.
4. **Broker side**, on the broker host: the server log shows authorization
   violations for refused connections.

## Hosts that stay out

Use the same evidence path. A host that is powered off returns when someone
powers it on and needs nothing. A host that is running but refused has no
working user pair or an agent that predates user credentials: put the pair on
it (the deployment script with `-NatsUser` / `-NatsPassword`) or upgrade it,
and restart the agent service; it then connects as soon as the credential is
present. If you cannot fix it by hand soon, and the host matters, revert,
fix the stragglers, and run Readiness again; do not leave a fleet of
unreachable machines in place hoping they recover.

## New machines during the window

A machine installed while the broker runs the users configuration needs the
agent role's user pair at install time, or it cannot connect. Use an
installer generated with the agent pair set in the backend's installer
settings, and still carrying the token (a revert needs it). Such a host is not
in the snapshot; it shows up under *New hosts* and in the user counts. A host
installed with the token only is refused and is invisible to the backend, so
it cannot be seen by any of this; after a successful switch, check that no
installer in circulation lacks the pair.

## Afterwards

1. Confirm connections are all the expected role users: the user table from a
   fresh `Compare` shows only `agent` for agents, and the backend's
   *Expected NATS authentication* findings are empty.
2. Only then plan the removal of the shared token from hosts and from the
   installer settings. That is a **separate, later step**: while the token is
   still stored, a revert still works. Do not remove it in the same window.

## Rehearsal

Run the whole sequence once against a throwaway broker before the first real
switch. Nothing in the rehearsal touches production.

1. **Throwaway broker.** Start a separate `nats-server` on its own port and
   store directory (the development broker uses port 4223), first with the
   shared token, using a configuration copied from the shipped one. Prepare
   the users configuration from the shipped template with bcrypt hashes of
   throwaway passwords.
2. **Backend** pointed at that broker, with its own data directory and API
   token, and **a few agents** with distinct ids, in these states:
   * normal: user pair and token, current version;
   * pair not distributed: the token only (on purpose);
   * old version: the oldest agent you have;
   * stopped: a registered agent that is not running.
3. **Readiness** with a check job that reports for them (or, with none,
   expect every host to be `no-check-result`; rehearse the other reasons and
   the exit code anyway). Confirm it names the undistributed, old and stopped
   hosts and exits 1. Fix them (or accept them) until the list is empty or
   known.
4. **Snapshot**, then **switch** the throwaway broker with a reload signal
   (not a restart), noting the UTC time.
5. **Compare** too early (expect exit 3), then after the wait. Confirm the
   deliberately unprepared agent shows as disappeared, the normal ones show
   as `agent`, and `-MaxDisappeared` above and below the real count gives
   exit 0 and 1.
6. **Reproduce a lock-out** and walk the "locked-out vs offline" section on
   it, including finding the authorization errors in the broker log.
7. **Pause the backend API** briefly while running `Compare`, to see the retry
   messages and the exit 2 when it stays down.
8. **Revert** to the token, restart the unprepared agent if its service
   manager does not, and `Compare` against the same snapshot to confirm the
   fleet is back.

The conformance test (`nats_role_conformance`, see the README) covers the
broker-side behaviour of this switch with live processes; the rehearsal
covers *your* hosts, tooling and access path, which is what goes wrong in
practice.

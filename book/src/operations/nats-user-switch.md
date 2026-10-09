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
8. **The deployment switch is available in the release you deploy from.** The
   switch and the revert are made by an opt-in mode of the broker deployment
   scripts (Windows and Linux paths), which is separate work from this page.
   Before relying on this procedure, confirm in that release's deployment
   documentation and scripts that a users mode **and** a way back to the
   token both exist, record the release or artifact you will run them from,
   and read their exact usage. If either is missing, stop: there is no
   supported switch or emergency revert to follow.

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

On the broker host, with the broker deployment's opt-in users mode (prerequisite
8). Use exactly the options its own documentation gives for your release; this
page does not repeat them, because they belong to the deployment scripts and
can differ between releases. Applying the mode must reload or restart the
broker; note whether your path does that itself or leaves it to you.

Write down the **UTC time** at which the broker applied the new configuration;
`Compare` needs it as `-SwitchedAt`. Then set *Settings → Server → Expected
NATS authentication* to `users` so the backend reports anything unexpected.
Its findings are held back for five minutes after that change, which does not
affect the script.

### 4. Compare, after the fleet has had time to reconnect

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Compare -SnapshotPath before.json `
    -SwitchedAt 2026-01-01T09:00:00Z -WaitSeconds <see below> -MaxDisappeared 0
```

A locked-out host still looks alive for up to two minutes after its last
heartbeat, so the script refuses to judge sooner than that plus one heartbeat
interval (`-HeartbeatSeconds`, default 60; set it to your fleet's configured
interval) and exits 3 "too early". `-WaitSeconds` makes it wait before
reading. Run it again after waiting rather than reading a partial answer.

**How long to wait is the larger of two numbers, both counted from the switch
time you wrote down:**

* the script's own floor: 120 s plus the fleet's heartbeat interval; and
* four times the worst reconnect time measured for your clients (below), plus
  the fleet's heartbeat interval.

The second term used to be a guess. An agent that has lost its connection does
not heartbeat again until it has reconnected, and on a reconnect the client's
credential selection first probes the broker with each credential it holds and
then connects for real, so a reconnect is several round trips with their own
timeouts and the client's reconnect backoff in between, not one. On a slow or
busy machine that can add up to tens of seconds. Comparing sooner than that
reads agents that are still reconnecting as disappeared.

#### How the reconnect time was measured

The conformance suite (`nats_role_conformance`, run by the Integration
workflow on Ubuntu, Windows and macOS) times it on every run. It restarts the
broker, or on Linux and macOS reloads it from the token to the users block with
every live connection cut through a proxy, under a running real backend and
agent, and counts from the instant before the restart or reload until each of
these has worked again, all measured side by side from that same instant:

* the agent's next heartbeat, seen by an observer connected straight to the
  broker;
* the backend's ping of the agent returning 200;
* a break-glass `kanade run` completing;
* a fresh call made with the agent role and with the backend role through the
  real `connect` helper (so the helper's probe is inside the number).

Conditions of that measurement, which are not your fleet's: a loopback broker
on a hosted CI runner, three test processes running in parallel on the
machine, the agent heartbeating every second (so the heartbeat adds about one
second, not your interval), and the broker version the workflow pins
(`NATS_SERVER_VERSION`). A real fleet has network latency, a busy broker,
many clients reconnecting at once and a longer heartbeat interval; those make
it slower, not faster. The CI figure is therefore a lower bound on what to
expect and not a recovery guarantee. The right number for your fleet comes
from rehearsing on a few hosts: note when each host's first heartbeat arrives
after a switch of a test broker or one site, and take the worst.

The distribution per operating system (count, minimum, median, 95th
percentile, maximum, and how many waits hit their bound) is written to the
Integration workflow's step summary, and a manual run of the workflow can
repeat the suite (`conformance_repeat`) to collect more samples. Read the
`reconnect` rows and use the worst one on your clients' operating system as
the figure above. If it is tens of seconds even on a loopback runner, plan the
wait in minutes, not in the two or three the old example implied.

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

On the broker host (through the way you tested in the prerequisites): use the
same deployment path's token mode to restore the token configuration, then
reload or restart the broker. Know the exact revert command **before** the
switch (prerequisite 8) and keep it where you can reach it without the fleet.
Hand edits made inside the shipped token block are not restored by it.

Set the expected authentication mode back to `token`. Confirm client recovery
and service-manager restart settings as described in the next section.
Confirm with:

```powershell
./scripts/ops/nats-switch-check.ps1 -Mode Compare -SnapshotPath before.json `
    -SwitchedAt <time of the revert, UTC> -WaitSeconds <as in step 4> -MaxDisappeared 0
```

## Recovery deadlines and the production switch gate

Do not switch production until the release containing the recovery watchdog
has passed a manual Integration dispatch with `conformance_repeat=20` and
zero failures on Ubuntu, Windows and macOS. The previous release's promises
were not sufficient: a process could stay alive without heartbeats. The
injected-fault tests and the repeated conformance results must both pass.

The client recovery targets, counted from when the broker becomes reachable
and accepts the newly provisioned credential, are:

* resume communication within **30 s** (`RESUME_BOUND`); or
* exit non-zero within **195 s** (`EXIT_BOUND`) so supervision can restart it.

These timers are defined in `crates/kanade-shared/src/nats_client.rs` and all
derive from the protocol PING interval, set explicitly to 60 s (async-nats'
default). A reconnect attempt allows the 4 s backoff, 3 s probe plus 1 s
guard, and 5 s handshake; 30 s allows two attempts plus slack. An
inconclusive probe skips the attempt instead of guessing from the previous
broker mode.

The watchdog observes each connection's own receive progress. Idle clients
send a protocol PING every 60 s and the broker's PONG is the progress, so
liveness costs no traffic beyond the library's default and needs no subject
permission or application message. A flush alone proves only a local write.
Every 5 s the watchdog runs a purely local check (counters and a queued
flush); it makes no network connection while receive progress is recent. Only
after **180 s** (three ping intervals, two full intervals of margin) without
receive progress does it open a fresh credential witness. If the witness
connects or is explicitly refused while the original connection still has no
progress, the process logs the failure and exits. The exit budget adds the
5 s check interval, 5 s flush timeout and 5 s witness timeout to the 180 s
stall threshold: 195 s, inside the 5-minute target. Supervision starts
immediately after client creation, before subscriptions or resource bootstrap
can block; fatal failure bypasses application shutdown so it cannot extend
this deadline.

After a broker outage the watchdog first lets a client that is still in its
reconnect backoff try again. A witness that cannot reach the broker never
causes an exit and is repeated at most every 30 s; the first witness that
reaches it again gives the client a further 30 s to resume, and any received
progress clears the outage state, so a later genuine stall is judged against
the normal 180 s threshold. The expected wait for a client that is merely
reconnecting is therefore its reconnect backoff (up to 4 s) plus the handshake,
well inside the 30 s resume bound; a silently stranded client costs up to
195 s before its restart.

A broker that is unavailable, or whose mode cannot be determined by the
probe, is waited for indefinitely. The deadlines assume the client can reach
and authenticate to the broker; they are not a network outage deadline.
Repeated actual credential refusals still fail visibly. A single refusal is
retried by the pinned connection library and does not trigger an immediate
exit. Restart coverage applies on all three operating systems; signal-based
reload coverage is provided by conformance on Linux and macOS only.

Service-manager delay is additional: Windows recovery actions wait 5 s, then
15 s, then up to 60 s; systemd uses `RestartSec=5`; launchd uses
`ThrottleInterval=10`. Thus the slowest configured restart after a watchdog
exit is 255 s from broker availability on Windows, 200 s on systemd, and 205 s
on launchd, before application startup and the next heartbeat. Start-rate
limits or disabled recovery can prevent a restart; check them before the
switch. The CLI is not a service: its caller must rerun it after non-zero exit.

### Expected logs

During a restart, expect `event: disconnected`, IO errors and possibly
`expected INFO, got nothing`. A refused user probe during token mode is
expected. `NATS credential selected` names the selected shape (`user` or
`token`) without exposing credentials. An inconclusive probe logs
`NATS credential probe inconclusive; deferring this attempt` at debug level.

On recovery, expect `event: connected` and resumed heartbeats. If the
original client stalls while a witness reaches the broker, expect
`NATS client made no receive progress while a fresh credential witness
reached the broker; exiting for a supervised restart`, followed by a non-zero process
exit and the service manager's restart. Persistent wrong credentials log
`the NATS broker keeps refusing this role's credential`; a terminated task
logs `NATS connection task has terminated`. A live, silent process after the
deadline is a defect requiring investigation, not an expected switch state.

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

## Dotted pc_ids

A pc_id is the agent's `[agent] id`, and on Linux and macOS that is often an OS
hostname with dots (`m1air.local`, `host.example.com`). Each dot makes another
subject token, so the permission lists in the users configuration cannot use a
single `*` where the pc_id goes. A pc_id of **up to 4 dot-separated labels**
is supported (`host.sub.example.com` is the longest shape intended); the lists
spell out one pattern per label count wherever a wildcard cannot express "one
or more tokens".

A pc_id with 5 or more labels is refused up front, on the token configuration
too: the agent exits at startup with an error naming the limit, `kanade run`
and `kanade agent logs` fail before sending anything, and the backend API
answers `400`. Under the users configuration such an agent would otherwise
connect and then lose every publish.

**Before the switch**, list the registered pc_ids and look for any with 5 or
more labels (that is, 4 or more dots). Give those hosts a shorter `[agent] id`
first; an existing deployment that has one will not start the agent after the
upgrade that adds this check.

**How a lock-out looks** if a host slips through (for example an agent built
before the limit existed): the agent connects as `agent`, so the backend shows
it with the new user, but nothing it publishes arrives. The host goes silent in
the SPA, and the broker log shows lines such as

```text
Publish Violation - Subject "heartbeat.m1air.local"
```

(logged against the `agent` user), and likewise for `host_perf.`, `obs.` and
the other subjects that carry the pc_id. Fix the id and restart the agent; it
needs no broker change.

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

# Updating kanade-agent itself

Agent self-update is the only component that **doesn't** use
`OBJECT_APP_PACKAGES` + a `script_object` job. It has dedicated
machinery because the agent has to swap its own running binary
without ssh — a tighter loop than the generic install jobs.

## Mechanism

| Bucket / Key | Purpose |
|--------------|---------|
| `OBJECT_AGENT_RELEASES` | Agent binaries, keyed by `<version>`. Separate from `OBJECT_APP_PACKAGES` so the rollout watcher only fires on agent updates. |
| `agent_config.<scope>.target_version` | The version each scope (global / group / pc) should be on. Watched by the agent's `self_update` loop. |

Flow:

```text
1. agent.self_update watches agent_config for target_version
2. If target_version != my agent_version:
   a. Pull `OBJECT_AGENT_RELEASES/<target_version>` to <exe>.new
   b. Sha-verify against the bucket's recorded digest
   c. Atomic swap: <exe> ← <exe>.new (via SCM stop/start)
   d. New binary boots, watcher arms again, loop closes
```

The rollout watcher has to survive a cold broker (e.g. agent and
broker boot at the same time after a host reboot). Pre-#226 a
permanent `Err(_) => return;` on the first `get_object_store`
call killed the watcher forever; the agent would never self-update
on that boot. Post-#226 the watcher retries with backoff until the
broker is reachable.

## Step-by-step

### 1. Build the agent

```pwsh
cargo build --release -p kanade-agent
```

Output: `target/release/kanade-agent.exe`.

### 2. Publish

```pwsh
kanade agent publish target/release/kanade-agent.exe
```

The CLI extracts the version from the PE VERSIONINFO resource — no
`--version` flag, no chance of a label / binary mismatch.

### 3. Roll out

Pick a scope. Start with one canary host:

```pwsh
kanade agent rollout 0.42.2 --pcs canary-01
```

Watch via ping:

```pwsh
kanade ping canary-01     # agent_version should flip to 0.42.2
                          # within a few seconds
```

If happy, widen:

```pwsh
kanade agent rollout 0.42.2 --groups office --jitter 5m
# or fleet-wide
kanade agent rollout 0.42.2 --global --jitter 30m
```

> `--jitter` spreads the actual swap moment across a window so a
> wide fan-out doesn't hammer the OS service manager on every
> host at once. Recommended for fleets ≥ 100 hosts.

### 4. Verify

```pwsh
kanade agent current
# → target_version = 0.42.2 (global)
```

Then a fleet-wide spot-check via the SPA Agents page (or
`/api/agents`): the `agent_version` column should converge to
the new version within `jitter` + ~30s heartbeat cadence.

## What can go wrong

| Symptom | Cause | Fix |
|---------|-------|-----|
| `kanade agent rollout` says "version not in OBJECT_AGENT_RELEASES" | Typo or wrong scope | Re-check with `kanade agent current` and `kanade jetstream object list agent_releases`. |
| `kanade ping <host>` still shows the old version after several minutes | Agent didn't self-update — either the watcher's dead (pre-#226 agent) or the host can't reach the broker | Check `%ProgramData%\Kanade\log\agent.*.log` on the target. If self_update is silent (no "checking target_version" log lines), the agent is too old; bootstrap manually with `deploy-agent.ps1`. |
| Agent flaps: starts, immediately exits with `exit_code: 1` | The new binary is bad on this host (config drift, missing dep, etc.). SCM's failure-actions restart it, it crashes again — observable in Event Viewer as a Service Control Manager error cluster | Roll back: `kanade agent rollout <prev-version> --pcs <host>`. The host will swap back at the next watcher tick. |

## Why a separate bucket / scope?

`OBJECT_APP_PACKAGES` is a generic blob store keyed by
`<name>/<version>`. The agent rollout pattern needs:

- A watcher that fires *only* on agent changes (cheap KV watch on
  one specific key, not a poll over a bucket of many names).
- A "current target" semantic per scope, not just "all known
  versions" — `agent_config.<scope>.target_version` IS the answer
  to "what should I be running" without the agent enumerating.
- Operator UX (`kanade agent publish` / `rollout`) that's
  divergent enough from `kanade app publish` to warrant its own
  subcommand tree.

So agents get `OBJECT_AGENT_RELEASES` + a layered config KV; the
other components share `OBJECT_APP_PACKAGES` + per-app jobs.

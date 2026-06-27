# Broker sizing & scaling

kanade runs on a single NATS + JetStream broker. As the fleet grows
(hundreds → thousands of agents) the broker, not the agents, is the
scaling bottleneck. This page covers the per-agent footprint, what the
`#512` work changed, the single-node limits to watch, and how to capture
real numbers during a scale-up so you can decide whether to add a startup
splay or grow/cluster the broker.

## Per-agent consumer footprint

Each running agent holds a handful of JetStream consumers — one ordered
push consumer per KV watch, plus its durable command-replay consumer:

| Consumer | Source |
|---|---|
| `agent_config` watch (key-filtered) | `config_supervisor` |
| `agent_groups` watch (membership → effective config) | `config_supervisor` |
| `agent_groups` watch (membership → subscriptions) | `groups.rs` |
| `schedules` watch | `local_scheduler` |
| `jobs` watch | `local_scheduler` |
| `fleet_config` freeze watch (single key) | `local_scheduler` |
| `EXEC` durable replay | `command_replay` |

That is **~7 consumers per agent**. The count is roughly fixed per agent
— it does **not** shrink with key-filtering — so the broker-side total
grows linearly with the fleet:

| Fleet size | ~Consumers on the broker |
|---|---|
| 15 | ~105 |
| 500 | ~3,500 |
| 3,000 | ~21,000 |

## What v0.43.96 changed (and what it did not)

`#512` (shipped in v0.43.96) attacked the **super-linear** costs, not the
consumer count:

- **`#832` — `agent_config` key-filtered watch.** The agent watches only
  `global`, `pcs.<self>`, and its `groups.<g>` keys instead of the whole
  bucket (`watch_all`). This removes two blow-ups:
  - **Per-PC write fan-out**: a write to one `pcs.<id>` no longer reaches
    all N agents (it used to, with N−1 classifying-and-dropping it).
  - **Reconnect re-sync storm**: re-sync is now 3–5 direct `get`s per
    agent instead of a `keys()` + per-key walk over the whole bucket —
    aggregate **O(N²) → O(N)**.
- **`#839` — single-key freeze watch.** `fleet_config` holds only
  `KEY_FREEZE`, so the freeze watcher uses `watch(KEY_FREEZE)` instead of
  `watch_all` + a client-side filter.

What is **unchanged**: the ~7-consumers-per-agent footprint, and the
`schedules` / `jobs` `watch_all` (those are a shared catalog every agent
must evaluate for targeting — removing them needs server-side targeting,
a separate change). So at 3,000 agents you still provision for ~21,000
consumers and the connection/consumer-create burst of a synchronised
reconnect.

## Single-node limits and the reconnect herd

Two things to size for on a single-node JetStream:

1. **Steady-state footprint** — ~21,000 consumers at 3,000 agents live in
   the JetStream meta (Raft) layer and cost memory + file handles. Size
   the broker host's RAM and `max_file`/`max_memory` JetStream limits
   accordingly, and watch the meta layer's health.
2. **The reconnect herd** — when many agents reconnect *at the same
   instant* (broker restart, the morning power-on wave, a network event
   hitting many PCs), they re-establish connections and **recreate their
   consumers in a burst**. Key-filtering already cut each agent's re-sync
   to a few cheap `get`s, so the dangerous O(N²) read-storm is gone — but
   the connection + consumer-create burst is still O(N) and synchronised.

   Note that random, *unsynchronised* reconnects (one laptop's wifi flap)
   are not a herd — only fleet-wide synchronised events are.

## Levers, in order of preference

1. **Broker sizing first.** Give the single node enough RAM / file limits
   for the steady-state consumer count at your target N. This is the
   primary lever; everything else is secondary.
2. **Startup splay — only if measured.** A deterministic per-PC delay
   (`hash(pc_id)`) before the reconnect re-sync would smear the
   consumer-create / connection burst across a window. It is **not in the
   product yet** by design: PR1 already removed the quadratic term, and
   async-nats' reconnect backoff + `nats_retry`'s ±25% jitter already
   spread the burst somewhat. A splay also adds latency to every single
   reconnect (including herd-less blips), so it is a net cost unless a
   herd actually stresses the broker. **Decide from data** (next section):
   if a synchronised event shows consumer-create latency, connection
   backlog, or JetStream API errors, add the splay (gated to the first
   sync after a `Disconnected → Connected`, capped a few seconds).
3. **Reduce consumers per agent.** Fold watches where possible (the freeze
   watch is already a single key; the two `agent_groups` watches are a
   candidate to merge) and keep KV history shallow (`agent_config` /
   `agent_groups` are at `history: 1`).
4. **Cluster JetStream.** Beyond what one node can hold, move to a
   JetStream cluster. This is the last resort and the biggest change.

## Measuring at scale (retro-analysis)

You usually cannot watch a production broker live during a ramp. Capture
the numbers instead, with the bundled `collect:` job, and review the
bundle afterwards.

Run it **on the backend / NATS host**, ideally **during or right after a
synchronised reconnect event** (the herd moment is what decides the splay
question):

```sh
kanade exec collect-broker-health --pcs <backend-host-id>
```

The job (`configs/jobs/collect-broker-health.yaml`) samples the broker
over ~3 minutes and uploads a bundle to `OBJECT_COLLECTIONS`; download it
from the SPA **Collect** page (or hand the zip to your reviewer). It is
read-only and needs **zero pre-setup**: it reads NATS' unauthenticated
HTTP monitoring port (default 8222 — the same `/jsz` endpoint kanade
already curls for `jetstream status`), so no `nats` CLI on the SYSTEM
PATH and no token are required. Tune the window with `KANADE_BH_SAMPLES` /
`KANADE_BH_INTERVAL_SEC` (and `KANADE_BH_MON_PORT` if the broker's
`http_port` differs) on the target if needed.

The bundle contains:

- **Time series** (`connz-*.json`, `jsz-*.json`) — connection count
  (`/connz`) and JetStream consumer count (`/jsz`) per sample. A spike
  here at the herd moment is the splay signal.
- **Consumer footprint** (`jsz-full.json`) — `/jsz?consumers=true&streams=true`:
  the full consumer list; confirms the ~7N total and which streams hold them.
- **Server health/resources** (`varz.json`, `healthz.json`) — `/varz`
  (memory, CPU, connections, slow consumers) and `/healthz` status.
- **Log tails** (redacted) — backend and nats-server.

### What to look for

- **Smooth** consumer/connection counts across the samples, healthy
  `/healthz`, head-room on mem/cpu in `/varz` → the broker absorbed the
  event; **no splay needed**, just keep sizing ahead of N.
- **Spiky** connection backlog or consumer-create latency at the event,
  JetStream API errors, or mem/cpu pegged → the herd is real → add the
  startup splay (lever 2) and/or grow the broker.

> The `#828` downgrade-flap regression is checked separately from the
> `OBS_EVENTS` `agent_update` timeline (via the backend API), not by this
> job — that data is already queryable fleet-wide.

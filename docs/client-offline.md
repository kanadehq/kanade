# Client App offline operation

The end-user Client App talks only to the local Agent (KLP). When the Agent has
no NATS/backend connectivity it can still show the **last synchronized** job
catalog and notification history, and — only for jobs that explicitly opt in —
run jobs. Viewing and executing are separate decisions.

## Policy summary

| Question | Answer |
|----------|--------|
| May a job run offline? | **No by default.** Only `client.offline.allow: true`. A manifest with no `offline` block (every existing manifest) or `allow: false` is never run offline. |
| How long after the last sync? | `client.offline.max_age`, **1s – 24h**, whole seconds, required when `allow: true`. |
| Counted from? | The **start** of the last successful *authoritative* sync (never extended by mere reconnection or a partial sync). |
| Does `staleness: strict` still apply? | Yes. The shorter of `offline.max_age` and `staleness.max_cache_age` wins. `cached` / `unchecked` add no extra bound. |
| After an Agent restart? | Browsing is restored from disk. **Execution stays denied until the next successful sync**, because a stored timestamp cannot prove how long the machine was off or whether the clock changed. |
| Online behaviour? | Unchanged. A reachable broker is always authoritative: an online `NotFound`, refusal or error is never answered from the cache. Agent-scheduled and operator executions are untouched. |

```yaml
client:
  name: "Clear temp files"
  category: troubleshoot
  offline:
    allow: true
    max_age: 1h      # 1s..24h; omit the whole block to deny offline runs
```

`allow: false` with a `max_age`, `allow: true` without one, a value outside
1s–24h, sub-second values and unknown keys are rejected by `Manifest::validate`
/ the schema (`docs/schemas/job.schema.json`).

## What an offline run requires (all of it)

1. `client.offline.allow: true` with a valid `max_age`.
2. The definition and its inline script body are in the local cache and match
   their recorded SHA-256 digests (re-checked on every read, including right
   before starting).
3. Not revoked (`script_status`) and visible to this PC (`client.visible_to`
   against the cached group membership) as of the last sync.
4. In-process freshness evidence for the loaded snapshot, within the validity
   period. Wall clock and the process monotonic clock are both used: a clock set
   backwards, a sync "in the future", or evidence that does not belong to the
   loaded snapshot all deny. Suspend (wall ahead of monotonic) counts the larger
   elapsed time.
5. The job can complete locally. Jobs needing the broker/backend are refused
   with `needs_broker` even when `allow: true`: `require_approval`,
   `deploy_authorization`, `collect`, `finalize`, `check`, `emit`, `inventory`,
   `feed`, `script_object` bodies and typed deployments.

Anything absent or uncertain denies (fail closed). Each Client row shows the
reason code and message (`offline_not_allowed`, `sync_expired`,
`resync_required`, `revoked`, `not_visible`, `definition_missing`,
`integrity_mismatch`, `clock_anomaly`, `needs_broker`, …) and the Client shows
an offline banner with the last synchronization time.

## What is cached, and where

`<data_dir>/client_cache/` (directory `0700`, files `0600` on Unix; symlinks,
non-regular and group/world-writable files are ignored; atomic replace):

* `catalog-*.json` — user-facing manifests (raw bytes + SHA-256), revocation
  set, effective group membership, sync start time. Per broker + PC scope.
* `notifications-*.json` — the user's complete notification history and read
  marks. Per broker + PC + OS user, so users never see each other's data.

Capacity: ≤ 2000 manifests (a larger catalog fails the sync and keeps the
previous snapshot), newest 500 notifications, 32 MiB per file.

Digests detect accidental corruption, **not tampering**; protection of the cache
rests on its OS permissions.

## Synchronization and reconnect

A background single-flight task syncs after boot, on every reconnect and every
5 minutes (30 s retry after a failure). A sync is *authoritative* only if every
read succeeds: catalog, both group-membership buckets and the full
`script_status` scan. A failed read is never rounded to "no revocations" or "no
groups": the previous snapshot and its freshness evidence are left untouched, so
the validity period is not extended. A successful sync replaces the snapshot
wholesale, so deleted manifests, new revocations and membership changes take
effect on reconnect. Notification history is refreshed whenever the user's list
is fetched online and replaced only after the complete replay succeeded
(recalled notifications disappear). Acknowledging a notification needs the
broker.

Nothing about a user's run request is stored or replayed: a request that
arrives while offline either starts once (and its result goes through the
existing durable result outbox, which is idempotent on delivery) or is refused.
A lost response means "unknown", never an automatic retry.

## Known limitations

* Revocations or membership changes made while the PC is disconnected reach it
  only after reconnect; `max_age` (≤ 24h) bounds that exposure. Pick the
  smallest value that serves the job.
* The sync is not snapshot-isolated against concurrent operator edits during
  its few reads; the next sync (≤ 5 min online) converges.

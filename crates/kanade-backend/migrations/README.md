# Migrations

The backend's SQLite store is a **projection** of the JetStream streams
(RESULTS / AUDIT / …) — derived, rebuildable state, not the source of
truth. `sqlx::migrate!("./migrations")` applies these on startup (see
`main.rs`) and in tests.

## Naming: timestamp-prefixed, never sequential

Add migrations with `sqlx migrate add <name>` (or hand-name with a
`YYYYMMDDHHMMSS_` prefix).

**Do not** use sequential `00NN_` numbers. Two PRs developed in parallel
each pick the same "next" number, pass CI on their own branch, and then
collide on the `_sqlx_migrations.version` primary key once both land —
which silently breaks `main`: every DB-backed test and the backend's
own startup `migrate!()` fail with
`UNIQUE constraint failed: _sqlx_migrations.version`. A timestamp prefix
makes two PRs picking the same version effectively impossible.

## Baseline

`20260604000000_baseline.sql` is a **squashed baseline** that folds the
former `0001..0011` migrations into one file. It's regenerated from the
cumulative schema, so it's an exact equivalent (verified object-by-object
at squash time).

At PoC stage, when a breaking schema change comes up, prefer
**re-squashing the baseline + wiping the (derived) DB** over chaining
many small `ALTER`s — the data re-projects from JetStream.

> **Upgrade note:** an existing `backend.db` whose `_sqlx_migrations`
> table still records `0001..0011` will fail sqlx's missing-migration
> check against this squashed set. Delete the SQLite file and let the
> backend recreate it (`deploy-backend.ps1 -WipeDb` does this and only
> touches the configured projector DB, never the agent's `state.db`).
>
> The wipe re-projects from JetStream, but is **not fully lossless**:
> `users`/accounts are not a projection (only the bootstrap admin
> re-seeds — recreate other accounts by hand), and re-projection only
> covers each stream's `max_age` window (history older than retention is
> gone, and `once_per_pc` completions older than retention look un-done,
> so those schedules can re-fire — idempotent kitting makes this OK at
> PoC).

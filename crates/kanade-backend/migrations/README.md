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

## Not everything here is rebuildable

The statement above holds for the projections. The `deploy_plans` and
`deploy_plan_applications` tables (typed deployment plans, SPEC §2.13) are
**authorization state**: approvals and the single-use ledger cannot be
re-derived from JetStream, and a wiped database must not be repaired by guessing
which plans were approved. A wipe therefore drops every plan and approval;
re-register and re-approve. The `deploy_*` capability-evidence columns on
`agents` are projection and refill from the next heartbeat.

## Permanent scheduler completions (#1660)

`job_completions` (job/PC) and `job_version_completions` (job/PC/version)
are durable state, exempt from execution-history retention. INSERT/UPDATE
triggers atomically retain the latest finished exit-0 success, including
manual exec, late results and later version enrichment. All successful
versions remain recorded, including versions no longer current. Unknown
versions contribute only to the version-blind ledger.

The migration backfills retained successful results; already-deleted history
cannot be reconstructed, so those PCs may execute again after upgrade.
`wipe-projector` snapshots/restores both ledgers (or salvages retained
history from a pre-ledger DB). Stream replay alone is insufficient to restore
permanent semantics. Back up these tables with the durable account data;
manually deleting the database loses them. Cooldown schedules still use
retained execution history.

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
> check against this squashed set. Run `kanade-backend wipe-projector`
> to preserve durable tables while rebuilding the schema
> (`deploy-backend.ps1 -WipeDb` delegates to this command and only
> touches the configured projector DB, never the agent's `state.db`).
>
> The wipe re-projects from JetStream, but is **not fully lossless**:
> accounts and permanent scheduler completions are not rebuildable from
> streams. Use `wipe-projector` to preserve them; raw DB deletion loses
> them. Re-projection only covers each stream's `max_age` window.

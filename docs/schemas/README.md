# `docs/schemas/` — checked-in JSON Schemas

Machine-readable JSON Schema (draft 2020-12) for the two operator-facing
manifest types, so the full field set is version-controlled and visible
in diffs instead of having to be reverse-engineered from the Rust types:

| File | Rust type | YAML you write |
|------|-----------|----------------|
| [`schedule.schema.json`](./schedule.schema.json) | `kanade_shared::manifest::Schedule` | `kanade schedule create <yaml>` |
| [`job.schema.json`](./job.schema.json) | `kanade_shared::manifest::Manifest` | `kanade job create <yaml>` |

## Source of truth

These are **generated** from the `#[derive(JsonSchema)]` impls on the
Rust types — the same `schemars::schema_for!` output the backend serves
live at `GET /api/schemas/schedule.json` and `/api/schemas/manifest.json`
(which the SPA's Monaco YAML editor uses for field completion, hover
docs, and inline validation). Every field `description` here is the
type's doc-comment. **Do not hand-edit these files** — edit the Rust
doc-comments / fields instead and regenerate.

## Staying fresh (CI-guarded)

A unit test (`kanade-shared`, `schema_files_are_current`) regenerates the
schema in-memory and fails if it differs from the checked-in copy — the
same "lockfile" guard the repo uses for `Cargo.lock`. So a `Schedule` /
`Manifest` field change that forgets to refresh these files turns CI red.

Regenerate after changing a type:

```sh
UPDATE_SCHEMAS=1 cargo test -p kanade-shared schema_files_are_current
```

then commit the updated `*.schema.json` alongside your change.

## Validating a manifest against the schema

Any JSON-Schema-aware tool works, e.g.:

```sh
# convert your YAML to JSON, then validate (example with `check-jsonschema`)
yq -o=json '.' my-schedule.yaml | check-jsonschema --schemafile docs/schemas/schedule.schema.json -
```

The backend additionally enforces cross-field rules the schema can't
express (`Schedule::validate` / `Manifest::validate`) at create time —
e.g. `runs_on: agent` + `max_concurrent` is rejected, a dated calendar
`at` excludes `days`. A worked, annotated example covering the field set
is tracked in #587.

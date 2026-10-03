# Agent-mediated updates

The agent is the universal installer. Once it's running on a
target host, the operator never needs to touch the host directly
to update any other component — including the backend it talks
to, the broker that carries its messages, and the agent itself.

This chapter has one page per component:

- [kanade-backend](./backend.md)
- [kanade-client](./client.md)
- [NATS server](./nats.md)
- [kanade-agent self-update](./agent-self.md)

Common machinery used by all of them:

| Bucket / Stream | Purpose |
|------------------|---------|
| `OBJECT_APP_PACKAGES` | Generic binary storage (backend, client, NATS server, …). Keyed by `<name>/<version>`. |
| `OBJECT_SCRIPTS` | PowerShell script bodies referenced by manifests via `script_object`. Keyed by `<name>/<version>`. |
| `OBJECT_AGENT_RELEASES` | Agent binaries only. Separate from `APP_PACKAGES` because agent rollout has its own watcher / target_version flow. |
| `agent_config` (KV) | Layered config — global / per-group / per-PC. `target_version` lives here. |
| `jobs` (KV) | Job catalog. Each entry is a manifest the operator can `exec`. |

The CLI surface:

`kanade app`, `kanade script` and `kanade agent` (publish / rollout /
current / logs) talk to the
backend HTTP API, not to NATS: they need `KANADE_AUTH_TOKEN` (see
`kanade login`) for an account with the operator role, and no broker
token. Publishes and deletes are audited against that account by the
backend. Operators who previously relied on the NATS token alone must now
export `KANADE_AUTH_TOKEN`; without it the backend answers 401 / 403.
`kanade agent publish` is capped by the backend at 64 MB for the whole
upload (a normal agent binary is well under that). `app publish` additionally downloads the package back from the backend
and checks its digest before reporting success.

| Command | What it does |
|---------|--------------|
| `kanade app publish <name> <file> [--version <version>]` | Upload to `OBJECT_APP_PACKAGES` through the backend API. |
| `kanade script publish <name> <version> <file>` | Upload to `OBJECT_SCRIPTS` through the backend API. |
| `kanade job create <yaml>` | Upsert a job manifest into the `jobs` KV. |
| `kanade exec <job-id> --pcs <pc> [--pcs <pc> …]` | Fire a registered job at a set of PCs. |
| `kanade agent publish <file> [--version <version>]` | Upload an agent binary through the backend API (version extracted from PE VERSIONINFO; `--version` for a Linux / macOS binary). |
| `kanade agent rollout <version> --pc \| --group \| --global` | Flip `target_version` on the chosen scope; agents pick it up via their self-update watcher. Goes through the backend API. |
| `kanade agent current` | Print the global `target_version` (group / pc overlays are not shown; use `kanade config get --group/--pc`). |
| `kanade agent logs <pc_id> [--tail <n>]` | Tail an online agent's log via the backend API. |

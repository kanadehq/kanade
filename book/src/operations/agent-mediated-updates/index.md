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

| Command | What it does |
|---------|--------------|
| `kanade app publish <name> <version> <file>` | Upload to `OBJECT_APP_PACKAGES`. |
| `kanade script publish <name> <version> <file>` | Upload to `OBJECT_SCRIPTS`. |
| `kanade job create <yaml>` | Upsert a job manifest into the `jobs` KV. |
| `kanade exec <job-id> --pcs <pc> [--pcs <pc> …]` | Fire a registered job at a set of PCs. |
| `kanade agent publish <file>` | Upload an agent binary (version extracted from PE VERSIONINFO). |
| `kanade agent rollout <version> --pc \| --group \| --global` | Flip `target_version` on the chosen scope; agents pick it up via their self-update watcher. |

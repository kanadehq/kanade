# Configuration Reference

`kanade` services rely on structured configurations loaded from TOML files, environment variables, or registry paths.

---

## 1. Agent Configuration

The agent searches for its configuration via the `KANADE_AGENT_CONFIG` environment variable or falls back to native paths.

### Dev Configuration (`configs/agent.dev.toml`)

```toml
# Dev configuration schema
[agent]
id = "dev-pc"
nats_url = "nats://localhost:4223"
data_dir = "target/dev-data/agent"

[log]
level = "debug"
file = "target/dev-data/agent/logs/agent.log"
```

### Configuration Parameters

| Field | Type | Description | Environment Override |
|---|---|---|---|
| `agent.id` | String | Unique hardware identifier (`pc_id`). | `KANADE_DEV_AGENT_ID` (templated) |
| `agent.nats_url` | String | Network address of the NATS broker. | `KANADE_NATS_URL` |
| `agent.data_dir` | Path | Root path to cache outbox scripts, state database, and local completions. | `KANADE_AGENT_DATA_DIR` |
| `log.level` | String | Logging verbosity (`error`, `warn`, `info`, `debug`, `trace`). | `RUST_LOG` |
| `log.file` | Path | Filepath destination for rolling logs. | - |

---

## 2. Backend Configuration

The backend coordination layer retrieves its configurations from the file specified by `KANADE_BACKEND_CONFIG` or registers default structures.

### Dev Configuration (`configs/backend.dev.toml`)

```toml
[backend]
listen_addr = "127.0.0.1:8081"
nats_url = "nats://localhost:4223"
database_url = "sqlite://target/dev-data/backend/state.db"

[auth]
# Auth settings
```

### Configuration Parameters

| Field | Type | Description | Environment Override |
|---|---|---|---|
| `backend.listen_addr` | String | Network bind address for HTTP/WebSocket traffic. | `KANADE_BIND_ADDR` |
| `backend.nats_url` | String | Target NATS broker URL. | `KANADE_NATS_URL` |
| `backend.database_url`| String | SQLite database connection string. | `DATABASE_URL` |
| `auth.disable` | Boolean| Set to true to disable operator token validation (dev environment only). | `KANADE_AUTH_DISABLE` |

---

## 3. Windows Registry Integration

In production environments, security-sensitive tokens (like NATS client tokens and administrative API bearer tokens) are stored in the secure Windows Registry rather than plaintext files.

### Key Paths
- **Agent Settings**: `HKLM:\SOFTWARE\Kanade\agent`
- **Backend Settings**: `HKLM:\SOFTWARE\Kanade\backend`

These registry paths are protected with local ACL configurations, allowing read permissions strictly to `SYSTEM` and designated operators.

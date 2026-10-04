# Installation and Deployment

This section details how to bootstrap `kanade` components as native Windows services in production or staging environments.

## Deployment Model

Production hosts and target endpoints run `kanade` components as background Windows services. This ensures high availability and automatic startup.

| Service Name | Triple / Binary | Config Source | Typical Target |
|---|---|---|---|
| **KanadeNats** | `nats-server.exe` | Hardened Registry / Registry-baked CLI flags | Central server |
| **KanadeBackend** | `kanade-backend.exe` | Hardened Registry / Config file | Central server |
| **KanadeAgent** | `kanade-agent.exe` | Hardened Registry / Local state DB | Managed endpoints |

---

## 1. Prerequisites

- **Host OS**: Windows 10/11 or Windows Server 2016+.
- **gsudo**: Required to perform elevated installations from standard user shells (or run commands from an Administrator-level PowerShell prompt).
- **Network Routing**: Managed endpoints must be able to reach the NATS server port (default `4222`) over TCP.

---

## 2. Setting Up the NATS Server (Broker)

The NATS server acts as the messaging core.

1. Stage the deployment bundle using `scripts/build-release.ps1 -Roles nats`.
2. Deploy the service with elevation:
   ```powershell
   # Elevated PowerShell prompt
   & "dist\nats\deploy-nats.ps1" -NatsToken "your-secure-nats-token" -Recreate
   ```
This installs the **KanadeNats** service, configures it to run under the local system account, sets up JetStream data directories, and locks down the secure authorization token in the Windows registry.

---

## 3. Deploying the Backend API & SPA

The backend manages operator connections and processes event logs.

1. Stage the backend binaries and React SPA bundle using `scripts/build-release.ps1 -Roles backend`.
2. Deploy the service:
   ```powershell
   # Elevated PowerShell prompt
   & "dist\backend\deploy-backend.ps1" `
       -NatsToken "your-secure-nats-token" `
       -StaticToken "your-operator-spa-bearer-token" `
       -ForceConfig -Recreate
   ```
- `-NatsToken`: Connects the backend to the local NATS server securely.
- `-StaticToken`: Defines the API bearer token required for operator CLI/SPA logins.

The deployment script registers the **KanadeBackend** Windows service, sets the appropriate ACLs, and verifies the endpoint.

---

## 4. Installing the Agent on Target Endpoints

Install the agent on every endpoint PC that you want to manage.

1. Stage the agent bundle using `scripts/build-release.ps1 -Roles agent`.
2. Copy the contents of the `dist/agent` folder to the target PC.
3. On the target PC, run the installer:
   ```powershell
   # Elevated PowerShell prompt
   & ".\deploy-agent.ps1" -NatsToken "your-secure-nats-token" -ForceConfig -Recreate
   ```
The script:
- Places `kanade-agent.exe` into its destination directory.
- Secures the configuration and NATS token in the Windows registry path (`HKLM:\SOFTWARE\Kanade\agent`).
- Registers and starts the **KanadeAgent** service.

#### Agent-role NATS user in generated installers

`deploy-agent.ps1` also accepts `-NatsUser` / `-NatsPassword` (the shell
scripts take `KANADE_NATS_USER` / `KANADE_NATS_PASSWORD`), both or neither.
The installers the backend generates (Windows ZIP, Linux / macOS tarball)
embed such a pair when the server settings' `agent_install` section holds
both a user and a password, next to the token, so machines installed from now
on already carry the credential the broker may later require.

- The pair is the **agent role's** credential and is **shared by all agents by
  design**. The backend's own credential and the break-glass credential must
  never be entered in these settings.
- It is write-only, like the token: the API returns only `nats_user_set` /
  `nats_password_set`, and a settings update that omits both keeps the stored
  pair. A settings update that sets only one of the two is rejected, so
  replacing the password means sending the user again.
- A generated `README.txt` states whether a user pair is included, without
  revealing it; with no paragraph about it, none was embedded and the
  installer is exactly what it was before this setting existed.

Once the service is active, the agent establishes an outbound NATS connection, subscribes to command streams, and reports its online heartbeat back to the fleet backend.

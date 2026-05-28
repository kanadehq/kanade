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

Once the service is active, the agent establishes an outbound NATS connection, subscribes to command streams, and reports its online heartbeat back to the fleet backend.

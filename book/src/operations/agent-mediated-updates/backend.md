# Updating kanade-backend

The backend lives on one (or more) of your managed hosts as a
Windows service. Agent-mediated update means: an agent running on
that host stops the service, swaps the binary, starts it back up
— while the operator never logs into the host.

## End-to-end flow

```text
┌── operator host ──────────────────────────────────────────┐
│  1. build kanade-backend.exe                              │
│  2. kanade app publish kanade-backend <v> <exe>           │
│  3. edit deploy-backend.ps1 (set $AgentSource* knobs)     │
│  4. kanade script publish deploy-backend <v> <edited.ps1> │
│  5. kanade job create install-kanade-backend.yaml         │
│  6. kanade exec install-kanade-backend --pcs <host>       │
└────────────────────────────────────────────────────────────┘
                      │
                      ▼
┌── target host (running kanade-agent as LocalSystem) ──────┐
│  • agent receives the Command on commands.pc.<host>       │
│  • fetches deploy-backend.ps1 from OBJECT_SCRIPTS         │
│    sha-verifies it (`script_object` machinery, #214)      │
│  • stages it under                                        │
│    C:\ProgramData\Kanade\agent-scripts\<UUID>\            │
│    kanade-<UUID>.ps1                                      │
│  • runs `powershell -File <launcher>` (PR #230 fix)       │
│  • launcher invokes the user script via `& '...'`         │
│    so [CmdletBinding()] / param() headers parse           │
│  • script downloads kanade-backend.exe from               │
│    OBJECT_APP_PACKAGES (via /api/app-packages/…)          │
│    sha-verifies it (separate hash, on the exe itself)     │
│  • Stop-Service KanadeBackend                             │
│  • copy exe over C:\Program Files\Kanade\…                │
│  • Start-Service KanadeBackend                            │
│  • exit 0 — result published to NATS                      │
└────────────────────────────────────────────────────────────┘
```

The two sha checks are intentional: the script body's hash is
verified by the agent before execution (script integrity); the
binary's hash is verified by the script before the swap (binary
integrity, defined by the operator in
`$AgentSourceSha256`).

## Step-by-step

### 1. Build kanade-backend

```pwsh
cargo build --release -p kanade-backend
```

Output: `target/release/kanade-backend.exe`.

### 2. Publish the binary

```pwsh
kanade app publish kanade-backend 0.43.0 target/release/kanade-backend.exe
```

This uploads the binary to `OBJECT_APP_PACKAGES/kanade-backend/0.43.0`
and prints the sha-256 digest. **Copy the digest** — you'll need
its lowercase-hex form for the script.

### 3. Edit `scripts/deploy/backend.ps1`

Make a local copy. Set the four `$Agent*` knobs at the top:

```powershell
$AgentSourceUrl       = 'http://kanade-backend.example.com:8080'
$AgentSourceVersion   = '0.43.0'
$AgentSourceSha256    = '<lowercase hex of kanade-backend.exe>'
$AgentSourceAuthToken = '<bearer for the backend HTTP API>'
```

Leave the rest of the script alone — those knobs are how the
script knows it's running in "agent mode" (downloading from the
backend) vs the manual-install mode (local folder of files).

> The `$AgentSourceSha256` is the hex form of
> `Get-FileHash kanade-backend.exe -Algorithm SHA256`.
>
> If you only have the base64url form printed by
> `kanade app publish`, decode it. The base64 from the CLI is
> URL-safe and may be unpadded, so the PowerShell snippet needs
> to re-pad before `FromBase64String` accepts it:
>
> ```powershell
> $b64 = '<paste the SHA-256= value here, without the SHA-256= prefix>'
> $b64 = $b64.Replace('-', '+').Replace('_', '/')
> if ($b64.Length % 4) { $b64 += '=' * (4 - $b64.Length % 4) }
> [BitConverter]::ToString([Convert]::FromBase64String($b64)).Replace('-', '').ToLowerInvariant()
> ```
>
> Or in Python:
> `python -c "import base64; print(base64.urlsafe_b64decode('<b64>' + '=' * (-len('<b64>') % 4)).hex())"`

> The `$AgentSourceAuthToken` is required as of the live test on
> 2026-05-26 — the backend's `/api/app-packages/<name>/<ver>`
> endpoint returns HTTP 401 without it. Leave empty only for
> no-auth lab setups.

### 4. Publish the edited script

```pwsh
kanade script publish deploy-backend 0.43.0 .\deploy-backend.edited.ps1
```

Upload goes to `OBJECT_SCRIPTS/deploy-backend/0.43.0`.

### 5. Register / update the job

`configs/jobs/installers/install-kanade-backend.yaml` in the repo is the template.
Edit `version:` + `script_object:` to point at the version you
just published, then upsert:

```yaml
id: install-kanade-backend
version: 0.43.0
execute:
  shell: powershell
  script_object: deploy-backend/0.43.0
  timeout: 300s
  run_as: system
require_approval: true
```

```pwsh
kanade job create jobs\install-kanade-backend.yaml
```

> `run_as: system` is required: `Stop-Service` / `Start-Service` /
> `sc.exe` all need admin. The agent already runs as LocalSystem
> in production.

### 6. Fire it

```pwsh
kanade exec install-kanade-backend --pcs <backend-host>
```

The CLI returns an `exec_id` immediately. The actual install
happens asynchronously on the target.

### 7. Verify

Query the backend's results endpoint (or watch the SPA Activity
view):

```pwsh
curl -H "Authorization: Bearer <token>" `
  "http://<backend>/api/results?limit=5"
```

Look for your `exec_id` with `exit_code: 0` and a `stdout` that
ends with `kanade-backend <new-version>`.

## What can go wrong

| Symptom | Cause | Fix |
|---------|-------|-----|
| `[CmdletBinding()]` / `param()` parse error in stderr | Agent older than 0.42.2 (running `-Command` mode) | Upgrade the agent first via `kanade agent rollout` (see [agent self-update](./agent-self.md)). |
| `Start-BitsTransfer : HTTP status 401` | `$AgentSourceAuthToken` empty but backend requires auth | Set it. |
| `Start-BitsTransfer : The transfer encountered an error` / job state `TransientError` | BITS service not running, or target machine's WinHTTP can't reach `$AgentSourceUrl` | `Get-Service BITS`; check WinHTTP proxy with `netsh winhttp show proxy` (BITS uses WinHTTP, not IE/WinINet). |
| `sha256 mismatch — expected=<x> actual=<y>` | Hash in script doesn't match the published binary | Re-publish or recompute the hash. The script aborts BEFORE the swap, so the existing install is intact. |
| Job runs but kanade-backend doesn't come back up | Service-failure / config drift on target | Read `C:\ProgramData\Kanade\log\backend.*.log` on the target. The agent can fetch it via `kanade logs <pc>` (when implemented) or you can pull the file directly. |

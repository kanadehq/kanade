# Updating kanade-client

The Tauri desktop client is shipped to endpoints the same way as
the backend: binary in `OBJECT_APP_PACKAGES`, script in
`OBJECT_SCRIPTS`, job in `jobs` KV. The shape mirrors
[backend updates](./backend.md) — only the script content and
package name differ.

## What's different from backend updates

| Aspect | kanade-backend | kanade-client |
|--------|----------------|---------------|
| Service to (re)start | `KanadeBackend` (Windows service) | None — the client is launched by the user |
| Install location | `%ProgramFiles%\Kanade\kanade-backend.exe` | `%ProgramFiles%\Kanade\kanade-client.exe` |
| Script in repo | `scripts/deploy/backend.ps1` | `configs/jobs/installers/scripts/install-kanade-client.ps1` (lives in the manifest's script_file path) |
| Manifest file ref | `script_object: deploy-backend/<v>` | `script_file: scripts/install-kanade-client.ps1` (relative to the manifest YAML; inlined at `kanade job create`) |
| Atomic swap pattern | Stop service → copy → start service | Stage to `<exe>.new` → `Move-Item` → drop `<exe>.old` |
| Inventory projection | None (the backend reports its own version) | `inventory:` block emits per-PC client version into the SPA Inventory page |

> Both shapes — `script_object` (referenced by hash from
> OBJECT_SCRIPTS, agent fetches on demand) and `script_file`
> (script body inlined into the manifest at
> `kanade job create` time) — are supported. The client manifest
> uses `script_file` for historical reasons; the backend manifest
> uses `script_object` because it was rewritten to test the
> Object Store path.

## Step-by-step

### 1. Build kanade-client

```pwsh
cargo build --release -p kanade-client
```

Output: `target/release/kanade-client.exe`.

### 2. Publish the binary

```pwsh
kanade app publish kanade-client 0.42.0 target/release/kanade-client.exe
```

### 3. Edit `configs/jobs/installers/scripts/install-kanade-client.ps1`

Set the three knobs at the top:

```powershell
$BackendBase    = 'http://kanade-backend.example.com:8080'
$Version        = '0.42.0'
$ExpectedSha256 = '<lowercase hex of kanade-client.exe>'
```

> Unlike the backend script, this one doesn't have an explicit
> auth-token knob yet — it relies on
> `Invoke-WebRequest -UseDefaultCredentials` or the absence of
> auth on the `/api/app-packages/kanade-client/<v>` route. If your
> backend gates that endpoint, add an `Authorization` header to
> the `Invoke-WebRequest` call the same way the backend script
> does (see
> [Updating kanade-backend](./backend.md#3-edit-scriptsdeploybackendps1)).
> A first-class `$ClientSourceAuthToken` knob is on the backlog.

### 4. Register / update the job

`configs/jobs/installers/install-kanade-client.yaml`:

```yaml
id: install-kanade-client
version: 0.42.0
execute:
  shell: powershell
  script_file: scripts/install-kanade-client.ps1   # body inlined at `job create` (relative to the manifest YAML)
  timeout: 180s
  run_as: system

require_approval: true

inventory:
  display:
    - { field: version, label: Version }
    - { field: path,    label: Install path }
  summary:
    - { field: version, label: Client version }
```

```pwsh
kanade job create jobs\install-kanade-client.yaml
```

> The `inventory:` block tells the projector that the script's
> stdout is a single JSON blob whose `version` / `path` fields
> populate the SPA's Inventory page. Operators can spot
> stragglers from a fleet-wide table — no ssh needed.

### 5. Fire it

```pwsh
kanade exec install-kanade-client --pcs <host> [--pcs <host> …]
```

Or against a group:

```pwsh
kanade exec install-kanade-client --groups office
```

### 6. Verify in the SPA

Open the SPA Inventory page (or query `/api/inventory?app=kanade-client`)
and confirm the target hosts report the new version.

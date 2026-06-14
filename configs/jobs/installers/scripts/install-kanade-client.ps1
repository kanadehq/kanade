#requires -Version 5.1
<#
Install / upgrade the kanade-client Tauri app on this PC.

Pulls the binary from OBJECT_APP_PACKAGES via the operator-facing
HTTP endpoint the backend serves at:

    GET /api/app-packages/kanade-client/<version>

(see kanade-shared::kv::OBJECT_APP_PACKAGES and
kanade-backend::api::app_packages — Sprint 8 / yukimemi/kanade#210).

The script's contract with the inventory projector is "emit a single
JSON object on stdout" — progress chatter therefore goes to STDERR via
`[Console]::Error.WriteLine(...)`, NOT `Write-Host`. The agent captures
the powershell process's stdout, and `Write-Host` output bleeds INTO
that captured stdout (it does NOT stay on a separate host stream once
stdout is redirected). That extra text breaks the projector's
`serde_json::from_str(stdout)` parse — it expects a single clean JSON
blob — and the inventory fact is silently dropped (backend logs
"stdout was not JSON"; see `projector::results::upsert_inventory`).
stderr is captured into the result's separate `stderr` field, which the
projector ignores, so routing progress there keeps stdout pure JSON. The
`inventory:` block in the parent manifest renders the JSON into the
SPA's Inventory page so operators can see "what version of
kanade-client is on each PC" without ssh-ing in.
#>

$ErrorActionPreference = 'Stop'

# --- Configurable knobs --------------------------------------------------
# Edit before `kanade job create`. Future: pass via Execute.env
# once that field lands so operators can re-target without
# rewriting the script.
$BackendBase = 'http://kanade-backend.local:8080'
$Version     = '0.42.0'
# Hex-encoded sha256 of the binary that was uploaded to
# OBJECT_APP_PACKAGES under `<Version>`. Compute locally before
# upload (`Get-FileHash kanade-client.exe -Algorithm SHA256`) and
# paste here. Required — leaving it blank fails fast at the
# verification step rather than silently installing whatever the
# backend serves. Supply-chain protection: a compromised /
# MITM-substituted binary won't match this hash, and the script
# refuses to promote it.
$ExpectedSha256 = ''
# Bearer token for the backend's `/api/app-packages/<name>/<version>`
# route. Required when backend auth is enabled (the production
# posture — KANADE_AUTH_STATIC_TOKEN / KANADE_JWT_SECRET set on the
# backend host). Leave empty only if the backend route is
# unauthenticated (dev / smoke-test setups). Mirrors the
# `$AgentSourceAuthToken` knob in `scripts/deploy/backend.ps1` — same
# token both scripts use against the same gated endpoint.
$ClientSourceAuthToken = ''
# How long BITS keeps retrying after a transient transfer error
# before giving up. Different shape from the old -TimeoutSec knob:
# the actual download time is unbounded (a healthy connection takes
# as long as it needs to ship the bytes — important once multi-GB
# kanade-client bundles land), and this only caps the recovery
# window after a connection drop / 5xx. 1800 = 30 min absorbs
# several `RetryInterval`s (default 600 s) so a wobbly link
# completes via resume rather than aborting. The parent manifest's
# `timeout:` still bounds the whole job, so a wedged backend
# surfaces there — bump that knob if a fleet starts shipping
# packages whose first-try download exceeds the current 180 s
# budget.
$DownloadRetryTimeoutSecs = 1800
# -------------------------------------------------------------------------

if ([string]::IsNullOrWhiteSpace($ExpectedSha256)) {
    throw 'install-kanade-client: $ExpectedSha256 must be set to the operator-computed sha256 of the uploaded binary (see Configurable knobs).'
}

$InstallDir = Join-Path $env:ProgramFiles 'Kanade'
$ExePath    = Join-Path $InstallDir 'kanade-client.exe'
$Url        = "$BackendBase/api/app-packages/kanade-client/$Version"

# Stage to <exe>.new, swap, drop <exe>.old — same atomic-replace
# pattern kanade-agent's self_update uses (cross-volume safe since
# `Move-Item` falls back to copy+delete when src and dst are on
# different drives).
$NewPath = "$ExePath.new"
$OldPath = "$ExePath.old"

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

# Clean up a leftover staging file from a previous failed run so
# the new download doesn't fail on `Move-Item -Force` colliding
# with a half-written .new. Leave any pre-existing `.old` alone —
# it's our rollback artifact if a prior swap aborted mid-way.
Remove-Item -Force -ErrorAction SilentlyContinue $NewPath

[Console]::Error.WriteLine("Downloading kanade-client $Version from $Url (BITS)")
# Use BITS (Background Intelligent Transfer Service) so multi-GB
# downloads survive transient network drops — BITS resumes from
# the last received byte instead of restarting from zero, which
# matters once the kanade-client bundle grows past ~100 MB.
# `-Priority Foreground` runs at interactive speed (operator is
# waiting on this install, not running it as a background job).
# Bearer auth via -CustomHeaders (BITS module on PS 5.1+).
$bitsHeaders = @()
if (-not [string]::IsNullOrWhiteSpace($ClientSourceAuthToken)) {
    # `.Trim()` guards against accidental whitespace from copy-paste
    # (a leading newline silently sends `Bearer \n<token>` and the
    # backend 401s with a confusing "missing bearer token" — Gemini #265).
    $bitsHeaders += "Authorization: Bearer $($ClientSourceAuthToken.Trim())"
}
$bitsArgs = @{
    Source       = $Url
    Destination  = $NewPath
    Priority     = 'Foreground'
    RetryTimeout = $DownloadRetryTimeoutSecs
}
if ($bitsHeaders.Count -gt 0) {
    $bitsArgs.CustomHeaders = $bitsHeaders
}
Start-BitsTransfer @bitsArgs

# --- Integrity check -----------------------------------------------------
# Compute sha256 of the downloaded bytes and compare to the
# operator's pin. Mismatch = abort (DO NOT overwrite the running
# binary) so a poisoned download leaves the existing install
# intact and the operator can investigate.
$actualSha = (Get-FileHash $NewPath -Algorithm SHA256).Hash.ToLowerInvariant()
$expectedSha = $ExpectedSha256.ToLowerInvariant()
if ($actualSha -ne $expectedSha) {
    Remove-Item -Force -ErrorAction SilentlyContinue $NewPath
    throw "install-kanade-client: downloaded binary sha256 mismatch: expected=$expectedSha actual=$actualSha — refusing to install (possible MITM / corrupted upload)"
}
[Console]::Error.WriteLine("sha256 verified: $actualSha")

# --- Atomic-replace with rollback ----------------------------------------
# Two-step swap (`<exe>` → `.old`, `.new` → `<exe>`). If the
# second step fails (file lock from a still-running launch, AV
# scan, etc.) restore from `.old` so the install stays usable.
# Only drop `.old` once the promotion completes cleanly.
$hadPrevious = Test-Path $ExePath
if ($hadPrevious) {
    Move-Item -Force $ExePath $OldPath
}
try {
    Move-Item -Force $NewPath $ExePath
} catch {
    if ($hadPrevious -and (Test-Path $OldPath)) {
        Move-Item -Force $OldPath $ExePath
        [Console]::Error.WriteLine('install-kanade-client: rolled back to previous binary after promotion failure')
    }
    throw
}
Remove-Item -Force -ErrorAction SilentlyContinue $OldPath

# --- Start-Menu shortcut with AppUserModelID -----------------------------
# REMOVED: the all-users Start-Menu shortcut (+ its System.AppUserModel.ID
# stamp, which lets WinRT toasts render) is now created and kept in step
# with the operator-configured product name by the AGENT
# (kanade-agent `client_shortcut` module, driven by
# agent_config.client_display_name). Creating it here too would just make a
# transient "Kanade Client" entry the agent immediately renames, so the
# install script no longer touches the Start-Menu.

# --- kanade-client:// URL protocol (#647 toast-click reveal) --------------
# A clicked emergency toast carries `launch="kanade-client://show?id=<id>"`
# (activationType="protocol"). Windows resolves the scheme via this registry
# entry and runs the client with the URI as `%1`; the single-instance guard
# forwards it to the running instance, which reveals the window and scrolls to
# the notification. Protocol activation needs no COM activator. Machine-wide
# (HKLM\SOFTWARE\Classes) — this job runs as SYSTEM.
try {
    $protoKey = 'HKLM:\SOFTWARE\Classes\kanade-client'
    New-Item -Path $protoKey -Force | Out-Null
    Set-ItemProperty -Path $protoKey -Name '(default)'    -Value 'URL:Kanade Client Protocol'
    Set-ItemProperty -Path $protoKey -Name 'URL Protocol' -Value ''
    $cmdKey = Join-Path $protoKey 'shell\open\command'
    New-Item -Path $cmdKey -Force | Out-Null
    Set-ItemProperty -Path $cmdKey -Name '(default)' -Value ('"{0}" "%1"' -f $ExePath)
    [Console]::Error.WriteLine("kanade-client:// protocol registered -> $ExePath")
} catch {
    [Console]::Error.WriteLine("install-kanade-client: protocol registration failed (toast-click reveal won't work): $_")
}

# --- Inventory payload ---------------------------------------------------
# Read back ProductVersion from the embedded VERSIONINFO so the
# inventory row reports what's actually on disk, not what the
# manifest asked for. winres builds the agent + client with this
# metadata populated (see kanade-{agent,client}/build.rs), but
# fall back gracefully if a hand-built / patched binary lacks it
# — emitting `version: "unknown"` is friendlier than crashing the
# inventory projector with malformed JSON.
$installed = $null
try {
    $installed = (Get-Item $ExePath).VersionInfo.ProductVersion
} catch {
    [Console]::Error.WriteLine("install-kanade-client: VersionInfo read failed: $_")
}
if ([string]::IsNullOrWhiteSpace($installed)) {
    $installed = 'unknown'
}

# Single line JSON keeps the projector's parse trivial; the
# manifest's `inventory.display` lists `version` + `path` so the
# SPA renders this without any further config. Use Write-Output
# explicitly — it's the ONE stdout line in this script.
$payload = [ordered]@{
    version = $installed
    path    = $ExePath
} | ConvertTo-Json -Compress
Write-Output $payload

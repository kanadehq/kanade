#requires -Version 5.1
<#
Install / upgrade the kanade-client Tauri app on this PC.

Pulls the binary from OBJECT_APP_PACKAGES via the operator-facing
HTTP endpoint the backend serves at:

    GET /api/app-packages/kanade-client/<version>

(see kanade-shared::kv::OBJECT_APP_PACKAGES and
kanade-backend::api::app_packages — Sprint 8 / yukimemi/kanade#210).

The script's contract with the inventory projector is "emit a single
JSON object on stdout" — `Write-Host` for progress chatter (which
goes to the host stream, NOT stdout, so the projector's JSON parse
stays clean — see backend `projector::results` for the
single-JSON-blob expectation). The `inventory:` block in the parent
manifest renders the JSON into the SPA's Inventory page so operators
can see "what version of kanade-client is on each PC" without
ssh-ing in.
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
# How long to wait for the binary download before aborting. The
# parent manifest's `timeout: 180s` budgets the whole job; pick a
# value comfortably below that so a wedged backend surfaces as a
# download failure rather than a job timeout (which can't
# distinguish "network slow" from "script broken").
$DownloadTimeoutSecs = 60
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

Write-Host "Downloading kanade-client $Version from $Url"
Invoke-WebRequest -Uri $Url -OutFile $NewPath -UseBasicParsing -TimeoutSec $DownloadTimeoutSecs | Out-Null

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
Write-Host "sha256 verified: $actualSha"

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
        Write-Host 'install-kanade-client: rolled back to previous binary after promotion failure'
    }
    throw
}
Remove-Item -Force -ErrorAction SilentlyContinue $OldPath

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
    Write-Host "install-kanade-client: VersionInfo read failed: $_"
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

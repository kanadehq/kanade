#requires -Version 5.1
<#
Install / upgrade the kanade-client Tauri app on this PC.

Pulls the binary from OBJECT_APP_PACKAGES via the operator-facing
HTTP endpoint the backend serves at:

    GET /api/app-packages/kanade-client/<version>

(see kanade-shared::kv::OBJECT_APP_PACKAGES and
kanade-backend::api::app_packages — Sprint 8 / yukimemi/kanade#210).

The script's contract with the inventory projector is "emit a single
JSON object on stdout" — the `inventory:` block in the parent
manifest renders that into the SPA's Inventory page so operators
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
# -------------------------------------------------------------------------

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

# Clean up any leftover staging file from a previous failed run so
# the download doesn't fail on `Move-Item -Force` colliding with a
# half-written .new. Use -ErrorAction SilentlyContinue because the
# absence of the file is the happy path.
Remove-Item -Force -ErrorAction SilentlyContinue $NewPath, $OldPath

Write-Output "Downloading kanade-client $Version from $Url"
Invoke-WebRequest -Uri $Url -OutFile $NewPath -UseBasicParsing | Out-Null

if (Test-Path $ExePath) {
    Move-Item -Force $ExePath $OldPath
}
Move-Item -Force $NewPath $ExePath
Remove-Item -Force -ErrorAction SilentlyContinue $OldPath

# Read back ProductVersion from the embedded VERSIONINFO so the
# inventory row reports what's actually on disk, not what the
# manifest asked for. (winres builds the agent + client with this
# metadata populated — see kanade-{agent,client}/build.rs.)
$installed = (Get-Item $ExePath).VersionInfo.ProductVersion

# Inventory payload. Single line JSON keeps the projector's parse
# trivial; the manifest's `inventory.display` lists `version` +
# `path` so the SPA renders this without any further config.
$payload = [ordered]@{
    version = $installed
    path    = $ExePath
} | ConvertTo-Json -Compress
Write-Output $payload

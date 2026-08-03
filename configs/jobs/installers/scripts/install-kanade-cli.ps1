#requires -Version 5.1
<#
Install / upgrade the `kanade` admin CLI on this PC.

Pulls the binary from OBJECT_APP_PACKAGES via the operator-facing
HTTP endpoint the backend serves at:

    GET /api/app-packages/kanade-cli/<version>

Same transport and same atomic-swap shape as
`install-kanade-client.ps1` -- read that script's header for the
stdout-must-stay-pure-JSON rationale (progress chatter goes to STDERR
via `[Console]::Error.WriteLine(...)`, never `Write-Host`, because the
agent captures stdout and the inventory projector parses it whole).

Why an install JOB for an operator tool at all: the CLI is the one
kanade component with no self-update path. `kanade agent rollout`
updates agents, `install-kanade-backend` updates the backend, and the
client has its own job -- but the CLI has been hand-copied onto whatever
host needed it, which is how a host ends up driving a newer backend
with a CLI old enough that `validate` and `create` disagree about the
manifest schema. Giving it the same publish -> job -> exec route as
everything else means version bumps ride the existing rollout, and the
`inventory:` block below answers "which CLI is on which host" without
logging in.

SCOPE. This is an operator-host tool, not fleet software. Exec it at
the specific hosts that run operator commands (the backend host, an
ops-management box) -- `--pcs <id>`, not `--all`. The binary carries no
credentials (it reads `$env:KANADE_*_TOKEN` / `kanade login`), so a
stray copy is not itself a privilege grant, but installing an admin CLI
on end-user endpoints is noise at best.
#>

$ErrorActionPreference = 'Stop'

# --- Configurable knobs --------------------------------------------------
# `scripts/fleet-deploy.ps1 -Role cli` rewrites these four in a TEMP copy
# of this file before publishing, so the version-controlled values below
# stay neutral. Edit them by hand only for the manual flow in
# ../README.md.
$BackendBase = 'http://kanade-backend.local:8080'
$Version     = '0.0.0'
# Hex sha256 of the binary uploaded to OBJECT_APP_PACKAGES under
# `<Version>`. Required -- blank fails fast rather than installing
# whatever the backend happens to serve. A poisoned / MITM-substituted
# binary won't match and is refused before it can replace the existing
# install.
$ExpectedSha256 = ''
# Bearer for the backend's `/api/app-packages/<name>/<version>` route.
# Required when backend auth is enabled (the production posture); leave
# empty only for unauthenticated dev setups.
$CliSourceAuthToken = ''
# How long BITS keeps retrying after a transient transfer error. See the
# same knob in install-kanade-client.ps1 -- the download itself is
# unbounded; this only caps the recovery window after a drop.
$DownloadRetryTimeoutSecs = 1800
# Put the install directory on the MACHINE Path. A CLI that isn't on
# PATH is half-installed: scheduled jobs and service-hosted scripts run
# without an interactive profile, so `kanade` has to resolve from the
# machine PATH or not at all. Set $false if the host already has the CLI
# on PATH from somewhere else (scoop, cargo bin) and you don't want two.
$AddToMachinePath = $true
# -------------------------------------------------------------------------

if ([string]::IsNullOrWhiteSpace($ExpectedSha256)) {
    throw 'install-kanade-cli: $ExpectedSha256 must be set to the operator-computed sha256 of the uploaded binary (see Configurable knobs).'
}

$InstallDir = Join-Path $env:ProgramFiles 'Kanade'
$ExePath    = Join-Path $InstallDir 'kanade.exe'
$Url        = "$BackendBase/api/app-packages/kanade-cli/$Version"

# Stage to <exe>.new, swap, drop <exe>.old -- the atomic-replace pattern
# kanade-agent's self_update uses. It matters more here than for the
# client: an operator may have a `kanade` running (a long `kanade query`,
# a watch loop) while this job installs. Windows permits RENAMING a
# running image, so `<exe>` -> `.old` succeeds where an in-place
# overwrite would fail with a sharing violation; the running process
# keeps its now-`.old` file open and exits normally.
#
# The rollback name is per-run (`.old.<8 hex>`) rather than a fixed
# `<exe>.old`, because the file this script most expects to be LOCKED is
# exactly that one: the `kanade` that was running during the previous
# install still holds its renamed image open, so the cleanup delete below
# fails and the file survives. With a fixed name the next install's
# `<exe>` -> `<exe>.old` rename then targets a locked destination and dies
# with a sharing violation before it has done anything -- an install
# refused by the debris of the last one. A fresh name per run can't
# collide.
$NewPath = "$ExePath.new"
$OldPath = "$ExePath.old.{0}" -f ([guid]::NewGuid().ToString('N').Substring(0, 8))

New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null

# Clear a leftover staging file from a previous failed run.
Remove-Item -Force -ErrorAction SilentlyContinue $NewPath

# Sweep rollback files left by earlier runs whose cleanup was blocked.
# Unique names mean nothing else ever reclaims them, so this is where
# they go; whatever is still locked right now simply fails and gets swept
# by a later run. Deliberately after the `.new` clear and before the
# swap, so a wedged file never blocks the install itself.
Get-ChildItem -LiteralPath $InstallDir -Filter 'kanade.exe.old.*' -File -ErrorAction SilentlyContinue |
    ForEach-Object { Remove-Item -Force -ErrorAction SilentlyContinue $_.FullName }

[Console]::Error.WriteLine("Downloading kanade CLI $Version from $Url (BITS)")
$bitsHeaders = @()
if (-not [string]::IsNullOrWhiteSpace($CliSourceAuthToken)) {
    # `.Trim()` guards against copy-paste whitespace -- a leading newline
    # sends `Bearer \n<token>` and the backend 401s with a confusing
    # "missing bearer token".
    $bitsHeaders += "Authorization: Bearer $($CliSourceAuthToken.Trim())"
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
$actualSha   = (Get-FileHash $NewPath -Algorithm SHA256).Hash.ToLowerInvariant()
$expectedSha = $ExpectedSha256.ToLowerInvariant()
if ($actualSha -ne $expectedSha) {
    Remove-Item -Force -ErrorAction SilentlyContinue $NewPath
    throw "install-kanade-cli: downloaded binary sha256 mismatch: expected=$expectedSha actual=$actualSha -- refusing to install (possible MITM / corrupted upload)"
}
[Console]::Error.WriteLine("sha256 verified: $actualSha")

# --- Atomic-replace with rollback ----------------------------------------
$hadPrevious = Test-Path $ExePath
if ($hadPrevious) {
    Move-Item -Force $ExePath $OldPath
}
try {
    Move-Item -Force $NewPath $ExePath
} catch {
    if ($hadPrevious -and (Test-Path $OldPath)) {
        Move-Item -Force $OldPath $ExePath
        [Console]::Error.WriteLine('install-kanade-cli: rolled back to previous binary after promotion failure')
    }
    throw
}
# Best-effort: a `kanade` that was running when the swap happened still
# holds this renamed image open, so the delete fails. Harmless -- the
# sweep at the top of the next run collects it once the process is gone,
# and the unique name means it can never block that run's own swap.
Remove-Item -Force -ErrorAction SilentlyContinue $OldPath

# --- Machine PATH --------------------------------------------------------
# Deliberately NOT `[Environment]::SetEnvironmentVariable('Path', ...,
# 'Machine')`: that API reads the value EXPANDED and writes it back as a
# plain REG_SZ, which permanently bakes this machine's current
# `%SystemRoot%` / `%ProgramFiles%` into every other entry and downgrades
# the value kind. Go through the registry directly with
# DoNotExpandEnvironmentNames and re-write with the ORIGINAL value kind so
# REG_EXPAND_SZ entries survive untouched.
# Returns 'added' or 'present'; throws if the value can't be read/written.
function Add-MachinePathEntry {
    param([Parameter(Mandatory)][string]$Directory)

    $subKey = 'SYSTEM\CurrentControlSet\Control\Session Manager\Environment'
    $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subKey, $true)
    if (-not $key) { throw "cannot open HKLM\$subKey for write" }
    try {
        $raw  = [string]$key.GetValue('Path', '', [Microsoft.Win32.RegistryValueOptions]::DoNotExpandEnvironmentNames)
        $kind = $key.GetValueKind('Path')
        $entries = @($raw -split ';' | Where-Object { -not [string]::IsNullOrWhiteSpace($_) })
        foreach ($e in $entries) {
            # `-eq` on strings is case-insensitive in PowerShell; trailing
            # backslashes are cosmetic on Windows paths.
            if ($e.Trim().TrimEnd('\') -eq $Directory.TrimEnd('\')) { return 'present' }
        }
        $key.SetValue('Path', (($entries + $Directory) -join ';'), $kind)
        return 'added'
    } finally {
        $key.Close()
    }
}

# Reported as `on_path` below -- the ACTUAL state after this run, not the
# knob. A failed PATH update must not show up in inventory as success.
$onPath = $false
if ($AddToMachinePath) {
    try {
        $pathState = Add-MachinePathEntry -Directory $InstallDir
        $onPath = $true
        if ($pathState -eq 'added') {
            # No WM_SETTINGCHANGE broadcast: this job runs as LocalSystem in
            # session 0, and HWND_BROADCAST does not cross session
            # boundaries -- the interactive desktop's explorer.exe would
            # never receive it. Already-running processes keep the old
            # environment either way (they inherited a copy at spawn), so
            # the honest contract is "new logon / service restart picks it
            # up", which is also when the jobs that need it run.
            [Console]::Error.WriteLine("machine PATH += $InstallDir (existing shells keep the old PATH until re-logon)")
        } else {
            [Console]::Error.WriteLine("machine PATH already contains $InstallDir")
        }
    } catch {
        # Non-fatal: the binary is installed and usable by absolute path.
        [Console]::Error.WriteLine("install-kanade-cli: machine PATH update failed (use the absolute path): $_")
    }
}

# --- Inventory payload ---------------------------------------------------
# Read ProductVersion back from the embedded VERSIONINFO
# (crates/kanade/build.rs stamps it) so the inventory row reports what is
# actually on disk, not what the manifest asked for.
$installed = $null
try {
    $installed = (Get-Item $ExePath).VersionInfo.ProductVersion
} catch {
    [Console]::Error.WriteLine("install-kanade-cli: VersionInfo read failed: $_")
}
if ([string]::IsNullOrWhiteSpace($installed)) {
    $installed = 'unknown'
}

# The ONE stdout line in this script -- single-line JSON keeps the
# projector's parse trivial.
$payload = [ordered]@{
    version = $installed
    path    = $ExePath
    on_path = $onPath
} | ConvertTo-Json -Compress
Write-Output $payload

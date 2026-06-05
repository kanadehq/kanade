#requires -Version 7.0
<#
.SYNOPSIS
  Publish a staged kanade binary and roll it out across the fleet via the
  agent — the repeatable "2nd deploy onwards" companion to
  `build-release.ps1`.

.DESCRIPTION
  `build-release.ps1` is the *first-time / staging* tool: it fetches (or
  builds) a role's binary into `dist/<role>/`. This script takes that
  staged binary the rest of the way — it does what an operator would
  otherwise type out by hand every release:

    backend / client (job-install pattern)
      1. kanade app publish kanade-<role> <exe>      (version auto-from-PE)
      2. inject the agent-mode download knobs into a temp copy of the
         role's deploy script (SourceUrl / Version / Sha256 / AuthToken,
         plus backend-only -WipeDb / -JwtSecret / -StaticToken /
         -BootstrapAdminPassword when asked)
      3. publish that script (backend: OBJECT_SCRIPTS `script_object`;
         client: inlined via the manifest's `script_file`)
      4. render a temp manifest pinned to this version + script ref
      5. kanade job create <temp manifest>
      6. kanade exec install-kanade-<role> --pcs <pc>
      7. verify (poll the locally-installed exe's version)

    agent (self-update pattern — NOT a job)
      1. kanade agent publish <exe>                  (version auto-from-PE)
      2. kanade agent rollout <version>              (flips target_version)
      3. verify (kanade agent current == version)

  Nothing under version control is mutated — the edited deploy script and
  the pinned manifest are written to temp files (the manifest's
  `script_file:` is resolved as an absolute path by `kanade job create`,
  which inlines it). Tokens default to the dev literals; override per
  environment.

.PARAMETER Role
  backend | agent | client. backend/client go through the install job;
  agent goes through publish + rollout.

.PARAMETER Pc
  exec target pc_id for the job-install roles (lower-cased automatically —
  the agent registers pc_ids lower-cased, so `MINIPC` must target
  `minipc` or the exec sticks at pending). Default `minipc`. Ignored for
  -Role agent (rollout is fleet-wide). Mutually exclusive with -Groups.

.PARAMETER Groups
  Alternative exec target: one or more group names (`--groups`). Mutually
  exclusive with -Pc.

.PARAMETER Version
  Version to publish / roll out. Default: read from the staged exe's PE
  VERSIONINFO so it can never drift from the binary.

.PARAMETER ExePath
  Staged binary. Default `dist/<role>/kanade-<role>.exe` (where
  build-release.ps1 puts it).

.PARAMETER Stage
  Run `build-release.ps1 -Roles <role> -Version <ver>` first to (re)stage
  the binary before publishing.

.PARAMETER SourceUrl
  Where the agent downloads the app package from (the backend's own
  app-packages HTTP). Default `http://127.0.0.1:8080` (co-located box).

.PARAMETER AuthToken
  Bearer for the backend HTTP app-packages endpoint. Default:
  $env:KANADE_AUTH_TOKEN, else `dev`.

.PARAMETER NatsToken
  Broker token for the `kanade` CLI calls. Default: $env:KANADE_NATS_TOKEN,
  else `dev`.

.PARAMETER WipeDb
  (backend only) Drop the projector DB on deploy — needed across a
  squashed-migration baseline. Off by default.

.PARAMETER JwtSecret
.PARAMETER StaticToken
.PARAMETER BootstrapAdminPassword
  (backend only) Provision these backend secrets during the deploy. Empty
  = leave the existing registry value untouched.

.PARAMETER NoVerify
  Skip the post-deploy version check.

.PARAMETER DryRun
  Print every kanade command without executing (knob injection + manifest
  render still run against temp files so you can inspect them).

.EXAMPLE
  # Repeat backend deploy to the minipc, no wipe (the common case):
  PS> .\scripts\fleet-deploy.ps1 -Role backend

.EXAMPLE
  # Fresh-RBAC backend box: wipe + seed an admin:
  PS> .\scripts\fleet-deploy.ps1 -Role backend -WipeDb `
        -JwtSecret dev -BootstrapAdminPassword dev

.EXAMPLE
  # Roll a new agent out to the whole fleet:
  PS> .\scripts\fleet-deploy.ps1 -Role agent -Stage

.EXAMPLE
  # See exactly what would run, change nothing:
  PS> .\scripts\fleet-deploy.ps1 -Role client -Groups canary -DryRun
#>
[CmdletBinding(DefaultParameterSetName = 'Pc')]
param(
    [Parameter(Mandatory)][ValidateSet('backend', 'agent', 'client')]
    [string]$Role,

    [Parameter(ParameterSetName = 'Pc')]
    [string]$Pc = 'minipc',

    [Parameter(ParameterSetName = 'Groups', Mandatory)]
    [string[]]$Groups,

    [string]$Version = '',
    [string]$ExePath = '',
    [switch]$Stage,

    [string]$SourceUrl = 'http://127.0.0.1:8080',
    [string]$AuthToken = '',
    [string]$NatsToken = '',

    [switch]$WipeDb,
    [string]$JwtSecret = '',
    [string]$StaticToken = '',
    [string]$BootstrapAdminPassword = '',

    [switch]$NoVerify,
    [switch]$DryRun
)

$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot

# Fail fast if the CLI we drive isn't on PATH — otherwise the script
# dies halfway with a generic command-not-found after already staging
# binaries / writing temp files.
if (-not (Get-Command kanade -ErrorAction SilentlyContinue)) {
    throw "kanade CLI not found on PATH — install it (or add it to PATH) before running this script."
}

# The job-install path writes a temp deploy-script copy + temp manifest
# that can carry plaintext secrets (-JwtSecret / -StaticToken /
# -BootstrapAdminPassword). Track them so the trap below + the success
# path can shred them; -DryRun keeps them for inspection.
$tmpScript = $null
$tmpManifest = $null
trap {
    if (-not $DryRun) {
        if ($tmpScript   -and (Test-Path $tmpScript))   { Remove-Item -Force -ErrorAction SilentlyContinue $tmpScript }
        if ($tmpManifest -and (Test-Path $tmpManifest)) { Remove-Item -Force -ErrorAction SilentlyContinue $tmpManifest }
    }
    break
}

# Token defaults — dev literals unless the environment / args override.
# ContainsKey (not `-not $AuthToken`) so an explicit `-AuthToken ''`
# is honoured rather than silently replaced.
if (-not $PSBoundParameters.ContainsKey('AuthToken')) { $AuthToken = if ($env:KANADE_AUTH_TOKEN) { $env:KANADE_AUTH_TOKEN } else { 'dev' } }
if (-not $PSBoundParameters.ContainsKey('NatsToken')) { $NatsToken = if ($env:KANADE_NATS_TOKEN) { $env:KANADE_NATS_TOKEN } else { 'dev' } }
$env:KANADE_AUTH_TOKEN = $AuthToken
$env:KANADE_NATS_TOKEN = $NatsToken

# ---- helpers --------------------------------------------------------------

function Invoke-Kanade {
    # Print the `kanade ...` invocation, then run it (unless -DryRun).
    # Returns captured stdout lines (empty under -DryRun).
    param([Parameter(Mandatory)][string[]]$CliArgs)
    Write-Host "  kanade $($CliArgs -join ' ')" -ForegroundColor DarkCyan
    if ($DryRun) { Write-Host '    [dry-run, skipped]' -ForegroundColor DarkGray; return @() }
    $out = & kanade @CliArgs 2>&1
    if ($LASTEXITCODE -ne 0) {
        $out | ForEach-Object { Write-Host "    $_" }
        throw "kanade $($CliArgs[0]) failed (exit $LASTEXITCODE)"
    }
    return $out
}

function Set-PsAssignment {
    # Replace a top-level `$Name = ...` line in a .ps1's line array.
    # -Raw keeps the value verbatim (e.g. `$true`); else single-quoted.
    param([string[]]$Lines, [string]$Name, [string]$Value, [switch]$Raw)
    # Double up single quotes so a secret/token containing `'` doesn't
    # break the single-quoted literal in the generated script.
    $new = if ($Raw) { "`$$Name = $Value" } else { "`$$Name = '$($Value -replace "'", "''")'" }
    $pat = '^\s*\$' + [regex]::Escape($Name) + '\s*='
    for ($i = 0; $i -lt $Lines.Count; $i++) {
        if ($Lines[$i] -match $pat) { $Lines[$i] = $new; return , $Lines }
    }
    throw "knob `$$Name not found in deploy script — can't inject"
}

function Set-YamlLine {
    param([string[]]$Lines, [string]$Pattern, [string]$NewLine)
    for ($i = 0; $i -lt $Lines.Count; $i++) {
        if ($Lines[$i] -match $Pattern) { $Lines[$i] = $NewLine; return , $Lines }
    }
    throw "manifest line /$Pattern/ not found"
}

function Get-ExeVersion {
    param([string]$Path)
    (Get-Item $Path).VersionInfo.ProductVersion
}

# ---- resolve the staged binary + version ----------------------------------

$exeName = "kanade-$Role.exe"
if (-not $ExePath) { $ExePath = Join-Path $repoRoot "dist\$Role\$exeName" }

if ($Stage) {
    Write-Host "=== stage ($Role) ===" -ForegroundColor Cyan
    $brArgs = @('-Roles', $Role)
    if ($Version) { $brArgs += @('-Version', $Version) }
    Write-Host "  build-release.ps1 $($brArgs -join ' ')" -ForegroundColor DarkCyan
    if (-not $DryRun) { & (Join-Path $PSScriptRoot 'build-release.ps1') @brArgs }
}

if (-not (Test-Path $ExePath)) {
    throw "Staged binary not found: $ExePath. Run build-release.ps1 -Roles $Role first (or pass -Stage / -ExePath)."
}
if (-not $Version) {
    $Version = Get-ExeVersion $ExePath
    if (-not $Version) {
        throw "Could not read a version from $ExePath's PE VERSIONINFO — pass -Version explicitly."
    }
    Write-Host "Version (from PE VERSIONINFO): $Version"
}

if ($Role -ne 'backend' -and ($WipeDb -or $JwtSecret -or $StaticToken -or $BootstrapAdminPassword)) {
    Write-Warning "-WipeDb / -JwtSecret / -StaticToken / -BootstrapAdminPassword apply to -Role backend only; ignored for $Role."
}

Write-Host ''
Write-Host "=== fleet-deploy: $Role v$Version ===" -ForegroundColor Cyan
Write-Host "  exe    : $ExePath"
Write-Host "  target : $(if ($Groups) { "groups=$($Groups -join ',')" } else { "pc=$($Pc.ToLower())" })"
if ($DryRun) { Write-Host '  (dry-run: no NATS writes, no exec)' -ForegroundColor Yellow }
Write-Host ''

# =========================================================================
# agent: self-update path (publish + rollout) — not a job
# =========================================================================
if ($Role -eq 'agent') {
    Write-Host '--- publish ---'
    Invoke-Kanade @('agent', 'publish', $ExePath) | ForEach-Object { Write-Host "    $_" }

    Write-Host '--- rollout ---'
    Invoke-Kanade @('agent', 'rollout', $Version) | ForEach-Object { Write-Host "    $_" }

    if (-not $NoVerify -and -not $DryRun) {
        Write-Host '--- verify (broadcast target_version) ---'
        $cur = & kanade agent current 2>&1
        $cur | ForEach-Object { Write-Host "    $_" }
        if ($cur -match [regex]::Escape($Version)) {
            Write-Host "OK: target_version = $Version" -ForegroundColor Green
        } else {
            Write-Warning "target_version does not yet show $Version — check 'kanade agent current'."
        }
    }
    Write-Host ''
    Write-Host "Done. Agents self-update to $Version on their next watcher tick." -ForegroundColor Green
    return
}

# =========================================================================
# backend / client: job-install path
# =========================================================================

# Per-role differences in one table.
$spec = @{
    backend = @{
        App          = 'kanade-backend'
        JobId        = 'install-kanade-backend'
        Manifest     = Join-Path $repoRoot 'configs\jobs\installers\install-kanade-backend.yaml'
        DeployScript = Join-Path $repoRoot 'scripts\deploy\backend.ps1'
        ScriptName   = 'deploy-backend'         # OBJECT_SCRIPTS name
        Delivery     = 'object'                 # script_object
        InstalledExe = Join-Path $env:ProgramFiles 'Kanade\kanade-backend.exe'
        Knobs        = @{ Url = 'AgentSourceUrl'; Ver = 'AgentSourceVersion'; Sha = 'AgentSourceSha256'; Tok = 'AgentSourceAuthToken' }
    }
    client  = @{
        App          = 'kanade-client'
        JobId        = 'install-kanade-client'
        Manifest     = Join-Path $repoRoot 'configs\jobs\installers\install-kanade-client.yaml'
        DeployScript = Join-Path $repoRoot 'configs\jobs\installers\scripts\install-kanade-client.ps1'
        Delivery     = 'file'                    # script_file (inlined at job create)
        InstalledExe = Join-Path $env:ProgramFiles 'Kanade\kanade-client.exe'
        Knobs        = @{ Url = 'BackendBase'; Ver = 'Version'; Sha = 'ExpectedSha256'; Tok = 'ClientSourceAuthToken' }
    }
}[$Role]

$sha256 = (Get-FileHash $ExePath -Algorithm SHA256).Hash.ToLower()
Write-Host "  sha256 : $sha256"
Write-Host ''

# 1. publish the app package (version auto-extracted from the exe).
Write-Host '--- app publish ---'
Invoke-Kanade @('app', 'publish', $spec.App, $ExePath) | ForEach-Object { Write-Host "    $_" }

# 2. inject the download knobs into a temp copy of the deploy script.
Write-Host '--- inject deploy-script knobs ---'
$lines = Get-Content -LiteralPath $spec.DeployScript
$k = $spec.Knobs
$lines = Set-PsAssignment $lines $k.Url $SourceUrl
$lines = Set-PsAssignment $lines $k.Ver $Version
$lines = Set-PsAssignment $lines $k.Sha $sha256
$lines = Set-PsAssignment $lines $k.Tok $AuthToken
if ($Role -eq 'backend') {
    if ($WipeDb)                 { $lines = Set-PsAssignment $lines 'AgentWipeDb' '$true' -Raw }
    if ($StaticToken)            { $lines = Set-PsAssignment $lines 'AgentStaticToken' $StaticToken }
    if ($JwtSecret)              { $lines = Set-PsAssignment $lines 'AgentJwtSecret' $JwtSecret }
    if ($BootstrapAdminPassword) { $lines = Set-PsAssignment $lines 'AgentBootstrapAdminPassword' $BootstrapAdminPassword }
    Write-Host "    knobs: $($k.Url)/$($k.Ver)/$($k.Sha)/$($k.Tok)$(if ($WipeDb) { ' +WipeDb' })$(if ($JwtSecret) { ' +JwtSecret' })$(if ($StaticToken) { ' +StaticToken' })$(if ($BootstrapAdminPassword) { ' +BootstrapAdminPassword' })"
} else {
    Write-Host "    knobs: $($k.Url)/$($k.Ver)/$($k.Sha)/$($k.Tok)"
}
$tmpScript = Join-Path ([System.IO.Path]::GetTempPath()) ("kanade-deploy-{0}-{1}-{2}.ps1" -f $Role, $Version, [System.Guid]::NewGuid().ToString('N'))
Set-Content -LiteralPath $tmpScript -Value $lines -Encoding utf8
Write-Host "    wrote $tmpScript"

# 3. + 4. publish the script + render a version-pinned temp manifest.
$manifestLines = Get-Content -LiteralPath $spec.Manifest
$manifestLines = Set-YamlLine $manifestLines '^version:' "version: $Version"
if ($spec.Delivery -eq 'object') {
    Write-Host '--- script publish ---'
    Invoke-Kanade @('script', 'publish', $spec.ScriptName, $Version, $tmpScript) | ForEach-Object { Write-Host "    $_" }
    $manifestLines = Set-YamlLine $manifestLines '^\s*script_object:' "  script_object: $($spec.ScriptName)/$Version"
} else {
    # client: the manifest's script_file is inlined by `kanade job create`.
    # Point it at the edited temp copy (absolute paths are honoured —
    # crates/kanade/src/cmd/job.rs resolve_script_file_path).
    $manifestLines = Set-YamlLine $manifestLines '^\s*script_file:' "  script_file: $tmpScript"
}
$tmpManifest = Join-Path ([System.IO.Path]::GetTempPath()) ("kanade-manifest-{0}-{1}-{2}.yaml" -f $Role, $Version, [System.Guid]::NewGuid().ToString('N'))
Set-Content -LiteralPath $tmpManifest -Value $manifestLines -Encoding utf8
Write-Host "    wrote $tmpManifest"

# 5. register the job.
Write-Host '--- job create ---'
Invoke-Kanade @('job', 'create', $tmpManifest) | ForEach-Object { Write-Host "    $_" }

# 6. exec at the target.
Write-Host '--- exec ---'
$execArgs = @('exec', $spec.JobId)
if ($Groups) { $execArgs += @('--groups', ($Groups -join ',')) }
else { $execArgs += @('--pcs', $Pc.ToLower()) }
Invoke-Kanade $execArgs | ForEach-Object { Write-Host "    $_" }

# 7. verify — poll the locally-installed exe (only meaningful when this box
#    is the target). Fleet-wide / remote targets: check the SPA Inventory.
$targetIsLocal = (-not $Groups) -and ($Pc.ToLower() -eq $env:COMPUTERNAME.ToLower())
if ($NoVerify -or $DryRun) {
    # nothing
} elseif (-not $targetIsLocal) {
    Write-Host ''
    Write-Host "Dispatched. Target is not this box — confirm rollout on the SPA Inventory page." -ForegroundColor Yellow
} else {
    Write-Host '--- verify (local install) ---'
    $deadline = (Get-Date).AddSeconds(120)
    $ok = $false
    while ((Get-Date) -lt $deadline) {
        if (Test-Path $spec.InstalledExe) {
            # The exe may be briefly locked while the service swaps it —
            # swallow transient access errors and retry rather than crash.
            $iv = $null
            try { $iv = Get-ExeVersion $spec.InstalledExe } catch { }
            if ($iv -eq $Version) { $ok = $true; break }
            Write-Host "    installed=$iv want=$Version ..."
        } else {
            Write-Host "    waiting for $($spec.InstalledExe) ..."
        }
        Start-Sleep -Seconds 5
    }
    if ($ok) {
        Write-Host "OK: $($spec.InstalledExe) is $Version" -ForegroundColor Green
    } else {
        throw "Timed out: $($spec.InstalledExe) did not reach $Version within 120s — check the agent / job result."
    }
}

# Shred the temp deploy-script + manifest (they can hold plaintext
# secrets). Kept under -DryRun so the rendered files can be inspected.
if (-not $DryRun) {
    if ($tmpScript   -and (Test-Path $tmpScript))   { Remove-Item -Force -ErrorAction SilentlyContinue $tmpScript }
    if ($tmpManifest -and (Test-Path $tmpManifest)) { Remove-Item -Force -ErrorAction SilentlyContinue $tmpManifest }
}

Write-Host ''
Write-Host "Done. $($spec.App) v$Version deployed via job $($spec.JobId)." -ForegroundColor Green

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
      2. kanade agent rollout <version> --pc/--group/--global
                                                     (flips target_version)
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
  Target pc_id, passed through VERBATIM (no case folding). The agent
  registers/subscribes with its `{{ system.host }}` = the OS hostname
  (Windows COMPUTERNAME), and NATS subjects are CASE-SENSITIVE:
  `commands.pc.<id>` must match the agent's subscription exactly or the
  exec is published to a subject no one is listening on and the install
  never fires. Hostname casing is NOT uniform across the fleet (some boxes
  register upper-, some lower-case), so there is no safe auto-transform —
  type the pc_id in its actual registered case (check the SPA Inventory /
  `kanade ping`). Maps to `--pcs` for the job-install roles and `--pc` for
  the agent rollout. No baked-in default: falls back to
  $env:KANADE_TARGET_PC, else one of -Pc / -Groups / -All is required.
  Mutually exclusive with -Groups / -All.

.PARAMETER Groups
  Alternative target: group names (`--groups` for job-install; the agent
  rollout takes exactly ONE `--group`). Mutually exclusive with -Pc / -All.

.PARAMETER All
  Whole-fleet target: `--all` for the job-install exec, `--global` for the
  agent rollout. Pair with -Jitter for agent rollouts so the fleet doesn't
  synchronise its downloads. Mutually exclusive with -Pc / -Groups.

.PARAMETER Jitter
  (agent only) `--jitter` for the rollout (humantime, e.g. `30m`) —
  recommended for -All so thousands of agents don't download at once.
  Defaults: single-host `-Pc` rollouts send `0s` (a one-PC pin gains
  nothing from de-sync — update immediately); -Groups / -All leave the
  existing scope value alone unless set explicitly.

.PARAMETER Version
  Version to publish / roll out. Three forms:
    - omitted   → read from the staged exe's PE VERSIONINFO (can't drift
                  from the binary).
    - `latest`  → resolve the newest GitHub release tag and fetch it
                  (implies -Stage).
    - `X.Y.Z`   → that exact version (must match the staged exe).

.PARAMETER GitHubRepo
  owner/repo to resolve `-Version latest` against. Default `yukimemi/kanade`.

.PARAMETER ExePath
  Staged binary. Default `dist/<role>/kanade-<role>.exe` (where
  build-release.ps1 puts it).

.PARAMETER Stage
  Run `build-release.ps1 -Roles <role> -Version <ver>` first to (re)stage
  the binary before publishing.

.PARAMETER Server
  NATS broker the `kanade` CLI publishes/execs against (app publish, script
  publish, job create, exec all go over NATS). Default
  $env:KANADE_NATS_URL, else `nats://127.0.0.1:4222`. Set this when running
  from an ops-management terminal rather than on the broker host.

.PARAMETER BackendUrl
  backend HTTP base the CLI uses for HTTP-path commands. Default
  $env:KANADE_BACKEND_URL, else `http://127.0.0.1:8080`.

.PARAMETER SourceUrl
  Where the *target host's* agent downloads the app package from — i.e. a
  backend app-packages HTTP reachable FROM THE TARGET (the agent knows
  only NATS, so it can't supply this itself). When omitted, resolved as:
    1. $env:KANADE_BACKEND_URL, if set and NOT loopback — the
       ops-terminal-to-remote-box case: the backend this terminal targets
       is almost always reachable from the target too, so inherit it
       rather than the useless 127.0.0.1 (which makes a remote agent try
       to download from itself — the install then times out).
    2. else `http://127.0.0.1:<port-from-staged-backend.toml>` — correct
       when the agent is co-located with the backend.
  Pass -SourceUrl explicitly only when the target must pull from a
  DIFFERENT backend than this terminal's (e.g. KANADE_BACKEND_URL is a
  terminal-only path like an SSH-tunnelled localhost). Distinct from
  -Server/-BackendUrl (which are this terminal -> infra).

.PARAMETER AuthToken
  Bearer for the backend HTTP app-packages endpoint. Default:
  $env:KANADE_AUTH_TOKEN, else `dev`.

.PARAMETER NatsToken
  Broker token for the `kanade` CLI calls. Default: $env:KANADE_NATS_TOKEN,
  else `dev`.

.PARAMETER WipeDb
  (backend only) Wipe the projector DB on deploy so it re-derives from
  JetStream — needed across a squashed-migration baseline. Preserves the
  `users` table (accounts survive the wipe). Off by default.

.PARAMETER JwtSecret
.PARAMETER StaticToken
.PARAMETER BootstrapAdminPassword
.PARAMETER MailPassword
  (backend only) Provision these backend secrets during the deploy. Empty
  = leave the existing registry value untouched. `MailPassword` is the SMTP
  AUTH password for the `[mail]` relay (e.g. a Gmail app password, spaces
  removed); the backend resolves it from the `MailPassword` registry value
  ahead of `$KANADE_MAIL_PASSWORD`.

.PARAMETER NoVerify
  Skip the post-deploy version check.

.PARAMETER DryRun
  Print every kanade command without executing (knob injection + manifest
  render still run against temp files so you can inspect them).

.EXAMPLE
  # Repeat backend deploy, no wipe (the common case):
  PS> .\scripts\fleet-deploy.ps1 -Role backend -Pc <pc-id>

.EXAMPLE
  # Fresh-RBAC backend box: wipe + seed an admin:
  PS> .\scripts\fleet-deploy.ps1 -Role backend -Pc <pc-id> -WipeDb `
        -JwtSecret dev -BootstrapAdminPassword dev

.EXAMPLE
  # Grab whatever the newest release is and deploy it to a host:
  PS> .\scripts\fleet-deploy.ps1 -Role backend -Version latest -Pc <pc-id>

.EXAMPLE
  # From an ops-management terminal (not the broker host):
  PS> .\scripts\fleet-deploy.ps1 -Role backend -Version latest `
        -Server nats://broker.corp:4222 -Pc <pc-id> -NatsToken $tok

.EXAMPLE
  # Set a default target for this terminal, then omit -Pc:
  PS> $env:KANADE_TARGET_PC = '<pc-id>'
  PS> .\scripts\fleet-deploy.ps1 -Role backend -Version latest

.EXAMPLE
  # Try a new agent on one box first, then roll it out fleet-wide:
  PS> .\scripts\fleet-deploy.ps1 -Role agent -Stage -Pc <pc-id>
  PS> .\scripts\fleet-deploy.ps1 -Role agent -All -Jitter 30m

.EXAMPLE
  # See exactly what would run, change nothing:
  PS> .\scripts\fleet-deploy.ps1 -Role client -Groups canary -DryRun
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('backend', 'agent', 'client')]
    [string]$Role,

    # No baked-in default — resolved from $env:KANADE_TARGET_PC, else one
    # of -Pc / -Groups is required (validated below). Kept out of a
    # ParameterSet so the env fallback can fill it without prompting.
    [string]$Pc = '',
    [string[]]$Groups,
    [switch]$All,
    [string]$Jitter = '',

    [string]$Version = '',
    [string]$GitHubRepo = 'yukimemi/kanade',
    [string]$ExePath = '',
    [switch]$Stage,

    [string]$Server = '',
    [string]$BackendUrl = '',
    # Empty default: when -SourceUrl is omitted it's resolved below
    # (KANADE_BACKEND_URL / -BackendUrl, else 127.0.0.1:<port>), so a
    # literal here would just be dead — kept '' to not mislead readers.
    [string]$SourceUrl = '',
    [string]$AuthToken = '',
    [string]$NatsToken = '',

    [switch]$WipeDb,
    [string]$JwtSecret = '',
    [string]$StaticToken = '',
    [string]$BootstrapAdminPassword = '',
    [string]$MailPassword = '',

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
if (-not $PSBoundParameters.ContainsKey('AuthToken')) { $AuthToken = if (-not [string]::IsNullOrWhiteSpace($env:KANADE_AUTH_TOKEN)) { $env:KANADE_AUTH_TOKEN } else { 'dev' } }
if (-not $PSBoundParameters.ContainsKey('NatsToken')) { $NatsToken = if (-not [string]::IsNullOrWhiteSpace($env:KANADE_NATS_TOKEN)) { $env:KANADE_NATS_TOKEN } else { 'dev' } }
$env:KANADE_AUTH_TOKEN = $AuthToken
$env:KANADE_NATS_TOKEN = $NatsToken

# Broker / backend endpoints the `kanade` CLI talks to. Default localhost
# (co-located dev box); from an ops-management terminal point -Server at
# the broker. An already-set env var flows through if the flag is omitted.
if (-not $PSBoundParameters.ContainsKey('Server'))     { $Server     = if (-not [string]::IsNullOrWhiteSpace($env:KANADE_NATS_URL))    { $env:KANADE_NATS_URL }    else { 'nats://127.0.0.1:4222' } }
if (-not $PSBoundParameters.ContainsKey('BackendUrl')) { $BackendUrl = if (-not [string]::IsNullOrWhiteSpace($env:KANADE_BACKEND_URL)) { $env:KANADE_BACKEND_URL } else { 'http://127.0.0.1:8080' } }
$env:KANADE_NATS_URL    = $Server
$env:KANADE_BACKEND_URL = $BackendUrl

# Deploy target — all roles. No baked-in host: fall back to
# $env:KANADE_TARGET_PC, then require exactly one of -Pc / -Groups / -All
# so we never silently fan a deploy at a guessed scope. The same flags map
# per role: job-install exec takes --pcs/--groups/--all; agent rollout
# takes --pc/--group/--global (and itself refuses an implicit scope).
# Env fallback only when NO explicit target was given — otherwise a set
# $env:KANADE_TARGET_PC would fill $Pc even alongside an explicit
# -Groups/-All and trip the mutual-exclusion check below.
if ([string]::IsNullOrWhiteSpace($Pc) -and -not $Groups -and -not $All -and -not [string]::IsNullOrWhiteSpace($env:KANADE_TARGET_PC)) {
    $Pc = $env:KANADE_TARGET_PC
}
# `pwsh -File` passes `-Groups a,b` as ONE literal string (no comma array
# binding) — normalise so both invocation styles see the same list, and
# trim whitespace so `-Groups "canary, dev"` yields exact group names.
if ($Groups) { $Groups = @($Groups | ForEach-Object { $_ -split ',' } | ForEach-Object { $_.Trim() } | Where-Object { $_ }) }
$hasPc = -not [string]::IsNullOrWhiteSpace($Pc)
if (@($hasPc, [bool]$Groups, [bool]$All).Where({ $_ }).Count -gt 1) {
    throw "-Pc / -Groups / -All are mutually exclusive — pick one target scope."
}
if (-not $hasPc -and -not $Groups -and -not $All) {
    throw "No deploy target. Pass -Pc <pc_id>, -Groups <group[,...]>, or -All (or set `$env:KANADE_TARGET_PC)."
}
if ($Role -eq 'agent' -and $Groups -and $Groups.Count -gt 1) {
    throw "agent rollout targets a single group — pass exactly one -Groups value."
}

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

function Resolve-LatestVersion {
    # Newest GitHub release tag (vX.Y.Z) -> X.Y.Z. Public repo, so the
    # unauthenticated releases API is fine; no `gh` dependency.
    param([string]$Repo)
    $api = "https://api.github.com/repos/$Repo/releases/latest"
    $tag = (Invoke-RestMethod -Uri $api -Headers @{ 'User-Agent' = 'kanade-fleet-deploy' }).tag_name
    if (-not $tag) { throw "could not resolve 'latest' release from $api" }
    return ($tag -replace '^v', '')
}

function Get-BackendHttpPort {
    # Auto-read the backend HTTP port from the staged backend.toml so the
    # default SourceUrl tracks config drift; fall back to 8080.
    $toml = Join-Path $repoRoot 'dist\backend\backend.toml'
    if (Test-Path $toml) {
        $m = Select-String -Path $toml -Pattern "^\s*bind\s*=\s*'[^']*:(\d+)'" | Select-Object -First 1
        if ($m) { return $m.Matches[0].Groups[1].Value }
    }
    return '8080'
}

# ---- resolve the staged binary + version ----------------------------------

# Default SourceUrl. The target's agent downloads the app package from
# this URL over HTTP (client: install-kanade-client.ps1 `$BackendBase`;
# backend: the deploy script's `AgentSourceUrl`), so it must be reachable
# FROM THE TARGET — and the agent itself knows only NATS, not any HTTP
# backend base, so it can't supply one. Resolution, when -SourceUrl is
# omitted:
#   1. $BackendUrl (resolved from -BackendUrl / KANADE_BACKEND_URL), IF
#      set and not loopback — the common ops-terminal case (deploying to a
#      remote box from a management shell): the same backend this terminal
#      talks to is almost always the one the target can reach too, so
#      inherit it instead of the useless 127.0.0.1 (which tells the remote
#      agent to download from ITSELF — BITS fails and the install never
#      completes / verify times out). The whole 127.0.0.0/8 loopback block
#      is skipped because it's no better than the fallback.
#   2. else http://127.0.0.1:<port-from-staged-backend.toml> — correct
#      when the agent is co-located with the backend (the original
#      assumption).
# A target that must pull from a DIFFERENT backend than this terminal's
# (e.g. KANADE_BACKEND_URL is a terminal-only path like an SSH-tunnelled
# localhost) still needs an explicit -SourceUrl.
if (-not $PSBoundParameters.ContainsKey('SourceUrl')) {
    # Read the already-resolved $BackendUrl (it was set from
    # KANADE_BACKEND_URL / -BackendUrl earlier), not $env:KANADE_BACKEND_URL
    # directly — same value here, but without the implicit dependency on
    # the env-var being mutated first. Reject the whole 127.0.0.0/8
    # loopback block (not just 127.0.0.1) so a 127.x backend never gets
    # inherited as a remote target's SourceUrl.
    if (-not [string]::IsNullOrWhiteSpace($BackendUrl) -and $BackendUrl -notmatch '://(127\.\d{1,3}\.\d{1,3}\.\d{1,3}|localhost|\[::1\])(:|/|$)') {
        $SourceUrl = $BackendUrl
    } else {
        $SourceUrl = "http://127.0.0.1:$(Get-BackendHttpPort)"
    }
}

$exeName = "kanade-$Role.exe"
if (-not $ExePath) { $ExePath = Join-Path $repoRoot "dist\$Role\$exeName" }

# `-Version latest` -> resolve the newest release tag and force a stage so
# the staged binary actually IS that version.
if ($Version -eq 'latest') {
    $Version = Resolve-LatestVersion $GitHubRepo
    Write-Host "Version (latest release): $Version"
    $Stage = $true
}

if ($Stage) {
    Write-Host "=== stage ($Role) ===" -ForegroundColor Cyan
    # Hashtable splat (NOT array) so the args bind by NAME — array
    # splatting is positional and would feed `-Roles` in as a value.
    # Propagate -GitHubRepo so a fork stages from the same repo `latest`
    # resolved against.
    $brParams = @{ Roles = @($Role); GitHubRepo = $GitHubRepo }
    if ($Version) { $brParams['Version'] = $Version }
    Write-Host "  build-release.ps1 -Roles $Role -GitHubRepo $GitHubRepo$(if ($Version) { " -Version $Version" })" -ForegroundColor DarkCyan
    if (-not $DryRun) { & (Join-Path $PSScriptRoot 'build-release.ps1') @brParams }
}

# Reconcile the requested version against what's actually staged. Under
# -DryRun staging is skipped (so a stale/absent exe is expected) — warn
# and trust the requested version instead of hard-failing; a real run
# stages first and the checks below bite. Outside dry-run we still refuse
# to publish a mislabelled binary.
$exeStaged = Test-Path $ExePath
$exeVer = if ($exeStaged) { Get-ExeVersion $ExePath } else { $null }

if (-not $Version) {
    # No version given: it can only come from the staged exe.
    if (-not $exeStaged) {
        throw "Staged binary not found: $ExePath. Run build-release.ps1 -Roles $Role first (or pass -Stage / -Version / -ExePath)."
    }
    if (-not $exeVer) {
        throw "Could not read a version from $ExePath's PE VERSIONINFO — pass -Version explicitly."
    }
    $Version = $exeVer
    Write-Host "Version (from PE VERSIONINFO): $Version"
} elseif (-not $exeStaged) {
    $msg = "Staged binary not found: $ExePath"
    if ($DryRun) { Write-Warning "$msg — dry-run skips staging; a real run stages it first." }
    else { throw "$msg. Run build-release.ps1 -Roles $Role first (or pass -Stage / -ExePath)." }
} elseif ($Version -ne $exeVer) {
    $msg = "Staged exe is '$exeVer' but requested version is '$Version'"
    if ($DryRun) { Write-Warning "$msg — dry-run skips staging; a real run with -Stage fetches '$Version'." }
    else { throw "$msg — re-run with -Stage to fetch it (or fix -ExePath)." }
}

if ($Role -ne 'backend' -and ($WipeDb -or $JwtSecret -or $StaticToken -or $BootstrapAdminPassword -or $MailPassword)) {
    Write-Warning "-WipeDb / -JwtSecret / -StaticToken / -BootstrapAdminPassword / -MailPassword apply to -Role backend only; ignored for $Role."
}

Write-Host ''
Write-Host "=== fleet-deploy: $Role v$Version ===" -ForegroundColor Cyan
Write-Host "  exe    : $ExePath"
Write-Host "  broker : $Server"
Write-Host "  target : $(if ($All) { if ($Role -eq 'agent') { 'global (fleet-wide rollout)' } else { 'all agents' } } elseif ($Groups) { "groups=$($Groups -join ',')" } else { "pc=$Pc" })"
if ($DryRun) { Write-Host '  (dry-run: no NATS writes, no exec)' -ForegroundColor Yellow }
Write-Host ''

# =========================================================================
# agent: self-update path (publish + rollout) — not a job
# =========================================================================
if ($Role -eq 'agent') {
    Write-Host '--- publish ---'
    Invoke-Kanade @('agent', 'publish', $ExePath) | ForEach-Object { Write-Host "    $_" }

    Write-Host '--- rollout ---'
    # `kanade agent rollout` refuses an implicit scope (a forgotten flag
    # must not fan a release fleet-wide) — map our target flags onto its
    # --global / --group / --pc.
    $rolloutArgs = @('agent', 'rollout', $Version)
    if ($All) { $rolloutArgs += '--global' }
    elseif ($Groups) { $rolloutArgs += @('--group', $Groups[0]) }
    else { $rolloutArgs += @('--pc', $Pc) }
    if ($Jitter) { $rolloutArgs += @('--jitter', $Jitter) }
    elseif (-not $All -and -not $Groups) {
        # Single-host pin: jitter exists to de-synchronise FLEET downloads,
        # so a one-PC rollout gains nothing from waiting — update now. An
        # inherited scope jitter (e.g. 5m from an earlier rollout) would
        # otherwise sit between you and the verify. Explicit -Jitter wins.
        $rolloutArgs += @('--jitter', '0s')
    }
    Invoke-Kanade $rolloutArgs | ForEach-Object { Write-Host "    $_" }

    if (-not $NoVerify -and -not $DryRun) {
        if ($All) {
            # `kanade agent current` reports the GLOBAL target_version, so
            # this check is only meaningful for a --global rollout.
            Write-Host '--- verify (global target_version) ---'
            $cur = & kanade agent current 2>&1
            $cur | ForEach-Object { Write-Host "    $_" }
            if ($cur -match [regex]::Escape($Version)) {
                Write-Host "OK: global target_version = $Version" -ForegroundColor Green
            } else {
                Write-Warning "global target_version does not yet show $Version — check 'kanade agent current'."
            }
        } else {
            Write-Host "Dispatched. 'kanade agent current' only reflects the GLOBAL scope — confirm this pc/group rollout on the SPA Agents page (agent version column)." -ForegroundColor Yellow
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

# Under -DryRun the exe may not be staged (staging is skipped), so the
# hash can't be computed — show a placeholder; a real run always has it.
$sha256 = if (Test-Path $ExePath) { (Get-FileHash $ExePath -Algorithm SHA256).Hash.ToLower() } else { '<sha256-at-runtime>' }
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
    # Inject only the backend secret knobs that were passed; build the
    # `+Flag` suffix incrementally so adding a knob is one line, not a
    # longer nested-`if` string interpolation.
    $extra = [System.Collections.Generic.List[string]]::new()
    if ($WipeDb)                 { $lines = Set-PsAssignment $lines 'AgentWipeDb' '$true' -Raw; $extra.Add('WipeDb') }
    if ($StaticToken)            { $lines = Set-PsAssignment $lines 'AgentStaticToken' $StaticToken; $extra.Add('StaticToken') }
    if ($JwtSecret)              { $lines = Set-PsAssignment $lines 'AgentJwtSecret' $JwtSecret; $extra.Add('JwtSecret') }
    if ($BootstrapAdminPassword) { $lines = Set-PsAssignment $lines 'AgentBootstrapAdminPassword' $BootstrapAdminPassword; $extra.Add('BootstrapAdminPassword') }
    if ($MailPassword)           { $lines = Set-PsAssignment $lines 'AgentMailPassword' $MailPassword; $extra.Add('MailPassword') }
    $suffix = if ($extra.Count) { ' +' + ($extra -join ' +') } else { '' }
    Write-Host "    knobs: $($k.Url)/$($k.Ver)/$($k.Sha)/$($k.Tok)$suffix"
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
if ($All) { $execArgs += '--all' }
elseif ($Groups) { $execArgs += @('--groups', ($Groups -join ',')) }
else { $execArgs += @('--pcs', $Pc) }
Invoke-Kanade $execArgs | ForEach-Object { Write-Host "    $_" }

# 7. verify — poll the locally-installed exe (only meaningful when this box
#    is the target). Fleet-wide / remote targets: check the SPA Inventory.
$targetIsLocal = (-not $Groups) -and (-not $All) -and ($Pc.ToLower() -eq $env:COMPUTERNAME.ToLower())
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
            try { $iv = Get-ExeVersion $spec.InstalledExe }
            catch { $iv = $null }  # transient lock while the service swaps the exe — retry
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

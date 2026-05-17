#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Install / update nats-server as a Windows service.

.DESCRIPTION
  Copies nats-server.exe into %ProgramFiles%\Kanade\, nats-server.conf
  into %ProgramData%\Kanade\config\, ensures the JetStream data
  directory exists under %ProgramData%\Kanade\nats\, hardens the ACL
  on nats-server.conf (token lives there in plaintext per NATS
  conventions), and (re-)registers a Windows service. Re-running
  upgrades the binary in place; existing nats-server.conf is
  preserved unless -ForceConfig is passed.

  nats-server.exe is shipped by NATS, not by kanade. Use
  `build-release.ps1 -Roles nats` to fetch it from
  https://github.com/nats-io/nats-server/releases.

  Firewall: opens TCP 4222 (broker) and 8222 (monitoring HTTP) by
  default. Pass -NoFirewall when an external firewall (corporate /
  WAF / cloud security group) is the source of truth.

.PARAMETER SourceDir
  Directory holding nats-server.exe and nats-server.conf. Defaults
  to the directory this script lives in.

.PARAMETER ServiceName
  Windows service name. Default: KanadeNats.

.PARAMETER ForceConfig
  Overwrite the installed nats-server.conf with the one in
  -SourceDir. Off by default so token edits on the target survive
  upgrades.

.PARAMETER NoFirewall
  Skip the firewall rule(s) even when binding looks public.

.PARAMETER Recreate
  Drop the existing Windows service entirely (sc.exe delete + wait
  for SCM to acknowledge) before re-creating it.

.PARAMETER NoStart
  Install + register the service but don't start it.

.PARAMETER NatsToken
  If set, substitute the broker's `authorization.token` value in
  the installed nats-server.conf with this string. NATS keeps the
  bearer token plaintext in its config file (the broker has no
  registry / DPAPI back-end like kanade-agent), so the script
  rewrites that one line and re-applies the SYSTEM + Administrators-
  only ACL afterwards. Matches the `-NatsToken` flag on
  deploy-agent.ps1 / deploy-backend.ps1 so the operator can run
  the same value on every host.

.EXAMPLE
  PS> .\deploy-nats.ps1
.EXAMPLE
  PS> .\deploy-nats.ps1 -NatsToken 'kanade-fleet-secret-2026'
.EXAMPLE
  PS> .\deploy-nats.ps1 -ForceConfig
.EXAMPLE
  PS> .\deploy-nats.ps1 -NoFirewall
.EXAMPLE
  PS> .\deploy-nats.ps1 -Recreate
#>

[CmdletBinding()]
param(
    [string]$SourceDir   = $PSScriptRoot,
    [string]$ServiceName = 'KanadeNats',
    [switch]$ForceConfig,
    [switch]$NoFirewall,
    [switch]$Recreate,
    [switch]$NoStart,
    [string]$NatsToken   = ''
)

# Rewrite `authorization.token: "..."` inside the installed
# nats-server.conf to the supplied value. Uses [System.IO.File] for
# byte-exact preservation of surrounding whitespace + CRLF; the
# regex replacement double-escapes any `$` in the token so the
# replacement string doesn't misinterpret it as a backref.
function Set-NatsServerToken {
    param(
        [Parameter(Mandatory)][string]$ConfigPath,
        [Parameter(Mandatory)][string]$Token
    )

    $content = [System.IO.File]::ReadAllText($ConfigPath)
    $pattern = '(?ms)(authorization\s*\{[^}]*?token:\s*)"[^"]*"'
    if ($content -notmatch $pattern) {
        throw @"
Couldn't find an `authorization.token` line inside `authorization { ... }` in
  $ConfigPath
to substitute. Either edit the file to include
  authorization { token: "<your-token>" }
or drop -NatsToken (the broker will then run unauthenticated, matching
the shipped sample's commented-out auth block).
"@
    }
    $escaped = $Token -replace '\$', '$$$$'
    $new = $content -replace $pattern, ('$1"' + $escaped + '"')
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($ConfigPath, $new, $utf8NoBom)
}

$ErrorActionPreference = 'Stop'

$binDir    = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot  = Join-Path $env:ProgramData  'Kanade'
$configDir = Join-Path $dataRoot 'config'
$natsDir   = Join-Path $dataRoot 'nats'
$jsDir     = Join-Path $natsDir  'jetstream'
$logsDir   = Join-Path $dataRoot 'logs'

$exeName    = 'nats-server.exe'
$configName = 'nats-server.conf'
$exeSrc     = Join-Path $SourceDir $exeName
$configSrc  = Join-Path $SourceDir $configName
$exeDst     = Join-Path $binDir    $exeName
$configDst  = Join-Path $configDir $configName

if (-not (Test-Path $exeSrc)) {
    throw "Missing '$exeName' in '$SourceDir'. Run `build-release.ps1 -Roles nats` first to populate it."
}
if (-not (Test-Path $configSrc)) {
    throw "Missing '$configName' in '$SourceDir'. Run `build-release.ps1 -Roles nats` first to populate it."
}

$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -ne 'Stopped') {
    Write-Host "Stopping $ServiceName..."
    Stop-Service -Name $ServiceName -Force
    $svc.WaitForStatus('Stopped', '00:00:30')
}

if ($Recreate -and $svc) {
    Write-Host "Removing existing $ServiceName (-Recreate)"
    & sc.exe delete $ServiceName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc.exe delete failed (exit $LASTEXITCODE)" }
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
    }
    if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
        throw "Timed out waiting for $ServiceName to be removed from SCM"
    }
    $svc = $null
}

foreach ($d in @($binDir, $configDir, $natsDir, $jsDir, $logsDir)) {
    if (-not (Test-Path $d)) {
        New-Item -ItemType Directory -Path $d -Force | Out-Null
    }
}

Write-Host "Installing $exeName -> $exeDst"
Copy-Item -Path $exeSrc -Destination $exeDst -Force

if ($ForceConfig -or -not (Test-Path $configDst)) {
    $verb = if (Test-Path $configDst) { 'Overwriting' } else { 'Seeding' }
    Write-Host "$verb $configName -> $configDst"
    Copy-Item -Path $configSrc -Destination $configDst -Force
} else {
    Write-Host "Keeping existing $configDst (pass -ForceConfig to overwrite)."
}

# Apply -NatsToken before the ACL gets locked down. The script runs
# as Admin so we still have write access either way, but doing the
# substitution first keeps the on-disk content right by the time
# the file is readable only by SYSTEM + Administrators.
if ($NatsToken) {
    Write-Host "Substituting authorization.token in $configDst"
    Set-NatsServerToken -ConfigPath $configDst -Token $NatsToken
}

# Harden the ACL on nats-server.conf. The NATS bearer token lives
# inside this file in plaintext (the broker doesn't have a registry
# back-end like kanade-agent / kanade-backend), so any logged-in user
# would otherwise be able to read it. Strip non-admin ACEs the same
# way deploy-{agent,backend}.ps1 do for their hardened registry keys.
#
# icacls.exe (Win32 native) instead of Get-Acl / Set-Acl: those
# cmdlets auto-load `Microsoft.PowerShell.Security`, which fails on
# some elevated pwsh sessions with a CouldNotAutoloadMatchingModule
# error. icacls is a bundled OS exe with no PowerShell-module
# dependency, so it works in every context the deploy script will
# ever run in.
#
# SIDs (instead of "SYSTEM" / "Administrators" names) are used to
# stay locale-agnostic — non-EN Windows installs translate the
# display names, but SID literals always resolve correctly:
#   *S-1-5-18         = NT AUTHORITY\SYSTEM
#   *S-1-5-32-544     = BUILTIN\Administrators
Write-Host "Hardening ACL on $configDst (SYSTEM + Administrators only)"
$icaclsOut = & icacls $configDst /inheritance:r /grant:r '*S-1-5-18:F' /grant:r '*S-1-5-32-544:F' 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "icacls failed on ${configDst} (exit $LASTEXITCODE):`n$icaclsOut"
}

# nats-server.exe detects when it's running under SCM via its own
# StartServiceCtrlDispatcher path; we don't need NSSM or sc.exe wrappers.
$binPath = "`"$exeDst`" --config `"$configDst`""
if (-not $svc) {
    Write-Host "Creating service $ServiceName"
    $null = New-Service `
        -Name           $ServiceName `
        -BinaryPathName $binPath `
        -StartupType    Automatic `
        -DisplayName    'Kanade NATS Broker' `
        -Description    'NATS broker for the kanade endpoint-management fleet (yukimemi/kanade).'
} else {
    Write-Host "Updating $ServiceName configuration"
    Set-Service -Name $ServiceName -StartupType Automatic
}

# Failure recovery — restart on any non-clean-stop exit.
#
#   actions= restart/5000/restart/15000/restart/60000
#     1st failure: wait 5s; 2nd: 15s; 3rd: 60s.
#   reset= 86400
#     Reset failure counter after 24h of clean uptime.
#   failureflag <svc> 1   (= bFailureActionsOnNonCrashFailures)
#     Trigger the actions on ANY non-stop exit, not just crashes.
Write-Host "Configuring failure recovery on $ServiceName"
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failure failed (exit $LASTEXITCODE)" }
& sc.exe failureflag $ServiceName 1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failureflag failed (exit $LASTEXITCODE)" }

if (-not $NoFirewall) {
    foreach ($entry in @(
        @{ Port = 4222; Label = 'broker'     },
        @{ Port = 8222; Label = 'monitoring' }
    )) {
        $ruleName = "$ServiceName ($($entry.Label) TCP $($entry.Port))"
        $existing = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
        if ($existing) {
            Write-Host "Firewall rule '$ruleName' already exists; leaving it alone."
        } else {
            New-NetFirewallRule `
                -DisplayName $ruleName `
                -Direction   Inbound `
                -Protocol    TCP `
                -LocalPort   $entry.Port `
                -Action      Allow `
                -Profile     Any `
                | Out-Null
            Write-Host "Created firewall rule '$ruleName'."
        }
    }
} else {
    Write-Host "Firewall: -NoFirewall set; skipping rules."
}

if (-not $NoStart) {
    Write-Host "Starting $ServiceName"
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus('Running', '00:00:30')
}

Write-Host ''
Write-Host "Installed bin: $exeDst"
Write-Host "Runtime root:  $natsDir"
Write-Host "Config (hardened ACL): $configDst"
& $exeDst --version

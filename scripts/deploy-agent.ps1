#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Install / update kanade-agent as a Windows service.

.DESCRIPTION
  Copies kanade-agent.exe into %ProgramFiles%\Kanade\ and agent.toml
  into %ProgramData%\Kanade\config\, ensures the runtime data / log
  dirs exist under %ProgramData%\Kanade\, and (re-)registers a
  Windows service that runs the agent against the installed config.
  Re-running upgrades the binary in place; existing agent.toml is
  preserved unless -ForceConfig is passed. The split follows the
  Windows convention used in the spec §2.11 layout (README "Production
  install layout"): Program Files is the read-only install root,
  ProgramData holds writable runtime state.

.PARAMETER SourceDir
  Directory holding kanade-agent.exe and agent.toml. Defaults to the
  directory this script lives in, so the common pattern is to drop
  exe + config + this script into one folder and run it.

.PARAMETER ServiceName
  Windows service name. Default: KanadeAgent.

.PARAMETER ForceConfig
  Overwrite the installed agent.toml with the one in -SourceDir. Off
  by default so config tweaks on the target machine survive upgrades.

.PARAMETER Recreate
  Drop the existing Windows service entirely (sc.exe delete + wait
  for SCM to acknowledge) before re-creating it. Useful for
  recovering from a half-installed service (e.g., one that was
  created without windows-service crate integration in v0.3.1 and
  now refuses to start). Implicitly stops the service first.

.PARAMETER NoStart
  Install + register the service but don't start it.

.PARAMETER NatsToken
  If set, write the NATS bearer token to
  HKLM\SOFTWARE\kanade\agent\NatsToken (REG_SZ) and harden the ACL
  on that key so only SYSTEM + Administrators can read it. The
  agent reads this at startup ahead of $env:KANADE_NATS_TOKEN.
  Required when the broker is started with `authorization { token: ... }`.

.EXAMPLE
  # Drop deploy-agent.ps1 + kanade-agent.exe + agent.toml in a folder,
  # then on the target:
  PS> .\deploy-agent.ps1

.EXAMPLE
  # Re-run after a binary update, forcing fresh config:
  PS> .\deploy-agent.ps1 -ForceConfig

.EXAMPLE
  # Provision a fleet-wide NATS token (production):
  PS> .\deploy-agent.ps1 -NatsToken 'kanade-fleet-secret-2026'

.EXAMPLE
  # Recover from a stuck / broken service:
  PS> .\deploy-agent.ps1 -Recreate
#>

[CmdletBinding()]
param(
    [string]$SourceDir   = $PSScriptRoot,
    [string]$ServiceName = 'KanadeAgent',
    [switch]$ForceConfig,
    [switch]$Recreate,
    [switch]$NoStart,
    [string]$NatsToken   = ''
)

$ErrorActionPreference = 'Stop'

# Provision the NATS bearer token under HKLM\SOFTWARE\kanade\agent and
# strip all non-admin ACEs from the key so a logged-in low-priv user
# can't `Get-ItemProperty` the secret. This is the production path
# read by kanade-shared::nats_client::connect(); $env:KANADE_NATS_TOKEN
# is dev-only fallback.
function Set-KanadeNatsToken {
    param([Parameter(Mandatory)][string]$Token)

    $regKey = 'HKLM:\SOFTWARE\kanade\agent'
    if (-not (Test-Path $regKey)) {
        New-Item -Path $regKey -Force | Out-Null
    }
    Set-ItemProperty -Path $regKey -Name 'NatsToken' -Value $Token -Type String

    $acl = Get-Acl -Path $regKey
    $acl.SetAccessRuleProtection($true, $false)
    @($acl.Access) | ForEach-Object { [void]$acl.RemoveAccessRule($_) }
    foreach ($id in 'NT AUTHORITY\SYSTEM', 'BUILTIN\Administrators') {
        $rule = New-Object System.Security.AccessControl.RegistryAccessRule(
            $id, 'FullControl', 'ContainerInherit', 'None', 'Allow')
        $acl.AddAccessRule($rule)
    }
    Set-Acl -Path $regKey -AclObject $acl

    Write-Host "Wrote NatsToken to $regKey (SYSTEM + Administrators only)."
}

$binDir    = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot  = Join-Path $env:ProgramData  'Kanade'
$configDir = Join-Path $dataRoot 'config'
$dataDir   = Join-Path $dataRoot 'data'
$logsDir   = Join-Path $dataRoot 'logs'

$exeName    = 'kanade-agent.exe'
$configName = 'agent.toml'
$exeSrc     = Join-Path $SourceDir $exeName
$configSrc  = Join-Path $SourceDir $configName
$exeDst     = Join-Path $binDir    $exeName
$configDst  = Join-Path $configDir $configName

if (-not (Test-Path $exeSrc)) {
    throw "Missing '$exeName' in '$SourceDir'. Place the release binary next to this script or pass -SourceDir."
}
if (-not (Test-Path $configSrc)) {
    throw "Missing '$configName' in '$SourceDir'. Place the sample config next to this script or pass -SourceDir."
}

# Stop the existing service first so the on-disk exe isn't locked.
$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -ne 'Stopped') {
    Write-Host "Stopping $ServiceName..."
    Stop-Service -Name $ServiceName -Force
    $svc.WaitForStatus('Stopped', '00:00:30')
}

# -Recreate: drop the existing service entirely so the New-Service
# path below runs even when a previous (broken) registration is
# already there. Useful for recovering from a stuck "service exists
# but won't start" state — Stop-Service + sc.exe delete + wait for
# SCM to actually remove the entry before continuing.
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

foreach ($d in @($binDir, $configDir, $dataDir, $logsDir)) {
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

if ($NatsToken) {
    Set-KanadeNatsToken -Token $NatsToken
}

# Service binPath = quoted exe + --config flag pointing at the
# installed toml. New-Service handles the embedded-quote +
# space-in-path mess that `sc.exe create` chokes on (exit 1639
# = ERROR_INVALID_COMMAND_LINE) when called through PowerShell
# without the --% stop-parsing token.
$binPath = "`"$exeDst`" --config `"$configDst`""
if (-not $svc) {
    Write-Host "Creating service $ServiceName"
    $null = New-Service `
        -Name           $ServiceName `
        -BinaryPathName $binPath `
        -StartupType    Automatic `
        -DisplayName    'Kanade Agent' `
        -Description    'Kanade endpoint management agent (yukimemi/kanade).'
} else {
    Write-Host "Updating $ServiceName configuration"
    # Existing service: binPath rarely needs changing (the exe path
    # is stable across upgrades since we always write to
    # %ProgramFiles%\Kanade\kanade-agent.exe), so only reconfirm
    # the start type. Operator with a custom binPath can adjust
    # manually via `sc.exe config`.
    Set-Service -Name $ServiceName -StartupType Automatic
}

# Failure recovery — restart on any non-clean-stop exit.
#
# This is what makes the agent's self-update path work: when
# self_update.rs swaps the binary into place it calls
# std::process::exit(64), and SCM has to interpret that exit as a
# recoverable failure for the configured restart action to fire.
#
#   actions= restart/5000/restart/15000/restart/60000
#     1st failure: wait 5s then restart
#     2nd failure: 15s
#     3rd failure: 60s
#   reset= 86400
#     Reset the failure counter after 24h of clean uptime.
#   failureflag <svc> 1   (= bFailureActionsOnNonCrashFailures)
#     Trigger the actions on ANY non-stop exit, not just crashes.
#     Without this, exit(64) is silently treated as a normal exit
#     and the service stays stopped after self-update.
Write-Host "Configuring failure recovery on $ServiceName"
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failure failed (exit $LASTEXITCODE)" }
& sc.exe failureflag $ServiceName 1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failureflag failed (exit $LASTEXITCODE)" }

if (-not $NoStart) {
    Write-Host "Starting $ServiceName"
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus('Running', '00:00:30')
}

Write-Host ''
Write-Host "Installed bin: $exeDst"
Write-Host "Runtime root:  $dataRoot"
& $exeDst --version

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

.PARAMETER NoStart
  Install + register the service but don't start it.

.EXAMPLE
  # Drop deploy-agent.ps1 + kanade-agent.exe + agent.toml in a folder,
  # then on the target:
  PS> .\deploy-agent.ps1

.EXAMPLE
  # Re-run after a binary update, forcing fresh config:
  PS> .\deploy-agent.ps1 -ForceConfig
#>

[CmdletBinding()]
param(
    [string]$SourceDir   = $PSScriptRoot,
    [string]$ServiceName = 'KanadeAgent',
    [switch]$ForceConfig,
    [switch]$NoStart
)

$ErrorActionPreference = 'Stop'

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

$binPath = "`"$exeDst`" --config `"$configDst`""
if (-not $svc) {
    Write-Host "Creating service $ServiceName"
    & sc.exe create $ServiceName binPath= $binPath start= auto DisplayName= 'Kanade Agent' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc.exe create failed (exit $LASTEXITCODE)" }
    & sc.exe description $ServiceName 'Kanade endpoint management agent (yukimemi/kanade).' | Out-Null
} else {
    Write-Host "Updating service binPath for $ServiceName"
    & sc.exe config $ServiceName binPath= $binPath start= auto | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc.exe config failed (exit $LASTEXITCODE)" }
}

if (-not $NoStart) {
    Write-Host "Starting $ServiceName"
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus('Running', '00:00:30')
}

Write-Host ''
Write-Host "Installed bin: $exeDst"
Write-Host "Runtime root:  $dataRoot"
& $exeDst --version

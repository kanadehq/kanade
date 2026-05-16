#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Install / update kanade-backend as a Windows service.

.DESCRIPTION
  Copies kanade-backend.exe into %ProgramFiles%\Kanade\ and
  backend.toml into %ProgramData%\Kanade\config\, ensures the
  runtime data / log dirs exist under %ProgramData%\Kanade\, and
  (re-)registers a Windows service that runs the backend against
  the installed config. Re-running upgrades the binary in place;
  existing backend.toml is preserved unless -ForceConfig is passed.
  The split follows the Windows convention used in the spec §2.11
  layout (README "Production install layout"): Program Files is the
  read-only install root, ProgramData holds writable runtime state.

  Pass -FirewallPort <int> to also open an inbound TCP rule with
  New-NetFirewallRule. The backend's HTTP bind port is set in
  backend.toml, so the value must match whatever you configured
  there (default 8443).

.PARAMETER SourceDir
  Directory holding kanade-backend.exe and backend.toml. Defaults to
  the directory this script lives in.

.PARAMETER ServiceName
  Windows service name. Default: KanadeBackend.

.PARAMETER ForceConfig
  Overwrite the installed backend.toml with the one in -SourceDir.
  Off by default so config tweaks on the target machine survive
  upgrades.

.PARAMETER FirewallPort
  If set, add a New-NetFirewallRule for TCP inbound on this port.
  Match it to the bind_addr in backend.toml.

.PARAMETER NoStart
  Install + register the service but don't start it.

.EXAMPLE
  PS> .\deploy-backend.ps1 -FirewallPort 8443

.EXAMPLE
  # Re-run after a binary update, forcing fresh config:
  PS> .\deploy-backend.ps1 -ForceConfig
#>

[CmdletBinding()]
param(
    [string]$SourceDir    = $PSScriptRoot,
    [string]$ServiceName  = 'KanadeBackend',
    [switch]$ForceConfig,
    [int]   $FirewallPort = 0,
    [switch]$NoStart
)

$ErrorActionPreference = 'Stop'

$binDir    = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot  = Join-Path $env:ProgramData  'Kanade'
$configDir = Join-Path $dataRoot 'config'
$dataDir   = Join-Path $dataRoot 'data'
$logsDir   = Join-Path $dataRoot 'logs'

$exeName    = 'kanade-backend.exe'
$configName = 'backend.toml'
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
    & sc.exe create $ServiceName binPath= $binPath start= auto DisplayName= 'Kanade Backend' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc.exe create failed (exit $LASTEXITCODE)" }
    & sc.exe description $ServiceName 'Kanade backend / projector / HTTP admin API (yukimemi/kanade).' | Out-Null
} else {
    Write-Host "Updating service binPath for $ServiceName"
    & sc.exe config $ServiceName binPath= $binPath start= auto | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc.exe config failed (exit $LASTEXITCODE)" }
}

if ($FirewallPort -gt 0) {
    $ruleName = "$ServiceName (TCP $FirewallPort)"
    $existing = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "Firewall rule '$ruleName' already exists; leaving it alone."
    } else {
        Write-Host "Opening inbound TCP $FirewallPort"
        New-NetFirewallRule `
            -DisplayName $ruleName `
            -Direction   Inbound `
            -Protocol    TCP `
            -LocalPort   $FirewallPort `
            -Action      Allow `
            -Profile     Any `
            | Out-Null
    }
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

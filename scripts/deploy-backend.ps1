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

  Firewall: the installed backend.toml is parsed for its `[server]
  bind` line. If the host part isn't loopback (127.0.0.1 / ::1 /
  localhost), the script opens that port via New-NetFirewallRule.
  Pass -FirewallPort <int> to override the parsed port (e.g. when
  there's a reverse proxy listening on a different external port),
  or -NoFirewall to suppress the rule altogether (managed firewall,
  separate WAF, etc.).

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
  Open this TCP port via New-NetFirewallRule, overriding the port
  parsed out of backend.toml's `[server] bind`. Useful when an
  external reverse proxy listens on a different port than the
  backend itself.

.PARAMETER NoFirewall
  Skip the firewall rule even when backend.toml's bind looks
  public. Use this when an external firewall (corporate / WAF /
  cloud security group) is the source of truth.

.PARAMETER NoStart
  Install + register the service but don't start it.

.EXAMPLE
  PS> .\deploy-backend.ps1                            # opens whatever backend.toml binds to

.EXAMPLE
  PS> .\deploy-backend.ps1 -FirewallPort 8443         # override the parsed port

.EXAMPLE
  PS> .\deploy-backend.ps1 -NoFirewall                # external firewall handles ingress

.EXAMPLE
  PS> .\deploy-backend.ps1 -ForceConfig               # re-run after binary update, fresh config
#>

[CmdletBinding()]
param(
    [string]$SourceDir    = $PSScriptRoot,
    [string]$ServiceName  = 'KanadeBackend',
    [switch]$ForceConfig,
    [int]   $FirewallPort = 0,
    [switch]$NoFirewall,
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

# Failure recovery — restart on any non-clean-stop exit.
#
# Backend doesn't self-update (deploy-backend.ps1 is the manual update
# path), so we don't strictly need the `failureflag 1` bit for an
# exit(64) handshake — but it's still cheap insurance: a panic that
# unwinds out of #[tokio::main] returns a non-zero exit code, and
# without failureflag SCM treats that as "service stopped" and leaves
# the projector/API offline until the operator notices. With it, SCM
# applies the restart actions just like a real crash.
#
#   actions= restart/5000/restart/15000/restart/60000
#     1st failure: wait 5s; 2nd: 15s; 3rd: 60s.
#   reset= 86400
#     Reset failure counter after 24h of clean uptime.
Write-Host "Configuring failure recovery on $ServiceName"
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failure failed (exit $LASTEXITCODE)" }
& sc.exe failureflag $ServiceName 1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failureflag failed (exit $LASTEXITCODE)" }

# Decide which port to open (if any). Priority:
#   1. Explicit -FirewallPort wins outright.
#   2. -NoFirewall short-circuits to "nothing".
#   3. Parse backend.toml's `[server] bind` and, if the host part
#      isn't loopback, open the port it carries.
#
# Loopback bind (127.0.0.1 / ::1 / localhost) means same-machine
# only — no firewall rule needed, and adding one would just be
# noise in the audit log.
$portToOpen = 0
if ($FirewallPort -gt 0) {
    $portToOpen = $FirewallPort
    Write-Host "Firewall: opening inbound TCP $portToOpen (explicit -FirewallPort)."
} elseif (-not $NoFirewall) {
    $bindLine = Select-String -Path $configDst -Pattern "^bind\s*=\s*['""]([^'""]+)['""]" |
                Select-Object -First 1
    if ($bindLine) {
        $bindAddr    = $bindLine.Matches[0].Groups[1].Value
        # IPv6 form is [::]:8080 — strip the brackets so the final
        # `:port` split is unambiguous.
        $unbracketed = $bindAddr -replace '^\[([^\]]+)\]', '$1'
        $lastColon   = $unbracketed.LastIndexOf(':')
        if ($lastColon -gt 0) {
            $bindHost  = $unbracketed.Substring(0, $lastColon)
            $bindPort  = $unbracketed.Substring($lastColon + 1) -as [int]
            $loopback  = @('127.0.0.1', '::1', 'localhost')
            if ($bindPort -and ($loopback -notcontains $bindHost)) {
                $portToOpen = $bindPort
                Write-Host "Firewall: bind '$bindAddr' is public; opening inbound TCP $portToOpen (pass -NoFirewall to skip)."
            } else {
                Write-Host "Firewall: bind '$bindAddr' is loopback; no rule needed."
            }
        } else {
            Write-Host "Firewall: couldn't parse a port out of bind '$bindAddr'; skipping rule. Pass -FirewallPort to open one explicitly."
        }
    } else {
        Write-Host "Firewall: no `[server] bind` line found in $configDst; skipping rule. Pass -FirewallPort to open one explicitly."
    }
} else {
    Write-Host "Firewall: -NoFirewall set; skipping rule."
}

if ($portToOpen -gt 0) {
    $ruleName = "$ServiceName (TCP $portToOpen)"
    $existing = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "Firewall rule '$ruleName' already exists; leaving it alone."
    } else {
        New-NetFirewallRule `
            -DisplayName $ruleName `
            -Direction   Inbound `
            -Protocol    TCP `
            -LocalPort   $portToOpen `
            -Action      Allow `
            -Profile     Any `
            | Out-Null
        Write-Host "Created firewall rule '$ruleName'."
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

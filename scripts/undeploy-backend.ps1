#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Remove kanade-backend from this PC.

.DESCRIPTION
  Counterpart to deploy-backend.ps1. Stops the Windows service,
  unregisters it from SCM, removes the installed binary, the
  firewall rule the deploy script opened, and optionally purges
  the runtime data / config / logs under %ProgramData%\Kanade\ +
  the registry-stored bearer secrets.

  ⚠️  -Purge removes the projector's SQLite database. Once gone,
  the historical results / heartbeats / inventory rows are
  unrecoverable unless you've backed up the .db file out-of-band.
  Default posture is safe: data + config + logs survive.

  Every step is idempotent — safe to re-run after a partial
  uninstall.

.PARAMETER ServiceName
  Windows service name. Default: KanadeBackend. Must match what
  deploy-backend.ps1 installed.

.PARAMETER Purge
  Also remove %ProgramData%\Kanade\ entries owned by the backend
  (config\backend.toml, data\*.db, logs\backend.*.log). Implies
  -RemoveSecrets unless -KeepSecrets is also passed.

.PARAMETER KeepSecrets
  Don't touch HKLM\SOFTWARE\kanade\backend\* even when -Purge is
  set. Use when the same StaticToken / JwtSecret is shared across
  components on the host (e.g. operator re-uses the token for the
  agent's NATS auth) — without this, -Purge would orphan them.

.PARAMETER KeepFirewall
  Skip the firewall cleanup step. By default removes every
  inbound rule whose DisplayName matches "$ServiceName (TCP *)"
  (the pattern deploy-backend.ps1 uses). Pass this when an
  external firewall is the source of truth and the rule was
  managed elsewhere.

.EXAMPLE
  PS> .\undeploy-backend.ps1
  # Stop + unregister + remove binary + remove firewall rule.
  # Preserve ProgramData (SQLite, config, logs) and registry secrets.

.EXAMPLE
  PS> .\undeploy-backend.ps1 -Purge
  # Full removal including the projector SQLite — **historical
  # results / heartbeats / inventory are wiped**. Make sure
  # backups exist.

.EXAMPLE
  PS> .\undeploy-backend.ps1 -KeepFirewall
  # Useful when host firewall is managed by Group Policy / WAF
  # and the rule we'd remove is actually a more general one with
  # a coincidentally-matching name.
#>

[CmdletBinding()]
param(
    [string]$ServiceName = 'KanadeBackend',
    [switch]$Purge,
    [switch]$KeepSecrets,
    [switch]$KeepFirewall
)

$ErrorActionPreference = 'Stop'

$binDir   = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot = Join-Path $env:ProgramData  'Kanade'
$exeName  = 'kanade-backend.exe'
$exeDst   = Join-Path $binDir $exeName

# --- Stop service ------------------------------------------------------------
$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($svc) {
    if ($svc.Status -ne 'Stopped') {
        Write-Host "Stopping $ServiceName..."
        Stop-Service -Name $ServiceName -Force
        $svc.WaitForStatus('Stopped', '00:00:30')
    } else {
        Write-Host "$ServiceName already stopped"
    }
} else {
    Write-Host "$ServiceName not present, skipping stop"
}

# --- Unregister from SCM -----------------------------------------------------
if ($svc) {
    Write-Host "Removing $ServiceName from SCM"
    & sc.exe delete $ServiceName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc.exe delete failed (exit $LASTEXITCODE)" }
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
    }
    if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
        throw "Timed out waiting for $ServiceName to be removed from SCM"
    }
}

# --- Remove binary -----------------------------------------------------------
if (Test-Path $exeDst) {
    Write-Host "Removing $exeDst"
    Remove-Item -Path $exeDst -Force
    $stale = @("$exeDst.new", "$exeDst.old") | Where-Object { Test-Path $_ }
    foreach ($s in $stale) {
        Write-Host "Removing stale $s"
        Remove-Item -Path $s -Force
    }
} else {
    Write-Host "$exeDst not present, skipping binary removal"
}

# --- Firewall ----------------------------------------------------------------
if (-not $KeepFirewall) {
    # Match the pattern deploy-backend.ps1 creates: "<ServiceName>
    # (TCP <port>)". Wildcarding the port handles operators who
    # passed -FirewallPort to override the value parsed from
    # backend.toml.
    $pattern = "$ServiceName (TCP *)"
    $rules = Get-NetFirewallRule -DisplayName $pattern -ErrorAction SilentlyContinue
    if ($rules) {
        foreach ($r in $rules) {
            Write-Host "Removing firewall rule '$($r.DisplayName)'"
            Remove-NetFirewallRule -DisplayName $r.DisplayName -ErrorAction SilentlyContinue
        }
    } else {
        Write-Host "No firewall rules matching '$pattern' found, skipping"
    }
} else {
    Write-Host "Keeping firewall rules (-KeepFirewall)"
}

# --- Registry secrets --------------------------------------------------------
$removeSecrets = $Purge -and -not $KeepSecrets
if ($removeSecrets) {
    $backendKey = 'HKLM:\SOFTWARE\kanade\backend'
    if (Test-Path $backendKey) {
        Write-Host "Removing $backendKey (contains StaticToken / JwtSecret)"
        Remove-Item -Path $backendKey -Recurse -Force
    } else {
        Write-Host "$backendKey not present, skipping registry cleanup"
    }
    $rootKey = 'HKLM:\SOFTWARE\kanade'
    if ((Test-Path $rootKey) -and -not (Get-ChildItem -Path $rootKey -ErrorAction SilentlyContinue)) {
        Write-Host "Removing empty $rootKey"
        Remove-Item -Path $rootKey -Force
    }
} else {
    if ($Purge) {
        Write-Host "Keeping HKLM:\SOFTWARE\kanade\backend (-KeepSecrets)"
    }
}

# --- Purge ProgramData -------------------------------------------------------
if ($Purge) {
    if (Test-Path $dataRoot) {
        # Backend-exclusive paths only — agent / nats may share
        # the root and their files mustn't be touched here.
        # config\backend.toml          → operator config (unrecoverable if not in backups)
        # data\*.db / *.db-shm / *.db-wal → projector SQLite (wipes history)
        # logs\backend.*.log           → log files
        $backendPaths = @(
            (Join-Path $dataRoot 'config\backend.toml')
        )
        $dataDir = Join-Path $dataRoot 'data'
        if (Test-Path $dataDir) {
            $backendPaths += Get-ChildItem -Path $dataDir -Include '*.db', '*.db-shm', '*.db-wal' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName
        }
        $logsDir = Join-Path $dataRoot 'logs'
        if (Test-Path $logsDir) {
            $backendPaths += Get-ChildItem -Path $logsDir -Filter 'backend*.log' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName
        }
        $backendPaths = $backendPaths | Where-Object { $_ -and (Test-Path $_) }
        foreach ($p in $backendPaths) {
            Write-Host "Purging $p"
            Remove-Item -Path $p -Recurse -Force -ErrorAction SilentlyContinue
        }
    } else {
        Write-Host "$dataRoot not present, nothing to purge"
    }
} else {
    Write-Host "Keeping $dataRoot (pass -Purge to remove backend.toml / SQLite / logs)"
}

Write-Host ''
Write-Host "undeploy-backend: done."
if (-not $Purge) {
    Write-Host "  (re-run with -Purge to also remove SQLite + config + registry secrets — destructive)"
}

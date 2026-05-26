#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Remove the kanade-bundled NATS server from this PC.

.DESCRIPTION
  Counterpart to deploy-nats.ps1. Stops the Windows service,
  unregisters it from SCM, removes the installed binary, and
  removes the broker (4222) + monitoring (8222) firewall rules
  the deploy script opened.

  ⚠️⚠️  -Purge removes the JetStream data directory
  (%ProgramData%\Kanade\nats\jetstream\). That holds **every KV
  bucket, every Object Store blob, and every stream** for the
  fleet — agent_config, jobs, results, agent_releases,
  app_packages, scripts. Losing this without backups means
  rebuilding the fleet state from scratch (re-publish all agent
  binaries, re-create every job, lose all historical results).
  Default posture is paranoid-safe: data + config survive.

  Re-running deploy-nats.ps1 against the same host AFTER this
  script (without -Purge) brings the broker back up with the
  existing JetStream state intact — a clean rollback path.

  Every step is idempotent — safe to re-run.

.PARAMETER ServiceName
  Windows service name. Default: KanadeNats. Must match what
  deploy-nats.ps1 installed.

.PARAMETER Purge
  ⚠️  Also remove %ProgramData%\Kanade\ entries owned by NATS:
  config\nats-server.conf, nats\* (including jetstream\). Wipes
  the entire fleet's persisted state. Use only when
  decommissioning the broker permanently AND you've backed up
  jetstream\ out-of-band (or you genuinely don't care about
  losing the state).

.PARAMETER KeepFirewall
  Skip the firewall cleanup step. By default removes inbound
  rules matching "$ServiceName (*)" — the pattern
  deploy-nats.ps1 uses for both broker and monitoring ports.

.EXAMPLE
  PS> .\undeploy-nats.ps1
  # Stop + unregister + remove binary + remove firewall rules.
  # Keeps nats-server.conf + jetstream\ for forensics / rollback.
  # A subsequent deploy-nats.ps1 picks the existing state back up.

.EXAMPLE
  PS> .\undeploy-nats.ps1 -Purge
  # ⚠️  Wipes JetStream data. Use only when decommissioning
  # the broker permanently. ALL FLEET STATE IS LOST.

.EXAMPLE
  PS> .\undeploy-nats.ps1 -KeepFirewall
  # Useful when corporate firewall / WAF manages 4222/8222
  # externally and the host rules were never actually in effect.
#>

[CmdletBinding()]
param(
    [string]$ServiceName = 'KanadeNats',
    [switch]$Purge,
    [switch]$KeepFirewall
)

$ErrorActionPreference = 'Stop'

$binDir   = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot = Join-Path $env:ProgramData  'Kanade'
$exeName  = 'nats-server.exe'
$exeDst   = Join-Path $binDir $exeName

# --- Loud warning before any destructive action ------------------------------
if ($Purge) {
    Write-Host ''
    Write-Host '==============================================================='
    Write-Host '  -Purge will REMOVE THE JETSTREAM DATA DIRECTORY.'
    Write-Host '  All KV buckets, Object Store blobs, and streams will be lost.'
    Write-Host '  Make sure you have backups of:'
    Write-Host "    $(Join-Path $dataRoot 'nats\jetstream')"
    Write-Host '  …if you want fleet state to survive.'
    Write-Host '==============================================================='
    Write-Host ''
}

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
} else {
    Write-Host "$exeDst not present, skipping binary removal"
}

# --- Firewall ----------------------------------------------------------------
if (-not $KeepFirewall) {
    # deploy-nats.ps1 creates "KanadeNats (broker TCP 4222)" and
    # "KanadeNats (monitoring TCP 8222)". Wildcard the suffix so
    # both go in one sweep.
    $pattern = "$ServiceName (*)"
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

# --- Purge ProgramData -------------------------------------------------------
if ($Purge) {
    if (Test-Path $dataRoot) {
        # NATS-exclusive paths only.
        $natsPaths = @(
            (Join-Path $dataRoot 'config\nats-server.conf'),
            (Join-Path $dataRoot 'nats')
        )
        $logsDir = Join-Path $dataRoot 'logs'
        if (Test-Path $logsDir) {
            $natsPaths += Get-ChildItem -Path $logsDir -Filter 'nats*.log' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName
        }
        $natsPaths = $natsPaths | Where-Object { $_ -and (Test-Path $_) }
        foreach ($p in $natsPaths) {
            Write-Host "Purging $p"
            Remove-Item -Path $p -Recurse -Force -ErrorAction SilentlyContinue
        }
    } else {
        Write-Host "$dataRoot not present, nothing to purge"
    }
} else {
    Write-Host "Keeping $dataRoot (pass -Purge to nuke jetstream\ + config)"
}

Write-Host ''
Write-Host "undeploy-nats: done."
if (-not $Purge) {
    Write-Host "  (re-run deploy-nats.ps1 to bring the broker back with existing JetStream state)"
}

#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Remove kanade-agent from this PC.

.DESCRIPTION
  Counterpart to deploy-agent.ps1. Stops the Windows service,
  unregisters it from SCM, removes the installed binary, and
  optionally purges the runtime data / config / logs under
  %ProgramData%\Kanade\ + the bearer-token secret under HKLM.

  Default posture is **safe**: data, config, and logs survive so
  the host can be re-deployed with `deploy-agent.ps1` later or
  forensics can be done after a bad rollout. Pass -Purge to nuke
  everything; pass -KeepSecrets if you specifically want the
  registry-stored NATS token to survive.

  Every step is idempotent: if the service is already stopped /
  the binary already removed / the registry key already gone, the
  script logs "not present, skipping" and moves on. Safe to re-
  run after a partial uninstall.

.PARAMETER ServiceName
  Windows service name. Default: KanadeAgent. Must match what
  deploy-agent.ps1 installed.

.PARAMETER Purge
  Also remove %ProgramData%\Kanade\ (config, data dir, logs,
  outbox). Implies -RemoveSecrets unless -KeepSecrets is also
  passed. Required for a clean "the agent was never here" state.

.PARAMETER KeepSecrets
  Don't touch HKLM\SOFTWARE\kanade\agent\* even when -Purge is
  set. Use when the same NATS token is shared across services on
  the host (e.g. backend reads the same token) — without this,
  -Purge would orphan the backend's NATS auth.

.PARAMETER KeepFirewall
  Skip the firewall cleanup step. deploy-agent.ps1 doesn't
  currently open any inbound ports for the agent (agent is
  outbound-only against the broker), so this is a no-op today —
  reserved for symmetry with the backend / NATS scripts and for
  any future firewall opens the agent grows.

.EXAMPLE
  PS> .\undeploy-agent.ps1
  # Stop + unregister + remove binary. Preserve ProgramData and
  # the registry-stored NATS token. Safe default for "this PC is
  # behaving badly, get the agent off it but don't destroy state".

.EXAMPLE
  PS> .\undeploy-agent.ps1 -Purge
  # Full removal: service + binary + ProgramData + registry secret.
  # Use when decommissioning the host or wiping for a fresh install.

.EXAMPLE
  PS> .\undeploy-agent.ps1 -Purge -KeepSecrets
  # Wipe ProgramData but keep the NATS token (e.g. backend on the
  # same host still needs it).
#>

[CmdletBinding()]
param(
    [string]$ServiceName = 'KanadeAgent',
    [switch]$Purge,
    [switch]$KeepSecrets,
    [switch]$KeepFirewall
)

$ErrorActionPreference = 'Stop'

$binDir   = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot = Join-Path $env:ProgramData  'Kanade'
$exeName  = 'kanade-agent.exe'
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
    # SCM removes the entry asynchronously — wait until Get-Service
    # confirms it's gone, otherwise a follow-up `deploy-agent.ps1
    # -Recreate` could race the pending deletion.
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
    # Self_update may have left an <exe>.new mid-swap. Sweep too.
    $stale = @("$exeDst.new", "$exeDst.old") | Where-Object { Test-Path $_ }
    foreach ($s in $stale) {
        Write-Host "Removing stale $s"
        Remove-Item -Path $s -Force
    }
    # If $binDir is empty after removal, leave it — the directory
    # itself is owned by Program Files conventions and harmless.
} else {
    Write-Host "$exeDst not present, skipping binary removal"
}

# --- Registry secrets --------------------------------------------------------
$removeSecrets = $Purge -and -not $KeepSecrets
if ($removeSecrets) {
    $agentKey = 'HKLM:\SOFTWARE\kanade\agent'
    if (Test-Path $agentKey) {
        Write-Host "Removing $agentKey (contains NatsToken)"
        Remove-Item -Path $agentKey -Recurse -Force
    } else {
        Write-Host "$agentKey not present, skipping registry cleanup"
    }
    # Sweep an empty parent — if backend / nats subkeys also gone,
    # the kanade root key serves no purpose.
    $rootKey = 'HKLM:\SOFTWARE\kanade'
    if ((Test-Path $rootKey) -and -not (Get-ChildItem -Path $rootKey -ErrorAction SilentlyContinue)) {
        Write-Host "Removing empty $rootKey"
        Remove-Item -Path $rootKey -Force
    }
} else {
    if ($Purge) {
        Write-Host "Keeping HKLM:\SOFTWARE\kanade\agent (-KeepSecrets)"
    }
}

# --- Firewall (no-op today, kept for symmetry) -------------------------------
if (-not $KeepFirewall) {
    # Reserved for future inbound rules. Agent is outbound-only
    # (NATS connect to broker) so deploy-agent.ps1 doesn't create
    # firewall entries — nothing to undo. The structural symmetry
    # vs undeploy-backend / undeploy-nats stays so operators can
    # use the same flag muscle-memory across all three.
}

# --- Purge ProgramData -------------------------------------------------------
if ($Purge) {
    if (Test-Path $dataRoot) {
        # Be precise about WHICH subdirs to remove — operators may
        # have backend.toml / nats config in the same root and we
        # mustn't take those out from under another component.
        # The agent's exclusive subdirs are: config\agent.toml,
        # logs\agent.*.log, outbox\*.
        $agentPaths = @(
            (Join-Path $dataRoot 'config\agent.toml'),
            (Join-Path $dataRoot 'logs')   | ForEach-Object { Get-ChildItem -Path $_ -Filter 'agent*.log' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty FullName },
            (Join-Path $dataRoot 'outbox')
        ) | Where-Object { $_ -and (Test-Path $_) }
        foreach ($p in $agentPaths) {
            Write-Host "Purging $p"
            Remove-Item -Path $p -Recurse -Force -ErrorAction SilentlyContinue
        }
    } else {
        Write-Host "$dataRoot not present, nothing to purge"
    }
} else {
    Write-Host "Keeping $dataRoot (pass -Purge to remove agent.toml / logs / outbox)"
}

Write-Host ""
Write-Host "undeploy-agent: done."
if (-not $Purge) {
    Write-Host "  (re-run with -Purge to also remove ProgramData + registry secret)"
}

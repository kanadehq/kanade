#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Remove kanade-client (Tauri app) from this PC.

.DESCRIPTION
  Counterpart to install-kanade-client.ps1 (the agent-driven
  install). Removes the installed binary plus any
  half-completed swap artefacts (.new / .old) left by an aborted
  install.

  The client is just a binary launched by the user — there's no
  Windows service, no firewall rule, no registry secrets. So
  undeploy-client is structurally simpler than the other three
  undeploy-* scripts. -Purge is currently a placeholder: it keeps
  per-user state (including persisted window size), protocol registration
  and shortcuts; this script removes only the binary and swap artifacts.

  Idempotent — safe to re-run.

.PARAMETER Purge
  Reserved. The flag is accepted for shape-symmetry with the other
  undeploy-* scripts, but per-user state is not currently removed.

.EXAMPLE
  PS> .\undeploy-client.ps1
  # Remove the installed kanade-client.exe + any stale swap files.

.EXAMPLE
  PS> .\undeploy-client.ps1 -Purge
  # Same binary-only cleanup as the default; per-user state is kept.
#>

[CmdletBinding()]
param(
    [switch]$Purge
)

$ErrorActionPreference = 'Stop'

$binDir  = Join-Path $env:ProgramFiles 'Kanade'
$exeName = 'kanade-client.exe'
$exeDst  = Join-Path $binDir $exeName

# --- Remove binary + swap artefacts ------------------------------------------
$removed = 0
foreach ($p in @($exeDst, "$exeDst.new", "$exeDst.old")) {
    if (Test-Path $p) {
        Write-Host "Removing $p"
        Remove-Item -Path $p -Force
        $removed++
    }
}
if ($removed -eq 0) {
    Write-Host "$exeDst not present (and no swap artefacts), nothing to remove"
}

# --- Per-user state ----------------------------------------------------------
if ($Purge) {
    # Placeholder: no per-user cleanup is implemented here.
    Write-Host "-Purge: per-user state is kept (cleanup not implemented)"
}

Write-Host ''
Write-Host "undeploy-client: done."

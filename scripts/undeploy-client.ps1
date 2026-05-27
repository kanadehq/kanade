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
  undeploy-* scripts and -Purge today is a near-no-op (no
  per-user state currently lives outside the binary itself; will
  grow if the client starts persisting settings under %APPDATA%).

  Idempotent — safe to re-run.

.PARAMETER Purge
  Reserved. Today the client has no per-user state to purge; the
  flag is accepted (for shape-symmetry with the other undeploy-*
  scripts and for forward-compat once kanade-client gains a
  settings directory under %APPDATA% or similar).

.EXAMPLE
  PS> .\undeploy-client.ps1
  # Remove the installed kanade-client.exe + any stale swap files.

.EXAMPLE
  PS> .\undeploy-client.ps1 -Purge
  # Same as default today. Will purge per-user settings once the
  # client gains a settings dir (none yet).
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
    # Placeholder: client currently has no per-user settings dir.
    # Once it grows one (e.g. %APPDATA%\kanade-client\settings.toml),
    # iterate user profiles here and remove the per-user state.
    Write-Host "-Purge: no per-user state to remove yet (placeholder)"
}

Write-Host ''
Write-Host "undeploy-client: done."

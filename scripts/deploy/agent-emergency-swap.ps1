#Requires -Version 5.1

<#
.SYNOPSIS
  Emergency out-of-band swap of the kanade-agent binary — the recovery
  path for when the agent's own self-update is wedged.

.DESCRIPTION
  The normal agent upgrade path is `kanade agent rollout`, which sets a
  `target_version` and lets the running agent SELF-UPDATE (download +
  atomic rename + exit(64) + SCM restart). That path is useless when the
  bug is *in self-update itself* — e.g. kanadehq/kanade#566, where a
  base64-padding mismatch in the staged-binary digest check made the
  agent reject every download and stay pinned to 0.43.46 with no way to
  pull the fix that would un-wedge it (a classic "can't update the
  updater" bootstrap).

  This script is that bootstrap. It is meant to run **as a kanade job**
  on the wedged agent: the agent executes it as LocalSystem, so it can
  reach Program Files and the SCM — but it CANNOT stop its own service
  inline (stopping KanadeAgent would kill this very script, which is a
  child of the service). So instead of swapping directly, it:

    1. downloads the target kanade-agent.exe from the backend's
       app-packages HTTP endpoint (same BITS + sha-verify posture as
       deploy-backend.ps1's agent mode),
    2. stages the verified binary + writes a self-contained swap runner,
    3. registers a one-shot SYSTEM Scheduled Task to run that runner a
       short delay later — fully detached from the agent's process tree,
       so it survives the service stop.

  The detached runner does the actual stop -> back up -> copy -> start,
  rolls back to the saved binary if the new one fails to start, logs to
  %ProgramData%\Kanade\logs\agent-emergency-swap.log, and deletes itself
  + the task + the staged file on the way out. The job's own result
  (this script's stdout) reports "swap scheduled" and lands normally
  because the agent is still up when the job finishes; the swap+restart
  happens `-SwapDelaySecs` later.

  Run-by-hand fallback: an operator with an elevated shell can also
  invoke this with explicit params to stage + schedule the swap without
  going through a job.

.PARAMETER SourceUrl
  Backend HTTP base that serves the app-packages endpoint, e.g.
  http://kanade-backend.local:8080. Job path fills `$AgentSourceUrl`.

.PARAMETER SourceVersion
  Target version label (must match an uploaded
  `kanade app publish kanade-agent <exe> --version <v>`). Job path fills
  `$AgentSourceVersion`.

.PARAMETER SourceSha256
  Lowercase hex SHA-256 of the target kanade-agent.exe. The download is
  rejected before staging if it doesn't match — a wedged self-update is
  bad, a silently-wrong emergency binary is worse. Job path fills
  `$AgentSourceSha256`.

.PARAMETER SourceAuthToken
  Bearer for the app-packages endpoint (the backend gates it). Job path
  fills `$AgentSourceAuthToken`.

.PARAMETER ServiceName
  Windows service to swap. Default: KanadeAgent.

.PARAMETER SwapDelaySecs
  Seconds between scheduling and the detached swap firing. Long enough
  for this job's result to publish before the agent bounces. Default 60.
#>

[CmdletBinding()]
param(
    [string]$SourceUrl       = '',
    [string]$SourceVersion   = '',
    [string]$SourceSha256    = '',
    [string]$SourceAuthToken = '',
    [string]$ServiceName     = 'KanadeAgent',
    [int]   $SwapDelaySecs   = 60
)

$ErrorActionPreference = 'Stop'

# === Agent / job-mode knobs ================================================
# Published copies of this script run with NO CLI args (the manifest
# references it via `execute.script_object`), so the operator edits these
# constants before `kanade script publish` and they fold into the params
# below. Mirrors deploy-backend.ps1's agent-mode hook. Leave them blank
# to keep the by-hand `-SourceUrl ...` invocation working unchanged.
$AgentSourceUrl       = ''   # e.g. 'http://127.0.0.1:8080'
$AgentSourceVersion   = ''   # e.g. '0.43.48'
$AgentSourceSha256    = ''   # lowercase hex of the target kanade-agent.exe
$AgentSourceAuthToken = ''   # bearer for /api/app-packages
$AgentSwapDelaySecs   = 0    # 0 = leave the -SwapDelaySecs default
# How long BITS keeps retrying a transient transfer error before giving
# up (the transfer itself is unbounded; the manifest `timeout:` bounds
# the whole job). 1800 = 30 min — see deploy-backend.ps1 for the rationale.
$AgentDownloadRetryTimeoutSecs = 1800

if ($AgentSourceUrl)       { $SourceUrl       = $AgentSourceUrl }
if ($AgentSourceVersion)   { $SourceVersion   = $AgentSourceVersion }
if ($AgentSourceSha256)    { $SourceSha256    = $AgentSourceSha256 }
if ($AgentSourceAuthToken) { $SourceAuthToken = $AgentSourceAuthToken }
if ($AgentSwapDelaySecs)   { $SwapDelaySecs   = $AgentSwapDelaySecs }
# ===========================================================================

if (-not $SourceUrl)     { throw 'agent-emergency-swap: SourceUrl is required (set $AgentSourceUrl before publishing).' }
if (-not $SourceVersion) { throw 'agent-emergency-swap: SourceVersion is required.' }
if (-not $SourceSha256)  { throw 'agent-emergency-swap: SourceSha256 is required — refusing to stage an unverified binary.' }

$dataRoot   = Join-Path $env:ProgramData 'Kanade'
$stagingDir = Join-Path $dataRoot 'staging'
$logsDir    = Join-Path $dataRoot 'logs'
$binDir     = Join-Path $env:ProgramFiles 'Kanade'
$exeDst     = Join-Path $binDir 'kanade-agent.exe'
$stagedExe  = Join-Path $stagingDir "kanade-agent-emergency-$SourceVersion.exe"
$runner     = Join-Path $dataRoot 'agent-emergency-swap-runner.ps1'
$taskName   = 'KanadeAgentEmergencySwap'

foreach ($d in @($stagingDir, $logsDir)) {
    if (-not (Test-Path $d)) { New-Item -ItemType Directory -Path $d -Force | Out-Null }
}

# --- 1. download the target binary from app-packages (BITS) ----------------
$url = "$($SourceUrl.TrimEnd('/'))/api/app-packages/kanade-agent/$SourceVersion"
Write-Host "agent-emergency-swap: downloading kanade-agent $SourceVersion from $url (BITS)"
$bitsArgs = @{
    Source       = $url
    Destination  = $stagedExe
    Priority     = 'Foreground'
    RetryTimeout = $AgentDownloadRetryTimeoutSecs
}
if ($SourceAuthToken) {
    $bitsArgs.CustomHeaders = @("Authorization: Bearer $($SourceAuthToken.Trim())")
}
Start-BitsTransfer @bitsArgs

# --- 2. verify sha + version BEFORE we let anything swap it in -------------
$actual   = (Get-FileHash $stagedExe -Algorithm SHA256).Hash.ToLowerInvariant()
$expected = $SourceSha256.ToLowerInvariant()
if ($actual -ne $expected) {
    Remove-Item -Force -ErrorAction SilentlyContinue $stagedExe
    throw "agent-emergency-swap: sha256 mismatch — expected=$expected actual=$actual. Refusing to stage (possible MITM / corrupted upload)."
}
Write-Host 'agent-emergency-swap: sha256 verified'

$reported = (& $stagedExe --version) -join ' '
if ($reported -notmatch [regex]::Escape($SourceVersion)) {
    Remove-Item -Force -ErrorAction SilentlyContinue $stagedExe
    throw "agent-emergency-swap: staged binary reports '$reported', expected version '$SourceVersion'. Refusing to stage."
}
Write-Host "agent-emergency-swap: staged binary is '$reported'"

# --- 3. write the detached swap runner ------------------------------------
# It runs LATER, as SYSTEM, from Task Scheduler — NOT as a child of the
# agent service — so it can Stop-Service KanadeAgent without killing
# itself. Everything it needs is baked in as literals here; it never
# touches the network. Robust by construction: back up first, roll the
# old binary back if the new one won't start, always self-clean.
$runnerBody = @"
`$ErrorActionPreference = 'Continue'
`$svc     = '$ServiceName'
`$exeDst  = '$exeDst'
`$staged  = '$stagedExe'
`$bak     = "`$exeDst.emergency-bak"
`$target  = '$SourceVersion'
`$log     = '$logsDir\agent-emergency-swap.log'
`$task    = '$taskName'
function Log(`$m) { "`$((Get-Date).ToString('o'))  `$m" | Out-File -FilePath `$log -Append -Encoding utf8 }
Log "=== emergency swap starting (target=`$target) ==="
try {
    Log "stopping `$svc"
    Stop-Service `$svc -Force
    (Get-Service `$svc).WaitForStatus('Stopped', '00:00:45')
    Log "backing up current binary -> `$bak"
    Copy-Item `$exeDst `$bak -Force
    Log "copying staged binary into place"
    Copy-Item `$staged `$exeDst -Force
    Log "starting `$svc"
    Start-Service `$svc
    (Get-Service `$svc).WaitForStatus('Running', '00:00:45')
    `$now = (& `$exeDst --version) -join ' '
    if (`$now -match [regex]::Escape(`$target)) {
        Log "swap OK — running `$now"
        Remove-Item -Force -ErrorAction SilentlyContinue `$staged
        Remove-Item -Force -ErrorAction SilentlyContinue `$bak
    } else {
        throw "post-swap version is '`$now', expected '`$target'"
    }
} catch {
    Log "ERROR: `$(`$_.Exception.Message) — rolling back to saved binary"
    try {
        if (Test-Path `$bak) { Copy-Item `$bak `$exeDst -Force }
        Start-Service `$svc
        (Get-Service `$svc).WaitForStatus('Running', '00:00:45')
        Log "rollback complete — `$svc status `$((Get-Service `$svc).Status)"
    } catch {
        Log "ROLLBACK FAILED: `$(`$_.Exception.Message) — manual repair needed (.emergency-bak holds the previous binary)"
    }
} finally {
    Log "removing scheduled task `$task"
    Unregister-ScheduledTask -TaskName `$task -Confirm:`$false -ErrorAction SilentlyContinue
    # Delete this runner itself too — PowerShell read the whole file into
    # memory at start, so self-deletion mid-run is safe and leaves nothing
    # executable behind under %ProgramData%\Kanade (gemini/claude #570).
    Remove-Item -Force -ErrorAction SilentlyContinue `$PSCommandPath
    Log "=== emergency swap finished ==="
}
"@
Set-Content -Path $runner -Value $runnerBody -Encoding utf8
Write-Host "agent-emergency-swap: wrote swap runner -> $runner"

# --- 4. register the one-shot detached SYSTEM task ------------------------
# DateTime trigger (not a HH:mm string) keeps it locale-independent.
$action  = New-ScheduledTaskAction -Execute 'powershell.exe' `
    -Argument "-NonInteractive -NoProfile -ExecutionPolicy Bypass -File `"$runner`""
$trigger = New-ScheduledTaskTrigger -Once -At ((Get-Date).AddSeconds($SwapDelaySecs))
$principal = New-ScheduledTaskPrincipal -UserId 'S-1-5-18' -RunLevel Highest -LogonType ServiceAccount
$null = Register-ScheduledTask -TaskName $taskName -Action $action -Trigger $trigger `
    -Principal $principal -Force `
    -Description 'One-shot out-of-band kanade-agent binary swap (self-update recovery).'

Write-Host ''
Write-Host "agent-emergency-swap: swap of $ServiceName -> $SourceVersion scheduled in ${SwapDelaySecs}s (task '$taskName')."
Write-Host "  staged : $stagedExe"
Write-Host "  runner : $runner"
Write-Host "  log    : $logsDir\agent-emergency-swap.log"
Write-Host 'The agent will briefly stop and restart on the new binary; this job result is published before that happens.'

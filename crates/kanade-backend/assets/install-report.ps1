$stage = 'platform'
$seq = 0
$code = 0
$tmp = $null
$script:uploadFailed = $false
$script:errorKind = 'command_failed'
function Report-Install([string]$outcome, [int]$exitCode) {
    $script:seq++
    $event = @{seq=$script:seq; stage=$script:stage; outcome=$outcome; exit_code=$exitCode}
    if ($outcome -eq 'failed') { $event.error_kind = $script:errorKind }
    $body = $event | ConvertTo-Json -Compress
    try {
        if ($tmp -and (Test-Path $tmp)) { Add-Content -LiteralPath (Join-Path $tmp 'progress.jsonl') -Value $body -ErrorAction Stop }
    } catch { }
    if ('@@KEY@@' -eq '') { $script:uploadFailed = $true; return }
    try {
        Invoke-WebRequest -UseBasicParsing -Uri @@URL@@ -Method Post -Headers @{'X-Install-Key'='@@KEY@@'} -ContentType 'application/json' -Body $body -TimeoutSec 3 | Out-Null
    } catch { $script:uploadFailed = $true }
}
if ('@@KEY@@' -eq '') { Write-Host 'Backend installation logging unavailable; diagnostics remain local.' }
Write-Host 'Installation attempt @@ID@@'
Report-Install progress 0
$script:phaseLines = 0
function Read-InstallStage {
    try {
        $lines = (Get-Content -LiteralPath (Join-Path $tmp 'install-stage') -Raw -ErrorAction Stop).Replace("`r", '').Split("`n")
        # The final split element may still be an incomplete child write.
        while ($script:phaseLines -lt $lines.Length - 1) {
            $next = $lines[$script:phaseLines]
            $script:phaseLines++
            if ($next -in @('placement', 'service_start')) {
                $script:stage = $next
                Report-Install progress 0
            }
        }
    } catch { }
}
function Wait-Install($process) {
    while (!$process.HasExited) {
        Read-InstallStage
        Start-Sleep -Milliseconds 500
        $process.Refresh()
    }
    $process.WaitForExit()
    Read-InstallStage
}

function Read-InstallErrorKind {
    try {
        $kind = Get-Content -LiteralPath (Join-Path $tmp 'install-error-kind') -Tail 1 -ErrorAction Stop
        foreach ($allowedKind in @('permission_denied','file_not_found','invalid_archive','io_error','service_error','elevation_cancelled','http_error','command_failed')) {
            if ($kind -eq $allowedKind) {
                # Get-Content strings carry provider metadata. Windows PowerShell
                # serializes it, so retain only the matching literal from this list.
                $script:errorKind = $allowedKind
                break
            }
        }
    } catch { }
}

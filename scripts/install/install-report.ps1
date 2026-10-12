$stage = 'platform'
$seq = 0
$code = 0
$tmp = $null
$script:uploadFailed = $false
function Report-Install([string]$outcome, [int]$exitCode) {
    $script:seq++
    $body = @{seq=$script:seq; stage=$script:stage; outcome=$outcome; exit_code=$exitCode} | ConvertTo-Json -Compress
    try {
        if ($tmp -and (Test-Path $tmp)) { Add-Content -LiteralPath (Join-Path $tmp 'progress.jsonl') -Value $body -ErrorAction Stop }
    } catch { }
    try {
        Invoke-WebRequest -UseBasicParsing -Uri @@URL@@ -Method Post -Headers @{'X-Install-Key'='@@KEY@@'} -ContentType 'application/json' -Body $body -TimeoutSec 3 | Out-Null
    } catch { $script:uploadFailed = $true }
}
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

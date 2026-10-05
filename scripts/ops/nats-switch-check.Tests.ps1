# Exercises nats-switch-check.ps1 with the backend faked, so no backend, broker
# or real token is needed. Each case runs the script in a child PowerShell
# whose Invoke-RestMethod and Start-Sleep are mock functions driven by a
# scenario JSON file (responses, a clock, how many calls fail first).
#
# Run with either shell: powershell -File ... / pwsh -File ...

$ErrorActionPreference = 'Stop'
$ps = if ($PSVersionTable.PSEdition -eq 'Core') { 'pwsh' } else { 'powershell' }
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("nats-switch-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$fail = 0
function Check($name, $ok, $detail = '') {
    if ($ok) { Write-Host "  PASS  $name" }
    else { Write-Host "  FAIL  $name $detail"; $script:fail++ }
}

$script_ = Join-Path $PSScriptRoot 'nats-switch-check.ps1'
$secret = 'TOKEN-s3cr3t-xyz'
$base = [datetime]::new(2026, 10, 5, 10, 0, 0, [DateTimeKind]::Utc)
$allOutput = New-Object System.Text.StringBuilder

function Iso([datetime]$t) { $t.ToString('yyyy-MM-ddTHH:mm:ssZ') }
function Agent($id, $ver, $hbAgoSec, $user = $null, $since = $null, $host_ = $null, [datetime]$now = $base) {
    $a = [ordered]@{ pc_id = $id; hostname = $(if ($host_) { $host_ } else { $id.ToLower() + '-host' }); agent_version = $ver }
    $a.last_heartbeat = if ($null -ne $hbAgoSec) { Iso $now.AddSeconds(-$hbAgoSec) } else { $null }
    if ($user) { $a.nats_user = $user }
    if ($since) { $a.nats_user_since = Iso $since }
    [pscustomobject]$a
}
function CheckRow($id, $status, $recordedAgoSec = 60, $stale = $false) {
    [pscustomobject]@{ pc_id = $id; check_name = 'cred-check'; status = $status; detail = "detail-$secret"; recorded_at = (Iso $base.AddSeconds(-$recordedAgoSec)); stale = $stale }
}
function Scenario($agents, $checks = @(), [datetime]$now = $base, $failFirst = 0, $failAlways = $false) {
    [pscustomobject]@{ agents = @($agents); rows = @($checks); date = $now.ToString('r'); fail_first = $failFirst; fail_always = $failAlways; secret = $secret }
}

# Run the script under the mock. Returns exit code, output and the mock's call log.
function Invoke-Case([string[]]$ScriptArgs, $Scn, $Token = $secret) {
    $id = [guid]::NewGuid().ToString('N')
    $scnPath = Join-Path $tmp "$id.json"
    $log = Join-Path $tmp "$id.log"
    $wrapper = Join-Path $tmp "$id.ps1"
    ($Scn | ConvertTo-Json -Depth 8) | Set-Content -LiteralPath $scnPath -Encoding utf8
    $body = @"
`$global:Sc = Get-Content -Raw -LiteralPath '$scnPath' | ConvertFrom-Json
`$global:LogPath = '$log'
`$global:Failed = 0
function global:Start-Sleep { param([int]`$Seconds) Add-Content -LiteralPath `$global:LogPath -Value "sleep `$Seconds" }
function global:Invoke-RestMethod {
    param(`$Uri, `$Headers, `$Method, `$TimeoutSec, `$ResponseHeadersVariable)
    Add-Content -LiteralPath `$global:LogPath -Value "get `$Uri"
    if (`$global:Sc.fail_always) { throw ('connection refused to ' + `$Uri + ' using ' + `$global:Sc.secret) }
    if (`$global:Failed -lt `$global:Sc.fail_first) { `$global:Failed++; throw ('timed out, token ' + `$global:Sc.secret) }
    if (`$ResponseHeadersVariable) { Set-Variable -Name `$ResponseHeadersVariable -Value @{ Date = @(`$global:Sc.date) } -Scope 1 }
    if (`$Uri -like '*/api/agents*') { return `$global:Sc.agents }
    if (`$Uri -like '*/api/checks*') { return [pscustomobject]@{ counts = @(); rows = @(`$global:Sc.rows); stale_days = 0; stale_attention = 0 } }
    throw "unexpected `$Uri"
}
& '$script_' @args
exit `$LASTEXITCODE
"@
    [System.IO.File]::WriteAllText($wrapper, $body)
    $env:KANADE_API_TOKEN = $Token
    try {
        $psArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $wrapper) + $ScriptArgs
        $out = & $ps @psArgs 2>&1 | ForEach-Object { "$_" }
        $code = $LASTEXITCODE
    } finally { Remove-Item Env:KANADE_API_TOKEN -ErrorAction SilentlyContinue }
    $text = $out -join "`n"
    [void]$allOutput.AppendLine($text)
    $calls = @(if (Test-Path $log) { Get-Content -LiteralPath $log })
    [pscustomobject]@{ Code = $code; Out = $text; Calls = $calls }
}

$url = @('-BackendUrl', 'http://backend.test:8080')

# --- Readiness ---
$fleet = @(
    (Agent 'A-READY' '0.62.1' 30)
    (Agent 'B-OLD' '0.60.0' 30)
    (Agent 'C-NOVER' $null 30)
    (Agent 'D-STALEHB' '0.62.1' (3 * 86400))
    (Agent 'E-NOCHECK' '0.62.1' 30)
    (Agent 'F-WARN' 'v0.62.1' 30)
    (Agent 'G-STALECHK' '0.62.1' 30)
    (Agent 'H-RECENT' '0.61.0-rc1' 3600)
    (Agent 'I-NEVER' '0.62.1' $null)
)
$rows = @(
    (CheckRow 'A-READY' 'ok'), (CheckRow 'B-OLD' 'ok'), (CheckRow 'C-NOVER' 'ok'), (CheckRow 'D-STALEHB' 'ok'),
    (CheckRow 'F-WARN' 'fail'), (CheckRow 'G-STALECHK' 'ok' 9999999 $true), (CheckRow 'H-RECENT' 'ok'), (CheckRow 'I-NEVER' 'ok')
)
$r = Invoke-Case (@('-Mode', 'Readiness', '-CheckName', 'cred-check') + $url) (Scenario $fleet $rows)
Check 'readiness: exit 1 when hosts are not ready' ($r.Code -eq 1) "code=$($r.Code)`n$($r.Out)"
Check 'readiness: ready host is not listed' ($r.Out -notmatch 'a-ready-host[^\n]*:' -and $r.Out -notmatch 'h-recent-host[^\n]*:')
Check 'readiness: old version named with both versions' ($r.Out -match 'b-old-host[^\n]*agent-version-too-old\(0\.60\.0 < 0\.61\.0\)')
Check 'readiness: missing version' ($r.Out -match 'c-nover-host[^\n]*agent-version-unknown')
Check 'readiness: no recent heartbeat' ($r.Out -match 'd-stalehb-host[^\n]*no-recent-heartbeat')
Check 'readiness: never heartbeated' ($r.Out -match 'i-never-host[^\n]*no-recent-heartbeat')
Check 'readiness: no check result' ($r.Out -match 'e-nocheck-host[^\n]*no-check-result')
Check 'readiness: failing check carries status' ($r.Out -match 'f-warn-host[^\n]*check-not-ok\(fail\)')
Check 'readiness: stale check' ($r.Out -match 'g-stalechk-host[^\n]*check-stale')
Check 'readiness: says what it cannot know' ($r.Out -match 'What this cannot know' -and $r.Out -match 'does not prove')
Check 'readiness: asks only for the named check, with stale and ok rows' ($r.Calls -contains 'get http://backend.test:8080/api/checks?check=cred-check&include_ok=true&include_stale=true')

$r = Invoke-Case (@('-Mode', 'Readiness', '-CheckName', 'cred-check', '-CheckedSince', (Iso $base.AddSeconds(-10))) + $url) (Scenario @((Agent 'A-READY' '0.62.1' 30)) @((CheckRow 'A-READY' 'ok' 60)))
Check 'readiness: an ok older than -CheckedSince does not count' ($r.Code -eq 1 -and $r.Out -match 'check-older-than-CheckedSince')

$r = Invoke-Case (@('-Mode', 'Readiness', '-CheckName', 'cred-check', '-MinAgentVersion', '0.63.0') + $url) (Scenario @((Agent 'A-READY' '0.62.1' 30)) @((CheckRow 'A-READY' 'ok')))
Check 'readiness: -MinAgentVersion overrides the default' ($r.Code -eq 1 -and $r.Out -match 'too-old\(0\.62\.1 < 0\.63\.0\)')

$r = Invoke-Case (@('-Mode', 'Readiness', '-CheckName', 'cred-check') + $url) (Scenario @((Agent 'A-READY' '0.62.1' 30), (Agent 'B-READY' '0.61.0' 100)) @((CheckRow 'A-READY' 'ok'), (CheckRow 'B-READY' 'ok')))
Check 'readiness: all ready exits 0' ($r.Code -eq 0 -and $r.Out -match 'every registered agent is ready') $r.Out

$r = Invoke-Case (@('-Mode', 'Readiness') + $url) (Scenario @())
Check 'readiness: -CheckName is required, never guessed' ($r.Code -eq 2 -and $r.Calls.Count -eq 0)

# --- Snapshot ---
$snap = Join-Path $tmp 'before.json'
$alive = @(
    (Agent 'X' '0.62.1' 20 'shared-token' ($base.AddDays(-3)))
    (Agent 'Y' '0.62.1' 40 'shared-token' ($base.AddDays(-3)))
    (Agent 'Z' '0.62.1' 50 'shared-token' ($base.AddDays(-3)))
    (Agent 'OFF' '0.62.1' 7200)
)
$r = Invoke-Case (@('-Mode', 'Snapshot', '-SnapshotPath', $snap) + $url) (Scenario $alive)
$j = if (Test-Path $snap) { Get-Content -Raw $snap | ConvertFrom-Json } else { $null }
Check 'snapshot: exit 0 and file written' ($r.Code -eq 0 -and $null -ne $j) $r.Out
Check 'snapshot: only live hosts, with the fields' (@($j.hosts).Count -eq 3 -and $j.hosts[0].pc_id -and $j.hosts[0].agent_version -and $j.hosts[0].nats_user -eq 'shared-token' -and $j.hosts[0].last_heartbeat)
Check 'snapshot: refuses to overwrite' ((Invoke-Case (@('-Mode', 'Snapshot', '-SnapshotPath', $snap) + $url) (Scenario $alive)).Code -eq 2)
Check 'snapshot: -Force overwrites' ((Invoke-Case (@('-Mode', 'Snapshot', '-SnapshotPath', $snap, '-Force') + $url) (Scenario $alive)).Code -eq 0)
$none = Join-Path $tmp 'none.json'
$r = Invoke-Case (@('-Mode', 'Snapshot', '-SnapshotPath', $none) + $url) (Scenario @((Agent 'OFF' '0.62.1' 7200)))
Check 'snapshot: nothing alive writes nothing' ($r.Code -eq 2 -and -not (Test-Path $none))
Check 'snapshot file holds no token' ((Get-Content -Raw $snap) -notmatch [regex]::Escape($secret))

# --- Compare ---
$switched = $base
$later = $base.AddSeconds(300)
$after = @(
    (Agent 'X' '0.62.1' 20 'agent' ($later.AddSeconds(-200)) $null $later)
    (Agent 'Y' '0.62.1' 30 'shared-token' ($base.AddDays(-3)) $null $later)
    (Agent 'Z' '0.62.1' 600 'shared-token' ($base.AddDays(-3)) $null $later)
    (Agent 'NEW' '0.62.1' 10 'agent' ($later.AddSeconds(-100)) $null $later)
)
$cmpArgs = @('-Mode', 'Compare', '-SnapshotPath', $snap, '-SwitchedAt', (Iso $switched)) + $url
$r = Invoke-Case ($cmpArgs + @('-MaxDisappeared', '0')) (Scenario $after @() $later)
Check 'compare: exit 1 when disappeared exceeds the threshold' ($r.Code -eq 1) "code=$($r.Code)`n$($r.Out)"
Check 'compare: lists the disappeared host' ($r.Out -match 'Disappeared[^\n]*: 1' -and $r.Out -match 'z-host \(Z\)')
Check 'compare: a live host is not disappeared' ($r.Out -notmatch 'x-host \(X\)  agent=')
Check 'compare: lists reconnected host' ($r.Out -match 'Heartbeated after the switch and alive now: 2 of 3')
Check 'compare: lists new host' ($r.Out -match 'New hosts[^\n]*: 1' -and $r.Out -match 'new-host \(NEW\)')
Check 'compare: counts by user, expected and unexpected' ($r.Out -match '2  agent \(expected\)' -and $r.Out -match '1  shared-token \(UNEXPECTED')
Check 'compare: flags values older than the switch' ($r.Out -match '1 live hosts carry a value last changed before the switch')
Check 'compare: says it cannot tell lock-out from power-off' ($r.Out -match 'look the same')
$r = Invoke-Case ($cmpArgs + @('-MaxDisappeared', '1')) (Scenario $after @() $later)
Check 'compare: within the threshold exits 0' ($r.Code -eq 0) "code=$($r.Code)"

$r = Invoke-Case ($cmpArgs + @('-MaxDisappeared', '0')) (Scenario $after @() ($base.AddSeconds(60)))
Check 'compare: too early to judge exits 3' ($r.Code -eq 3 -and $r.Out -match 'Too early')

$r = Invoke-Case ($cmpArgs + @('-MaxDisappeared', '5', '-WaitSeconds', '150')) (Scenario $after @() $later)
Check 'compare: -WaitSeconds sleeps before reading' ($r.Calls[0] -eq 'sleep 150' -and $r.Calls[1] -like 'get *')

$gone = @((Agent 'Q' '0.62.1' 900 $null $null $null $later))
$r = Invoke-Case ($cmpArgs + @('-MaxDisappeared', '9')) (Scenario $gone @() $later)
Check 'compare: everything gone points at the backend' ($r.Out -match 'backend itself is probably locked out')

$r = Invoke-Case ($cmpArgs + @('-MaxDisappeared', '0')) (Scenario @((Agent 'X' '0.62.1' 20 'root' ($later.AddSeconds(-5)) $null $later), (Agent 'Y' '0.62.1' 20 'no-auth' $null $null $later), (Agent 'Z' '0.62.1' 20 'unknown' $null $null $later)) @() $later)
Check 'compare: no-auth, unknown, absent and other users are all visible' ($r.Out -match 'no-auth \(UNEXPECTED' -and $r.Out -match 'unknown \(UNEXPECTED' -and $r.Out -match "other user 'root'")

# pc_ids that differ only by case are different hosts.
$caseSnap = Join-Path $tmp 'case.json'
[void](Invoke-Case (@('-Mode', 'Snapshot', '-SnapshotPath', $caseSnap) + $url) (Scenario @((Agent 'PC1' '0.62.1' 10), (Agent 'pc1' '0.62.1' 7200))))
$r = Invoke-Case (@('-Mode', 'Compare', '-SnapshotPath', $caseSnap, '-SwitchedAt', (Iso $switched), '-MaxDisappeared', '0') + $url) (Scenario @((Agent 'PC1' '0.62.1' 900 $null $null $null $later), (Agent 'pc1' '0.62.1' 5 $null $null $null $later)) @() $later)
Check 'compare: pc_id case is preserved' ($r.Code -eq 1 -and $r.Out -match 'Disappeared[^\n]*: 1' -and $r.Out -match 'New hosts[^\n]*: 1')

$bad = Join-Path $tmp 'bad.json'
Set-Content -LiteralPath $bad -Value '{"nope":1}'
$r = Invoke-Case (@('-Mode', 'Compare', '-SnapshotPath', $bad, '-SwitchedAt', (Iso $switched), '-MaxDisappeared', '0') + $url) (Scenario @())
Check 'compare: malformed snapshot exits 2 without calling the backend' ($r.Code -eq 2 -and $r.Calls.Count -eq 0)
$r = Invoke-Case ($cmpArgs) (Scenario $after @() $later)
Check 'compare: -MaxDisappeared is required' ($r.Code -eq 2)

# --- Tolerance of a failing backend ---
$r = Invoke-Case ($cmpArgs + @('-MaxDisappeared', '0', '-RetryDelaySeconds', '7')) (Scenario $after @() $later 2)
Check 'retry: two failures then success gives a normal result' ($r.Code -eq 1 -and $r.Out -match 'Disappeared')
Check 'retry: says why it retries' ($r.Out -match 'attempt 1 of 5' -and $r.Out -match 'reconnecting to the broker')
Check 'retry: waits between attempts' (@($r.Calls | Where-Object { $_ -eq 'sleep 7' }).Count -eq 2)

$r = Invoke-Case ($cmpArgs + @('-MaxDisappeared', '9', '-Retries', '3')) (Scenario $after @() $later 0 $true)
Check 'retry: total failure exits 2, not an empty result' ($r.Code -eq 2 -and $r.Out -match 'NOT an empty result' -and $r.Out -cnotmatch 'Result:')
Check 'retry: stops after -Retries attempts' (@($r.Calls | Where-Object { $_ -like 'get *' }).Count -eq 3)
$r = Invoke-Case (@('-Mode', 'Readiness', '-CheckName', 'cred-check', '-Retries', '2') + $url) (Scenario @() @() $base 0 $true)
Check 'retry: readiness cannot report "all ready" from a failed fetch' ($r.Code -eq 2 -and $r.Out -notmatch 'every registered agent is ready')

# --- No secret anywhere ---
Check 'no output ever contained the token' ($allOutput.ToString().IndexOf($secret) -lt 0)
Check 'no output echoed check detail' ($allOutput.ToString() -notmatch 'detail-')

Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
if ($fail) { Write-Host "$fail check(s) FAILED"; exit 1 }
Write-Host 'all checks passed'

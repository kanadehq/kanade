# Exercises jetstream-delete.ps1 / jetstream-reset.ps1 with `nats` mocked, so
# no broker, admin rights or real credential is needed. Each case runs the
# script in a child PowerShell whose `nats` is a recording function.
#
# Run with either shell: powershell -File ... / pwsh -File ...

$ErrorActionPreference = 'Stop'
$ps = if ($PSVersionTable.PSEdition -eq 'Core') { 'pwsh' } else { 'powershell' }
$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("js-ops-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$fail = 0
function Check($name, $ok, $detail = '') {
    if ($ok) { Write-Host "  PASS  $name" }
    else { Write-Host "  FAIL  $name $detail"; $script:fail++ }
}

$secret = 'S3cr3t-pw-9x'
$resources = [System.IO.File]::ReadAllText((Join-Path $PSScriptRoot 'jetstream-resources.json')) | ConvertFrom-Json
$expectedTotal = @($resources.streams).Count + @($resources.kv).Count + @($resources.object).Count + @($resources.lazy_kv).Count

# Run a script under the mock. $Mode picks the mock's behaviour:
#   ok | notfound | authfail (every call) | failone:<name> (only that resource, with the secret in its text)
function Invoke-Case($Script, [string[]]$ScriptArgs, $Mode = 'ok', $StdIn = $null, $Env = @{}) {
    $log = Join-Path $tmp ([guid]::NewGuid().ToString('N') + '.log')
    $wrapper = Join-Path $tmp ([guid]::NewGuid().ToString('N') + '.ps1')
    $body = @"
`$global:LogPath = '$log'
`$global:Mode = '$Mode'
function global:nats {
    Add-Content -LiteralPath `$global:LogPath -Value (`$args -join ' ')
    `$pw = if (`$env:NATS_PASSWORD) { 'pw=' + `$env:NATS_PASSWORD } else { '' }
    Add-Content -LiteralPath `$global:LogPath -Value ('ENVPW:' + `$env:NATS_PASSWORD)
    switch -Wildcard (`$global:Mode) {
        'ok' { `$global:LASTEXITCODE = 0; 'removed'; return }
        'notfound' { 'nats: error: nats: stream not found'; `$global:LASTEXITCODE = 1; return }
        'authfail' { 'nats: error: nats: Authorization Violation ' + `$pw; `$global:LASTEXITCODE = 1; return }
        'failone:*' {
            `$target = `$global:Mode.Substring(8)
            if (`$args -contains `$target) { 'nats: error: permissions violation ' + `$pw; `$global:LASTEXITCODE = 1; return }
            `$global:LASTEXITCODE = 0; return
        }
    }
}
& '$Script' @args
exit `$LASTEXITCODE
"@
    [System.IO.File]::WriteAllText($wrapper, $body)
    foreach ($k in $Env.Keys) { Set-Item -Path "Env:$k" -Value $Env[$k] }
    try {
        $psArgs = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $wrapper) + $ScriptArgs
        if ($null -ne $StdIn) { $out = $StdIn | & $ps @psArgs 2>&1 | ForEach-Object { "$_" } }
        else { $out = & $ps @psArgs 2>&1 | ForEach-Object { "$_" } }
        $code = $LASTEXITCODE
    } finally {
        foreach ($k in $Env.Keys) { Remove-Item -Path "Env:$k" -ErrorAction SilentlyContinue }
    }
    $calls = @(if (Test-Path $log) { @(Get-Content -LiteralPath $log | Where-Object { $_ -notlike 'ENVPW:*' }) })
    [pscustomobject]@{ Code = $code; Out = ($out -join "`n"); Calls = $calls }
}

$reset = Join-Path $PSScriptRoot 'jetstream-reset.ps1'
$delete = Join-Path $PSScriptRoot 'jetstream-delete.ps1'

# --- reset ---
$r = Invoke-Case $reset @()
Check 'reset dry run: exit 0' ($r.Code -eq 0)
Check 'reset dry run: nats never called' ($r.Calls.Count -eq 0)
Check 'reset dry run: lists lazy bucket views' ($r.Out -match 'kv\s+views')
Check 'reset dry run: lists every resource' ($r.Out -match "Total: $expectedTotal resources")

$r = Invoke-Case $reset @('-Yes', '-Server', 'nats://h:4222')
Check 'reset -Yes: exit 0' ($r.Code -eq 0) $r.Out
Check 'reset -Yes: one delete call per resource' ($r.Calls.Count -eq $expectedTotal) "got $($r.Calls.Count)"
Check 'reset -Yes: stream rm issued' ($r.Calls -contains '--server nats://h:4222 stream rm RESULTS -f')
Check 'reset -Yes: kv del issued (lazy bucket)' ($r.Calls -contains '--server nats://h:4222 kv del scheduler_dispatch -f')
Check 'reset -Yes: object rm issued' ($r.Calls -contains '--server nats://h:4222 object rm agent_releases -f')
Check 'reset -Yes: says resources are recreated on backend start' ($r.Out -match 'recreated the next time kanade-backend starts')
Check 'reset -Yes: mentions stopping backend' ($r.Out -match 'kanade-backend is stopped')

$r = Invoke-Case $reset @('-Yes') 'notfound'
Check 'reset: not-found tolerated, exit 0' ($r.Code -eq 0) $r.Out
Check 'reset: keeps going after not-found' ($r.Calls.Count -eq $expectedTotal)

$r = Invoke-Case $reset @('-Yes') 'authfail'
Check 'reset: real failure gives exit 1' ($r.Code -eq 1)
Check 'reset: continues after failure' ($r.Calls.Count -eq $expectedTotal)

$r = Invoke-Case $reset @('-Yes') 'failone:RESULTS'
Check 'reset: one failure among successes -> exit 1' ($r.Code -eq 1)
Check 'reset: others still deleted' ($r.Out -match 'deleted\s+stream\s+INVENTORY')

# --- credentials ---
$r = Invoke-Case $reset @('-Yes', '-User', 'admin', '-Password', $secret) 'authfail'
Check 'password never printed (even when the broker echoes it)' (-not $r.Out.Contains($secret))
Check 'password not on the nats command line' (-not (($r.Calls -join "`n").Contains($secret)))

$r = Invoke-Case $reset @('-Yes') 'authfail' $null @{ NATS_USER = 'admin'; NATS_PASSWORD = $secret }
Check 'password from environment never printed' (-not $r.Out.Contains($secret))

$r = Invoke-Case $reset @('-Yes', '-Server', 'nats://u:' + $secret + '@h:4222')
Check 'password inside server URL never printed' (-not $r.Out.Contains($secret))

$credFile = Join-Path $tmp 'my admin.creds'
Set-Content -LiteralPath $credFile -Value 'x'
$r = Invoke-Case $reset @('-Yes', '-Creds', $credFile)
Check 'creds path with spaces passed intact' ($r.Calls[0] -eq "--creds $credFile stream rm INVENTORY -f" -or $r.Calls -contains "--creds $credFile kv del script_current -f")

$r = Invoke-Case $reset @('-Yes', '-Server', 'nats://arg:4222') 'ok' $null @{ NATS_URL = 'nats://env:4222' }
Check 'explicit -Server beats NATS_URL' ($r.Calls[0] -like '--server nats://arg:4222 *')
$r = Invoke-Case $reset @('-Yes') 'ok' $null @{ NATS_URL = 'nats://env:4222' }
Check 'NATS_URL used when no -Server' ($r.Calls[0] -like '--server nats://env:4222 *')

$r = Invoke-Case $reset @('-Yes', '-Creds', $credFile, '-User', 'u', '-Password', 'p')
Check 'creds plus user/password is refused' ($r.Code -eq 2 -and $r.Calls.Count -eq 0)

# --- delete ---
$r = Invoke-Case $delete @('-Kind', 'stream', '-Name', 'RESULTS', '-Yes')
Check 'delete -Yes: one stream rm call' ($r.Code -eq 0 -and $r.Calls.Count -eq 1 -and $r.Calls[0] -eq 'stream rm RESULTS -f') ($r.Calls -join '|')
Check 'delete: known name gives no warning' (-not ($r.Out -match 'not a stream that kanade creates'))

$r = Invoke-Case $delete @('-Kind', 'object', '-Name', 'collections', '-Yes')
Check 'delete object: whole bucket' ($r.Calls[0] -eq 'object rm collections -f')
$r = Invoke-Case $delete @('-Kind', 'kv', '-Name', 'views', '-Yes')
Check 'delete kv (lazy bucket)' ($r.Calls[0] -eq 'kv del views -f' -and $r.Out -notmatch 'not a kv')

$r = Invoke-Case $delete @('-Kind', 'stream', '-Name', 'OTHER_SYS', '-Yes')
Check 'delete: unknown name warns but proceeds' ($r.Out -match 'not a stream that kanade creates' -and $r.Calls.Count -eq 1)
$r = Invoke-Case $delete @('-Kind', 'stream', '-Name', 'results', '-Yes')
Check 'delete: name match is case-sensitive' ($r.Out -match 'not a stream that kanade creates')

foreach ($bad in @('a.b', 'a b', 'a>b', '-rf', 'x*', 'a/b')) {
    $r = Invoke-Case $delete @('-Kind', 'stream', '-Name', $bad, '-Yes')
    Check "delete: bad name '$bad' rejected before nats" ($r.Code -ne 0 -and $r.Calls.Count -eq 0)
}

$r = Invoke-Case $delete @('-Kind', 'stream', '-Name', 'RESULTS') 'ok' "RESULTS"
Check 'delete: typed name confirms' ($r.Calls.Count -eq 1 -and $r.Code -eq 0) $r.Out
$r = Invoke-Case $delete @('-Kind', 'stream', '-Name', 'RESULTS') 'ok' "y"
Check 'delete: wrong confirmation deletes nothing' ($r.Calls.Count -eq 0 -and $r.Code -ne 0)

$r = Invoke-Case $delete @('-Kind', 'stream', '-Name', 'RESULTS', '-Yes') 'notfound'
Check 'delete: not-found is not an error' ($r.Code -eq 0)
$r = Invoke-Case $delete @('-Kind', 'stream', '-Name', 'RESULTS', '-Yes') 'authfail'
Check 'delete: real failure gives exit 1' ($r.Code -eq 1)
$r = Invoke-Case $delete @('-Kind', 'stream', '-Name', 'RESULTS', '-Yes', '-Password', $secret, '-User', 'a') 'authfail'
Check 'delete: password never printed on failure' (-not $r.Out.Contains($secret))

Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
if ($fail -gt 0) { Write-Host "$fail check(s) FAILED"; exit 1 }
Write-Host 'all checks passed'

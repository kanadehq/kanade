# Exercise the opt-in role-level users configuration in deploy-nats.ps1 against
# the files this repo ships, with a real nats-server parsing the result.
#
# No admin, no service: the helper functions are cut out of the deploy script
# (between the `nats-users` markers) and run on copies in a temp dir. The
# broker steps need `nats-server` in PATH (or NATS_SERVER_BIN); without it they
# are reported as SKIP locally and fail under CI.
#
#   pwsh scripts/deploy/nats-users.Tests.ps1

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$src = Join-Path $PSScriptRoot 'nats.ps1'
$text = [System.IO.File]::ReadAllText($src)

$start = $text.IndexOf('# >>> nats-users' + "`n")
$end = $text.IndexOf('# <<< nats-users' + "`n")
if ($start -lt 0 -or $end -lt 0) { throw 'nats-users markers not found in nats.ps1' }
Invoke-Expression $text.Substring($start, $end - $start)

$tmp = Join-Path ([System.IO.Path]::GetTempPath()) ('nats-users-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$fail = 0
function Check($name, $ok, $detail = '') {
    if ($ok) { Write-Host "  PASS  $name" }
    else { Write-Host "  FAIL  $name $detail"; $script:fail++ }
}
function Throws($block) { try { & $block; $false } catch { $true } }

$natsServer = if ($env:NATS_SERVER_BIN) { $env:NATS_SERVER_BIN } else { (Get-Command nats-server -ErrorAction SilentlyContinue).Source }
function Parse-Conf($path) {
    $out = & $natsServer -t -c $path 2>&1 | Out-String
    return @{ Ok = ($LASTEXITCODE -eq 0); Out = $out }
}
function Parse-Check($name, $path) {
    if (-not $natsServer) {
        if ($env:CI) { Check "$name (nats-server required under CI)" $false } else { Write-Host "  SKIP  $name (no nats-server)" }
        return
    }
    $r = Parse-Conf $path
    Check $name $r.Ok $r.Out
}

# bcrypt hashes of throwaway passwords, `$`, `.` and `/` included, as
# `nats server passwd` mints them.
$hashes = @{
    AGENT      = '$2a$04$hp8sSRHivlHlui1HEcNGee1fLUvKMbC9H3KPoVYuIltSr90aCzoOi'
    BACKEND    = '$2a$04$tPooi64/C8SMMIZ9Dr0i9eAduFcR9jOZ/Wl.upFfo4KdDFRqVLIzO'
    BREAKGLASS = '$2a$04$JM0AX5q9kVFyZDkRC5R9AuazrRXX8UeHwH8o760Ku5LXkn0OyW9eG'
}
$template = [System.IO.File]::ReadAllText((Join-Path $repo 'configs/nats-server.users.conf'))
$tokenConfs = @{
    windows = Join-Path $repo 'configs/nats-server.conf'
    linux   = Join-Path $repo 'deploy/linux/nats-server.conf'
}

# 1. Substitution: every hash lands quoted, exactly once, nothing else moves.
$users = Format-NatsUsersConf -Template $template -Hashes $hashes
foreach ($role in $UsersRoles) {
    $h = $hashes[$role]
    Check "$role hash present, quoted, once" (([regex]::Matches($users, [regex]::Escape('password: "' + $h + '"'))).Count -eq 1)
}
Check 'no password reference left' (-not ($users -match '(?m)^\s*password:\s*\$KANADE_'))
$a = $template -split "`n"; $b = $users -split "`n"
$changed = @(0..($a.Count - 1) | Where-Object { $a[$_] -ne $b[$_] })
Check 'only the three password lines differ' ($a.Count -eq $b.Count -and $changed.Count -eq 3)
$uf = Join-Path $tmp 'users-only.conf'
[System.IO.File]::WriteAllText($uf, $users)
Parse-Check 'substituted users file parses on a real nats-server' $uf

# 2. Rejections, before anything is written.
foreach ($bad in @(
        @('plaintext', 'hunter2'),
        @('$2y$ prefix', '$2y$04$hp8sSRHivlHlui1HEcNGee1fLUvKMbC9H3KPoVYuIltSr90aCzoOi'),
        @('too short', '$2a$04$abc'),
        @('embedded quote', '$2a$04$hp8sSRHivlHlui1HEcNGee1fLUvKMbC9H3KPoVYuIltSr90aCzo"i'),
        @('backslash', '$2a$04$hp8sSRHivlHlui1HEcNGee1fLUvKMbC9H3KPoVYuIltSr90aCzo\i'),
        @('trailing newline', ($hashes.AGENT + "`n")))) {
    Check "refused: $($bad[0])" (-not (Test-NatsPasswordHash $bad[1]))
}
Check 'a real hash is accepted' (Test-NatsPasswordHash $hashes.AGENT)
$short = @{ AGENT = $hashes.AGENT; BACKEND = $hashes.BACKEND; BREAKGLASS = 'plain' }
Check 'Format throws on a bad hash' (Throws { Format-NatsUsersConf -Template $template -Hashes $short })
Check 'Format throws on a template with a missing reference' (Throws { Format-NatsUsersConf -Template 'x: 1' -Hashes $hashes })

# 3. Main config conversion on both shipped token configs, then the revert.
foreach ($k in $tokenConfs.Keys) {
    $orig = [System.IO.File]::ReadAllText($tokenConfs[$k])
    $c = Join-Path $tmp "$k.conf"
    [System.IO.File]::WriteAllText($c, $orig)
    Set-NatsServerUsersInclude -ConfigPath $c
    $conv = [System.IO.File]::ReadAllText($c)
    Check "${k}: include replaces the block" ($conv.Contains($UsersInclude) -and $conv -notmatch '(?m)^authorization')
    Check "${k}: header comment mentioning the block is untouched" ($conv.Contains('authorization { ... }') -or $k -eq 'linux')
    $ol = $orig -split "`n"; $cl = $conv -split "`n"
    $kept = @($cl | Where-Object { $_ -ne $UsersInclude })
    Check "${k}: every other line is byte-identical" (@($ol | Where-Object { $kept -contains $_ }).Count -ge $kept.Count)
    Set-NatsServerUsersInclude -ConfigPath $c
    Check "${k}: converting twice is a no-op" ([System.IO.File]::ReadAllText($c) -eq $conv)

    # a real broker parses the converted main file together with the substituted users file
    $dir = Join-Path $tmp "run-$k"
    New-Item -ItemType Directory -Path $dir | Out-Null
    $main = (($conv -split "`n") | Where-Object { $_ -notmatch '^\s*(store_dir|listen):' }) -join "`n"
    [System.IO.File]::WriteAllText((Join-Path $dir 'nats-server.conf'), $main)
    [System.IO.File]::WriteAllText((Join-Path $dir 'nats-server.users.conf'), $users)
    Parse-Check "${k}: converted config + users file parse on a real nats-server" (Join-Path $dir 'nats-server.conf')

    Restore-NatsServerTokenBlock -ConfigPath $c -SourceConfigPath $tokenConfs[$k]
    Check "${k}: revert restores the shipped config byte for byte" ([System.IO.File]::ReadAllText($c) -eq $orig)
}

# 4. CRLF survives the round trip, and a missing/duplicate block is refused.
$crlf = Join-Path $tmp 'crlf.conf'
$crlfText = "port: 4222`r`nauthorization {`r`n  token: `"old`"`r`n}`r`nhttp: `"127.0.0.1:8222`"`r`n"
[System.IO.File]::WriteAllText($crlf, $crlfText)
Set-NatsServerUsersInclude -ConfigPath $crlf
Check 'CRLF preserved around the include' ([System.IO.File]::ReadAllText($crlf) -eq "port: 4222`r`n$UsersInclude`r`nhttp: `"127.0.0.1:8222`"`r`n")
$src2 = Join-Path $tmp 'crlf-src.conf'
[System.IO.File]::WriteAllText($src2, $crlfText)
Restore-NatsServerTokenBlock -ConfigPath $crlf -SourceConfigPath $src2
Check 'CRLF revert is byte for byte' ([System.IO.File]::ReadAllText($crlf) -eq $crlfText)
$none = Join-Path $tmp 'none.conf'
[System.IO.File]::WriteAllText($none, "port: 4222`n")
Check 'no block to replace throws' (Throws { Set-NatsServerUsersInclude -ConfigPath $none })
$two = Join-Path $tmp 'two.conf'
[System.IO.File]::WriteAllText($two, "authorization {`n token: `"a`"`n}`nauthorization {`n token: `"b`"`n}`n")
Check 'two blocks throws' (Throws { Set-NatsServerUsersInclude -ConfigPath $two })

# 5. Default behaviour: the switches and hashes have no default, and the
#    opt-in code is reached only through them.
$ast = [System.Management.Automation.Language.Parser]::ParseFile($src, [ref]$null, [ref]$null)
$params = @{}
foreach ($p in $ast.ParamBlock.Parameters) { $params[$p.Name.VariablePath.UserPath] = $p }
foreach ($n in 'UseNatsUsers', 'UseNatsToken') {
    Check "-$n is a switch" ($params[$n].StaticType -eq [System.Management.Automation.SwitchParameter])
}
foreach ($n in 'AgentPasswordHash', 'BackendPasswordHash', 'BreakglassPasswordHash') {
    Check "-$n has no default" ($null -eq $params[$n].DefaultValue)
}
foreach ($n in 'SourceDir', 'ServiceName', 'ForceConfig', 'NoFirewall', 'Recreate', 'NoStart', 'NatsToken') {
    Check "existing -$n is still declared" ($params.ContainsKey($n))
}
$body = $text.Substring($end)
foreach ($call in 'Format-NatsUsersConf', 'Set-NatsServerUsersInclude', 'Restore-NatsServerTokenBlock') {
    $at = $body.IndexOf($call)
    $guard = $body.LastIndexOf('if ($UseNatsUsers)', $at)
    $guard2 = $body.LastIndexOf('elseif ($UseNatsToken)', $at)
    Check "$call is only reached behind a switch" ($at -ge 0 -and ($guard -ge 0 -or $guard2 -ge 0))
}

Remove-Item -Recurse -Force $tmp
if ($fail -gt 0) { Write-Host "$fail check(s) failed"; exit 1 }
Write-Host 'all checks passed'

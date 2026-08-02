# Exercise Set-NatsServerToken against the config this repo actually ships,
# which is the step whose absence caused the outage: the docs said to pass
# -NatsToken, and nobody had ever run it against this file.
#
# No admin, no service, no broker -- dot-sources the function out of the deploy
# script and works on copies in a temp dir.

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$src = Join-Path $PSScriptRoot 'nats.ps1'

# Pull just the function out (the script body needs admin + a service).
$text = [System.IO.File]::ReadAllText($src)
$start = $text.IndexOf('function Set-NatsServerToken')
$end = $text.IndexOf("`n}", $start) + 2
Invoke-Expression $text.Substring($start, $end - $start)

$tmp = Join-Path $env:TEMP ("nats-token-test-" + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$fail = 0
function Check($name, $ok, $detail = '') {
    if ($ok) { Write-Host "  PASS  $name" }
    else { Write-Host "  FAIL  $name $detail"; $script:fail++ }
}

# 1. The shipped config -- the exact case that threw in production.
$c1 = Join-Path $tmp 'shipped.conf'
Copy-Item (Join-Path $repo 'configs\nats-server.conf') $c1
Set-NatsServerToken -ConfigPath $c1 -Token 'dev'
$r1 = [System.IO.File]::ReadAllText($c1)
Check 'shipped config: token substituted' ($r1 -match '(?m)^\s*token:\s*"dev"$')
Check 'shipped config: placeholder gone' (-not $r1.Contains('CHANGE-ME-generate-a-per-fleet-secret'))
Check 'shipped config: the header comment mentioning authorization { ... } is untouched' `
    ($r1.Contains('authorization { ... }'))
Check 'shipped config: loopback bind preserved' ($r1 -match '(?m)^http:\s*"127\.0\.0\.1:8222"')

# 2. Idempotent: running twice is the normal upgrade path.
Set-NatsServerToken -ConfigPath $c1 -Token 'dev'
Check 'second run is a no-op' ([System.IO.File]::ReadAllText($c1) -eq $r1)

# 3. A token containing `$` must not be read as a regex backreference.
$c2 = Join-Path $tmp 'dollar.conf'
Copy-Item (Join-Path $repo 'configs\nats-server.conf') $c2
Set-NatsServerToken -ConfigPath $c2 -Token 'a$1b$$c'
Check 'literal $ in token survives' ([System.IO.File]::ReadAllText($c2) -match '(?m)^\s*token:\s*"a\$1b\$\$c"$')

# 4. Line endings and the rest of the file are preserved byte-for-byte apart
#    from the token itself.
$c3 = Join-Path $tmp 'crlf.conf'
[System.IO.File]::WriteAllText($c3, "authorization {`r`n  token: `"old`"`r`n}`r`n")
Set-NatsServerToken -ConfigPath $c3 -Token 'new'
Check 'CRLF preserved' ([System.IO.File]::ReadAllText($c3) -eq "authorization {`r`n  token: `"new`"`r`n}`r`n")

# 5. No token line at all -> throws, does not silently succeed.
$c4 = Join-Path $tmp 'none.conf'
[System.IO.File]::WriteAllText($c4, "port: 4222`n# token: `"commented out`"`n")
$threw = $false
try { Set-NatsServerToken -ConfigPath $c4 -Token 'x' } catch { $threw = $true }
Check 'no uncommented token line throws' $threw
Check 'commented token line left alone' ([System.IO.File]::ReadAllText($c4).Contains('# token: "commented out"'))

# 6. Two token lines -> refuses rather than rewriting an arbitrary one.
$c5 = Join-Path $tmp 'two.conf'
[System.IO.File]::WriteAllText($c5, "authorization {`n  token: `"a`"`n}`naccounts {`n  SYS {`n    token: `"b`"`n  }`n}`n")
$threw2 = $false
try { Set-NatsServerToken -ConfigPath $c5 -Token 'x' } catch { $threw2 = $true }
Check 'ambiguous config refuses' $threw2

# 7. A token that cannot be written into a config string is refused BEFORE the
#    file is touched. Writing it produces `token: "ab"cd"`, which nats-server
#    cannot parse -- the broker then fails to start, which is the failure this
#    whole function exists to prevent.
$c6 = Join-Path $tmp 'quote.conf'
Copy-Item (Join-Path (Join-Path $repo 'configs') 'nats-server.conf') $c6
$before6 = [System.IO.File]::ReadAllText($c6)
$threw3 = $false
try { Set-NatsServerToken -ConfigPath $c6 -Token 'ab"cd' } catch { $threw3 = $true }
Check 'token with a double quote is refused' $threw3
Check 'refused token leaves the file untouched' ([System.IO.File]::ReadAllText($c6) -eq $before6)

$threw4 = $false
try { Set-NatsServerToken -ConfigPath $c6 -Token "ab`ncd" } catch { $threw4 = $true }
Check 'token with a line break is refused' $threw4

Remove-Item -Recurse -Force $tmp
if ($fail) { Write-Host "`n$fail FAILED"; exit 1 } else { Write-Host "`nall checks passed"; exit 0 }

# Exercises the CommandKeys public_key validation in agent.ps1 and the copy of
# it the backend ships. A mistyped key used to install fine and leave the agent
# with a ring it could not parse.
#
# The script itself needs admin and a service, so this pulls the function out
# with the PowerShell parser and runs it directly. Runs anywhere.
#
# Run with either shell: powershell -File ... / pwsh -File ...

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$agentPs1 = Join-Path $PSScriptRoot 'agent.ps1'
$assetPs1 = Join-Path (Join-Path (Join-Path (Join-Path $repo 'crates') 'kanade-backend') 'assets') 'deploy-agent.ps1'

$fail = 0
function Check($name, $ok, $detail = '') {
    if ($ok) { Write-Host "  PASS  $name" }
    else { Write-Host "  FAIL  $name $detail"; $script:fail++ }
}

function Import-ScriptFunctions($Path, [string[]]$Names) {
    $tokens = $null; $errors = $null
    $ast = [System.Management.Automation.Language.Parser]::ParseFile($Path, [ref]$tokens, [ref]$errors)
    foreach ($n in $Names) {
        $fn = $ast.FindAll({ param($a) $a -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $a.Name -eq $n }, $true) | Select-Object -First 1
        if (-not $fn) { throw "$n not found in $Path" }
        Invoke-Expression ($fn.Extent.Text -replace '^function\s+', 'function global:')
    }
}

Check 'the shipped deploy-agent.ps1 equals scripts/deploy/agent.ps1' `
    ([System.IO.File]::ReadAllText($assetPs1) -eq [System.IO.File]::ReadAllText($agentPs1))

function B64($n) { [Convert]::ToBase64String([byte[]](1..$n | ForEach-Object { $_ % 256 })) }
$good = B64 32

foreach ($p in @($agentPs1, $assetPs1)) {
    $leaf = Split-Path -Leaf $p
    Import-ScriptFunctions $p @('Assert-KanadeCommandKeys')

    $cases = @(
        @{ n = 'valid 32-byte key'; k = $good; ok = $true },
        @{ n = 'valid key with surrounding whitespace'; k = "  $good`n"; ok = $true },
        @{ n = 'not Base64'; k = 'this is not base64!!'; ok = $false },
        @{ n = 'truncated Base64'; k = $good.Substring(0, $good.Length - 1); ok = $false },
        @{ n = '31 bytes'; k = (B64 31); ok = $false },
        @{ n = '33 bytes'; k = (B64 33); ok = $false },
        @{ n = '64 bytes'; k = (B64 64); ok = $false }
    )
    foreach ($c in $cases) {
        $threw = $false; $msg = ''
        try { Assert-KanadeCommandKeys -Entries @([pscustomobject]@{ kid = 'k1'; public_key = $c.k }) } catch { $threw = $true; $msg = $_.Exception.Message }
        Check "$leaf $($c.n)" ($threw -eq (-not $c.ok)) $msg
        if ($threw) { Check "$leaf $($c.n): error names the kid" $msg.Contains("'k1'") }
    }

    $threw = $false; $msg = ''
    try {
        Assert-KanadeCommandKeys -Entries @(
            [pscustomobject]@{ kid = 'good'; public_key = $good },
            [pscustomobject]@{ kid = 'bad'; public_key = (B64 31) })
    } catch { $threw = $true; $msg = $_.Exception.Message }
    Check "$leaf one bad entry among good ones is refused, by kid" ($threw -and $msg.Contains("'bad'") -and -not $msg.Contains("'good'"))

    # ConvertFrom-Json output, the shape the script really passes.
    $json = '[{"kid":"a","public_key":"' + $good + '"},{"kid":"b","public_key":"' + $good + '"}]'
    $parsed = ConvertFrom-Json -InputObject $json
    $threw = $false
    try { Assert-KanadeCommandKeys -Entries @($parsed) } catch { $threw = $true }
    Check "$leaf accepts a parsed two-key ring" (-not $threw)
}

# The call sites exist: the early check and the write path.
$src = [System.IO.File]::ReadAllText($agentPs1)
Check 'agent.ps1 validates before touching the machine' ($src.IndexOf('Assert-KanadeCommandKeys -Entries @($earlyEntries)') -ge 0 -and $src.IndexOf('Assert-KanadeCommandKeys -Entries @($earlyEntries)') -lt $src.IndexOf('Stop-Service'))
Check 'agent.ps1 validates the existing registry ring for -RequireSignedCommands' ($src.Contains('Assert-KanadeCommandKeys -Entries @($validEntries)'))

if ($fail) { Write-Host "$fail check(s) failed"; exit 1 }
Write-Host 'all checks passed'

# Exercise the interactive hash helper in child shells with Read-Host and nats
# mocked. Also run by scripts/deploy/nats-users.Tests.ps1 in Integration.
#   pwsh -NoProfile -File scripts/ops/nats-password-hash.Tests.ps1
$ErrorActionPreference = 'Stop'
$ps = if ($PSVersionTable.PSEdition -eq 'Core') { 'pwsh' } else { 'powershell' }
$tmp = Join-Path ([IO.Path]::GetTempPath()) ('nats-hash-test-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
$helper = Join-Path $PSScriptRoot 'nats-password-hash.ps1'
$hash = '$2a$04$hp8sSRHivlHlui1HEcNGee1fLUvKMbC9H3KPoVYuIltSr90aCzoOi'
$fail = 0
function Check($name, $ok) {
    if ($ok) { Write-Host "  PASS  $name" }
    else { Write-Host "  FAIL  $name"; $script:fail++ }
}
$random = '7xZ2mR9vK4sT8pL3nB6cD1fH5qJ0aW9u'
$cases = @(
    @{ Password = 'short-pass-1'; Cost = $null; Warn = $true; Forwarded = 8 }
    @{ Password = 'abcdefghijklmnopqrstuvwxyzABCDEF'; Cost = 8; Warn = $true; Forwarded = 8 }
    @{ Password = '12345678901234567890123456789012'; Cost = 9; Warn = $true; Forwarded = 9 }
    @{ Password = $random; Cost = $null; Warn = $false; Forwarded = 8 }
    @{ Password = $random; Cost = 9; Warn = $false; Forwarded = 9 }
    @{ Password = 'short-pass-1'; Cost = 10; Warn = $false; Forwarded = 10 }
    @{ Password = 'short-pass-1'; Cost = 11; Warn = $false; Forwarded = 11 }
)
foreach ($word in 'PASSWORD', 'QwErTy', 'letmein', 'welcome', 'admin', 'kanade') {
    $cases += @{ Password = "7xZ2mR9vK4sT8pL3nB6cD1fH5$word"; Cost = 8; Warn = $true; Forwarded = 8 }
}
try {
    $wrapper = Join-Path $tmp 'wrapper.ps1'
    $scenario = Join-Path $tmp 'case.json'
    $argvLog = Join-Path $tmp 'argv.txt'
    $stdout = Join-Path $tmp 'stdout.txt'
    $stderr = Join-Path $tmp 'stderr.txt'
    @'
param($Scenario, $Helper, $ArgvLog)
$ErrorActionPreference = 'Stop'
$global:Case = Get-Content -Raw -LiteralPath $Scenario | ConvertFrom-Json
$global:ArgvLog = $ArgvLog
function global:Read-Host {
    param($Prompt, [switch]$AsSecureString)
    ConvertTo-SecureString $global:Case.Password -AsPlainText -Force
}
function global:nats {
    [IO.File]::WriteAllText($global:ArgvLog, ($args -join ' '))
    if ($env:PASSWORD -cne $global:Case.Password) { throw 'missing child credential' }
    $global:LASTEXITCODE = 0
    '$2a$04$hp8sSRHivlHlui1HEcNGee1fLUvKMbC9H3KPoVYuIltSr90aCzoOi'
}
if ($null -eq $global:Case.Cost) { & $Helper }
else { & $Helper -Cost $global:Case.Cost }
if (Test-Path Env:PASSWORD) { throw 'child credential was not cleared' }
'@ | Set-Content -LiteralPath $wrapper -Encoding utf8
    foreach ($case in $cases) {
        $case | ConvertTo-Json | Set-Content -LiteralPath $scenario -Encoding utf8
        & $ps -NoProfile -File $wrapper $scenario $helper $argvLog > $stdout 2> $stderr
        $rc = $LASTEXITCODE
        $out = [IO.File]::ReadAllText($stdout).Trim()
        $err = [IO.File]::ReadAllText($stderr)
        $argv = [IO.File]::ReadAllText($argvLog)
        Check 'helper succeeds and stdout contains only the hash' ($rc -eq 0 -and $out -ceq $hash)
        Check "helper forwards cost $($case.Forwarded)" ($argv -eq "server passwd --cost $($case.Forwarded)")
        Check "expected warning=$($case.Warn)" (($err.Contains('low bcrypt cost is only appropriate for long random passwords')) -eq $case.Warn)
        if ($case.Warn) { Check 'warning includes a cryptographic generator' ($err.Contains('System.Security.Cryptography.RandomNumberGenerator') -and $err.Contains('-lt 248')) }
        Check 'plaintext never appears in output or arguments' (-not ($out.Contains($case.Password) -or $err.Contains($case.Password) -or $argv.Contains($case.Password)))
    }
} finally {
    Remove-Item -Recurse -Force $tmp
}
if ($fail -gt 0) { throw "$fail hash helper check(s) failed" }
Write-Host 'all hash helper checks passed'

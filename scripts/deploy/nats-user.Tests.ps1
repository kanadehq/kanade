# Exercises the per-role NATS user (-NatsUser / -NatsPassword) in
# agent.ps1 and backend.ps1, and the copy of agent.ps1 the backend ships.
#
# The scripts themselves need admin, a service and a real exe, so this pulls
# the functions out with the PowerShell parser and runs them directly. Pair
# validation and the source-level checks run anywhere; the registry checks
# need Windows and an elevated shell, and write only to a scratch tree under
# HKCU, never to HKLM\SOFTWARE\kanade.
#
# Run with either shell: powershell -File ... / pwsh -File ...

$ErrorActionPreference = 'Stop'
$repo = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)
$agentPs1 = Join-Path $PSScriptRoot 'agent.ps1'
$backendPs1 = Join-Path $PSScriptRoot 'backend.ps1'
$assetPs1 = Join-Path (Join-Path (Join-Path (Join-Path $repo 'crates') 'kanade-backend') 'assets') 'deploy-agent.ps1'
$ps = if ($PSVersionTable.PSEdition -eq 'Core') { 'pwsh' } else { 'powershell' }
$isWin = ($PSVersionTable.PSEdition -ne 'Core') -or $IsWindows

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
$fnNames = 'Assert-KanadeNatsUserPair', 'Set-KanadeRegistrySecrets', 'Set-KanadeRegistrySecret'

$secret = "pa ss'wo`$rd`"``%x"   # space, single quote, $, double quote, backtick, %

# --- both copies of the helpers are the same text, and the shipped copy of
# agent.ps1 is byte-for-byte the canonical one ---
Check 'the shipped deploy-agent.ps1 equals scripts/deploy/agent.ps1' `
    ([System.IO.File]::ReadAllText($assetPs1) -eq [System.IO.File]::ReadAllText($agentPs1))

foreach ($p in @($agentPs1, $backendPs1)) {
    $leaf = Split-Path -Leaf $p
    Import-ScriptFunctions $p $fnNames

    # --- pair validation ---
    $cases = @(
        @{ n = 'both'; u = 'u1'; p = 'p1'; ok = $true },
        @{ n = 'neither'; u = ''; p = ''; ok = $true },
        @{ n = 'user only'; u = 'u1'; p = ''; ok = $false },
        @{ n = 'password only'; u = ''; p = $secret; ok = $false }
    )
    foreach ($c in $cases) {
        $threw = $false; $msg = ''
        try { Assert-KanadeNatsUserPair -User $c.u -Password $c.p } catch { $threw = $true; $msg = $_.Exception.Message }
        Check "$leaf pair validation: $($c.n)" ($threw -eq (-not $c.ok))
        if ($threw) { Check "$leaf pair validation: error never echoes the password ($($c.n))" (-not $msg.Contains($secret)) }
    }

    # A half pair fails the real script before it touches anything: it must
    # report the pair error, not the missing-exe error that comes later.
    $out = & $ps -NoProfile -ExecutionPolicy Bypass -File $p -SourceDir (Join-Path ([System.IO.Path]::GetTempPath()) 'no-such-dir') -NatsUser 'u1' 2>&1 | ForEach-Object { "$_" }
    $code = $LASTEXITCODE
    Check "$leaf half pair: script exits non-zero" ($code -ne 0)
    Check "$leaf half pair: refused with the pair error, before any other work" (($out -join "`n") -match 'must be given together')

    # --- source-level checks of what is written where ---
    $src = [System.IO.File]::ReadAllText($p)
    $role = if ($p -eq $agentPs1) { 'agent' } else { 'backend' }
    $other = if ($role -eq 'agent') { 'backend' } else { 'agent' }
    Check "$leaf writes the pair to the $role key in one call" `
        ($src -match "if \(\`$NatsUser\) \{\s*Set-KanadeRegistrySecrets -Subkey '$role' -Values @\{ NatsUser = \`$NatsUser; NatsPassword = \`$NatsPassword \}")
    Check "$leaf never writes NatsUser/NatsPassword under the $other key" `
        (-not ($src -match "-Subkey '$other'[^\r\n]*Nats(User|Password)") -and -not ($src -match "Subkey '$other' -Values @\{ NatsUser"))
    Check "$leaf has no shared-user key" (-not ($src -match "NatsUser[^\r\n]*kanade\\\\shared|kanade\\\\nats\\\\"))
    Check "$leaf only writes the pair when one was given (a re-run without it leaves it alone)" `
        ($src -notmatch 'Remove-ItemProperty|DeleteValue')
    Check "$leaf passes no secret to Write-Host/Start-Transcript" `
        ($src -notmatch 'Start-Transcript' -and $src -notmatch 'Write-(Host|Output|Verbose|Debug)[^\r\n]*\$NatsPassword')
}

# --- registry behaviour: Windows + elevated only ---
$elevated = $false
if ($isWin) {
    $elevated = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}
if (-not ($isWin -and $elevated)) {
    Write-Host '  SKIP  registry checks (need Windows and an elevated shell)'
} else {
    foreach ($p in @($agentPs1, $backendPs1)) {
        $leaf = Split-Path -Leaf $p
        Import-ScriptFunctions $p $fnNames
        $base = 'SOFTWARE\kanade-test-' + [guid]::NewGuid().ToString('N')
        $root = [Microsoft.Win32.Registry]::CurrentUser
        $sub = 'agent'
        try {
            $null = Set-KanadeRegistrySecrets -Subkey $sub -BasePath $base -Root $root -Values @{ NatsUser = 'u 1'; NatsPassword = $secret } 6>&1
            $k = $root.OpenSubKey("$base\$sub")
            Check "$leaf registry: user written" ($k.GetValue('NatsUser') -eq 'u 1')
            Check "$leaf registry: password round-trips special characters" ($k.GetValue('NatsPassword') -ceq $secret)
            Check "$leaf registry: stored as REG_SZ" (($k.GetValueKind('NatsUser') -eq 'String') -and ($k.GetValueKind('NatsPassword') -eq 'String'))
            $sec = $k.GetAccessControl()
            $sids = @($sec.GetAccessRules($true, $false, [System.Security.Principal.SecurityIdentifier]) | ForEach-Object { $_.IdentityReference.Value } | Sort-Object)
            Check "$leaf registry: ACL is SYSTEM + Administrators only" (($sids -join ',') -eq 'S-1-5-18,S-1-5-32-544')
            Check "$leaf registry: ACL does not inherit" $sec.AreAccessRulesProtected
            $k.Close()

            # Same values again: idempotent.
            $null = Set-KanadeRegistrySecrets -Subkey $sub -BasePath $base -Root $root -Values @{ NatsUser = 'u 1'; NatsPassword = $secret } 6>&1
            $k = $root.OpenSubKey("$base\$sub")
            Check "$leaf registry: same values twice is a no-op" (($k.GetValue('NatsUser') -eq 'u 1') -and ($k.GetValue('NatsPassword') -ceq $secret))
            $k.Close()

            # A later token-only run writes another value and leaves the pair.
            $null = Set-KanadeRegistrySecrets -Subkey $sub -BasePath $base -Root $root -Values @{ NatsToken = 'tok' } 6>&1
            $k = $root.OpenSubKey("$base\$sub")
            Check "$leaf registry: a run without the pair leaves it untouched" (($k.GetValue('NatsUser') -eq 'u 1') -and ($k.GetValue('NatsPassword') -ceq $secret))
            $k.Close()

            # New values replace both halves together.
            $null = Set-KanadeRegistrySecrets -Subkey $sub -BasePath $base -Root $root -Values @{ NatsUser = 'u2'; NatsPassword = 'p2' } 6>&1
            $k = $root.OpenSubKey("$base\$sub")
            Check "$leaf registry: new values replace both halves" (($k.GetValue('NatsUser') -eq 'u2') -and ($k.GetValue('NatsPassword') -eq 'p2'))
            $k.Close()

            $out = Set-KanadeRegistrySecrets -Subkey $sub -BasePath $base -Root $root -Values @{ NatsUser = 'u2'; NatsPassword = $secret } 6>&1 | ForEach-Object { "$_" }
            Check "$leaf registry: nothing printed contains the password" (-not (($out -join "`n").Contains($secret)))
        } finally {
            $root.DeleteSubKeyTree($base, $false)
        }
    }
}

if ($fail) { Write-Host "`n$fail FAILED"; exit 1 } else { Write-Host "`nall checks passed"; exit 0 }

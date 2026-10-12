<#
.SYNOPSIS
  Mint the bcrypt hash the broker's users configuration takes in place of a
  plaintext password, without echoing it or leaving it in history or a file.

.DESCRIPTION
  Prompts for the password twice (masked) and writes ONLY the hash to the
  output stream. Wraps `nats server passwd` from the nats CLI
  (https://github.com/nats-io/natscli/releases), so no hashing dependency is
  added. The password reaches the CLI in the child's environment (PASSWORD),
  never on a command line. The CLI refuses passwords under 10 characters. The
  result is checked to be a `$2a$` hash, the only form nats-server recognises.

  Defaults to cost 8; use -Cost to override. Costs below 10 are appropriate
  only for long random passwords. Short or obviously non-random inputs warn
  on stderr without adding a refusal. The heuristic cannot prove randomness
  or guarantee password strength. See book/src/operations/nats-user-switch.md
  for broker burst measurements.

  Use the output with deploy-nats.ps1 -AgentPasswordHash / -BackendPasswordHash
  / -BreakglassPasswordHash, or setup.sh's KANADE_NATS_<ROLE>_PASSWORD_HASH.

.EXAMPLE
  PS> $agentHash = .\nats-password-hash.ps1
#>
[CmdletBinding()]
param([int]$Cost = 8)

$ErrorActionPreference = 'Stop'
if (-not (Get-Command nats -ErrorAction SilentlyContinue)) {
    throw "the nats CLI is required (it provides 'nats server passwd'): https://github.com/nats-io/natscli/releases"
}

function ConvertFrom-Secure([securestring]$s) {
    $b = [Runtime.InteropServices.Marshal]::SecureStringToBSTR($s)
    try { [Runtime.InteropServices.Marshal]::PtrToStringBSTR($b) }
    finally { [Runtime.InteropServices.Marshal]::ZeroFreeBSTR($b) }
}
$p1 = ConvertFrom-Secure (Read-Host 'Password' -AsSecureString)
$p2 = ConvertFrom-Secure (Read-Host 'Again' -AsSecureString)
if (-not $p1 -or $p1 -cne $p2) { throw 'passwords are empty or do not match' }

if ($Cost -lt 10 -and ($p1.Length -lt 24 -or $p1 -cmatch '^[a-zA-Z]+$' -or
        $p1 -cmatch '^[0-9]+$' -or $p1 -match 'password|qwerty|letmein|welcome|admin|kanade')) {
    [Console]::Error.WriteLine('warning: a low bcrypt cost is only appropriate for long random passwords (at least 24 characters). Mint a random password, for example:')
    # Rejection sampling avoids modulo bias: 248 is a multiple of 62.
    [Console]::Error.WriteLine('  $rng = [System.Security.Cryptography.RandomNumberGenerator]::Create(); try { $chars = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789"; $byte = New-Object byte[] 1; $pw = ""; while ($pw.Length -lt 32) { $rng.GetBytes($byte); if ($byte[0] -lt 248) { $pw += $chars[$byte[0] % 62] } }; $pw } finally { $rng.Dispose() }')
}

$env:PASSWORD = $p1
try { $hash = (& nats server passwd --cost $Cost | Out-String).Trim() }
finally { Remove-Item Env:PASSWORD -ErrorAction SilentlyContinue }
if ($LASTEXITCODE -ne 0) { throw 'nats server passwd failed' }
if ($hash -cnotmatch '\A\$2a\$\d\d\$[./A-Za-z0-9]{53}\z') { throw 'nats server passwd did not return a bcrypt hash' }
$hash

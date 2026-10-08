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

  Use the output with deploy-nats.ps1 -AgentPasswordHash / -BackendPasswordHash
  / -BreakglassPasswordHash, or setup.sh's KANADE_NATS_<ROLE>_PASSWORD_HASH.

.EXAMPLE
  PS> $agentHash = .\nats-password-hash.ps1
#>
[CmdletBinding()]
param([int]$Cost = 11)

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

$env:PASSWORD = $p1
try { $hash = (& nats server passwd --cost $Cost | Out-String).Trim() }
finally { Remove-Item Env:PASSWORD -ErrorAction SilentlyContinue }
if ($LASTEXITCODE -ne 0) { throw 'nats server passwd failed' }
if ($hash -cnotmatch '\A\$2a\$\d\d\$[./A-Za-z0-9]{53}\z') { throw 'nats server passwd did not return a bcrypt hash' }
$hash

<#
.SYNOPSIS
  Assemble a fully-offline kanade AGENT deployment bundle, on Windows.

.DESCRIPTION
  Windows-native equivalent of bundle-agent.sh. Run on a machine WITH
  internet (your dev box / minipc) — NOT the deployment target. Collects
  the agent binary, its config, the systemd unit, and the installer into
  one tarball. Copy it to the target, extract, and run ./setup-agent.sh
  there.

  The agent binary is not built here: point -Agent at a prebuilt Linux
  kanade-agent (a GitHub Release asset,
  kanade-agent-<arch>-unknown-linux-musl.tar.gz, extracted) matching the
  TARGET's architecture. Windows cannot cross-build it, but it can
  collect it.

  Uses only tools present on Windows 10+/PowerShell 7: the built-in
  tar.exe and Get-FileHash. Existing repo files are byte-copied (no text
  rewrite), so LF line endings survive; setup-agent.sh re-applies 0755 on
  install, so the archive need not carry exec bits.

.EXAMPLE
  .\bundle-agent.ps1 -Agent C:\path\to\kanade-agent
#>
[CmdletBinding()]
param(
	[Parameter(Mandatory = $true)][string]$Agent,
	[string]$Out = '.'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

# -PathType Leaf: reject a directory, which would otherwise be staged as
# a bogus "binary" that setup-agent.sh only catches after it has begun
# changing the target.
if (-not (Test-Path -LiteralPath $Agent -PathType Leaf)) { throw "agent binary not found (or not a file): $Agent" }
$here = $PSScriptRoot
$repoRoot = (Resolve-Path (Join-Path $here '..\..')).Path

$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("kabundle-" + [System.IO.Path]::GetRandomFileName())
$root = Join-Path $stage 'kanade-linux-agent-bundle'
New-Item -ItemType Directory -Force -Path (Join-Path $root 'bin'), (Join-Path $root 'etc'), (Join-Path $root 'systemd') | Out-Null
try {
	Write-Host "==> agent: $Agent"
	Copy-Item -LiteralPath $Agent -Destination (Join-Path $root 'bin\kanade-agent')

	Write-Host "==> config, unit, installer (byte-copied, LF preserved)"
	Copy-Item -LiteralPath (Join-Path $repoRoot 'configs\agent.toml')        -Destination (Join-Path $root 'etc\agent.toml')
	Copy-Item -LiteralPath (Join-Path $here 'systemd\kanade-agent.service')  -Destination (Join-Path $root 'systemd\kanade-agent.service')
	Copy-Item -LiteralPath (Join-Path $here 'setup-agent.sh')               -Destination (Join-Path $root 'setup-agent.sh')

	New-Item -ItemType Directory -Force -Path $Out | Out-Null
	$outFile = Join-Path ((Resolve-Path $Out).Path) 'kanade-linux-agent-bundle.tar.gz'
	& tar.exe -C $stage -czf $outFile 'kanade-linux-agent-bundle'
	$sum = (Get-FileHash -Algorithm SHA256 -LiteralPath $outFile).Hash
	"$sum  kanade-linux-agent-bundle.tar.gz" | Set-Content -LiteralPath "$outFile.sha256" -NoNewline

	Write-Host ""
	Write-Host "==> Bundle: $outFile"
	Write-Host "    sha256: $sum"
	Write-Host "    Copy to the target, then:"
	Write-Host "      tar -xzf kanade-linux-agent-bundle.tar.gz"
	Write-Host "      cd kanade-linux-agent-bundle"
	Write-Host "      sudo bash ./setup-agent.sh                  # co-located with a backend"
	Write-Host "      # or, standalone against a remote broker:"
	Write-Host "      sudo KANADE_NATS_URL=wss://nats.kanade.example.com KANADE_NATS_TOKEN=<token> bash ./setup-agent.sh"
}
finally {
	Remove-Item -Recurse -Force -LiteralPath $stage -ErrorAction SilentlyContinue
}

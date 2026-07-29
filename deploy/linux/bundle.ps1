<#
.SYNOPSIS
  Assemble a fully-offline ARM64 Linux deployment bundle, on Windows.

.DESCRIPTION
  Windows-native equivalent of bundle.sh. Run on a machine WITH internet
  (your dev box / minipc) — NOT the deployment server. Collects the three
  binaries and the config/units/scripts into one tarball. Copy it to the
  closed-network server, extract, and run ./setup.sh there.

  The backend binary is not built here: point -Backend at a prebuilt
  aarch64-linux kanade-backend (a GitHub Release asset once the
  aarch64-linux target lands, #1174; or the output of build-aarch64.sh on
  an ARM64 build box until then). Windows cannot cross-build it, but it can
  collect it.

  Uses only tools present on Windows 10+/PowerShell 7: Invoke-WebRequest,
  Get-FileHash, and the built-in tar.exe. Existing repo files are
  byte-copied (no text rewrite), so LF line endings survive; setup.sh
  re-applies 0755 on install, so the archive need not carry exec bits.

.EXAMPLE
  .\bundle.ps1 -Backend C:\path\to\kanade-backend
#>
[CmdletBinding()]
param(
	[Parameter(Mandatory = $true)][string]$Backend,
	# Must match the architecture of the -Backend binary you pass; there
	# is no way to verify that here (build-release.ps1 keeps them in step).
	[ValidateSet('x86_64', 'aarch64')][string]$Arch = 'aarch64',
	[string]$Out = '.',
	[string]$NatsVersion = 'v2.14.3',
	[string]$CaddyVersion = '2.10.2'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'   # faster Invoke-WebRequest

if (-not (Test-Path -LiteralPath $Backend -PathType Leaf)) { throw "backend binary not found (or not a file): $Backend" }
$here = $PSScriptRoot
$repoRoot = (Resolve-Path (Join-Path $here '..\..')).Path

# Map the user-facing arch (x86_64 / aarch64) to the nats/caddy asset arch
# (amd64 / arm64).
$dlArch = if ($Arch -eq 'x86_64') { 'amd64' } else { 'arm64' }
$bundleName = "kanade-linux-$Arch-bundle"

function Get-Checked([string]$Url, [string]$OutFile, [string]$SumsUrl, [string]$Name, [string]$Algo = 'SHA256') {
	# Download $Url to $OutFile and verify it against the checksum for
	# $Name listed in the file at $SumsUrl (format: "<hash>  <name>").
	# $Algo differs per project: NATS's SHA256SUMS is SHA-256, Caddy's
	# checksums.txt is SHA-512.
	Invoke-WebRequest -Uri $Url -OutFile $OutFile
	$sums = Join-Path (Split-Path $OutFile) 'SUMS.txt'
	Invoke-WebRequest -Uri $SumsUrl -OutFile $sums
	$line = Select-String -Path $sums -SimpleMatch $Name | Select-Object -First 1
	if (-not $line) { throw "no checksum entry for $Name" }
	$want = ($line.Line -split '\s+')[0]
	$got = (Get-FileHash -Algorithm $Algo -LiteralPath $OutFile).Hash
	if ($got -ne $want) { throw "checksum mismatch for $Name`n  want $want`n  got  $got" }
}

$stage = Join-Path ([System.IO.Path]::GetTempPath()) ("kbundle-" + [System.IO.Path]::GetRandomFileName())
$root = Join-Path $stage $bundleName
New-Item -ItemType Directory -Force -Path (Join-Path $root 'bin'), (Join-Path $root 'etc'), (Join-Path $root 'systemd') | Out-Null
try {
	Write-Host "==> backend: $Backend"
	Copy-Item -LiteralPath $Backend -Destination (Join-Path $root 'bin\kanade-backend')

	$dl = Join-Path $stage 'dl'
	New-Item -ItemType Directory -Force -Path $dl | Out-Null

	Write-Host "==> nats-server $NatsVersion (linux-$dlArch), checksum-verified"
	$nbase = "nats-server-$NatsVersion-linux-$dlArch.tar.gz"
	$nrel = "https://github.com/nats-io/nats-server/releases/download/$NatsVersion"
	$ntar = Join-Path $dl $nbase
	Get-Checked "$nrel/$nbase" $ntar "$nrel/SHA256SUMS" $nbase
	& tar.exe -xzf $ntar -C $dl
	Copy-Item -LiteralPath (Join-Path $dl "nats-server-$NatsVersion-linux-$dlArch\nats-server") -Destination (Join-Path $root 'bin\nats-server')

	Write-Host "==> caddy $CaddyVersion (linux-$dlArch), checksum-verified"
	$cbase = "caddy_${CaddyVersion}_linux_$dlArch.tar.gz"
	$crel = "https://github.com/caddyserver/caddy/releases/download/v$CaddyVersion"
	$ctar = Join-Path $dl $cbase
	Get-Checked "$crel/$cbase" $ctar "$crel/caddy_${CaddyVersion}_checksums.txt" $cbase 'SHA512'
	& tar.exe -xzf $ctar -C $dl
	Copy-Item -LiteralPath (Join-Path $dl 'caddy') -Destination (Join-Path $root 'bin\caddy')

	Write-Host "==> configs, units, installer (byte-copied, LF preserved)"
	Copy-Item -LiteralPath (Join-Path $here 'nats-server.conf')           -Destination (Join-Path $root 'etc\nats-server.conf')
	Copy-Item -LiteralPath (Join-Path $here 'Caddyfile')                  -Destination (Join-Path $root 'etc\Caddyfile')
	Copy-Item -LiteralPath (Join-Path $repoRoot 'configs\backend.toml')   -Destination (Join-Path $root 'etc\backend.toml')
	Copy-Item -LiteralPath (Join-Path $here 'systemd\nats-server.service')    -Destination (Join-Path $root 'systemd\nats-server.service')
	Copy-Item -LiteralPath (Join-Path $here 'systemd\kanade-backend.service') -Destination (Join-Path $root 'systemd\kanade-backend.service')
	Copy-Item -LiteralPath (Join-Path $here 'systemd\caddy.service')          -Destination (Join-Path $root 'systemd\caddy.service')
	Copy-Item -LiteralPath (Join-Path $here 'setup.sh')                   -Destination (Join-Path $root 'setup.sh')
	Copy-Item -LiteralPath (Join-Path $here 'README.md')                  -Destination (Join-Path $root 'README.md')

	New-Item -ItemType Directory -Force -Path $Out | Out-Null
	$archiveName = "$bundleName.tar.gz"
	$outFile = Join-Path ((Resolve-Path $Out).Path) $archiveName
	& tar.exe -C $stage -czf $outFile $bundleName
	$sum = (Get-FileHash -Algorithm SHA256 -LiteralPath $outFile).Hash
	"$sum  $archiveName" | Set-Content -LiteralPath "$outFile.sha256" -NoNewline

	Write-Host ""
	Write-Host "==> Bundle: $outFile"
	Write-Host "    sha256: $sum"
	Write-Host "    Copy to the server, then:"
	Write-Host "      tar -xzf $archiveName"
	Write-Host "      cd $bundleName"
	Write-Host "      sudo KANADE_DOMAIN=kanade.example.com bash ./setup.sh"
}
finally {
	Remove-Item -Recurse -Force -LiteralPath $stage -ErrorAction SilentlyContinue
}

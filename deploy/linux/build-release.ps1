<#
.SYNOPSIS
  One role-parameterized collector for the Linux deploy bundles, on
  Windows — the symmetric counterpart of scripts\build-release.ps1 (which
  stages the WINDOWS fleet). This one emits LINUX bundles.

.DESCRIPTION
  Windows-native twin of build-release.sh. Run on a machine WITH internet
  (dev box / minipc) — NOT the deployment target. Fetches the kanade role
  binaries for the target arch from GitHub Releases and hands each to the
  matching Linux bundler (bundle.ps1 / bundle-agent.ps1), producing one
  tarball per role. The tarballs install offline via setup.sh /
  setup-agent.sh on the Linux target.

  Pass -BackendBin / -AgentBin to use a locally supplied binary instead of
  downloading that role.

.EXAMPLE
  # Both roles, latest workspace version, for an x86_64 target:
  PS> .\build-release.ps1 -Arch x86_64

.EXAMPLE
  # Just the agent, a pinned version:
  PS> .\build-release.ps1 -Roles agent -Version 0.44.35 -Arch aarch64
#>
[CmdletBinding()]
param(
	[string[]]$Roles = @('backend', 'agent'),
	[string]  $Version,
	[ValidateSet('x86_64', 'aarch64')][string]$Arch = 'x86_64',
	[string]  $Out = 'dist',
	[string]  $GitHubRepo = 'yukimemi/kanade',
	[string]  $BackendBin,
	[string]  $AgentBin,
	[string]  $NatsVersion = 'v2.14.3',
	[string]  $CaddyVersion = '2.10.2'
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$here = $PSScriptRoot
$repoRoot = (Resolve-Path (Join-Path $here '..\..')).Path

if (-not $Version) {
	$m = Select-String -Path (Join-Path $repoRoot 'Cargo.toml') -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
	if (-not $m) { throw "couldn't read version from Cargo.toml — pass -Version." }
	$Version = $m.Matches[0].Groups[1].Value
}
$tag = "v" + ($Version -replace '^v', '')

if (-not [System.IO.Path]::IsPathRooted($Out)) { $Out = Join-Path $repoRoot $Out }
$null = New-Item -ItemType Directory -Force -Path $Out

# Download a role's release binary for $Arch and return the extracted path.
function Get-RoleBin([string]$Crate) {
	$target = "$Arch-unknown-linux-musl"
	$asset = "$Crate-$target.tar.gz"
	$url = "https://github.com/$GitHubRepo/releases/download/$tag/$asset"
	$d = Join-Path ([System.IO.Path]::GetTempPath()) ("kbr-" + [System.IO.Path]::GetRandomFileName())
	New-Item -ItemType Directory -Force -Path $d | Out-Null
	Write-Host "    downloading $url"
	Invoke-WebRequest -Uri $url -OutFile (Join-Path $d $asset)
	& tar.exe -xzf (Join-Path $d $asset) -C $d
	# The archived binary carries the target triple
	# (kanade-backend-x86_64-unknown-linux-musl), not the bare crate name,
	# so match on the crate prefix.
	$bin = Get-ChildItem -Path $d -Recurse -File | Where-Object { $_.Name -like "$Crate*" } | Select-Object -First 1
	if (-not $bin) { throw "no '$Crate*' binary found inside $asset" }
	return $bin.FullName
}

Write-Host "==> build-release: roles=[$($Roles -join ',')] version=$tag arch=$Arch out=$Out"

foreach ($role in $Roles) {
	$role = $role.Trim()
	if (-not $role) { continue }
	Write-Host ""
	Write-Host "=== $role ==="
	switch ($role) {
		'backend' {
			$bin = if ($BackendBin) { $BackendBin } else { Get-RoleBin 'kanade-backend' }
			& (Join-Path $here 'bundle.ps1') -Backend $bin -Arch $Arch -Out $Out `
				-NatsVersion $NatsVersion -CaddyVersion $CaddyVersion
		}
		'agent' {
			$bin = if ($AgentBin) { $AgentBin } else { Get-RoleBin 'kanade-agent' }
			& (Join-Path $here 'bundle-agent.ps1') -Agent $bin -Out $Out
		}
		default { throw "unknown role '$role' (supported: backend, agent)" }
	}
}

Write-Host ""
Write-Host "==> Done. Bundles in $Out :"
Get-ChildItem -Path $Out -Filter 'kanade-linux-*bundle.tar.gz' | ForEach-Object { Write-Host "    $($_.Name)" }

#Requires -Version 5.1

<#
.SYNOPSIS
  Build deploy-ready stage folders for kanade-agent and kanade-backend.

.DESCRIPTION
  Runs on a build host (the one with the Rust toolchain installed) to
  produce one folder per role under -OutDir, ready to be copied onto a
  target host and run with the matching deploy-<role>.ps1 as Admin.

  By default it pulls the binary from crates.io with `cargo install`
  at the version recorded in this repo's workspace Cargo.toml, so
  every operator pulls bit-identical builds. Pass -FromSource to
  build from the local checkout instead (useful for unreleased dev
  builds).

  Output layout, per role:

    <OutDir>\<role>\
      ├── kanade-<role>.exe
      ├── <role>.toml             (edit before deploy)
      └── deploy-<role>.ps1       (run on the target as Admin)

  Pass -Zip to also produce <OutDir>\<role>.zip (Compress-Archive)
  for handoff via filesharing tools that prefer single archives.

.PARAMETER Version
  Crate version to install from crates.io. Defaults to the version
  in the workspace Cargo.toml. Ignored under -FromSource.

.PARAMETER OutDir
  Stage root, resolved relative to the repo root. Default: 'dist'.

.PARAMETER FromSource
  Use `cargo install --path crates\kanade-<role>` instead of pulling
  from crates.io. Builds the current working-tree code.

.PARAMETER Zip
  After staging, Compress-Archive each role folder into a sibling
  .zip. Useful when the operator's transport prefers a single file.

.PARAMETER Roles
  Which roles to stage. Default: agent + backend. Pass a subset
  (e.g. -Roles agent) to skip one.

.EXAMPLE
  # Stage release matching the workspace version (the common case):
  PS> .\scripts\build-release.ps1

.EXAMPLE
  # Stage from source, agent only, and zip the result:
  PS> .\scripts\build-release.ps1 -FromSource -Roles agent -Zip

.EXAMPLE
  # Pin a specific crates.io version (e.g. for a hotfix older than HEAD):
  PS> .\scripts\build-release.ps1 -Version 0.1.4
#>

[CmdletBinding()]
param(
    [string]  $Version,
    [string]  $OutDir = 'dist',
    [switch]  $FromSource,
    [switch]  $Zip,
    [string[]]$Roles = @('agent', 'backend')
)

$ErrorActionPreference = 'Stop'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo not found on PATH. Run this on a build host with the Rust toolchain installed."
}

$repoRoot = Split-Path -Parent $PSScriptRoot

if (-not $Version) {
    $cargoToml = Join-Path $repoRoot 'Cargo.toml'
    $match = Select-String -Path $cargoToml -Pattern '^version\s*=\s*"([^"]+)"' | Select-Object -First 1
    if (-not $match) {
        throw "Couldn't find a top-level version line in '$cargoToml'. Pass -Version explicitly."
    }
    $Version = $match.Matches[0].Groups[1].Value
}

if (-not [System.IO.Path]::IsPathRooted($OutDir)) {
    $OutDir = Join-Path $repoRoot $OutDir
}
$null = New-Item -ItemType Directory -Path $OutDir -Force

Write-Host ("Staging kanade v{0} into {1}" -f $Version, $OutDir)
if ($FromSource) { Write-Host "Source: local checkout ($repoRoot)" }
else             { Write-Host "Source: crates.io" }

foreach ($role in $Roles) {
    $crate    = "kanade-$role"
    $exeName  = "$crate.exe"
    $cfgName  = "$role.toml"
    $deployPs = "deploy-$role.ps1"
    $stage    = Join-Path $OutDir $role

    Write-Host ''
    Write-Host "=== $role ==="

    $cfgSrc    = Join-Path $repoRoot $cfgName
    $deploySrc = Join-Path $repoRoot "scripts\$deployPs"
    if (-not (Test-Path $cfgSrc))    { throw "Missing $cfgName in repo root ($cfgSrc)." }
    if (-not (Test-Path $deploySrc)) { throw "Missing $deployPs under scripts\ ($deploySrc)." }

    if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
    $null = New-Item -ItemType Directory -Path $stage

    $temp = Join-Path ([System.IO.Path]::GetTempPath()) ("kanade-stage-{0}-{1}" -f $role, [System.Guid]::NewGuid().ToString('N'))
    try {
        if ($FromSource) {
            $cratePath = Join-Path $repoRoot "crates\$crate"
            if (-not (Test-Path $cratePath)) { throw "Missing crate path '$cratePath'." }
            Write-Host "cargo install --path $cratePath --root $temp"
            & cargo install --root $temp --locked --path $cratePath
        } else {
            Write-Host "cargo install $crate@$Version --root $temp"
            & cargo install --root $temp --version $Version $crate
        }
        if ($LASTEXITCODE -ne 0) { throw "cargo install failed for $crate (exit $LASTEXITCODE)." }

        $exeSrc = Join-Path $temp "bin\$exeName"
        if (-not (Test-Path $exeSrc)) { throw "Built binary not found at '$exeSrc'." }

        Copy-Item $exeSrc    (Join-Path $stage $exeName)
        Copy-Item $cfgSrc    (Join-Path $stage $cfgName)
        Copy-Item $deploySrc (Join-Path $stage $deployPs)

        Write-Host "Staged $stage"
        Get-ChildItem -Path $stage | ForEach-Object { Write-Host ("  {0,-30}  {1,10:N0} bytes" -f $_.Name, $_.Length) }

        if ($Zip) {
            $zipPath = "$stage.zip"
            if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
            Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zipPath
            Write-Host "Archived $zipPath"
        }
    }
    finally {
        if (Test-Path $temp) { Remove-Item -Recurse -Force $temp }
    }
}

Write-Host ''
Write-Host "Done. Copy each stage folder (or zip) to the target host and run"
Write-Host "the matching deploy-<role>.ps1 as Administrator."

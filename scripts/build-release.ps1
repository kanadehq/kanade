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

  Reruns are cheap: a persistent Cargo target dir (`-TargetDir`,
  default `<repo>\.cargo-stage-cache`) keeps build artifacts between
  invocations, and if the stage already has a binary whose
  `--version` matches the requested version the cargo install is
  skipped entirely. Pass `-Force` to rebuild regardless, and note
  that `-FromSource` always rebuilds (working-tree code may differ
  from what `--version` reports).

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

.PARAMETER TargetDir
  Cargo `--target-dir` for cached build artifacts shared across
  runs. Default: `<repo>\.cargo-stage-cache`. Shared between roles
  on purpose — agent and backend reuse most of the dep graph
  (tokio / serde / async-nats / tracing / …) so a shared cache
  shrinks the total artefact footprint.

.PARAMETER Force
  Rebuild every selected role even when the staged binary already
  reports the requested version. The default fast-path is intended
  for "I edited the deploy script and want to re-stage" reruns; pass
  -Force when you need cargo to actually re-resolve / re-link.

.EXAMPLE
  # Stage release matching the workspace version (the common case):
  PS> .\scripts\build-release.ps1

.EXAMPLE
  # Stage from source, agent only, and zip the result:
  PS> .\scripts\build-release.ps1 -FromSource -Roles agent -Zip

.EXAMPLE
  # Pin a specific crates.io version (e.g. for a hotfix older than HEAD):
  PS> .\scripts\build-release.ps1 -Version 0.1.4

.EXAMPLE
  # Force a clean rebuild despite the cached stage being up to date:
  PS> .\scripts\build-release.ps1 -Force
#>

[CmdletBinding()]
param(
    [string]  $Version,
    [string]  $OutDir    = 'dist',
    [switch]  $FromSource,
    [switch]  $Zip,
    [string[]]$Roles     = @('agent', 'backend'),
    [string]  $TargetDir,
    [switch]  $Force
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

if (-not $TargetDir) {
    $TargetDir = Join-Path $repoRoot '.cargo-stage-cache'
}
if (-not [System.IO.Path]::IsPathRooted($TargetDir)) {
    $TargetDir = Join-Path $repoRoot $TargetDir
}
$null = New-Item -ItemType Directory -Path $TargetDir -Force

Write-Host ("Staging kanade v{0} into {1}" -f $Version, $OutDir)
if ($FromSource) { Write-Host "Source: local checkout ($repoRoot)" }
else             { Write-Host "Source: crates.io" }
Write-Host ("Cache:  {0}" -f $TargetDir)

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

    $exeDst = Join-Path $stage $exeName

    # Fast-path: if the staged binary already reports the requested
    # version, skip cargo install entirely. -FromSource always rebuilds
    # (working-tree code may differ from what --version reports).
    $skipBuild = $false
    if (-not $FromSource -and -not $Force -and (Test-Path $exeDst)) {
        try {
            $verLine = (& $exeDst --version 2>$null) | Select-Object -First 1
            if ($verLine) {
                $installed = ($verLine -split '\s+', 2)[-1].Trim()
                if ($installed -eq $Version) {
                    Write-Host "Cached: $exeName already at v$Version (pass -Force to rebuild)."
                    $skipBuild = $true
                } else {
                    Write-Host "Stale: $exeName reports '$installed', want '$Version' — rebuilding."
                }
            }
        } catch {
            # Couldn't run the staged exe — fall through to a rebuild.
        }
    }

    if (-not $skipBuild) {
        # Don't blow the stage dir away wholesale — if anything (e.g.,
        # Windows Defender mid-scan, a running kanade-<role>.exe, an
        # open Explorer preview) holds even one file inside, the
        # recursive Remove-Item fails noisily. Just ensure the dir
        # exists; Copy-Item -Force below overwrites individual files,
        # and any locked file produces a focused error pointing at
        # that one path instead of taking down the whole rebuild.
        if (-not (Test-Path $stage)) {
            $null = New-Item -ItemType Directory -Path $stage
        }

        $temp = Join-Path ([System.IO.Path]::GetTempPath()) ("kanade-stage-{0}-{1}" -f $role, [System.Guid]::NewGuid().ToString('N'))
        try {
            if ($FromSource) {
                $cratePath = Join-Path $repoRoot "crates\$crate"
                if (-not (Test-Path $cratePath)) { throw "Missing crate path '$cratePath'." }
                Write-Host "cargo install --path $cratePath --root $temp --target-dir $TargetDir"
                & cargo install --root $temp --locked --path $cratePath --target-dir $TargetDir
            } else {
                Write-Host "cargo install $crate@$Version --root $temp --target-dir $TargetDir"
                & cargo install --root $temp --version $Version $crate --target-dir $TargetDir
            }
            if ($LASTEXITCODE -ne 0) { throw "cargo install failed for $crate (exit $LASTEXITCODE)." }

            $exeSrc = Join-Path $temp "bin\$exeName"
            if (-not (Test-Path $exeSrc)) { throw "Built binary not found at '$exeSrc'." }
            try {
                Copy-Item $exeSrc $exeDst -Force
            } catch [System.IO.IOException] {
                throw @"
$exeDst is locked (file in use by another process). Likely culprits:
  - Windows Defender real-time scan finishing on the previous build
    (usually clears in seconds; just rerun).
  - A still-running kanade-$role.exe (Get-Process kanade-$role).
  - The Windows service (Stop-Service Kanade$($role -replace '^(.)', { $_.Value.ToUpper() })
    or use deploy-$role.ps1 -Recreate after the next stage).
Original error: $($_.Exception.Message)
"@
            }
        }
        finally {
            if (Test-Path $temp) { Remove-Item -Recurse -Force $temp }
        }
    }

    # Always refresh the non-binary artefacts. They're tiny and may
    # have been edited (config tweaks, deploy-script bumps) since the
    # last stage even when the exe didn't change.
    if (-not (Test-Path $stage)) { $null = New-Item -ItemType Directory -Path $stage }
    Copy-Item $cfgSrc    (Join-Path $stage $cfgName)    -Force
    Copy-Item $deploySrc (Join-Path $stage $deployPs)   -Force

    Write-Host "Staged $stage"
    Get-ChildItem -Path $stage | ForEach-Object { Write-Host ("  {0,-30}  {1,10:N0} bytes" -f $_.Name, $_.Length) }

    if ($Zip) {
        $zipPath = "$stage.zip"
        if (Test-Path $zipPath) { Remove-Item -Force $zipPath }
        Compress-Archive -Path (Join-Path $stage '*') -DestinationPath $zipPath
        Write-Host "Archived $zipPath"
    }
}

Write-Host ''
Write-Host "Done. Copy each stage folder (or zip) to the target host and run"
Write-Host "the matching deploy-<role>.ps1 as Administrator."

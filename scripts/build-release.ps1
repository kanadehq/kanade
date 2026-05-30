#Requires -Version 5.1

<#
.SYNOPSIS
  Build deploy-ready stage folders for kanade-agent, kanade-backend, and nats-server.

.DESCRIPTION
  Runs on a host with no development tooling at all — by default it
  downloads pre-built binaries straight from GitHub Releases over
  HTTPS (Invoke-WebRequest), so a clean Windows box can assemble a
  full kanade deploy bundle without Rust / bun / git installed.

  Pass -FromSource to compile from the local checkout (needs cargo +
  bun for kanade-backend's SPA bundle), or -FromCrates to install via
  cargo from crates.io (needs cargo).

  Output layout, per role:

    <OutDir>\<role>\
      ├── kanade-<role>.exe   (or nats-server.exe for the nats role)
      ├── <role>.toml         (or nats-server.conf — edit before deploy)
      └── deploy-<role>.ps1   (run on the target as Admin)

  Default roles include nats, so a clean checkout produces everything
  needed to bootstrap a fleet (the agent, the backend, and the
  broker) in one invocation. Pass -Roles to restrict.

  Pass -Zip to also produce <OutDir>\<role>.zip (Compress-Archive)
  for handoff via filesharing tools that prefer single archives.

  Reruns are cheap: a persistent Cargo target dir (`-TargetDir`,
  default `<repo>\.cargo-stage-cache`) keeps build artifacts between
  invocations, and if the stage already has a binary whose
  `--version` matches the requested version the fetch is skipped
  entirely. `-Force` bypasses the cache; `-FromSource` always
  rebuilds (working-tree code may differ from what `--version`
  reports).

.PARAMETER Version
  kanade release tag to download (or crate version under -FromCrates).
  Defaults to the version in the workspace Cargo.toml.

.PARAMETER NatsVersion
  nats-server release tag to download from nats-io/nats-server.
  Defaults to a pinned recent stable. Operators are expected to bump
  this to match the broker version they want to run.

.PARAMETER OutDir
  Stage root, resolved relative to the repo root. Default: 'dist'.

.PARAMETER FromSource
  Use `cargo install --path crates\kanade-<role>` instead of pulling
  from Releases / crates.io. Builds the current working-tree code.
  Implies cargo on PATH; for the backend role, also bun (Vite SPA bundle).

.PARAMETER FromCrates
  Use `cargo install kanade-<role>` from crates.io instead of pulling
  from Releases. Implies cargo on PATH.

.PARAMETER Zip
  After staging, Compress-Archive each role folder into a sibling
  .zip.

.PARAMETER Roles
  Which roles to stage. Default: agent, backend, nats.

.PARAMETER TargetDir
  Cargo `--target-dir` for cached build artifacts shared across runs.
  Default: `<repo>\.cargo-stage-cache`. Only used for -FromSource / -FromCrates.

.PARAMETER GitHubRepo
  GitHub `owner/repo` to pull kanade release assets from. Default:
  `yukimemi/kanade`. Override if you fork.

.PARAMETER Force
  Rebuild every selected role even when the staged binary already
  reports the requested version.

.EXAMPLE
  # No-toolchain bootstrap (the common case on a fresh ops jump box):
  PS> .\scripts\build-release.ps1

.EXAMPLE
  # Stage from local source, agent only, and zip:
  PS> .\scripts\build-release.ps1 -FromSource -Roles agent -Zip

.EXAMPLE
  # Pin a specific kanade version (older hotfix, etc):
  PS> .\scripts\build-release.ps1 -Version 0.10.0

.EXAMPLE
  # Pull from crates.io instead of GitHub Releases:
  PS> .\scripts\build-release.ps1 -FromCrates
#>

[CmdletBinding()]
param(
    [string]  $Version,
    [string]  $NatsVersion = '2.11.10',
    [string]  $OutDir      = 'dist',
    [switch]  $FromSource,
    [switch]  $FromCrates,
    [string]  $GitHubRepo  = 'yukimemi/kanade',
    [switch]  $Zip,
    [string[]]$Roles       = @('agent', 'backend', 'nats'),
    [string]  $TargetDir,
    [switch]  $Force
)

$ErrorActionPreference = 'Stop'

if ($FromSource -and $FromCrates) {
    throw "-FromSource and -FromCrates are mutually exclusive."
}
# cargo is only required for the compile paths.
if (($FromSource -or $FromCrates) -and -not (Get-Command cargo -ErrorAction SilentlyContinue)) {
    throw "cargo not found on PATH. Install Rust, or drop -FromSource/-FromCrates to download a pre-built binary from GitHub Releases instead."
}

$repoRoot = Split-Path -Parent $PSScriptRoot
$configsDir = Join-Path $repoRoot 'configs'

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

Write-Host ("Staging into {0}" -f $OutDir)
if     ($FromSource)  { Write-Host "Source: local checkout ($repoRoot)" }
elseif ($FromCrates)  { Write-Host "Source: crates.io" }
else                  { Write-Host "Source: GitHub Releases (default)" }
Write-Host ("Kanade version: v{0}    NATS version: v{1}" -f $Version, $NatsVersion)
if ($FromSource -or $FromCrates) {
    $null = New-Item -ItemType Directory -Path $TargetDir -Force
    Write-Host ("Cache:  {0}" -f $TargetDir)
}

# Role metadata table. Keeps the per-role differences in one place
# so the main loop below stays linear.
$roleSpec = @{
    'agent' = @{
        Crate       = 'kanade-agent'
        ExeName     = 'kanade-agent.exe'
        ConfigName  = 'agent.toml'
        DeployScript = 'deploy-agent.ps1'
        ServiceName = 'KanadeAgent'
    }
    'backend' = @{
        Crate       = 'kanade-backend'
        ExeName     = 'kanade-backend.exe'
        ConfigName  = 'backend.toml'
        DeployScript = 'deploy-backend.ps1'
        ServiceName = 'KanadeBackend'
    }
    'nats' = @{
        # nats-server is shipped by NATS, not by kanade. The
        # -FromSource / -FromCrates paths are not supported for it;
        # we always Invoke-WebRequest from nats-io/nats-server.
        ExeName     = 'nats-server.exe'
        ConfigName  = 'nats-server.conf'
        DeployScript = 'deploy-nats.ps1'
        ServiceName = 'KanadeNats'
        External    = $true
    }
}

foreach ($role in $Roles) {
    $spec = $roleSpec[$role]
    if (-not $spec) { throw "Unknown role '$role'. Supported: $($roleSpec.Keys -join ', ')" }
    $exeName  = $spec.ExeName
    $cfgName  = $spec.ConfigName
    $deployPs = $spec.DeployScript
    $stage    = Join-Path $OutDir $role

    Write-Host ''
    Write-Host "=== $role ==="

    $cfgSrc    = Join-Path $configsDir $cfgName
    # `deploy-<role>.ps1` was reorganised to `scripts/deploy/<role>.ps1`
    # — the source filename dropped the verb prefix once we had a
    # dedicated subdir, but the dist staging keeps the verb-prefixed
    # name so the artifact end-users extract still reads as
    # "deploy-agent.ps1" / "deploy-backend.ps1" / "deploy-nats.ps1".
    $deploySrcName = $deployPs.Replace('deploy-', '')
    $deploySrc     = Join-Path $repoRoot "scripts\deploy\$deploySrcName"
    if (-not (Test-Path $cfgSrc))    { throw "Missing $cfgName under configs/ ($cfgSrc)." }
    if (-not (Test-Path $deploySrc)) { throw "Missing $deploySrcName under scripts\deploy\ ($deploySrc)." }

    $exeDst = Join-Path $stage $exeName

    # Fast-path: skip the fetch when the staged binary already
    # reports the desired version. nats-server has no embedded
    # workspace metadata, so we still rely on its `--version` line
    # for the cache hit check.
    $wantVer = if ($spec.External) { $NatsVersion } else { $Version }
    $skipBuild = $false
    if (-not $FromSource -and -not $Force -and (Test-Path $exeDst)) {
        try {
            $verLine = (& $exeDst --version 2>$null) | Select-Object -First 1
            if ($verLine) {
                # `nats-server: v2.11.10` vs `kanade-agent 0.10.0` — split
                # on whitespace and take the LAST token, then strip a
                # leading 'v' if present.
                $installed = ($verLine -split '\s+')[-1].TrimStart('v').Trim()
                if ($installed -eq $wantVer) {
                    Write-Host "Cached: $exeName already at v$wantVer (pass -Force to rebuild)."
                    $skipBuild = $true
                } else {
                    Write-Host "Stale: $exeName reports '$installed', want '$wantVer' — rebuilding."
                }
            }
        } catch {
            # Couldn't run the staged exe — fall through to a rebuild.
        }
    }

    if (-not (Test-Path $stage)) {
        $null = New-Item -ItemType Directory -Path $stage
    }

    if (-not $skipBuild) {
        if ($spec.External) {
            # NATS server: only ever downloaded — there's no
            # -FromSource analog for an external project.
            $natsAsset = "nats-server-v$NatsVersion-windows-amd64"
            $zipUrl    = "https://github.com/nats-io/nats-server/releases/download/v$NatsVersion/$natsAsset.zip"
            Write-Host "Downloading $zipUrl"
            $tempZip = Join-Path ([System.IO.Path]::GetTempPath()) "nats-server-v$NatsVersion-$([System.Guid]::NewGuid().ToString('N')).zip"
            $tempDir = Join-Path ([System.IO.Path]::GetTempPath()) "nats-server-v$NatsVersion-$([System.Guid]::NewGuid().ToString('N'))"
            try {
                Invoke-WebRequest -Uri $zipUrl -OutFile $tempZip -UseBasicParsing
                Expand-Archive -Path $tempZip -DestinationPath $tempDir -Force
                $extracted = Join-Path $tempDir "$natsAsset\nats-server.exe"
                if (-not (Test-Path $extracted)) {
                    throw "nats-server.exe not found inside $zipUrl (expected under '$natsAsset\')."
                }
                Copy-Item $extracted $exeDst -Force
            } catch {
                throw @"
Failed to fetch nats-server v$NatsVersion
  - Check https://github.com/nats-io/nats-server/releases for the current tag
    and pass -NatsVersion <ver> if the pinned default has been retracted.
Original error: $($_.Exception.Message)
"@
            } finally {
                if (Test-Path $tempZip) { Remove-Item -Force $tempZip }
                if (Test-Path $tempDir) { Remove-Item -Recurse -Force $tempDir }
            }
        } elseif ($FromSource) {
            $cratePath = Join-Path $repoRoot "crates\$($spec.Crate)"
            if (-not (Test-Path $cratePath)) { throw "Missing crate path '$cratePath'." }
            $temp = Join-Path ([System.IO.Path]::GetTempPath()) ("kanade-stage-{0}-{1}" -f $role, [System.Guid]::NewGuid().ToString('N'))
            try {
                Write-Host "cargo install --path $cratePath --root $temp --target-dir $TargetDir"
                & cargo install --root $temp --locked --path $cratePath --target-dir $TargetDir
                if ($LASTEXITCODE -ne 0) { throw "cargo install failed for $($spec.Crate) (exit $LASTEXITCODE)." }
                $exeSrc = Join-Path $temp "bin\$exeName"
                if (-not (Test-Path $exeSrc)) { throw "Built binary not found at '$exeSrc'." }
                Copy-Item $exeSrc $exeDst -Force
            }
            finally {
                if (Test-Path $temp) { Remove-Item -Recurse -Force $temp }
            }
        } elseif ($FromCrates) {
            $temp = Join-Path ([System.IO.Path]::GetTempPath()) ("kanade-stage-{0}-{1}" -f $role, [System.Guid]::NewGuid().ToString('N'))
            try {
                Write-Host "cargo install $($spec.Crate)@$Version --root $temp --target-dir $TargetDir"
                & cargo install --root $temp --version $Version $spec.Crate --target-dir $TargetDir
                if ($LASTEXITCODE -ne 0) { throw "cargo install failed for $($spec.Crate) (exit $LASTEXITCODE)." }
                $exeSrc = Join-Path $temp "bin\$exeName"
                if (-not (Test-Path $exeSrc)) { throw "Built binary not found at '$exeSrc'." }
                Copy-Item $exeSrc $exeDst -Force
            }
            finally {
                if (Test-Path $temp) { Remove-Item -Recurse -Force $temp }
            }
        } else {
            # Default: download from GitHub Releases. release.yml stages
            # bins as `<crate>-<target>.exe` (target-suffixed so a single
            # release page can carry every platform without name clashes),
            # so the asset name differs from the local deploy filename
            # (`$exeName`, e.g. `kanade-agent.exe`) that deploy-<role>.ps1
            # expects.
            $assetName = "$($spec.Crate)-x86_64-pc-windows-msvc.exe"
            $url = "https://github.com/$GitHubRepo/releases/download/v$Version/$assetName"
            Write-Host "Downloading $url"
            try {
                Invoke-WebRequest -Uri $url -OutFile $exeDst -UseBasicParsing
            } catch {
                throw @"
Failed to download $url
  - Asset missing on the release? Check https://github.com/$GitHubRepo/releases/tag/v$Version
  - release.yml's Windows matrix entry silently drops kanade-{agent,backend,CLI}.exe;
    release-extras.yml's upload-windows-bins job backfills them on tag push. If you're
    pulling an older tag from before that job landed, run
    `gh workflow run release-extras.yml --ref v$Version` to backfill the missing assets.
  - First v0.3.x tags published before the GitHub-Release-upload step landed won't have assets;
    use -FromCrates for those, or -FromSource against a checkout.
Original error: $($_.Exception.Message)
"@
            }
        }
    }

    # Always refresh the non-binary artefacts. They're tiny and may
    # have been edited (config tweaks, deploy-script bumps) since the
    # last stage even when the exe didn't change.
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

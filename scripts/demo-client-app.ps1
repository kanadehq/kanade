<#
.SYNOPSIS
    Run the Client App demo in a real desktop window.

.DESCRIPTION
    The browser demo (`cargo make demo-client`) is enough for most
    screenshots. This one exists for what a browser tab cannot show: the
    app as a window with no address bar, and — the part that actually
    matters — OS toasts, which is where a notice reaches the user when
    the app is not in front of them.

    Three things have to line up, and each one is a trap on its own:

      1. `--no-default-features`, so `custom-protocol` is OFF and the
         binary loads `build.devUrl` instead of the frontend embedded at
         compile time. With it on, `cargo tauri dev` starts Vite and
         then launches a binary that ignores it — the window shows a
         stale build while the same URL renders correctly in a browser
         beside it, and nothing in the output says why.

      2. The demo config overlay, which moves the app off the shipped
         `identifier` so its single-instance mutex cannot collide with
         an installed client.

      3. The `kanade-client://` protocol, pointed at THIS build for the
         current user. The native toast activates through that scheme
         (#647), and on a machine with kanade installed it is
         registered to the shipped binary — so clicking the demo's own
         toast opened the real client. Observed, not theorised.

    Point 3 changes the machine, so the registration is removed again in
    `finally`. Ctrl-C and a closed window both run it. A hard kill does
    not, which is why the registration carries a marker and
    `demo-client-protocol.ps1 -Unregister` is safe to run at any time.
#>
[CmdletBinding()]
param(
    # Port for the Vite dev server. Must match `build.devUrl`, which the
    # binary reads from the config at compile time.
    [int]$Port = 1420
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path -Parent $PSScriptRoot
$CrateDir = Join-Path $RepoRoot 'crates/kanade-client'
$ExePath = Join-Path $RepoRoot 'target/debug/kanade-client.exe'
$Protocol = Join-Path $PSScriptRoot 'demo-client-protocol.ps1'

Push-Location $CrateDir
try {
    # Build BEFORE registering: the protocol registration has to name a
    # file that exists, and on a clean checkout `tauri dev` would not
    # have produced it yet. `tauri dev` reuses this build, so the only
    # cost is doing the compile in a predictable order.
    Write-Host '==> building the demo binary (this is the slow part)'
    cargo build -p kanade-client --no-default-features
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }

    Write-Host '==> pointing kanade-client:// at the demo build'
    & $Protocol -Register -ExePath $ExePath

    try {
        $env:KANADE_CLIENT_DEMO = '1'
        $env:KANADE_CLIENT_DEMO_PORT = "$Port"
        Write-Host '==> starting; close the window or press Ctrl-C to stop'
        cargo tauri dev -c web/demo/tauri.demo.conf.json -- --no-default-features
    }
    finally {
        Write-Host '==> restoring kanade-client://'
        & $Protocol -Unregister
        Remove-Item Env:KANADE_CLIENT_DEMO -ErrorAction SilentlyContinue
        Remove-Item Env:KANADE_CLIENT_DEMO_PORT -ErrorAction SilentlyContinue
    }
}
finally {
    Pop-Location
}

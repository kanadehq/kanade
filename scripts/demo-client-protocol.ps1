<#
.SYNOPSIS
    Point the `kanade-client://` protocol at a demo build, and put it back.

.DESCRIPTION
    The Client App's native toast carries
    `launch="kanade-client://<notification-id>"` with
    `activationType=protocol` (#647), so clicking the toast — body or the
    確認 button — reaches the app through the shell's protocol handler
    rather than through the process that sent it.

    On any machine with kanade installed, that handler is registered
    machine-wide to the shipped binary:

        HKLM\Software\Classes\kanade-client\shell\open\command
          = "C:\Program Files\Kanade\kanade-client.exe" "%1"

    So a demo running from a dev build sends a toast, and clicking it
    opens the REAL client — in front of whoever is being shown the
    product. Registering the same scheme under HKCU wins for the current
    user (the shell prefers HKCU over HKLM), which routes the demo's own
    toasts back to the demo.

    This is a change to the machine, not to the repo, so it is written to
    be reversible and careful:

      * Register refuses to touch a pre-existing HKCU registration it did
        not create — if you already had one, it is yours, and losing it
        silently would be worse than the demo not working.
      * Every key written carries a marker value, and Unregister removes
        the tree only when that marker is present.
      * Nothing under HKLM is read for anything but reporting, and never
        written. The shipped client's registration is left intact; it
        simply stops being the one the shell picks for this user until
        Unregister runs.

.EXAMPLE
    ./scripts/demo-client-protocol.ps1 -Register -ExePath target/debug/kanade-client.exe
    ./scripts/demo-client-protocol.ps1 -Unregister
#>
[CmdletBinding(DefaultParameterSetName = 'Status')]
param(
    [Parameter(ParameterSetName = 'Register', Mandatory = $true)]
    [switch]$Register,

    # The build to hand the toast clicks to. Resolved to an absolute path
    # because the shell runs it with an unrelated working directory.
    [Parameter(ParameterSetName = 'Register', Mandatory = $true)]
    [string]$ExePath,

    [Parameter(ParameterSetName = 'Unregister', Mandatory = $true)]
    [switch]$Unregister,

    [Parameter(ParameterSetName = 'Status')]
    [switch]$Status
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$Root = 'HKCU:\Software\Classes\kanade-client'
$CommandKey = "$Root\shell\open\command"
# Presence of this value is the whole basis for Unregister deleting
# anything. Without it we cannot tell our registration apart from one the
# user (or an installer) put there on purpose.
$MarkerName = 'KanadeDemoManaged'

function Get-HklmHandler {
    $k = 'HKLM:\Software\Classes\kanade-client\shell\open\command'
    if (Test-Path $k) { (Get-ItemProperty -Path $k).'(default)' } else { $null }
}

function Test-OurRegistration {
    if (-not (Test-Path $Root)) { return $false }
    $props = Get-ItemProperty -Path $Root
    return ($null -ne $props.PSObject.Properties[$MarkerName])
}

if ($Register) {
    $resolved = (Resolve-Path -LiteralPath $ExePath).Path
    if (-not (Test-Path -LiteralPath $resolved -PathType Leaf)) {
        throw "not a file: $resolved"
    }

    if ((Test-Path $Root) -and -not (Test-OurRegistration)) {
        throw @"
$Root already exists and was not created by this script.
Refusing to overwrite it. Inspect it, and remove it yourself if it is
stale:  Remove-Item -Recurse '$Root'
"@
    }

    New-Item -Path $CommandKey -Force | Out-Null
    # `URL:` display string + the empty `URL Protocol` value are what mark
    # a key as a protocol handler; the shell ignores the key without them.
    Set-ItemProperty -Path $Root -Name '(default)' -Value 'URL:kanade-client (kanade demo)'
    Set-ItemProperty -Path $Root -Name 'URL Protocol' -Value ''
    Set-ItemProperty -Path $Root -Name $MarkerName -Value 1 -Type DWord
    Set-ItemProperty -Path $CommandKey -Name '(default)' -Value ('"{0}" "%1"' -f $resolved)

    Write-Host "registered kanade-client:// -> $resolved (HKCU, this user only)"
    $hklm = Get-HklmHandler
    if ($hklm) { Write-Host "  shadowing HKLM handler: $hklm" }
    Write-Host "  undo: ./scripts/demo-client-protocol.ps1 -Unregister"
    return
}

if ($Unregister) {
    if (-not (Test-Path $Root)) {
        Write-Host 'nothing to remove'
        return
    }
    if (-not (Test-OurRegistration)) {
        Write-Warning "$Root exists but has no $MarkerName marker — leaving it alone."
        return
    }
    Remove-Item -Path $Root -Recurse -Force
    Write-Host 'removed the demo registration'
    $hklm = Get-HklmHandler
    if ($hklm) { Write-Host "  kanade-client:// is back to: $hklm" }
    return
}

# Status (default): report, change nothing.
if (Test-Path $Root) {
    $cmd = if (Test-Path $CommandKey) { (Get-ItemProperty -Path $CommandKey).'(default)' } else { '(no command)' }
    $ours = if (Test-OurRegistration) { 'demo-managed' } else { 'NOT demo-managed' }
    Write-Host "HKCU: $cmd  [$ours]"
} else {
    Write-Host 'HKCU: (not registered)'
}
$hklm = Get-HklmHandler
Write-Host ("HKLM: {0}" -f ($(if ($hklm) { $hklm } else { '(not registered)' })))

#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Install / update kanade-backend as a Windows service.

.DESCRIPTION
  Copies kanade-backend.exe into %ProgramFiles%\Kanade\ and
  backend.toml into %ProgramData%\Kanade\config\, ensures the
  runtime data / log dirs exist under %ProgramData%\Kanade\, and
  (re-)registers a Windows service that runs the backend against
  the installed config. Re-running upgrades the binary in place;
  existing backend.toml is preserved unless -ForceConfig is passed.
  The split follows the Windows convention used in the spec §2.11
  layout (README "Production install layout"): Program Files is the
  read-only install root, ProgramData holds writable runtime state.

  Firewall: the installed backend.toml is parsed for its `[server]
  bind` line. If the host part isn't loopback (127.0.0.1 / ::1 /
  localhost), the script opens that port via New-NetFirewallRule.
  Pass -FirewallPort <int> to override the parsed port (e.g. when
  there's a reverse proxy listening on a different external port),
  or -NoFirewall to suppress the rule altogether (managed firewall,
  separate WAF, etc.).

.PARAMETER SourceDir
  Directory holding kanade-backend.exe and backend.toml. Defaults to
  the directory this script lives in.

.PARAMETER ServiceName
  Windows service name. Default: KanadeBackend.

.PARAMETER ForceConfig
  Overwrite the installed backend.toml with the one in -SourceDir.
  Off by default so config tweaks on the target machine survive
  upgrades.

.PARAMETER FirewallPort
  Open this TCP port via New-NetFirewallRule, overriding the port
  parsed out of backend.toml's `[server] bind`. Useful when an
  external reverse proxy listens on a different port than the
  backend itself.

.PARAMETER NoFirewall
  Skip the firewall rule even when backend.toml's bind looks
  public. Use this when an external firewall (corporate / WAF /
  cloud security group) is the source of truth.

.PARAMETER Recreate
  Drop the existing Windows service entirely (sc.exe delete + wait
  for SCM to acknowledge) before re-creating it. Useful for
  recovering from a half-installed service registration.

.PARAMETER NoStart
  Install + register the service but don't start it.

.PARAMETER NatsToken
  If set, write the NATS bearer token to
  HKLM\SOFTWARE\kanade\agent\NatsToken (REG_SZ) and harden the ACL
  on that key so only SYSTEM + Administrators can read it. The
  backend reads this at startup ahead of $env:KANADE_NATS_TOKEN.
  Required when the broker is started with `authorization { token: ... }`.

.PARAMETER StaticToken
  If set, write the HTTP static-token bearer to
  HKLM\SOFTWARE\kanade\backend\StaticToken (REG_SZ, hardened ACL).
  Backend resolves it ahead of $env:KANADE_AUTH_STATIC_TOKEN.
  Choose this OR -JwtSecret, not both — backend's middleware
  prefers StaticToken when present.

.PARAMETER JwtSecret
  If set, write the HS256 JWT signing secret to
  HKLM\SOFTWARE\kanade\backend\JwtSecret (REG_SZ, hardened ACL).
  Backend resolves it ahead of $env:KANADE_JWT_SECRET. Operators
  sign tokens out-of-band with `aud=kanade` + a future `exp`.

.EXAMPLE
  PS> .\deploy-backend.ps1                            # opens whatever backend.toml binds to

.EXAMPLE
  PS> .\deploy-backend.ps1 -FirewallPort 8443         # override the parsed port

.EXAMPLE
  PS> .\deploy-backend.ps1 -NoFirewall                # external firewall handles ingress

.EXAMPLE
  PS> .\deploy-backend.ps1 -NatsToken 'kanade-fleet-secret-2026'   # provision NATS bearer token

.EXAMPLE
  PS> .\deploy-backend.ps1 -StaticToken 'kanade-fleet-secret-2026' # provision HTTP static-token bearer

.EXAMPLE
  PS> .\deploy-backend.ps1 -JwtSecret  '<long-hs256-secret>'       # provision HS256 JWT signing key

.EXAMPLE
  PS> .\deploy-backend.ps1 -ForceConfig               # re-run after binary update, fresh config

.EXAMPLE
  PS> .\deploy-backend.ps1 -Recreate                  # recover from a stuck / broken service
#>

[CmdletBinding()]
param(
    [string]$SourceDir    = $PSScriptRoot,
    [string]$ServiceName  = 'KanadeBackend',
    [switch]$ForceConfig,
    [int]   $FirewallPort = 0,
    [switch]$NoFirewall,
    [switch]$Recreate,
    [switch]$NoStart,
    [string]$NatsToken    = '',
    [string]$StaticToken  = '',
    [string]$JwtSecret    = ''
)

$ErrorActionPreference = 'Stop'

# === Agent-mode knobs (yukimemi/kanade#210 follow-up) =====================
# When this script is uploaded to OBJECT_SCRIPTS via
# `kanade script publish deploy-backend <v> <edited-copy>` and a
# manifest references it through `execute.script_object`, PowerShell
# runs the body with NO CLI args — the `param()` block above takes
# defaults, so `$SourceDir = $PSScriptRoot` ends up `$null` and the
# existing folder-install path fails fast.
#
# The agent-mode hook below fills that gap: when an operator edits
# the three `$Agent*` constants BEFORE publishing, the script
# downloads kanade-backend.exe from OBJECT_APP_PACKAGES into a temp
# staging dir + reuses the existing backend.toml on the destination
# host + then runs the existing install flow against that staging
# dir.
#
# Leave the three knobs empty (= default) to keep the original
# manual `-SourceDir <folder>` flow working unchanged for operators
# invoking the script by hand.
#
# `Get-FileHash <kanade-backend.exe> -Algorithm SHA256` for the
# Sha256 value — mismatch aborts before the swap so a MITM /
# corrupted upload leaves the existing install intact.
$AgentSourceUrl     = ''   # e.g. 'http://kanade-backend.local:8080'
$AgentSourceVersion = ''   # e.g. '0.43.0'
$AgentSourceSha256  = ''   # lowercase hex of the uploaded .exe
$AgentDownloadTimeoutSecs = 120
# ===========================================================================

# If the agent-mode knobs are set, download the binary into a temp
# staging dir + repoint `$SourceDir` at it before the existing
# install flow runs. The `trap` block at the bottom of this section
# cleans up the staging dir on any subsequent failure so a repeated
# failed upgrade doesn't leak temp dirs.
$AgentStaging = $null
if ($AgentSourceUrl) {
    if (-not $AgentSourceVersion) {
        throw 'deploy-backend (agent mode): $AgentSourceVersion must be set alongside $AgentSourceUrl.'
    }
    if (-not $AgentSourceSha256) {
        throw 'deploy-backend (agent mode): $AgentSourceSha256 must be set — leaving it blank would silently install whatever the backend serves.'
    }

    $tmpRoot = [System.IO.Path]::GetTempPath()
    $AgentStaging = Join-Path $tmpRoot ('kanade-deploy-' + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $AgentStaging | Out-Null
    $stagedExe = Join-Path $AgentStaging 'kanade-backend.exe'

    $url = "$($AgentSourceUrl.TrimEnd('/'))/api/app-packages/kanade-backend/$AgentSourceVersion"
    Write-Host "deploy-backend (agent mode): downloading kanade-backend $AgentSourceVersion from $url"
    Invoke-WebRequest -Uri $url -OutFile $stagedExe -UseBasicParsing -TimeoutSec $AgentDownloadTimeoutSecs | Out-Null

    # Sha verify BEFORE swap — same posture as install-kanade-client.ps1.
    $actual = (Get-FileHash $stagedExe -Algorithm SHA256).Hash.ToLowerInvariant()
    $expected = $AgentSourceSha256.ToLowerInvariant()
    if ($actual -ne $expected) {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $AgentStaging
        throw "deploy-backend (agent mode): sha256 mismatch — expected=$expected actual=$actual. Refusing to install (possible MITM / corrupted upload)."
    }
    Write-Host "deploy-backend (agent mode): sha256 verified"

    # backend.toml: prefer the one already installed (operator's
    # production config). Agent-mode is an upgrade path, not a
    # fresh install — error fast if no existing config is present
    # rather than silently reaching for the default sample.
    $existingConfig = Join-Path (Join-Path $env:ProgramData 'Kanade') 'config\backend.toml'
    if (Test-Path $existingConfig) {
        Copy-Item -Force $existingConfig (Join-Path $AgentStaging 'backend.toml')
        Write-Host "deploy-backend (agent mode): reusing existing backend.toml from $existingConfig"
    } else {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $AgentStaging
        throw "deploy-backend (agent mode): no existing backend.toml at $existingConfig — agent-mode is an upgrade path, not a fresh install. Run with -SourceDir <folder> manually for the initial install."
    }

    $SourceDir = $AgentStaging
}

# Anything that goes wrong from here on must still clean up the
# agent-mode staging dir. The trap fires on terminating errors
# (ErrorActionPreference = 'Stop' makes most things terminating).
# `break` re-throws after cleanup — don't swallow.
trap {
    if ($AgentStaging -and (Test-Path $AgentStaging)) {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $AgentStaging
    }
    break
}

# Write a secret value to HKLM\SOFTWARE\kanade\<subkey>\<value> and
# strip non-admin ACEs from the leaf key. Used to provision NATS
# tokens, HTTP static tokens, and JWT signing secrets — see the
# resolution order in kanade-shared::secrets and kanade-backend::auth.
#
# Implementation note: uses the pure .NET Microsoft.Win32.Registry
# API rather than the New-Item / Set-ItemProperty / Get-Acl / Set-Acl
# cmdlets. The cmdlet path auto-loads Microsoft.PowerShell.Management
# + Microsoft.PowerShell.Security on first use, and those module
# loads fail on some elevated / constrained-language pwsh sessions
# with a CouldNotAutoloadMatchingModule error. The .NET classes are
# always reachable from any PowerShell context.
function Set-KanadeRegistrySecret {
    param(
        [Parameter(Mandatory)][string]$Subkey,
        [Parameter(Mandatory)][string]$ValueName,
        [Parameter(Mandatory)][string]$Value
    )

    $subkeyPath = "SOFTWARE\kanade\$Subkey"
    $key = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey($subkeyPath, $true)
    if (-not $key) {
        $key = [Microsoft.Win32.Registry]::LocalMachine.CreateSubKey($subkeyPath)
    }
    try {
        $key.SetValue($ValueName, $Value, [Microsoft.Win32.RegistryValueKind]::String)

        $sec = $key.GetAccessControl()
        $sec.SetAccessRuleProtection($true, $false)
        @($sec.Access) | ForEach-Object { [void]$sec.RemoveAccessRule($_) }
        # SIDs (locale-agnostic): SYSTEM = S-1-5-18, Administrators = S-1-5-32-544
        foreach ($sid in 'S-1-5-18', 'S-1-5-32-544') {
            $sidObj = [System.Security.Principal.SecurityIdentifier]::new($sid)
            $rule = [System.Security.AccessControl.RegistryAccessRule]::new(
                $sidObj, 'FullControl', 'ContainerInherit', 'None', 'Allow')
            $sec.AddAccessRule($rule)
        }
        $key.SetAccessControl($sec)
    } finally {
        $key.Close()
    }

    Write-Host "Wrote $ValueName to HKLM:\$subkeyPath (SYSTEM + Administrators only)."
}

$binDir    = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot  = Join-Path $env:ProgramData  'Kanade'
$configDir = Join-Path $dataRoot 'config'
$dataDir   = Join-Path $dataRoot 'data'
$logsDir   = Join-Path $dataRoot 'logs'

$exeName    = 'kanade-backend.exe'
$configName = 'backend.toml'
$exeSrc     = Join-Path $SourceDir $exeName
$configSrc  = Join-Path $SourceDir $configName
$exeDst     = Join-Path $binDir    $exeName
$configDst  = Join-Path $configDir $configName

if (-not (Test-Path $exeSrc)) {
    throw "Missing '$exeName' in '$SourceDir'. Place the release binary next to this script or pass -SourceDir."
}
if (-not (Test-Path $configSrc)) {
    throw "Missing '$configName' in '$SourceDir'. Place the sample config next to this script or pass -SourceDir."
}

$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -ne 'Stopped') {
    Write-Host "Stopping $ServiceName..."
    Stop-Service -Name $ServiceName -Force
    $svc.WaitForStatus('Stopped', '00:00:30')
}

# -Recreate: drop the existing service entirely so the New-Service
# path below runs even when a previous registration is already
# there. Same recovery valve as deploy-agent.ps1 — Stop +
# sc.exe delete + wait for SCM to acknowledge.
if ($Recreate -and $svc) {
    Write-Host "Removing existing $ServiceName (-Recreate)"
    & sc.exe delete $ServiceName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "sc.exe delete failed (exit $LASTEXITCODE)" }
    $deadline = (Get-Date).AddSeconds(30)
    while ((Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 250
    }
    if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) {
        throw "Timed out waiting for $ServiceName to be removed from SCM"
    }
    $svc = $null
}

foreach ($d in @($binDir, $configDir, $dataDir, $logsDir)) {
    if (-not (Test-Path $d)) {
        New-Item -ItemType Directory -Path $d -Force | Out-Null
    }
}

Write-Host "Installing $exeName -> $exeDst"
Copy-Item -Path $exeSrc -Destination $exeDst -Force

if ($ForceConfig -or -not (Test-Path $configDst)) {
    $verb = if (Test-Path $configDst) { 'Overwriting' } else { 'Seeding' }
    Write-Host "$verb $configName -> $configDst"
    Copy-Item -Path $configSrc -Destination $configDst -Force
} else {
    Write-Host "Keeping existing $configDst (pass -ForceConfig to overwrite)."
}

if ($NatsToken) {
    Set-KanadeRegistrySecret -Subkey 'agent'   -ValueName 'NatsToken'   -Value $NatsToken
}
if ($StaticToken) {
    Set-KanadeRegistrySecret -Subkey 'backend' -ValueName 'StaticToken' -Value $StaticToken
}
if ($JwtSecret) {
    Set-KanadeRegistrySecret -Subkey 'backend' -ValueName 'JwtSecret'   -Value $JwtSecret
}

# Service binPath = quoted exe + --config flag pointing at the
# installed toml. New-Service handles the embedded-quote +
# space-in-path mess that `sc.exe create` chokes on (exit 1639
# = ERROR_INVALID_COMMAND_LINE) when called through PowerShell
# without the --% stop-parsing token.
$binPath = "`"$exeDst`" --config `"$configDst`""
if (-not $svc) {
    Write-Host "Creating service $ServiceName"
    $null = New-Service `
        -Name           $ServiceName `
        -BinaryPathName $binPath `
        -StartupType    Automatic `
        -DisplayName    'Kanade Backend' `
        -Description    'Kanade backend / projector / HTTP admin API (yukimemi/kanade).'
} else {
    Write-Host "Updating $ServiceName configuration"
    # Existing service: binPath rarely needs changing (exe path is
    # stable across upgrades). Reconfirm the start type only.
    Set-Service -Name $ServiceName -StartupType Automatic
}

# Failure recovery — restart on any non-clean-stop exit.
#
# Backend doesn't self-update (deploy-backend.ps1 is the manual update
# path), so we don't strictly need the `failureflag 1` bit for an
# exit(64) handshake — but it's still cheap insurance: a panic that
# unwinds out of #[tokio::main] returns a non-zero exit code, and
# without failureflag SCM treats that as "service stopped" and leaves
# the projector/API offline until the operator notices. With it, SCM
# applies the restart actions just like a real crash.
#
#   actions= restart/5000/restart/15000/restart/60000
#     1st failure: wait 5s; 2nd: 15s; 3rd: 60s.
#   reset= 86400
#     Reset failure counter after 24h of clean uptime.
Write-Host "Configuring failure recovery on $ServiceName"
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failure failed (exit $LASTEXITCODE)" }
& sc.exe failureflag $ServiceName 1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failureflag failed (exit $LASTEXITCODE)" }

# Decide which port to open (if any). Priority:
#   1. Explicit -FirewallPort wins outright.
#   2. -NoFirewall short-circuits to "nothing".
#   3. Parse backend.toml's `[server] bind` and, if the host part
#      isn't loopback, open the port it carries.
#
# Loopback bind (127.0.0.1 / ::1 / localhost) means same-machine
# only — no firewall rule needed, and adding one would just be
# noise in the audit log.
$portToOpen = 0
if ($FirewallPort -gt 0) {
    $portToOpen = $FirewallPort
    Write-Host "Firewall: opening inbound TCP $portToOpen (explicit -FirewallPort)."
} elseif (-not $NoFirewall) {
    $bindLine = Select-String -Path $configDst -Pattern "^bind\s*=\s*['""]([^'""]+)['""]" |
                Select-Object -First 1
    if ($bindLine) {
        $bindAddr    = $bindLine.Matches[0].Groups[1].Value
        # IPv6 form is [::]:8080 — strip the brackets so the final
        # `:port` split is unambiguous.
        $unbracketed = $bindAddr -replace '^\[([^\]]+)\]', '$1'
        $lastColon   = $unbracketed.LastIndexOf(':')
        if ($lastColon -gt 0) {
            $bindHost  = $unbracketed.Substring(0, $lastColon)
            $bindPort  = $unbracketed.Substring($lastColon + 1) -as [int]
            $loopback  = @('127.0.0.1', '::1', 'localhost')
            if ($bindPort -and ($loopback -notcontains $bindHost)) {
                $portToOpen = $bindPort
                Write-Host "Firewall: bind '$bindAddr' is public; opening inbound TCP $portToOpen (pass -NoFirewall to skip)."
            } else {
                Write-Host "Firewall: bind '$bindAddr' is loopback; no rule needed."
            }
        } else {
            Write-Host "Firewall: couldn't parse a port out of bind '$bindAddr'; skipping rule. Pass -FirewallPort to open one explicitly."
        }
    } else {
        Write-Host "Firewall: no `[server] bind` line found in $configDst; skipping rule. Pass -FirewallPort to open one explicitly."
    }
} else {
    Write-Host "Firewall: -NoFirewall set; skipping rule."
}

if ($portToOpen -gt 0) {
    $ruleName = "$ServiceName (TCP $portToOpen)"
    $existing = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "Firewall rule '$ruleName' already exists; leaving it alone."
    } else {
        New-NetFirewallRule `
            -DisplayName $ruleName `
            -Direction   Inbound `
            -Protocol    TCP `
            -LocalPort   $portToOpen `
            -Action      Allow `
            -Profile     Any `
            | Out-Null
        Write-Host "Created firewall rule '$ruleName'."
    }
}

if (-not $NoStart) {
    Write-Host "Starting $ServiceName"
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus('Running', '00:00:30')
}

Write-Host ''
Write-Host "Installed bin: $exeDst"
Write-Host "Runtime root:  $dataRoot"
& $exeDst --version

# Agent-mode staging dir cleanup on success. The `trap` above
# handles failure paths; this clears the temp dir for the happy
# path so the agent doesn't leak a staging dir per upgrade.
if ($AgentStaging -and (Test-Path $AgentStaging)) {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $AgentStaging
}

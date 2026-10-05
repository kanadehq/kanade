#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Install / update nats-server as a Windows service.

.DESCRIPTION
  Copies nats-server.exe into %ProgramFiles%\Kanade\, nats-server.conf
  into %ProgramData%\Kanade\config\, ensures the JetStream data
  directory exists under %ProgramData%\Kanade\nats\, hardens the ACL
  on nats-server.conf (token lives there in plaintext per NATS
  conventions), and (re-)registers a Windows service. Re-running
  upgrades the binary in place; existing nats-server.conf is
  preserved unless -ForceConfig is passed.

  nats-server.exe is shipped by NATS, not by kanade. Use
  `build-release.ps1 -Roles nats` to fetch it from
  https://github.com/nats-io/nats-server/releases.

  Firewall: opens TCP 4222 (broker) by default, and REMOVES the
  monitoring rule this script used to create for 8222. Pass
  -NoFirewall when an external firewall (corporate / WAF / cloud
  security group) is the source of truth.

  Why 8222 is no longer opened: the NATS monitoring endpoint has no
  authentication of its own, and whoever reaches it can enumerate
  every connection, its IP and its subscriptions (/connz), plus the
  server and JetStream state (/varz, /jsz). Nothing in kanade reads it
  remotely -- the backend's own poller and the collect-broker-health
  job both use http://127.0.0.1:8222 -- so the inbound rule granted
  access to strangers and to nobody else. deploy/linux/README.md has
  said to keep 8222 private since the Linux deploy landed; this brings
  the Windows path in line.

  NOTE the rule is not the whole story. A program-scoped allow rule
  (the kind Windows offers to create the first time a binary listens)
  admits traffic to EVERY port nats-server listens on, regardless of
  the port rules here. What actually closes monitoring is the bind in
  nats-server.conf -- `http: "127.0.0.1:8222"` rather than the bare
  `http_port: 8222`, which listens on all interfaces. Applying that to
  an installed host needs -ForceConfig, which overwrites the config
  with this repo's copy -- placeholder token and all. Pass -NatsToken
  with it. If you forget, or if the substitution fails, the script now
  restores the previous config and refuses to start the broker rather
  than leaving one that cannot authenticate its own fleet.

  Agent-mediated update mode (#234): like deploy-backend.ps1, this
  script has a second mode for upgrading the broker through the fleet
  itself instead of RDP-ing to the broker host. Set the `$AgentSource*`
  knobs near the top BEFORE `kanade script publish`, reference the
  uploaded copy from a manifest via `execute.script_object`, and run it
  with `kanade exec install-kanade-nats`: the script downloads
  nats-server.exe from `/api/app-packages/nats-server/<version>`,
  sha-verifies it, reuses the existing nats-server.conf on the target
  (so the operator's token edits survive), and then runs the normal
  install flow against the downloaded staging dir. Leave the knobs empty
  for the original operator-direct (`-SourceDir`) flow used on first-host
  bootstrap.

  CAVEAT unique to NATS: the agent reaches the broker OVER the broker,
  so stopping nats-server for the swap drops the agent's own connection
  mid-job. This is safe -- not lossy: the agent's outbox queues results
  during the outage and drains them on reconnect once the upgraded
  broker is back up. The job result is therefore "delayed, not lost".
  Expect the `install-kanade-nats` exec result to land a few seconds
  late (after the broker restarts and the agent reconnects).

.PARAMETER SourceDir
  Directory holding nats-server.exe and nats-server.conf. Defaults
  to the directory this script lives in.

.PARAMETER ServiceName
  Windows service name. Default: KanadeNats.

.PARAMETER ForceConfig
  Overwrite the installed nats-server.conf with the one in
  -SourceDir. Off by default so token edits on the target survive
  upgrades.

.PARAMETER NoFirewall
  Skip the firewall rule(s) even when binding looks public.

.PARAMETER Recreate
  Drop the existing Windows service entirely (sc.exe delete + wait
  for SCM to acknowledge) before re-creating it.

.PARAMETER NoStart
  Install + register the service but don't start it.

.PARAMETER NatsToken
  If set, substitute the broker's `authorization.token` value in
  the installed nats-server.conf with this string. NATS keeps the
  bearer token plaintext in its config file (the broker has no
  registry / DPAPI back-end like kanade-agent), so the script
  rewrites that one line and re-applies the SYSTEM + Administrators-
  only ACL afterwards. Matches the `-NatsToken` flag on
  deploy-agent.ps1 / deploy-backend.ps1 so the operator can run
  the same value on every host.

.EXAMPLE
  PS> .\deploy-nats.ps1
  # Binary + service only, keeping whatever config is already installed.
  # On a FIRST install there is no config yet, so the repo's sample is
  # seeded -- placeholder token and all -- and the script installs and
  # registers the service but refuses to START it until a real token is
  # set. Re-run with -NatsToken to finish.
.EXAMPLE
  PS> .\deploy-nats.ps1 -NatsToken '<your-fleet-token>'
.EXAMPLE
  PS> .\deploy-nats.ps1 -ForceConfig -NatsToken '<your-fleet-token>'
  # -ForceConfig overwrites the installed nats-server.conf with the one
  # in this repo, whose token is the PLACEHOLDER. Pair it with
  # -NatsToken or the broker comes back up rejecting the whole fleet.
.EXAMPLE
  PS> .\deploy-nats.ps1 -NoFirewall
.EXAMPLE
  PS> .\deploy-nats.ps1 -Recreate
#>

[CmdletBinding()]
param(
    [string]$SourceDir   = $PSScriptRoot,
    [string]$ServiceName = 'KanadeNats',
    [switch]$ForceConfig,
    [switch]$NoFirewall,
    [switch]$Recreate,
    [switch]$NoStart,
    [string]$NatsToken   = ''
)

# Rewrite the `token: "..."` line in the installed nats-server.conf to the
# supplied value. Uses [System.IO.File] for byte-exact preservation of
# surrounding whitespace + CRLF; the regex replacement double-escapes any `$`
# in the token so the replacement string doesn't misinterpret it as a backref.
#
# LINE-oriented, not brace-oriented, and that is the fix rather than a style
# choice. The previous pattern was
#
#     (?ms)(authorization\s*\{[^}]*?token:\s*)"[^"]*"
#
# which fails outright on the config this repo ships, because the header
# comment contains the words
#
#     # ... drop the `authorization { ... }` block and start with `nats-server -js`
#
# `[^}]*?` cannot cross the `}` on that line, so the match dies there and the
# real block below is never reached. `-ForceConfig -NatsToken` therefore threw
# on every run against the shipped file -- after the config had already been
# overwritten with the placeholder and the service stopped, which is how it
# left a broker that could not authenticate its own fleet.
#
# A comment cannot be mistaken for the setting here: nats-server comments start
# with `#`, and `^\s*token:` requires the line to begin with the key. The
# match count is asserted, so a config that grows a second `token:` (an
# accounts block, a leafnode remote) fails loudly instead of having one of them
# silently rewritten.
function Set-NatsServerToken {
    param(
        [Parameter(Mandatory)][string]$ConfigPath,
        [Parameter(Mandatory)][string]$Token
    )

    # Reject what cannot round-trip BEFORE touching the file. A token holding
    # a double quote writes `token: "ab"cd"`, which nats-server cannot parse --
    # so the broker fails to start, which is the exact failure class this
    # function exists to prevent (review #1290, coderabbit). The read-back
    # below would not catch it either: the corrupt line still matches a
    # substring search for the escaped token. A line break breaks the
    # line-oriented pattern the same way.
    if ($Token -match '["\r\n]') {
        throw @"
The supplied NATS token contains a double quote or a line break, which cannot
be written into a nats-server config string. Generate one without them, e.g.
  [Convert]::ToBase64String((1..32|%{Get-Random -Max 256}))
"@
    }

    $content = [System.IO.File]::ReadAllText($ConfigPath)
    $pattern = '(?m)^([^\S\r\n]*token:[^\S\r\n]*)"[^"]*"'
    # NOT $matches: that is a PowerShell automatic variable.
    $hits = [regex]::Matches($content, $pattern)
    if ($hits.Count -eq 0) {
        throw @"
No uncommented ``token: "..."`` line found in
  $ConfigPath
to substitute. Either edit the file to include
  authorization { token: "<your-token>" }
or drop -NatsToken (the broker will then run unauthenticated, matching
the shipped sample's commented-out auth block).
"@
    }
    if ($hits.Count -gt 1) {
        throw @"
Found $($hits.Count) ``token: "..."`` lines in
  $ConfigPath
and cannot tell which one is the broker's authorization token. Set it by hand
and re-run without -NatsToken.
"@
    }
    $escaped = $Token -replace '\$', '$$$$'
    $new = [regex]::Replace($content, $pattern, ('$1"' + $escaped + '"'))
    $utf8NoBom = New-Object System.Text.UTF8Encoding $false
    [System.IO.File]::WriteAllText($ConfigPath, $new, $utf8NoBom)

    # Read back rather than trust the write. This function's failure mode is
    # not "throws" -- it is "leaves a config the fleet cannot authenticate
    # against", and the caller has already stopped the service by this point.
    $after = [System.IO.File]::ReadAllText($ConfigPath)
    $want = '(?m)^[^\S\r\n]*token:[^\S\r\n]*"' + [regex]::Escape($Token) + '"'
    if ($after -notmatch $want) {
        throw "token substitution did not take effect in $ConfigPath"
    }
}

$ErrorActionPreference = 'Stop'

# === Agent-mode knobs (#234 -- mirror of deploy-backend.ps1) ================
# When this script is uploaded to OBJECT_SCRIPTS via
# `kanade script publish deploy-nats <v> <edited-copy>` and a manifest
# references it through `execute.script_object`, PowerShell runs the body
# with NO CLI args -- the `param()` block takes defaults, so
# `$SourceDir = $PSScriptRoot` ends up `$null` and the folder-install path
# fails fast.
#
# Set the `$AgentSource*` constants BEFORE publishing to switch into
# agent mode: the script downloads nats-server.exe from
# OBJECT_APP_PACKAGES into a temp staging dir, reuses the existing
# nats-server.conf on the destination host, then runs the existing install
# flow against that staging dir. Leave them empty (= default) to keep the
# manual `-SourceDir <folder>` flow working unchanged for first-host
# bootstrap.
#
# `Get-FileHash <nats-server.exe> -Algorithm SHA256` for the Sha256 value
# -- a mismatch aborts BEFORE the swap, so a MITM / corrupted upload leaves
# the running broker intact.
#
# Unlike deploy-backend.ps1 there is no boot-sentinel quarantine / arm-for-
# swap step: nats-server is upstream's binary, not a kanade build, so it
# carries none of the `check-quarantine` / `arm-for-swap` subcommands.
$AgentSourceUrl       = ''   # e.g. 'http://kanade-backend.local:8080'
$AgentSourceVersion   = ''   # e.g. '2.10.20' (the nats-server release)
$AgentSourceSha256    = ''   # lowercase hex of the uploaded nats-server.exe
# Bearer for /api/app-packages (the endpoint returns HTTP 401 without it
# when the backend gates app-packages on auth). Same token the agent uses
# against the backend HTTP API. Leave empty if app-packages is unauthed.
$AgentSourceAuthToken = ''
# How long BITS keeps retrying after a transient transfer error before
# giving up (the download time itself is unbounded). Mirrors the backend
# knob; the outer manifest `timeout:` still bounds the whole job.
$AgentDownloadRetryTimeoutSecs = 1800
# ===========================================================================

# If the agent-mode knobs are set, download the binary into a temp staging
# dir + repoint `$SourceDir` at it before the existing install flow runs.
$AgentStaging = $null

# Trap defined BEFORE any throw-prone agent-mode code: PowerShell's `trap`
# only catches terminating errors that fire AFTER its definition in the
# same scope, so placing it later would leak the staging dir on a failed
# download / sha-verify. `$AgentStaging` is $null above so the Test-Path
# guard never throws on a not-yet-bound name. `break` re-throws the
# original error after cleanup.
trap {
    if ($AgentStaging -and (Test-Path $AgentStaging)) {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $AgentStaging
    }
    break
}

if ($AgentSourceUrl) {
    if (-not $AgentSourceVersion) {
        throw 'deploy-nats (agent mode): $AgentSourceVersion must be set alongside $AgentSourceUrl.'
    }
    if (-not $AgentSourceSha256) {
        throw 'deploy-nats (agent mode): $AgentSourceSha256 must be set -- leaving it blank would silently install whatever the backend serves.'
    }

    $tmpRoot = [System.IO.Path]::GetTempPath()
    $AgentStaging = Join-Path $tmpRoot ('kanade-deploy-nats-' + [System.Guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Path $AgentStaging | Out-Null
    $stagedExe = Join-Path $AgentStaging 'nats-server.exe'

    $url = "$($AgentSourceUrl.TrimEnd('/'))/api/app-packages/nats-server/$AgentSourceVersion"
    Write-Host "deploy-nats (agent mode): downloading nats-server $AgentSourceVersion from $url (BITS)"
    # BITS so a transient network drop resumes from the last byte instead
    # of restarting; -Priority Foreground runs at interactive speed during
    # the deploy window. Bearer auth via -CustomHeaders (BITS on PS 5.1+).
    # On any throw (HTTP error, BITS stopped, sha mismatch below) the trap
    # above clears $AgentStaging before re-throwing -- no leaked tmp dir.
    $bitsHeaders = @()
    if ($AgentSourceAuthToken) {
        $bitsHeaders += "Authorization: Bearer $($AgentSourceAuthToken.Trim())"
    }
    $bitsArgs = @{
        Source       = $url
        Destination  = $stagedExe
        Priority     = 'Foreground'
        RetryTimeout = $AgentDownloadRetryTimeoutSecs
    }
    if ($bitsHeaders.Count -gt 0) {
        $bitsArgs.CustomHeaders = $bitsHeaders
    }
    Start-BitsTransfer @bitsArgs

    # Sha verify BEFORE swap. Inline Remove-Item + throw is redundant with
    # the trap (which would also fire) but keeps the mismatch message even
    # if a future refactor bypasses the trap.
    $actual = (Get-FileHash $stagedExe -Algorithm SHA256).Hash.ToLowerInvariant()
    $expected = $AgentSourceSha256.ToLowerInvariant()
    if ($actual -ne $expected) {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $AgentStaging
        throw "deploy-nats (agent mode): sha256 mismatch -- expected=$expected actual=$actual. Refusing to install (possible MITM / corrupted upload)."
    }
    Write-Host "deploy-nats (agent mode): sha256 verified"

    # nats-server.conf: reuse the one already installed (the operator's
    # production config, with their token edits). Agent-mode is an upgrade
    # path, not a fresh install -- error fast if it's absent rather than
    # reaching for a default sample that would clobber the live token.
    #
    # This path is spelled out literally rather than via $configDst because
    # $configDir / $configDst are computed AFTER this block (forward
    # reference). It MUST stay in sync with $configDst below if the install
    # layout ever changes. (Mirrors deploy/backend.ps1's agent-mode block.)
    $existingConfig = Join-Path (Join-Path $env:ProgramData 'Kanade') 'config\nats-server.conf'
    if (Test-Path $existingConfig) {
        Copy-Item -Force $existingConfig (Join-Path $AgentStaging 'nats-server.conf')
        Write-Host "deploy-nats (agent mode): reusing existing nats-server.conf from $existingConfig"
    } else {
        Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $AgentStaging
        throw "deploy-nats (agent mode): no existing nats-server.conf at $existingConfig -- agent-mode is an upgrade path, not a fresh install. Run with -SourceDir <folder> manually for the initial install."
    }

    # The install vars below ($exeSrc / $configSrc) are computed from
    # $SourceDir *after* this block, so repointing it here is enough.
    $SourceDir = $AgentStaging
}

$binDir    = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot  = Join-Path $env:ProgramData  'Kanade'
$configDir = Join-Path $dataRoot 'config'
$natsDir   = Join-Path $dataRoot 'nats'
$jsDir     = Join-Path $natsDir  'jetstream'
$logsDir   = Join-Path $dataRoot 'logs'

$exeName    = 'nats-server.exe'
$configName = 'nats-server.conf'
$exeSrc     = Join-Path $SourceDir $exeName
$configSrc  = Join-Path $SourceDir $configName
$exeDst     = Join-Path $binDir    $exeName
$configDst  = Join-Path $configDir $configName

if (-not (Test-Path $exeSrc)) {
    throw "Missing '$exeName' in '$SourceDir'. Run `build-release.ps1 -Roles nats` first to populate it."
}
if (-not (Test-Path $configSrc)) {
    throw "Missing '$configName' in '$SourceDir'. Run `build-release.ps1 -Roles nats` first to populate it."
}

$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -ne 'Stopped') {
    Write-Host "Stopping $ServiceName..."
    Stop-Service -Name $ServiceName -Force
    $svc.WaitForStatus('Stopped', '00:00:30')
}

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

foreach ($d in @($binDir, $configDir, $natsDir, $jsDir, $logsDir)) {
    if (-not (Test-Path $d)) {
        New-Item -ItemType Directory -Path $d -Force | Out-Null
    }
}

Write-Host "Installing $exeName -> $exeDst"
Copy-Item -Path $exeSrc -Destination $exeDst -Force

# Keep the config that is currently working, so a failure between here and the
# token substitution can put it back. The service is already stopped at this
# point: without this, a throw below leaves the host with the repo's
# PLACEHOLDER token and a dead broker -- which is not a config error an
# operator can see, it is a fleet that cannot authenticate, discovered later.
#
# IN MEMORY, not a .bak file. A copy on disk would be a plaintext duplicate of
# the fleet credential inheriting whatever ACL $configDir hands out -- the
# hardening two thirds of this file exists for, undone -- and one more of them
# would accumulate per run (review #1290, claude). The config is a few KB.
$configBefore = if (Test-Path $configDst) { [System.IO.File]::ReadAllText($configDst) } else { $null }

if ($ForceConfig -or -not (Test-Path $configDst)) {
    $verb = if (Test-Path $configDst) { 'Overwriting' } else { 'Seeding' }
    Write-Host "$verb $configName -> $configDst"
    Copy-Item -Path $configSrc -Destination $configDst -Force
} else {
    Write-Host "Keeping existing $configDst (pass -ForceConfig to overwrite)."
}

# Apply -NatsToken before the ACL gets locked down. The script runs
# as Admin so we still have write access either way, but doing the
# substitution first keeps the on-disk content right by the time
# the file is readable only by SYSTEM + Administrators.
if ($NatsToken) {
    Write-Host "Substituting authorization.token in $configDst"
    try {
        Set-NatsServerToken -ConfigPath $configDst -Token $NatsToken
    } catch {
        # Roll the config back before rethrowing. The alternative -- what this
        # script did until now -- is a stopped service next to a config holding
        # a credential nobody provisioned, and the operator finds out when the
        # fleet stops answering.
        if ($null -ne $configBefore) {
            $utf8NoBom = New-Object System.Text.UTF8Encoding $false
            [System.IO.File]::WriteAllText($configDst, $configBefore, $utf8NoBom)
            Write-Warning "Token substitution failed; restored the previous $configName. The service is still stopped -- start it with: Start-Service $ServiceName"
        }
        throw
    }
}

# Harden the ACL on nats-server.conf. The NATS bearer token lives
# inside this file in plaintext (the broker doesn't have a registry
# back-end like kanade-agent / kanade-backend), so any logged-in user
# would otherwise be able to read it. Strip non-admin ACEs the same
# way deploy-{agent,backend}.ps1 do for their hardened registry keys.
#
# icacls.exe (Win32 native) instead of Get-Acl / Set-Acl: those
# cmdlets auto-load `Microsoft.PowerShell.Security`, which fails on
# some elevated pwsh sessions with a CouldNotAutoloadMatchingModule
# error. icacls is a bundled OS exe with no PowerShell-module
# dependency, so it works in every context the deploy script will
# ever run in.
#
# SIDs (instead of "SYSTEM" / "Administrators" names) are used to
# stay locale-agnostic -- non-EN Windows installs translate the
# display names, but SID literals always resolve correctly:
#   *S-1-5-18         = NT AUTHORITY\SYSTEM
#   *S-1-5-32-544     = BUILTIN\Administrators
Write-Host "Hardening ACL on $configDst (SYSTEM + Administrators only)"
$icaclsOut = & icacls $configDst /inheritance:r /grant:r '*S-1-5-18:F' /grant:r '*S-1-5-32-544:F' 2>&1
if ($LASTEXITCODE -ne 0) {
    throw "icacls failed on ${configDst} (exit $LASTEXITCODE):`n$icaclsOut"
}

# nats-server.exe detects when it's running under SCM via its own
# StartServiceCtrlDispatcher path; we don't need NSSM or sc.exe wrappers.
$binPath = "`"$exeDst`" --config `"$configDst`""
if (-not $svc) {
    Write-Host "Creating service $ServiceName"
    $null = New-Service `
        -Name           $ServiceName `
        -BinaryPathName $binPath `
        -StartupType    Automatic `
        -DisplayName    'Kanade NATS Broker' `
        -Description    'NATS broker for the kanade endpoint-management fleet (kanadehq/kanade).'
} else {
    Write-Host "Updating $ServiceName configuration"
    Set-Service -Name $ServiceName -StartupType Automatic
}

# Failure recovery -- restart on any non-clean-stop exit.
#
#   actions= restart/5000/restart/15000/restart/60000
#     1st failure: wait 5s; 2nd: 15s; 3rd: 60s.
#   reset= 86400
#     Reset failure counter after 24h of clean uptime.
#   failureflag <svc> 1   (= bFailureActionsOnNonCrashFailures)
#     Trigger the actions on ANY non-stop exit, not just crashes.
Write-Host "Configuring failure recovery on $ServiceName"
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failure failed (exit $LASTEXITCODE)" }
& sc.exe failureflag $ServiceName 1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failureflag failed (exit $LASTEXITCODE)" }

if (-not $NoFirewall) {
    foreach ($entry in @(
        @{ Port = 4222; Label = 'broker' }
    )) {
        $ruleName = "$ServiceName ($($entry.Label) TCP $($entry.Port))"
        $existing = Get-NetFirewallRule -DisplayName $ruleName -ErrorAction SilentlyContinue
        if ($existing) {
            Write-Host "Firewall rule '$ruleName' already exists; leaving it alone."
        } else {
            New-NetFirewallRule `
                -DisplayName $ruleName `
                -Direction   Inbound `
                -Protocol    TCP `
                -LocalPort   $entry.Port `
                -Action      Allow `
                -Profile     Any `
                | Out-Null
            Write-Host "Created firewall rule '$ruleName'."
        }
    }
    # Converge, don't just stop creating it. Every host deployed before
    # this change carries the monitoring rule, and operators re-run the
    # deploy script -- they do not undeploy -- so dropping it from the list
    # above would leave the rule in place forever on exactly the hosts
    # that already have it.
    $staleName = "$ServiceName (monitoring TCP 8222)"
    if (Get-NetFirewallRule -DisplayName $staleName -ErrorAction SilentlyContinue) {
        # Report what is true, not what was attempted. `-ErrorAction
        # SilentlyContinue` on the removal would swallow the failure and the
        # next line would claim a port was closed that is still open -- the
        # one outcome a hardening step must never produce. `Continue` (not
        # `Stop`, and not the script-wide 'Stop' preference set above)
        # because this block runs BEFORE Start-Service: aborting here would
        # leave the broker down over a cleanup step, trading an open
        # monitoring port for a fleet-wide outage.
        Remove-NetFirewallRule -DisplayName $staleName -ErrorAction Continue
        if (Get-NetFirewallRule -DisplayName $staleName -ErrorAction SilentlyContinue) {
            Write-Warning "Firewall rule '$staleName' is STILL PRESENT after an attempted removal -- the monitoring port may remain reachable from the network. A rule pushed by Group Policy cannot be removed locally: drop it in the policy. Either way the loopback bind in nats-server.conf is what actually closes the port (see .DESCRIPTION)."
        } else {
            Write-Host "Removed firewall rule '$staleName' (monitoring is loopback-only; see .DESCRIPTION)."
        }
    }
    # Not detectable cheaply from here, and worth saying out loud rather
    # than leaving an operator to conclude monitoring is closed when a
    # program-scoped rule is still admitting it.
    # Backtick, not backslash: `\"` is not an escape in PowerShell, so the
    # backslash renders literally and the quote terminates the string.
    Write-Host "NOTE: a program-scoped allow rule for nats-server.exe (created by the Windows first-run prompt) admits every port it listens on, whatever the rules above say. Closing monitoring for real means 'http: `"127.0.0.1:8222`"' in nats-server.conf; check with: netsh advfirewall firewall show rule name=all dir=in | Select-String nats"
} else {
    Write-Host "Firewall: -NoFirewall set; skipping rules."
}

if (-not $NoStart) {
    # Refuse to START on the repo's placeholder token -- but only to start.
    # The check belongs here, not beside the config copy: a fresh install
    # legitimately seeds the sample config, `-NoStart` legitimately ends with
    # no running broker, and a binary-only upgrade should still replace the
    # exe on a host whose config was left at the placeholder. Blocking those
    # was the first version of this guard, and it broke the bare
    # `.\deploy-nats.ps1` first-install flow documented above (review #1290,
    # claude). What is never acceptable is the broker actually coming up on a
    # credential published in a public repository: locked away from its own
    # fleet, and open to anyone who read the README.
    $installed = [System.IO.File]::ReadAllText($configDst)
    if ($installed -match 'token:\s*"CHANGE-ME') {
        throw @"
Not starting ${ServiceName}: $configName still carries this repo's PLACEHOLDER
token. The binary and the service are installed -- finish the job with:

  .\deploy-nats.ps1 -NatsToken '<your-fleet-token>'

(or edit $configDst by hand and run: Start-Service $ServiceName)
"@
    }
    Write-Host "Starting $ServiceName"
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus('Running', '00:00:30')
}

Write-Host ''
Write-Host "Installed bin: $exeDst"
Write-Host "Runtime root:  $natsDir"
Write-Host "Config (hardened ACL): $configDst"
& $exeDst --version

# Agent-mode staging dir cleanup on success. The `trap` above handles
# failure paths; this clears the temp dir for the happy path so the agent
# doesn't leak a staging dir per upgrade.
if ($AgentStaging -and (Test-Path $AgentStaging)) {
    Remove-Item -Recurse -Force -ErrorAction SilentlyContinue $AgentStaging
}

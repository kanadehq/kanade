#Requires -RunAsAdministrator
#Requires -Version 5.1

<#
.SYNOPSIS
  Install / update kanade-agent as a Windows service.

.DESCRIPTION
  Copies kanade-agent.exe into %ProgramFiles%\Kanade\ and agent.toml
  into %ProgramData%\Kanade\config\, ensures the runtime data / log
  dirs exist under %ProgramData%\Kanade\, and (re-)registers a
  Windows service that runs the agent against the installed config.
  Re-running upgrades the binary in place; existing agent.toml is
  preserved unless -ForceConfig is passed. The split follows the
  Windows convention used in the spec §2.11 layout (README "Production
  install layout"): Program Files is the read-only install root,
  ProgramData holds writable runtime state.

.PARAMETER SourceDir
  Directory holding kanade-agent.exe and agent.toml. Defaults to the
  directory this script lives in, so the common pattern is to drop
  exe + config + this script into one folder and run it.

.PARAMETER ServiceName
  Windows service name. Default: KanadeAgent.

.PARAMETER ForceConfig
  Overwrite the installed agent.toml with the one in -SourceDir. Off
  by default so config tweaks on the target machine survive upgrades.

.PARAMETER Recreate
  Drop the existing Windows service entirely (sc.exe delete + wait
  for SCM to acknowledge) before re-creating it. Useful for
  recovering from a half-installed service (e.g., one that was
  created without windows-service crate integration in v0.3.1 and
  now refuses to start). Implicitly stops the service first.

.PARAMETER NoStart
  Install + register the service but don't start it.

.PARAMETER NatsToken
  If set, write the NATS bearer token to
  HKLM\SOFTWARE\kanade\agent\NatsToken (REG_SZ) and harden the ACL
  on that key so only SYSTEM + Administrators can read it. The
  agent reads this at startup ahead of $env:KANADE_NATS_TOKEN.
  Required when the broker is started with `authorization { token: ... }`.

.PARAMETER NatsUser
  Optional per-role NATS user, written to HKLM\SOFTWARE\kanade\agent\NatsUser
  (REG_SZ) with the same hardened ACL as the token. Must be given together
  with -NatsPassword: a half pair is refused before anything is written,
  because the client treats a half pair as a configuration error too.

  This is the agent's own credential. There is deliberately no shared user,
  so it is never written under another role's key, and this script never
  provisions the backend's, a CLI's or the break-glass credential.

  The token is unaffected: a host given both keeps both, and the client
  chooses between them. Re-running without these leaves an existing pair
  untouched; passing new values replaces both halves together. Nothing here
  switches the broker from the shared token to users -- that is a separate
  broker-side change, and the user is inert until it happens.

.PARAMETER NatsPassword
  The password for -NatsUser, written to
  HKLM\SOFTWARE\kanade\agent\NatsPassword (REG_SZ, hardened ACL). Never
  echoed or logged by this script. Pass both -NatsUser and -NatsPassword, or
  neither.

.PARAMETER CommandKeys
  If set, write the command-signing keyring (#1165) to
  HKLM\SOFTWARE\kanade\agent\CommandKeys (REG_SZ) with the same
  hardened ACL. A JSON ARRAY of entries, each `{kid, public_key,
  label?, max_age_secs?}` -- include BOTH the backend key and the
  break-glass key; the value is replaced, not merged.

  Provisioned here rather than over NATS because the NATS route is
  circular: once agents reject unsigned commands, the command that
  would carry the keyring to a machine with an empty ring is itself
  rejected. Get the entries from `kanade-backend command-key-generate`
  (backend key) and `kanade command-key break-glass` (break-glass).

  These are PUBLIC keys, so the value is not a secret -- the ACL is
  there to stop tampering, not disclosure. Whoever can write this
  decides what the machine will execute.

.PARAMETER RequireSignedCommands
  If set, write RequireSignedCommands=1 to
  HKLM\SOFTWARE\kanade\agent\RequireSignedCommands (REG_SZ), the value
  `command_verify.rs::enforce_requested()` reads once at agent startup.
  Once set, the agent REFUSES any command it cannot verify against its
  CommandKeys ring instead of merely reporting it.

  Requires a non-empty keyring: pass -CommandKeys alongside this on a
  brand-new machine, or make sure CommandKeys is already provisioned
  on one being re-run, and every public_key must be Base64 for a 32-byte
  Ed25519 key. An enforcing agent with an empty ring REFUSES every command
  (it fails closed, see command_verify.rs), so this script refuses to
  install rather than write a value that would leave the host unable to run
  anything.

.EXAMPLE
  # Drop deploy-agent.ps1 + kanade-agent.exe + agent.toml in a folder,
  # then on the target:
  PS> .\deploy-agent.ps1

.EXAMPLE
  # Re-run after a binary update, forcing fresh config:
  PS> .\deploy-agent.ps1 -ForceConfig

.EXAMPLE
  # Provision a fleet-wide NATS token (production):
  PS> .\deploy-agent.ps1 -NatsToken '<your-fleet-token>'

.EXAMPLE
  # Kit a host with its per-role NATS user next to the token (the broker is
  # not switched to users by this; the client uses the user once it is):
  PS> .\deploy-agent.ps1 -NatsToken '<your-fleet-token>' -NatsUser '<agent-user>' -NatsPassword '<agent-password>'

.EXAMPLE
  # Kit a new machine with both the NATS token and the command-signing
  # keyring, so it starts out able to verify rather than needing someone
  # to remember a follow-up step:
  PS> .\deploy-agent.ps1 -NatsToken '<your-fleet-token>' -CommandKeys @'
  [{"kid":"backend-20260728","label":"backend","public_key":"..."},
   {"kid":"break-glass-20260730-1432","label":"break-glass","public_key":"...","max_age_secs":3600}]
  '@

.EXAMPLE
  # Kit a brand-new machine that should enforce signed commands from day
  # one, rather than leaving that as a manual post-install step:
  PS> .\deploy-agent.ps1 -NatsToken '<your-fleet-token>' -CommandKeys @'
  [{"kid":"backend-20260728","label":"backend","public_key":"..."}]
  '@ -RequireSignedCommands

.EXAMPLE
  # Recover from a stuck / broken service:
  PS> .\deploy-agent.ps1 -Recreate
#>

[CmdletBinding()]
param(
    [string]$SourceDir   = $PSScriptRoot,
    [string]$ServiceName = 'KanadeAgent',
    [switch]$ForceConfig,
    [switch]$Recreate,
    [switch]$NoStart,
    [string]$NatsToken   = '',
    [string]$NatsUser    = '',
    [string]$NatsPassword = '',
    [string]$CommandKeys = '',
    [switch]$RequireSignedCommands
)

$ErrorActionPreference = 'Stop'

# Write a secret value to HKLM\SOFTWARE\kanade\<subkey>\<value> and
# strip non-admin ACEs from the leaf key. Mirrors the helper in
# deploy-backend.ps1; here we only ever write NatsToken, but the
# generic shape keeps both scripts in sync. The path is what
# kanade-shared::secrets::read_hklm_value() reads at startup,
# preferred ahead of $env:KANADE_NATS_TOKEN.
#
# Implementation note: uses the pure .NET Microsoft.Win32.Registry
# API rather than the New-Item / Set-ItemProperty / Get-Acl / Set-Acl
# cmdlets. The cmdlet path auto-loads Microsoft.PowerShell.Management
# + Microsoft.PowerShell.Security on first use, and those module
# loads fail on some elevated / constrained-language pwsh sessions
# with a CouldNotAutoloadMatchingModule error. The .NET classes are
# always reachable from any PowerShell context.
function Set-KanadeRegistrySecrets {
    param(
        [Parameter(Mandatory)][string]$Subkey,
        # Value name -> value. Written through ONE open key so a pair lands
        # together; two separate calls could leave a half pair behind if the
        # second failed, and the client reads a half pair as an error.
        [Parameter(Mandatory)][System.Collections.IDictionary]$Values,
        # Overridable only so tests can aim at a scratch tree instead of the
        # real machine key.
        [string]$BasePath = 'SOFTWARE\kanade',
        [Microsoft.Win32.RegistryKey]$Root = [Microsoft.Win32.Registry]::LocalMachine
    )

    $subkeyPath = "$BasePath\$Subkey"
    $key = $Root.OpenSubKey($subkeyPath, $true)
    if (-not $key) {
        $key = $Root.CreateSubKey($subkeyPath)
    }
    # What each value held before, so a failure part-way can put it back. A
    # registry key has no multi-value transaction, and a pair with one new half
    # and one old (or one half missing) is a configuration error for the client.
    $previous = @{}
    foreach ($name in $Values.Keys) {
        $kind = $null
        $old = $key.GetValue($name, $null)
        if ($null -ne $old) { $kind = $key.GetValueKind($name) }
        $previous[$name] = @{ Value = $old; Kind = $kind }
    }
    try {
        foreach ($name in $Values.Keys) {
            $key.SetValue($name, [string]$Values[$name], [Microsoft.Win32.RegistryValueKind]::String)
        }

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
    } catch {
        $failure = $_
        foreach ($name in $previous.Keys) {
            try {
                if ($null -eq $previous[$name].Value) { $key.DeleteValue($name, $false) }
                else { $key.SetValue($name, $previous[$name].Value, $previous[$name].Kind) }
            } catch { }
        }
        throw $failure
    } finally {
        $key.Close()
    }

    # Names only, never values.
    foreach ($name in $Values.Keys) {
        Write-Host "Wrote $name to HKLM:\$subkeyPath (SYSTEM + Administrators only)."
    }
}

function Set-KanadeRegistrySecret {
    param(
        [Parameter(Mandatory)][string]$Subkey,
        [Parameter(Mandatory)][string]$ValueName,
        [Parameter(Mandatory)][string]$Value
    )
    Set-KanadeRegistrySecrets -Subkey $Subkey -Values @{ $ValueName = $Value }
}

# Both halves of the per-role NATS user or neither. Called before anything on
# the machine is touched, so a half pair fails the run with the old install
# still in place. The message names the parameters, never the values.
function Assert-KanadeNatsUserPair {
    param([string]$User, [string]$Password)
    $hasUser = -not [string]::IsNullOrEmpty($User)
    $hasPass = -not [string]::IsNullOrEmpty($Password)
    if ($hasUser -ne $hasPass) {
        throw '-NatsUser and -NatsPassword must be given together (or neither). A half pair is a configuration error for the client, so nothing was changed.'
    }
}

# Every CommandKeys entry's public_key must be standard Base64 for exactly 32
# bytes -- what the agent's `parse_keyring` decodes. A mistyped key otherwise
# installs fine and leaves the agent with a ring it cannot parse, so refuse
# here, before anything on the machine is touched. Names the kid, never echoes
# the key material beyond the length.
function Assert-KanadeCommandKeys {
    param([object[]]$Entries)
    foreach ($e in $Entries) {
        $bytes = $null
        try {
            $bytes = [Convert]::FromBase64String(([string]$e.public_key).Trim())
        } catch {
            throw "CommandKeys entry '$($e.kid)' has a public_key that is not valid Base64. Nothing was changed."
        }
        if ($bytes.Length -ne 32) {
            throw "CommandKeys entry '$($e.kid)' has a public_key that decodes to $($bytes.Length) bytes; an Ed25519 public key is exactly 32. Nothing was changed."
        }
    }
}

Assert-KanadeNatsUserPair -User $NatsUser -Password $NatsPassword

# Key material is checked this early too, so a mistyped public_key fails the run
# with the old install still in place. The fuller shape checks further down stay
# where they are; this only looks at entries that parse far enough to have a
# string public_key.
if ($CommandKeys -and $CommandKeys.Trim().StartsWith('[')) {
    $earlyParsed = $null
    try { $earlyParsed = ConvertFrom-Json -InputObject $CommandKeys.Trim() } catch { }
    $earlyEntries = @($earlyParsed) | Where-Object { $_ -and ($_.public_key -is [string]) }
    if ($earlyEntries) { Assert-KanadeCommandKeys -Entries @($earlyEntries) }
}

$binDir    = Join-Path $env:ProgramFiles 'Kanade'
$dataRoot  = Join-Path $env:ProgramData  'Kanade'
$configDir = Join-Path $dataRoot 'config'
$dataDir   = Join-Path $dataRoot 'data'
$logsDir   = Join-Path $dataRoot 'logs'

$exeName    = 'kanade-agent.exe'
$configName = 'agent.toml'
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

# Stop the existing service first so the on-disk exe isn't locked.
$svc = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
if ($svc -and $svc.Status -ne 'Stopped') {
    Write-Host "Stopping $ServiceName..."
    Stop-Service -Name $ServiceName -Force
    $svc.WaitForStatus('Stopped', '00:00:30')
}

# -Recreate: drop the existing service entirely so the New-Service
# path below runs even when a previous (broken) registration is
# already there. Useful for recovering from a stuck "service exists
# but won't start" state — Stop-Service + sc.exe delete + wait for
# SCM to actually remove the entry before continuing.
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
    Set-KanadeRegistrySecret -Subkey 'agent' -ValueName 'NatsToken' -Value $NatsToken
}
# The agent's own user, under the agent key only. Absent parameters write
# nothing, so a re-run without them never deletes a pair already there.
if ($NatsUser) {
    Set-KanadeRegistrySecrets -Subkey 'agent' -Values @{ NatsUser = $NatsUser; NatsPassword = $NatsPassword }
}

# #1165: the command-signing keyring, provisioned here for the same reason
# the NATS token is. Bootstrapping it over NATS instead is circular -- once
# agents reject unsigned commands, the command that would carry the keyring
# to a machine with an empty ring is itself rejected. A machine that cannot
# be handed a NATS token cannot join the fleet at all, so this channel is
# already the fleet's trust bootstrap; the keyring belongs beside it.
#
# Rotation does NOT come through here: the `provision-command-keys` job
# updates a ring that already exists. This is the entry that makes a newly
# kitted machine start out trusted rather than needing someone to remember.
if ($CommandKeys) {
    # Validated before writing, because every way this can be wrong is
    # silent later. A malformed ring leaves the agent holding nothing, which
    # reads as "not provisioned yet" -- and at stage 3 that machine rejects
    # every command it is sent, including the one that would fix it.
    $trimmed = $CommandKeys.Trim()
    if (-not $trimmed.StartsWith('[')) {
        throw "CommandKeys must be a JSON ARRAY of entries, even for a single key. Got: $($trimmed.Substring(0, [Math]::Min(40, $trimmed.Length)))..."
    }
    try {
        # Assign FIRST, then wrap. `@($x | ConvertFrom-Json)` looks equivalent
        # and is not: on Windows PowerShell 5.1 -- which is what runs in
        # production -- that form collapses the whole array into ONE element,
        # so a two-key ring counts as one and the duplicate check below never
        # sees a second entry to compare. Measured: 5.1 gives 1, pwsh 7 gives
        # 2, and `-InputObject` inside `@()` is no better. Assigning
        # materialises a real Object[], and `@()` on that is identity.
        $parsed = ConvertFrom-Json -InputObject $trimmed
    } catch {
        throw "CommandKeys is not valid JSON: $($_.Exception.Message)"
    }
    $entries = @($parsed)
    if ($entries.Count -eq 0) {
        throw 'CommandKeys is an empty array — that provisions no keys at all. Omit the parameter if that is what you meant.'
    }
    foreach ($e in $entries) {
        # Types are checked, not just presence. `IsNullOrWhiteSpace` coerces
        # its argument, so an unquoted `"kid": 20260728` reads as the non-empty
        # string "20260728" here and sails through — while the agent, whose
        # `KeyEntry.kid` is a `String`, rejects the JSON number outright. That
        # would push the discovery from "installing one machine" to "the ring
        # is already on the fleet", which is the whole thing this block exists
        # to prevent. Same for a bare `true` or an object where a string
        # belongs. (A non-object element, e.g. `["kid1","kid2"]`, yields $null
        # for these properties in non-strict mode, so it lands here too rather
        # than raising a .NET error — measured on 5.1 and pwsh 7.)
        if ($e.kid -isnot [string] -or $e.public_key -isnot [string]) {
            throw "every CommandKeys entry needs STRING 'kid' and 'public_key' (quote them); got: $($e | ConvertTo-Json -Compress)"
        }
        if ([string]::IsNullOrWhiteSpace($e.kid) -or [string]::IsNullOrWhiteSpace($e.public_key)) {
            throw "every CommandKeys entry needs a non-empty 'kid' and 'public_key'; got: $($e | ConvertTo-Json -Compress)"
        }
        if ($null -ne $e.label -and $e.label -isnot [string]) {
            throw "CommandKeys entry '$($e.kid)' has a non-string 'label'; got: $($e | ConvertTo-Json -Compress)"
        }
        # `max_age_secs` is what makes the agent treat an entry as break-glass
        # at all, so a quoted "900" silently demoting it to an ordinary
        # unbounded key is exactly the mistake worth catching before the fleet
        # has it.
        if ($null -ne $e.max_age_secs -and $e.max_age_secs -isnot [int] -and $e.max_age_secs -isnot [long]) {
            throw "CommandKeys entry '$($e.kid)' has a non-numeric 'max_age_secs' (do not quote it); got: $($e | ConvertTo-Json -Compress)"
        }
        # A zero window bricks the key silently. The agent turns any present
        # `max_age_secs` into a break-glass policy, and `Duration::from_secs(0)`
        # makes `verify` reject every signature whose age is not exactly zero —
        # so the entry looks provisioned, reports nothing wrong, and fails the
        # first time someone reaches for it, which is during an incident. A
        # negative value is refused by the agent's `Option<u64>` instead, but
        # only after the ring has been distributed. `kanade command-key
        # break-glass` already refuses `--max-age-mins 0`; this is the same
        # guard on the path that takes hand-authored input.
        if ($null -ne $e.max_age_secs -and $e.max_age_secs -le 0) {
            throw "CommandKeys entry '$($e.kid)' has max_age_secs = $($e.max_age_secs). A non-positive window rejects every signature made with the key. Pick a window a human can act inside."
        }
    }
    Assert-KanadeCommandKeys -Entries $entries

    # The agent keys its ring by id, so a repeat would silently drop one key
    # and every command signed by it would stop verifying with nothing to
    # explain it. `parse_keyring` refuses such a ring outright; catching it
    # here means the operator finds out while installing one machine rather
    # than after distributing to the fleet.
    $dupes = @($entries | Group-Object -Property kid | Where-Object { $_.Count -gt 1 })
    if ($dupes.Count) {
        throw "CommandKeys lists these ids more than once: $($dupes.Name -join ', '). Two different keys must never share an id."
    }

    Set-KanadeRegistrySecret -Subkey 'agent' -ValueName 'CommandKeys' -Value $trimmed

    # Read back rather than echoing the input: the point is what this machine
    # actually holds, and a report that repeats its argument proves nothing.
    # Closed in a finally, matching Set-KanadeRegistrySecret above. Leaking it
    # is harmless while this is the last thing the script does with the key,
    # but the shape gets copied.
    $rk = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SOFTWARE\kanade\agent')
    try {
        $written = $rk.GetValue('CommandKeys')
    } finally {
        $rk.Close()
    }
    # Same 5.1 trap as above — assign, then enumerate.
    $readBack = ConvertFrom-Json -InputObject $written
    $kids = @($readBack) | ForEach-Object { $_.kid }
    Write-Host "CommandKeys provisioned. kids: $($kids -join ', ')"
}

# #1155/#1165 day-1 enforcement: RequireSignedCommands=1 tells the agent to
# REFUSE unverified commands instead of just reporting them. This is the
# value `command_verify.rs::enforce_requested()` reads once at startup.
#
# With an empty ring the agent refuses EVERY command (it fails closed), so
# writing the value without a usable ring would leave the machine unable to
# run anything until someone re-provisions it locally. So this checks for a
# ring rather than trusting the operator remembered one: either -CommandKeys was passed alongside
# this run, or one is already sitting in the registry from an earlier run.
if ($RequireSignedCommands) {
    # -CommandKeys, if passed, was already validated non-empty above (the
    # empty-array throw earlier in this script) -- so a $CommandKeys value
    # reaching here always means at least one entry. The registry fallback
    # below cannot make the same assumption: a value that merely EXISTS is
    # not the same as one that holds a key. `[]`, whitespace, or corrupt
    # JSON left over from some other tooling would pass a whitespace check
    # while still leaving the agent with an empty ring -- exactly the
    # silently-inert state this guard exists to catch.
    $hasRing = [bool]$CommandKeys
    if (-not $hasRing) {
        $existingKeysKey = [Microsoft.Win32.Registry]::LocalMachine.OpenSubKey('SOFTWARE\kanade\agent')
        if ($existingKeysKey) {
            try {
                $existing = $existingKeysKey.GetValue('CommandKeys')
            } finally {
                $existingKeysKey.Close()
            }
            if (-not [string]::IsNullOrWhiteSpace($existing)) {
                try {
                    $existingParsed = ConvertFrom-Json -InputObject $existing
                } catch {
                    throw "-RequireSignedCommands was passed, and this machine has a CommandKeys registry value, but it is not valid JSON: $($_.Exception.Message). Fix or re-provision it with -CommandKeys before enabling enforcement."
                }
                # Count is not enough: `[null]` or `[{}]` parse as one
                # element but hold no usable key, so the agent's own
                # `parse_keyring` (String kid + public_key required) rejects
                # them and starts with an empty ring anyway -- same
                # silently-inert outcome the whole guard exists to prevent.
                # Only count entries that actually look like a KeyEntry, the
                # same shape check the -CommandKeys write path above applies.
                $validEntries = @($existingParsed) | Where-Object {
                    $_ -and
                    ($_.kid -is [string]) -and (-not [string]::IsNullOrWhiteSpace($_.kid)) -and
                    ($_.public_key -is [string]) -and (-not [string]::IsNullOrWhiteSpace($_.public_key))
                }
                $hasRing = @($validEntries).Count -gt 0
                # A key that is present but not a real 32-byte Ed25519 key leaves
                # the agent with an empty ring, which with enforcement on
                # refuses every command. Refuse before writing the flag.
                if ($hasRing) { Assert-KanadeCommandKeys -Entries @($validEntries) }
            }
        }
    }
    if (-not $hasRing) {
        throw "-RequireSignedCommands was passed but this machine has no CommandKeys (neither -CommandKeys nor a non-empty existing registry value). Enforcing with an empty ring makes the agent refuse every command, so this refuses rather than writing that value. Pass -CommandKeys too."
    }
    Set-KanadeRegistrySecret -Subkey 'agent' -ValueName 'RequireSignedCommands' -Value '1'
    Write-Host 'RequireSignedCommands provisioned. This agent will refuse unverified commands.'
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
        -DisplayName    'Kanade Agent' `
        -Description    'Kanade endpoint management agent (kanadehq/kanade).'
} else {
    Write-Host "Updating $ServiceName configuration"
    # Existing service: binPath rarely needs changing (the exe path
    # is stable across upgrades since we always write to
    # %ProgramFiles%\Kanade\kanade-agent.exe), so only reconfirm
    # the start type. Operator with a custom binPath can adjust
    # manually via `sc.exe config`.
    Set-Service -Name $ServiceName -StartupType Automatic
}

# Failure recovery — restart on any non-clean-stop exit.
#
# This is what makes the agent's self-update path work: when
# self_update.rs swaps the binary into place it calls
# std::process::exit(64), and SCM has to interpret that exit as a
# recoverable failure for the configured restart action to fire.
#
#   actions= restart/5000/restart/15000/restart/60000
#     1st failure: wait 5s then restart
#     2nd failure: 15s
#     3rd failure: 60s
#   reset= 86400
#     Reset the failure counter after 24h of clean uptime.
#   failureflag <svc> 1   (= bFailureActionsOnNonCrashFailures)
#     Trigger the actions on ANY non-stop exit, not just crashes.
#     Without this, exit(64) is silently treated as a normal exit
#     and the service stays stopped after self-update.
Write-Host "Configuring failure recovery on $ServiceName"
& sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/15000/restart/60000 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failure failed (exit $LASTEXITCODE)" }
& sc.exe failureflag $ServiceName 1 | Out-Null
if ($LASTEXITCODE -ne 0) { throw "sc.exe failureflag failed (exit $LASTEXITCODE)" }

if (-not $NoStart) {
    Write-Host "Starting $ServiceName"
    Start-Service -Name $ServiceName
    (Get-Service -Name $ServiceName).WaitForStatus('Running', '00:00:30')
}

Write-Host ''
Write-Host "Installed bin: $exeDst"
Write-Host "Runtime root:  $dataRoot"
& $exeDst --version

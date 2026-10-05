<#
.SYNOPSIS
  Readiness before, and fallout after, switching the broker from the shared
  token to the role-level NATS users. Talks only to the backend HTTP API.

.DESCRIPTION
  The switch is atomic: the broker cannot accept the old token and the new
  users at once, so a host that does not hold a working user pair is locked
  out at that instant. Such a host also drops off the broker's connection
  list and stops heartbeating, which looks exactly like a powered-off one.
  So readiness has to be established beforehand, from outside the broker, and
  the damage measured afterwards by comparison. This script does both. It
  never connects to the broker, and it never switches anything; the procedure
  is in book/src/operations/nats-user-switch.md and is run by a person.

  Modes
    Readiness  List every registered agent that is NOT ready; exit 1 if any.
               Ready means all of:
                 * an agent version >= -MinAgentVersion (default 0.61.0, the
                   release that introduced user credentials in the client),
                 * online now, or its last heartbeat is within -RecentWithin,
                 * the credential-presence check named by -CheckName has
                   reported `ok` (and is not stale).
               The check job is not part of this repository, so -CheckName
               has no default and is never guessed.
    Snapshot   Write the hosts alive right now to -SnapshotPath.
    Compare    Read a snapshot and list hosts that were alive and are not
               now, hosts that heartbeated after the switch, new hosts and a
               count of live hosts by authenticated NATS user. Exit 1 when
               more than -MaxDisappeared hosts disappeared.

  Backend and break-glass credentials are not distributed to agents and are
  not covered here; the procedure has a manual checklist for them.

  Exit codes: 0 fine, 1 findings (not ready / too many disappeared),
  2 cannot determine (usage, backend unreachable, bad snapshot),
  3 Compare ran too early to tell a lock-out from a host that has not yet
  missed enough heartbeats.

  The API token comes from -Token or $env:KANADE_API_TOKEN, the URL from
  -BackendUrl or $env:KANADE_BACKEND_URL. Neither is ever printed, and
  response bodies and check details are never printed.

.EXAMPLE
  ./scripts/ops/nats-switch-check.ps1 -Mode Readiness -BackendUrl https://kanade.example -CheckName <credential-check>
  ./scripts/ops/nats-switch-check.ps1 -Mode Snapshot -SnapshotPath before.json
  ./scripts/ops/nats-switch-check.ps1 -Mode Compare -SnapshotPath before.json -SwitchedAt 2026-01-01T09:00:00Z -WaitSeconds 150 -MaxDisappeared 0
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('Readiness', 'Snapshot', 'Compare')][string]$Mode,
    [string]$BackendUrl,
    [string]$Token,
    [string]$CheckName,
    [string]$MinAgentVersion = '0.61.0',
    [int]$RecentWithinHours = 24,
    [string]$CheckedSince,
    [string]$SnapshotPath,
    [switch]$Force,
    [string]$SwitchedAt,
    [int]$WaitSeconds = 0,
    [int]$MaxDisappeared = -1,
    [int]$HeartbeatSeconds = 60,
    [string]$ExpectedUser = 'agent',
    [int]$Retries = 5,
    [int]$RetryDelaySeconds = 10
)

$ErrorActionPreference = 'Stop'

# The backend counts an agent online when its last heartbeat is newer than
# this (ALIVE_THRESHOLD in crates/kanade-backend/src/api/agents.rs).
$AliveSeconds = 120
$SnapshotFormat = 1
# The backend caps what one /api/agents call examines at this many rows.
$FetchCap = 10000

if (-not $BackendUrl) { $BackendUrl = $env:KANADE_BACKEND_URL }
if (-not $Token) { $Token = $env:KANADE_API_TOKEN }

function Fail([string]$Message, [int]$Code = 2) {
    [Console]::Error.WriteLine("error: $(Hide-Secrets $Message)")
    exit $Code
}

function Hide-Secrets([string]$Text) {
    if ($null -eq $Text) { return '' }
    if ($Token) { $Text = $Text.Replace($Token, '***') }
    # Drop user:pass@ from any URL in the text.
    $Text = $Text -replace '([a-zA-Z][a-zA-Z0-9+.-]*://)[^/@\s]*@', '$1'
    # Strip control characters so a hostname cannot rewrite the terminal.
    return ($Text -replace '[\x00-\x1F\x7F]', '?')
}

function Say([string]$Text = '') { Write-Host (Hide-Secrets $Text) }

function ConvertTo-Utc($Value) {
    if ($null -eq $Value -or "$Value" -eq '') { return $null }
    if ($Value -is [datetime]) { return $Value.ToUniversalTime() }
    $parsed = [datetime]::MinValue
    $style = [System.Globalization.DateTimeStyles]::AssumeUniversal -bor [System.Globalization.DateTimeStyles]::AdjustToUniversal
    if ([datetime]::TryParse("$Value", [System.Globalization.CultureInfo]::InvariantCulture, $style, [ref]$parsed)) { return $parsed }
    return $null
}

function Test-Url {
    if (-not $BackendUrl) { Fail 'no backend URL: pass -BackendUrl or set KANADE_BACKEND_URL.' }
    $u = $null
    if (-not ([uri]::TryCreate($BackendUrl, [UriKind]::Absolute, [ref]$u)) -or $u.Scheme -notin @('http', 'https')) {
        Fail 'the backend URL must be an absolute http(s) URL.'
    }
    if ($u.UserInfo) { Fail 'put the token in -Token, not in the URL.' }
    $script:Base = $BackendUrl.TrimEnd('/')
}

# The one place that talks to the backend. Retries because the backend may
# itself be reconnecting to the broker while the switch is in flight. A
# request that fails to the end is an error, never an empty answer: an empty
# host list would read as "nothing is wrong".
$script:ServerNow = $null
$script:UsedLocalClock = $false
$script:CanReadHeaders = $false

function Invoke-Backend([string]$Path) {
    $headers = @{}
    if ($Token) { $headers['Authorization'] = "Bearer $Token" }
    $uri = "$script:Base$Path"
    $last = ''
    for ($i = 1; $i -le $Retries; $i++) {
        try {
            $hv = $null
            # Windows PowerShell 5.1 cannot return response headers from
            # Invoke-RestMethod; the clock fallback below covers it.
            if ($script:CanReadHeaders) {
                $body = Invoke-RestMethod -Uri $uri -Headers $headers -Method Get -TimeoutSec 30 -ResponseHeadersVariable hv
            } else {
                $body = Invoke-RestMethod -Uri $uri -Headers $headers -Method Get -TimeoutSec 30
            }
            $date = $null
            if ($hv -and $hv['Date']) { $date = ConvertTo-Utc (@($hv['Date'])[0]) }
            if ($date) { $script:ServerNow = $date } else { $script:ServerNow = $null }
            return $body
        } catch {
            $status = $null
            try { $status = [int]$_.Exception.Response.StatusCode } catch { $status = $null }
            if ($status -in 401, 403) { Fail "the backend refused the API token (HTTP $status). Nothing was determined." }
            $last = Hide-Secrets ("$($_.Exception.Message)")
            if ($last.Length -gt 200) { $last = $last.Substring(0, 200) }
            Say "warning: request to the backend failed (attempt $i of $Retries): $last"
            if ($i -lt $Retries) {
                Say '  The backend may be reconnecting to the broker during the switch; retrying.'
                Start-Sleep -Seconds $RetryDelaySeconds
            }
        }
    }
    Fail "the backend could not be reached after $Retries attempts ($last). This is NOT an empty result: nothing was determined, so do not read it as 'no hosts affected'."
}

function Get-Now {
    if ($script:ServerNow) { return $script:ServerNow }
    if (-not $script:UsedLocalClock) {
        Say 'note: the backend sent no usable Date header; using this machine''s clock, which can skew the alive/offline judgement.'
        $script:UsedLocalClock = $true
    }
    return [datetime]::UtcNow
}

function Get-Agents {
    $r = Invoke-Backend '/api/agents'
    $now = Get-Now
    $list = if ($null -eq $r) { @() } else { @($r) }
    if ($list.Count -ge $FetchCap) {
        Say "warning: the backend returned $($list.Count) agents, the most it examines in one call; the list may be incomplete."
    }
    $map = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([System.StringComparer]::Ordinal)
    foreach ($a in $list) {
        if (-not $a.pc_id) { continue }
        $hb = ConvertTo-Utc $a.last_heartbeat
        $since = ConvertTo-Utc $a.nats_user_since
        $map["$($a.pc_id)"] = [pscustomobject]@{
            pc_id           = "$($a.pc_id)"
            hostname        = "$($a.hostname)"
            agent_version   = if ($a.agent_version) { "$($a.agent_version)" } else { $null }
            last_heartbeat  = $hb
            nats_user       = if ($null -ne $a.nats_user) { "$($a.nats_user)" } else { $null }
            nats_user_since = $since
            alive           = ($null -ne $hb -and ($now - $hb).TotalSeconds -le $AliveSeconds)
        }
    }
    return [pscustomobject]@{ Now = $now; Hosts = $map }
}

function Get-Label($h) {
    if ($h.hostname -and $h.hostname -ne $h.pc_id) { return "$($h.hostname) ($($h.pc_id))" }
    return $h.pc_id
}

function ConvertTo-Version([string]$Text) {
    if ($Text -notmatch '^\s*v?(\d+)\.(\d+)(?:\.(\d+))?') { return $null }
    $patch = if ($Matches[3]) { $Matches[3] } else { '0' }
    return [version]"$($Matches[1]).$($Matches[2]).$patch"
}

function Invoke-Readiness {
    if (-not $CheckName) { Fail '-CheckName is required: the name of the check job that reports whether the user pair is present. It is not guessed.' }
    $min = ConvertTo-Version $MinAgentVersion
    if (-not $min) { Fail '-MinAgentVersion is not a version number.' }
    $since = $null
    if ($CheckedSince) {
        $since = ConvertTo-Utc $CheckedSince
        if (-not $since) { Fail '-CheckedSince is not a date.' }
    }
    $agents = Get-Agents
    $q = '/api/checks?check=' + [uri]::EscapeDataString($CheckName) + '&include_ok=true&include_stale=true'
    $c = Invoke-Backend $q
    $checks = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([System.StringComparer]::Ordinal)
    foreach ($row in @($c.rows)) {
        if ($row -and $row.pc_id -and "$($row.check_name)" -ceq $CheckName) { $checks["$($row.pc_id)"] = $row }
    }
    $now = $agents.Now
    $recent = [TimeSpan]::FromHours($RecentWithinHours)
    $notReady = @()
    foreach ($h in ($agents.Hosts.Values | Sort-Object { $_.pc_id })) {
        $why = @()
        $v = if ($h.agent_version) { ConvertTo-Version $h.agent_version } else { $null }
        if (-not $h.agent_version -or -not $v) { $why += 'agent-version-unknown' }
        elseif ($v -lt $min) { $why += "agent-version-too-old($(Hide-Secrets $h.agent_version) < $MinAgentVersion)" }
        if (-not $h.alive -and ($null -eq $h.last_heartbeat -or ($now - $h.last_heartbeat) -gt $recent)) { $why += 'no-recent-heartbeat' }
        $row = $null
        if (-not $checks.TryGetValue($h.pc_id, [ref]$row)) { $why += 'no-check-result' }
        else {
            $st = "$($row.status)"
            if ($st -notmatch '^[a-z]{1,16}$') { $st = 'other' }
            if ($row.stale -eq $true) { $why += 'check-stale' }
            elseif ($st -ne 'ok') { $why += "check-not-ok($st)" }
            elseif ($since -and (ConvertTo-Utc $row.recorded_at) -lt $since) { $why += 'check-older-than-CheckedSince' }
        }
        if ($why.Count) { $notReady += [pscustomobject]@{ Host = Get-Label $h; Why = ($why -join ', ') } }
    }
    $total = $agents.Hosts.Count
    Say "Readiness for the switch (minimum agent $MinAgentVersion, check '$CheckName'): $total registered agents."
    if ($notReady.Count) {
        Say "NOT READY: $($notReady.Count) of $total"
        foreach ($n in $notReady) { Say "  $($n.Host): $($n.Why)" }
    }
    Say ''
    Say 'What this cannot know:'
    Say '  - An ok check is the agent''s own report that a user and password are present; it does not prove the values are right or that the broker will accept them.'
    Say '  - If the check name is wrong or the check is not fleet-wide, every host shows no-check-result.'
    Say '  - Hosts that never registered with the backend are not listed. The backend and break-glass credentials are covered by the manual checklist.'
    if ($notReady.Count) { Say 'Result: NOT READY. Do not switch.'; exit 1 }
    Say 'Result: every registered agent is ready.'
    exit 0
}

function Invoke-Snapshot {
    if (-not $SnapshotPath) { Fail '-SnapshotPath is required.' }
    if ((Test-Path -LiteralPath $SnapshotPath) -and -not $Force) { Fail "$SnapshotPath already exists; pass -Force to overwrite it." }
    $agents = Get-Agents
    $alive = @($agents.Hosts.Values | Where-Object { $_.alive } | Sort-Object { $_.pc_id })
    if (-not $alive.Count) { Fail 'no agent is alive right now; a snapshot of nothing would make the comparison meaningless. Nothing was written.' }
    $doc = [ordered]@{
        format                = $SnapshotFormat
        taken_at              = $agents.Now.ToString('o')
        alive_threshold_secs  = $AliveSeconds
        hosts                 = @($alive | ForEach-Object {
                [ordered]@{
                    pc_id           = $_.pc_id
                    hostname        = $_.hostname
                    agent_version   = $_.agent_version
                    last_heartbeat  = if ($_.last_heartbeat) { $_.last_heartbeat.ToString('o') } else { $null }
                    nats_user       = $_.nats_user
                    nats_user_since = if ($_.nats_user_since) { $_.nats_user_since.ToString('o') } else { $null }
                }
            })
    }
    [System.IO.File]::WriteAllText($SnapshotPath, ($doc | ConvertTo-Json -Depth 5))
    Say "Snapshot written: $($alive.Count) hosts alive of $($agents.Hosts.Count) registered, taken at $($agents.Now.ToString('o'))."
    Say 'Hosts not alive now are not in the snapshot, so they are never counted as lock-outs later.'
    exit 0
}

function Read-Snapshot {
    if (-not $SnapshotPath -or -not (Test-Path -LiteralPath $SnapshotPath)) { Fail 'the snapshot file was not found.' }
    try { $s = [System.IO.File]::ReadAllText($SnapshotPath) | ConvertFrom-Json } catch { Fail 'the snapshot is not valid JSON.' }
    if ($null -eq $s -or $s.format -ne $SnapshotFormat -or $null -eq $s.hosts -or -not (ConvertTo-Utc $s.taken_at)) {
        Fail 'the snapshot is not in the expected format (written by -Mode Snapshot of this script).'
    }
    $map = New-Object 'System.Collections.Generic.Dictionary[string,object]' ([System.StringComparer]::Ordinal)
    foreach ($h in @($s.hosts)) {
        if (-not $h.pc_id) { Fail 'the snapshot has a host without pc_id.' }
        $map["$($h.pc_id)"] = $h
    }
    if (-not $map.Count) { Fail 'the snapshot lists no hosts.' }
    return [pscustomobject]@{ TakenAt = (ConvertTo-Utc $s.taken_at); Hosts = $map }
}

function Invoke-Compare {
    if ($MaxDisappeared -lt 0) { Fail '-MaxDisappeared is required (0 or more): the number of vanished hosts you accept.' }
    if (-not $SwitchedAt) { Fail '-SwitchedAt is required: the UTC time the broker was reloaded.' }
    $switched = ConvertTo-Utc $SwitchedAt
    if (-not $switched) { Fail '-SwitchedAt is not a date.' }
    $snap = Read-Snapshot
    if ($WaitSeconds -gt 0) {
        Say "Waiting $WaitSeconds s before reading the backend."
        Start-Sleep -Seconds $WaitSeconds
    }
    $agents = Get-Agents
    $now = $agents.Now
    $needed = $AliveSeconds + $HeartbeatSeconds
    $elapsed = [int]($now - $switched).TotalSeconds
    if ($elapsed -lt $needed) {
        Say "Too early to tell: $elapsed s since the switch, and a locked-out host still looks alive for up to $AliveSeconds s after its last heartbeat. Wait until at least $needed s have passed (about two heartbeat intervals) and run again."
        exit 3
    }
    $disappeared = @(); $reconnected = @(); $noRecord = 0
    foreach ($id in $snap.Hosts.Keys) {
        $was = $snap.Hosts[$id]
        $cur = $null
        if ($agents.Hosts.TryGetValue($id, [ref]$cur) -and $cur.alive) {
            if ($cur.last_heartbeat -gt $switched) { $reconnected += $cur }
        } else {
            if ($null -eq $cur) { $noRecord++ }
            $label = if ($was.hostname -and $was.hostname -ne $id) { "$($was.hostname) ($id)" } else { $id }
            $last = if ($cur -and $cur.last_heartbeat) { $cur.last_heartbeat.ToString('o') } else { 'never/unknown' }
            $disappeared += [pscustomobject]@{ Label = $label; Version = "$($was.agent_version)"; LastHeartbeat = $last; WasUser = "$($was.nats_user)" }
        }
    }
    $new = @($agents.Hosts.Values | Where-Object { $_.alive -and -not $snap.Hosts.ContainsKey($_.pc_id) })
    $aliveNow = @($agents.Hosts.Values | Where-Object { $_.alive })

    Say "Compare: snapshot of $($snap.Hosts.Count) hosts taken $($snap.TakenAt.ToString('o')); switch at $($switched.ToString('o')); read $($now.ToString('o')) ($elapsed s after the switch)."
    Say ''
    Say "Disappeared (alive in the snapshot, not alive now) - lock-out candidates: $($disappeared.Count)"
    foreach ($d in ($disappeared | Sort-Object Label)) { Say "  $($d.Label)  agent=$($d.Version)  last heartbeat=$($d.LastHeartbeat)  user before=$(if ($d.WasUser) { $d.WasUser } else { '(absent)' })" }
    if ($noRecord) { Say "  ($noRecord of these are no longer registered at all.)" }
    Say ''
    Say "Heartbeated after the switch and alive now: $($reconnected.Count) of $($snap.Hosts.Count)"
    foreach ($r in ($reconnected | Sort-Object { $_.pc_id } | Select-Object -First 50)) { Say "  $(Get-Label $r)" }
    if ($reconnected.Count -gt 50) { Say "  ... and $($reconnected.Count - 50) more" }
    Say ''
    Say "New hosts (alive now, not in the snapshot): $($new.Count)"
    foreach ($n in ($new | Sort-Object { $_.pc_id } | Select-Object -First 50)) { Say "  $(Get-Label $n)" }
    Say ''
    Say "Live hosts by authenticated NATS user (expected: '$ExpectedUser'):"
    $buckets = @{}
    $old = 0
    foreach ($h in $aliveNow) {
        $u = $h.nats_user
        $k = if ($null -eq $u) { '(absent: never correlated)' }
        elseif ($u -ceq $ExpectedUser) { "$ExpectedUser (expected)" }
        elseif ($u -ceq 'shared-token') { 'shared-token (UNEXPECTED: still the old token)' }
        elseif ($u -ceq 'no-auth') { 'no-auth (UNEXPECTED: broker authenticated nobody)' }
        elseif ($u -ceq 'unknown') { 'unknown (UNEXPECTED: credential not nameable)' }
        else { $t = if ($u.Length -gt 40) { $u.Substring(0, 40) } else { $u }; "other user '$t' (UNEXPECTED for an agent host)" }
        $buckets[$k] = 1 + [int]$buckets[$k]
        if ($null -ne $u -and ($null -eq $h.nats_user_since -or $h.nats_user_since -lt $switched)) { $old++ }
    }
    foreach ($k in ($buckets.Keys | Sort-Object)) { Say "  $($buckets[$k])  $k" }
    if ($old) { Say "  note: $old live hosts carry a value last changed before the switch; the backend keeps the last value it saw, so for those it may be pre-switch." }
    Say ''
    $backendHint = ($aliveNow.Count -eq 0) -or ($disappeared.Count -eq $snap.Hosts.Count)
    if ($backendHint) { Say 'WARNING: no host from the snapshot is alive. The backend itself is probably locked out of the broker (its credential is not in the new config): revert and check the backend host first.' }
    Say 'What this cannot know:'
    Say '  - A locked-out host and a powered-off host look the same from here; use the procedure to tell them apart.'
    Say '  - A missing NATS user means never correlated, not the old credential. A recorded user is sticky and can outlive a stopped heartbeat.'
    Say '  - A heartbeat after the switch is indirect evidence of reconnection, not proof of which credential was used.'
    Say '  - Hosts that were offline at snapshot time, the backend and the break-glass CLI are not covered.'
    if ($disappeared.Count -gt $MaxDisappeared) { Say "Result: $($disappeared.Count) hosts disappeared, more than the accepted $MaxDisappeared."; exit 1 }
    Say "Result: $($disappeared.Count) disappeared (accepted: $MaxDisappeared)."
    exit 0
}

Test-Url
$script:CanReadHeaders = [bool](Get-Command Invoke-RestMethod).Parameters.ContainsKey('ResponseHeadersVariable')
switch ($Mode) {
    'Readiness' { Invoke-Readiness }
    'Snapshot' { Invoke-Snapshot }
    'Compare' { Invoke-Compare }
}

# Shared helpers for jetstream-delete.ps1 / jetstream-reset.ps1 (dot-sourced).
# Drives the standard `nats` CLI so recovery works while kanade-backend cannot
# start (e.g. a drifted stream config makes its bootstrap fail).

$script:ResourceFile = Join-Path $PSScriptRoot 'jetstream-resources.json'

function Get-KanadeResources {
    if (-not (Test-Path -LiteralPath $script:ResourceFile)) {
        throw "resource list not found: $script:ResourceFile"
    }
    $j = [System.IO.File]::ReadAllText($script:ResourceFile) | ConvertFrom-Json
    # @() keeps single-element lists as arrays on Windows PowerShell 5.1.
    [pscustomobject]@{
        Streams = @($j.streams)
        Kv      = @($j.kv)
        Object  = @($j.object)
        LazyKv  = @($j.lazy_kv)
    }
}

function Test-ResourceName([string]$Name) {
    # The broker's charset. A leading '-' is refused as well so the name can
    # never be read by `nats` as an option.
    return ($Name -cmatch '^[A-Za-z0-9_][A-Za-z0-9_-]*$')
}

function Assert-NatsAvailable {
    if (-not (Get-Command nats -ErrorAction SilentlyContinue)) {
        [Console]::Error.WriteLine("error: 'nats' was not found on PATH. Install the NATS CLI (https://github.com/nats-io/natscli) and retry.")
        exit 2
    }
}

function Resolve-Connection($Server, $Creds, $User, $Password) {
    # Explicit arguments win over the environment. Nothing is passed when
    # neither is set, so `nats` falls back to its own context / environment.
    if (-not $Server)   { $Server   = $env:NATS_URL }
    if (-not $Creds)    { $Creds    = $env:NATS_CREDS }
    if (-not $User)     { $User     = $env:NATS_USER }
    if (-not $Password) { $Password = $env:NATS_PASSWORD }
    if ($Creds -and ($User -or $Password)) {
        [Console]::Error.WriteLine('error: use either a creds file or user/password, not both.')
        exit 2
    }
    if ($Creds -and -not (Test-Path -LiteralPath $Creds)) {
        [Console]::Error.WriteLine('error: creds file not found.')
        exit 2
    }
    [pscustomobject]@{ Server = $Server; Creds = $Creds; User = $User; Password = $Password }
}

function Get-DisplayServer($Conn) {
    if (-not $Conn.Server) { return '(from nats context / environment)' }
    # Drop any user:pass@ so a credential in the URL is never printed.
    return ($Conn.Server -replace '^([a-zA-Z][a-zA-Z0-9+.-]*://)[^/@]*@', '$1')
}

function Hide-Secrets([string]$Text, $Conn) {
    foreach ($s in @($Conn.Password, $Conn.User)) {
        if ($s -and $s.Length -ge 1) { $Text = $Text.Replace($s, '***') }
    }
    if ($Conn.Server) { $Text = $Text.Replace($Conn.Server, (Get-DisplayServer $Conn)) }
    return $Text
}

function Invoke-Nats($Conn, [string[]]$NatsArgs) {
    $all = @()
    if ($Conn.Server) { $all += @('--server', $Conn.Server) }
    if ($Conn.Creds)  { $all += @('--creds', $Conn.Creds) }
    if ($Conn.User)   { $all += @('--user', $Conn.User) }
    $all += $NatsArgs
    # The password goes through the environment, not argv, so it is not
    # visible in the process list.
    $old = $env:NATS_PASSWORD
    $oldEap = $ErrorActionPreference
    try {
        if ($Conn.Password) { $env:NATS_PASSWORD = $Conn.Password }
        # 5.1 turns native stderr into ErrorRecords; do not let that throw.
        $ErrorActionPreference = 'Continue'
        $out = & nats @all 2>&1 | ForEach-Object { "$_" }
        $code = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $oldEap
        $env:NATS_PASSWORD = $old
    }
    [pscustomobject]@{ Code = $code; Text = Hide-Secrets (($out -join "`n").Trim()) $Conn }
}

function Remove-JetStreamResource($Conn, [string]$Kind, [string]$Name) {
    $cmd = switch ($Kind) {
        'stream' { @('stream', 'rm', $Name, '-f') }
        'kv'     { @('kv', 'del', $Name, '-f') }
        'object' { @('object', 'rm', $Name, '-f') }
    }
    $r = Invoke-Nats $Conn $cmd
    if ($r.Code -eq 0) { return [pscustomobject]@{ Status = 'deleted'; Detail = '' } }
    # Auth / connection problems are real failures even if the text also
    # happens to mention something missing (e.g. a missing creds file).
    if ($r.Text -match '(?i)authorization|authentication|permissions violation|no servers available|connection refused|timeout|i/o timeout|nkey|credentials|creds') {
        return [pscustomobject]@{ Status = 'failed'; Detail = $r.Text }
    }
    if ($r.Text -match '(?i)not found') {
        return [pscustomobject]@{ Status = 'not-found'; Detail = '' }
    }
    return [pscustomobject]@{ Status = 'failed'; Detail = $r.Text }
}

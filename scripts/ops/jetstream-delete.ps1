<#
.SYNOPSIS
  Delete ONE JetStream resource (stream, KV bucket or object store) with the
  standard `nats` CLI. Works when kanade-backend cannot start.

.DESCRIPTION
  Use after a stream/bucket has drifted from its expected config or is
  corrupted: delete it, then start kanade-backend, which recreates it.
  The deletion destroys the data in the resource. Stop kanade-backend first
  (its projectors hold durable consumers), start it again afterwards.

  Needs `nats` on PATH and an administrative credential. Nothing is deleted
  until you type the resource name back, unless -Yes is given (required when
  there is no interactive console).

  Connection: -Server / -Creds / -User / -Password, or the environment
  variables NATS_URL / NATS_CREDS / NATS_USER / NATS_PASSWORD. Arguments win.
  Credentials are never printed, and the password is not put on the command line.

.EXAMPLE
  ./scripts/ops/jetstream-delete.ps1 -Kind stream -Name RESULTS -Server nats://127.0.0.1:4222 -Creds ./admin.creds

.EXAMPLE
  ./scripts/ops/jetstream-delete.ps1 -Kind kv -Name views -Yes
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)][ValidateSet('stream', 'kv', 'object')][string]$Kind,
    [Parameter(Mandatory)][string]$Name,
    [string]$Server,
    [string]$Creds,
    [string]$User,
    [string]$Password,
    [Alias('Force')][switch]$Yes
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'jetstream-common.ps1')

if (-not (Test-ResourceName $Name)) {
    [Console]::Error.WriteLine("error: '$Name' is not a valid name (letters, digits, '_' and '-' only, not starting with '-').")
    exit 2
}
Assert-NatsAvailable
$conn = Resolve-Connection $Server $Creds $User $Password

$res = Get-KanadeResources
$known = switch ($Kind) {
    'stream' { $res.Streams }
    'kv'     { @($res.Kv) + @($res.LazyKv) }
    'object' { $res.Object }
}
if ($known -cnotcontains $Name) {
    Write-Warning "'$Name' is not a $Kind that kanade creates. On a shared broker it may belong to another system."
}

Write-Host "About to delete $Kind '$Name' on $(Get-DisplayServer $conn)."
Write-Host 'All data in it is lost. Stop kanade-backend first; start it again afterwards.'
if (-not $Yes) {
    $answer = Read-Host "Type the name ($Name) to confirm"
    if ($answer -cne $Name) {
        Write-Host 'Confirmation did not match. Nothing was deleted.'
        exit 1
    }
}

$r = Remove-JetStreamResource $conn $Kind $Name
switch ($r.Status) {
    'deleted'   { Write-Host "Deleted $Kind '$Name'."; exit 0 }
    'not-found' { Write-Host "$Kind '$Name' does not exist; nothing to delete."; exit 0 }
    default     { [Console]::Error.WriteLine("error: failed to delete $Kind '$Name': $($r.Detail)"); exit 1 }
}

<#
.SYNOPSIS
  Delete EVERY JetStream stream, KV bucket and object store kanade uses, with
  the standard `nats` CLI. Works when kanade-backend cannot start.

.DESCRIPTION
  Dry run by default: prints exactly what would be deleted and touches
  nothing. Add -Yes to perform it. Only the names in jetstream-resources.json
  are touched (including buckets kanade creates lazily); anything else on the
  broker is left alone. Resources that do not exist are skipped, and the run
  continues after any failure; the exit code is 1 if any deletion failed for a
  reason other than not-found.

  All data in these resources is lost. Stop kanade-backend BEFORE running
  (its projectors hold durable consumers) and start it AFTERWARDS: it
  recreates what bootstrap guarantees on start; lazily created buckets come
  back on first use.

  Connection: -Server / -Creds / -User / -Password, or the environment
  variables NATS_URL / NATS_CREDS / NATS_USER / NATS_PASSWORD. Arguments win.
  Credentials are never printed, and the password is not put on the command line.

.EXAMPLE
  ./scripts/ops/jetstream-reset.ps1 -Server nats://127.0.0.1:4222 -Creds ./admin.creds

.EXAMPLE
  ./scripts/ops/jetstream-reset.ps1 -Server nats://127.0.0.1:4222 -Creds ./admin.creds -Yes
#>
[CmdletBinding()]
param(
    [string]$Server,
    [string]$Creds,
    [string]$User,
    [string]$Password,
    [switch]$Yes
)
$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'jetstream-common.ps1')

$res = Get-KanadeResources
$items = @()
foreach ($n in (@($res.Kv) + @($res.LazyKv))) { $items += [pscustomobject]@{ Kind = 'kv'; Name = $n } }
foreach ($n in $res.Object)  { $items += [pscustomobject]@{ Kind = 'object'; Name = $n } }
foreach ($n in $res.Streams) { $items += [pscustomobject]@{ Kind = 'stream'; Name = $n } }

if (-not $Yes) {
    Write-Host 'DRY RUN - nothing is deleted. Would delete:'
    foreach ($i in $items) { Write-Host ("  {0,-7} {1}" -f $i.Kind, $i.Name) }
    Write-Host ''
    Write-Host "Total: $($items.Count) resources. Re-run with -Yes to delete them."
    Write-Host 'Stop kanade-backend before a reset and start it afterwards.'
    exit 0
}

Assert-NatsAvailable
$conn = Resolve-Connection $Server $Creds $User $Password
Write-Host "Deleting $($items.Count) resources on $(Get-DisplayServer $conn)"
Write-Host 'Make sure kanade-backend is stopped (its projectors hold durable consumers).'

$counts = @{ 'deleted' = 0; 'not-found' = 0; 'failed' = 0 }
foreach ($i in $items) {
    $r = Remove-JetStreamResource $conn $i.Kind $i.Name
    $counts[$r.Status]++
    $line = "  {0,-9} {1,-7} {2}" -f $r.Status, $i.Kind, $i.Name
    if ($r.Status -eq 'failed') { Write-Host $line; Write-Host "            $($r.Detail)" }
    else { Write-Host $line }
}

Write-Host ''
Write-Host "Done: $($counts['deleted']) deleted, $($counts['not-found']) not found, $($counts['failed']) failed."
Write-Host 'The resources are recreated the next time kanade-backend starts (lazily created buckets on first use). The deleted data is not restored.'
Write-Host 'Start kanade-backend now if you stopped it.'
if ($counts['failed'] -gt 0) { exit 1 }
exit 0

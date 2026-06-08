# Writing scripts for the agent

PowerShell scripts the agent will run are *almost* normal `.ps1`
files. This page collects the gotchas that aren't obvious from
the script source alone.

## The agent stages scripts on disk and runs them via `-File`

As of [PR #230](https://github.com/yukimemi/kanade/pull/230)
(agent version 0.42.0+), the agent:

1. Writes your script body to a temp `.ps1` under
   `%ProgramData%\Kanade\agent-scripts\<UUID>\kanade-<UUID>.ps1`
   (Windows) or `$TMPDIR/kanade-agent-<UUID>/kanade-<UUID>.ps1`
   (non-Windows dev only).
2. Writes a *launcher* `.ps1` next to it that sets UTF-8 console
   encoding then `& '<your-script>' @args`.
3. Spawns `powershell -NoProfile -NonInteractive -ExecutionPolicy
   Bypass -File <launcher>`.

This means your script:

- **Can** have `[CmdletBinding()]` and `param(...)` at the top.
  The call-operator boundary in the launcher gives your script
  its own scope where those headers are valid.
- **Should not** rely on `$PSCommandPath` matching the operator's
  source path — it'll be the staged temp file.
- **Should not** write to `$PSScriptRoot` (see next section).

Pre-0.42.0 agents used `powershell -Command "<body>"`, which
parses the body as a command-line expression and rejects
`[CmdletBinding()]` as a syntax error. If you see
"Unexpected token '[CmdletBinding()]'" in stderr, the host's
agent is too old — upgrade it (see
[agent self-update](../operations/agent-mediated-updates/agent-self.md)).

## `$PSScriptRoot` is read-only for `run_as: user`

When `run_as: user` (or `system_gui`), the child process runs as
the logged-in user — not as the LocalSystem agent that wrote the
staged file. The staging directory inherits its ACL from
`%ProgramData%`, which grants users **Read & Execute but not
Modify**.

That means:

```powershell
# OK from any run_as
Get-ChildItem $PSScriptRoot              # list contents
Get-Content   $PSScriptRoot\anything     # read

# NG from run_as: user (access denied)
New-Item    -Path $PSScriptRoot\out.txt
Set-Content -Path $PSScriptRoot\log.log
```

Write to `$env:TEMP`, `$env:LOCALAPPDATA`, or an absolute path
under the user's profile instead. Even for `run_as: system` (where
SYSTEM can write to its own staged dir), the directory is cleaned
up when the script exits, so writing siblings is fragile either
way.

## Identity table

| `run_as:` (manifest) | Child identity | Reads `$PSScriptRoot` | Writes `$PSScriptRoot` | Has admin |
|---|---|---|---|---|
| `system` (default) | LocalSystem | ✓ | ✓ but pointless (GC'd) | yes |
| `user` | Logged-in user | ✓ | ✗ access denied | no |
| `system_gui` | LocalSystem, in user session | ✓ | ✓ but pointless (GC'd) | yes |

`system_gui` is the "PsExec `-i -s`" pattern — admin privilege but
visible in the user's desktop session (useful for GUI tools that
need both elevation and an interactive window).

## stdout vs Write-Host

The backend's result projector reads **stdout** as the script's
output. If your manifest has an `inventory:` block, stdout is
parsed as a single JSON blob.

**Do NOT use `Write-Host` for progress chatter in an inventory
script.** Contrary to a common assumption, `Write-Host` does NOT
stay on a separate host stream once the agent runs your script with
stdout redirected — its output bleeds INTO the captured stdout. That
extra text breaks the projector's single-JSON-blob parse
(`serde_json::from_str` over the whole stdout, in
`crates/kanade-backend/src/projector/results.rs::upsert_inventory`),
and your inventory fact is silently dropped (the backend logs
`stdout was not JSON`).

Send progress chatter to **stderr** via `[Console]::Error.WriteLine(...)`.
stderr is captured into the result's separate `stderr` field, which the
projector ignores, so stdout stays a single clean JSON line.

```powershell
[Console]::Error.WriteLine("Downloading...")  # → stderr (logged, ignored by projector)
Write-Output ($obj | ConvertTo-Json)          # → stdout (the ONE JSON line, parsed)
```

Keep stdout to exactly one `Write-Output` — anything else on stdout
(a stray `Write-Host`, `Write-Output`, or a cmdlet's pipeline output)
that isn't the expected JSON will fail the inventory parse.
See `configs/jobs/installers/scripts/install-kanade-client.ps1` for a
worked example.

## UTF-8 by default

The launcher sets `[Console]::OutputEncoding = UTF-8` and
`$OutputEncoding = UTF-8` before invoking your script, so any
stdout / stderr you produce is UTF-8 regardless of the host's
system codepage. Operator-shipped scripts with Japanese / DE /
KR / CN strings show up correctly in the SPA Activity view
without per-host workarounds.

If you explicitly need OEM / CP932 / Shift-JIS output (e.g.
calling a legacy CLI that ignores `$OutputEncoding`), set it
yourself in the script after the launcher prelude has run —
your assignment takes precedence.

## Native command exit codes

If your script ends with a successful native command run, the
overall exit is 0 — that's PowerShell's default. If a native
command fails (`$LASTEXITCODE -ne 0`) and you DON'T handle it,
PowerShell still exits 0 — `$ErrorActionPreference = 'Stop'`
does **not** save you here.

> Windows PowerShell 5.1 (the default on Windows endpoints — and
> what the agent's `powershell.exe` resolves to) treats native
> command non-zero exits as non-terminating regardless of
> `$ErrorActionPreference`. PowerShell 7.3+ adds
> `$PSNativeCommandUseErrorActionPreference = $true` which makes
> them terminating, but that's not available in the deployment
> target. **Always check `$LASTEXITCODE` explicitly.**

The agent does NOT auto-propagate `$LASTEXITCODE` either — that
would exit nonzero even when your script handled the native error
gracefully. If you want the script's exit code to reflect a
specific native call, propagate it yourself:

```powershell
& git pull
if ($LASTEXITCODE -ne 0) { throw "git pull failed with exit code $LASTEXITCODE" }
# or, if you want the exact native code propagated:
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
```

`throw` is usually preferable because it produces a clean
PowerShell error record (which the `trap { … break }` cleanup
pattern can intercept) and exits non-zero. `exit $LASTEXITCODE`
is right when the caller cares about the exact code.

## Timeouts

The manifest's `timeout:` is enforced by the agent. When it
fires, the agent calls `child.kill()` on the PowerShell process
— no graceful shutdown, no trap, no finally. Plan for it:

- Budget the script to finish in `timeout * 0.6` and leave
  headroom.
- Use `trap { ... ; break }` for cleanup of resources that need
  explicit release (staging dirs, lock files) — `trap` fires on
  terminating errors, NOT on the agent's kill. Don't rely on it
  for the timeout case.
- If you need cooperative cancellation, poll a sentinel file or
  a registry value and exit early. The agent has no way to send
  the script a graceful "wrap up" signal.

## Killing a running job

`kanade kill <exec_id>` publishes a kill message the agent
subscribes to. On receipt, the agent calls `child.kill()` —
same hard-kill as the timeout path. Operators get an immediate
result row marked `Killed` with whatever stdout / stderr the
agent managed to capture before termination.

# macOS session temporary-directory diagnostic

Status: prepared for owner review only. Registration updates, execution on a
managed host, production jobs and agent deployment require explicit approval
through the separate owning task. No managed-host measurement is claimed here.
The configuration repository's fixed `run.lock` / `FileShare.None` change is
independent and is not modified by this change.

The reported local reproduction used agent 0.99.2, PowerShell 7.6.9 and .NET
10.0.14. A credentials-free `env -i` reproduction failed during named Mutex
initialization when `TMPDIR` was absent and succeeded when a valid value was set
before PowerShell started. The source inspected then was
`01eff06f3f00690551e69367536f6f0c67e05a7d`; correspondence to the failing
installed executable is **unverified**. This is evidence for a missing startup
environment value, not proof of the original incident's root cause.

## Procedure after approval

1. On the intended macOS host, identify the installed LaunchDaemon executable
   from its `ProgramArguments[0]`, without dumping its environment or full
   service configuration. Record only its `--version` output and SHA-256
   (`shasum -a 256`), plus the candidate commit/build identity. Keep paths,
   account identifiers and host identifiers in restricted local evidence.
   Confirm the existing service is running that executable; hash a copied
   artifact separately if an update has replaced the file since service start.
2. Review `configs/jobs/diagnostics/probe-macos-tempdir.yaml`. It has no
   schedule, targets no production state and logs no credentials or environment
   dump. The two temporary paths can identify an account; restrict the raw
   result and redact them before publishing evidence.
3. With owner approval, register and invoke this diagnostic through the existing
   supported job route on the already installed LaunchDaemon. Use the exact
   registered target identifier. Do not run the script directly in an operator
   shell: that bypasses the bootstrap, credential switch and staged UTF-8
   launcher being tested. The `pwsh` job host uses
   `-NoProfile -NonInteractive -File` automatically.
4. Record the requested `run_as`, effective UID, PowerShell/.NET versions,
   `TMPDIR`, `GetTempPath`, Mutex creation/acquisition/release booleans and exit
   status. A host-initialization failure may occur before JSON is produced;
   retain that fact and the narrowly relevant error, never a full environment.
5. Repeat with a separately reviewed copy setting **both** `execute.run_as` and
   the script's `run_as` label to `system_gui`. It must retain the agent's
   effective UID, whereas `user` must use the current console user's UID.
   Their temporary-directory owners must match these respective identities.
6. Compare the old installed build and candidate only after a separately approved
   candidate deployment. Keep the same supported route and PowerShell version.
   Check successful Mutex release, correct directory ownership, and absence of
   inherited daemon secrets. Also smoke-test screen capture and screen permission
   onboarding because they share the session command builder. Do not infer
   incident binary/source equivalence from a successful candidate probe.

## Expected candidate behavior

Before launching the host, the agent runs `getconf DARWIN_USER_TEMP_DIR` using
its existing `launchctl asuser` / numeric-UID `sudo -n` / `env -i` route. It
normalizes the standard system alias, rejects other links, checks every path
component and requires a private target-owned leaf. A second bounded command
creates an exclusive random file and removes it under that same identity.
Neither command inherits the daemon environment. Failure stops the launch;
there is no shared-directory fallback or privileged directory repair. Each
command has a three-second timeout and a 4096-byte stdout limit, discards
stderr, and kills its process group and reaps its child on timeout. Console UID
is checked again before the host command is returned. Script staging remains
unchanged.

Focused local verification: `cargo test -p kanade-agent --bin kanade-agent
process_as_user_macos::tests --locked`. These unit tests validate parsing,
permissions, argv/environment construction and probe lifecycle with ordinary
local child processes; they do not replace the approved LaunchDaemon probe.

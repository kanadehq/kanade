# Installed macOS agent: Screen Recording onboarding

The root LaunchDaemon polls `SCDynamicStoreCopyConsoleUser` immediately after
logging starts, before awaiting NATS, then every five seconds. No remote viewer
or job is needed. The login window and setup account are excluded. A console
ownership timestamp supplements the UID when available; unavailable generation
information coalesces sessions conservatively. A brief login/switch between polls
may be missed until that user is foreground again.

On macOS 14 and later, a short-lived `--screen-permission-check <uid>` process
runs the **same installed executable**, via the same
`launchctl asuser → sudo -n -u → env -i` builder used by real screen capture.
Both parent and child recheck the console user. Preflight uses
[`CGPreflightScreenCaptureAccess`](https://developer.apple.com/documentation/coregraphics/cgpreflightscreencaptureaccess()).
If already allowed, the helper exits without requesting. Otherwise it waits for
the daemon to durably save intent before calling
[`CGRequestScreenCaptureAccess`](https://developer.apple.com/documentation/coregraphics/cgrequestscreencaptureaccess()).
It never enumerates displays, acquires pixels, connects to NATS or starts the
agent runtime. This launch route is necessary for attribution, but **does not
prove which executable TCC attributes the request to**. That requires the real
installed acceptance test below.

History is root-owned under the agent data directory's `screen-permission/`:
one record per UID, executable path, and signed team/identifier where available.
Unsigned/ad-hoc binaries use the path alone. Versions and cdhash are excluded so
routine updates retain history. A signature identity or installation path change
creates a separate history; preserve the release signing identity and path.
A nonblocking per-record lock excludes overlapping daemon instances. Before each
launch a finite retry budget is persisted: at most three launch attempts, with 60-second
then 600-second delays. Pre-request launch/protocol/timeouts may retry; an intent
already saved, including a crash before delivery, never retries the request.
Corrupt or unwritable history suppresses onboarding for that observed session.
Do not delete history as a routine troubleshooting step: it cannot reset OS
consent and could produce unwanted attempts.

The daemon supervises the process separately from normal work, killing its
process group on logout, switching, shutdown or a 30-second timeout. The helper
also arms its own 35-second process deadline in case the daemon dies abruptly. A pending
system dialog may outlive the requesting process; its result is unknown. A false
preflight or request result cannot distinguish denial from unset/pending consent.
An earlier refusal does **not** mean macOS will show another dialog. Enable the
installed agent in **System Settings → Privacy & Security → Screen Recording**
(or **Screen & System Audio Recording**, depending on OS version). Onboarding
and capture errors log this recovery route. Existing remote authorization remains
required; this mechanism grants no consent itself.

After enabling access, disconnect and reconnect the viewer to create a fresh
capture child. If that still reports missing permission, restart the installed
agent service and reconnect; honor any macOS restart instruction. Whether a
service restart is required must be recorded for each tested OS/signing setup.
A permission granted to Terminal, or an entry under Files and Folders, does not
validate the installed capture process.

## Manual acceptance on a real installed, signed agent

**Status: unverified on an installed signed agent.** Native compilation and unit
tests do not verify the OS dialog, Settings registration, TCC responsibility,
grant persistence, or later capture. No live deployment is part of this change.
Use a disposable test account with fresh consent; do not reset TCC or modify its
database. Record OS version, agent version and signature team/identifier privately.

1. With no viewer and no jobs, start the installed root LaunchDaemon while the
   test user is already logged in. Repeat with daemon startup before login using
   another fresh test account. Confirm startup, heartbeat and jobs continue even
   with no user, denied access or a pending dialog. When testing broker outages,
   verify local onboarding still starts before NATS can connect.
2. Record the OS permission dialog verbatim and which application it names.
   Follow its Settings action where offered. Confirm the Screen Recording entry
   appears through this request **without manually selecting a binary with +**.
   Record its exact display name and executable path using the Settings UI and
   available OS diagnostics. If the OS offers no registration/action, record
   that limitation instead of claiming success.
3. Observe the helper process tree, effective UID and GUI bootstrap context and
   compare them with the normal capture child. Use Console or read-only unified
   logs (for example, `log stream --info --predicate 'subsystem ==
   "com.apple.TCC"'`) where accessible to inspect the requesting and responsible
   process identities. Compare the installed executable's `codesign -dv --verbose=4`
   and designated requirement (`codesign -dr -`) with the attributed process.
   Do not accept a root/system identity, Terminal consent, Files and Folders entry,
   or a matching display name alone as proof. If diagnostics redact identity,
   record attribution as unverified pending further evidence.
4. Refuse or leave the request pending. Confirm it is bounded, no pixels or remote
   traffic are produced, and repeating polls, service restarts and user switching
   do not re-request for that user/identity. The log must refer to Settings rather
   than promise another prompt. Enable the exact installed executable there.
5. Connect through the normal authorized viewer path and verify a real desktop
   appears. Disconnect/reconnect for a new capture child; record whether service
   restart was additionally necessary. Check Settings and successful viewing after
   daemon restart, logout/login, reboot and an ordinary same-identity update.
6. Switch between two signed-in users. Each fresh user should get its own attempt
   only while foreground, with its own consent. Switch or log out during helper
   startup/request and confirm the old helper is stopped without targeting the new
   user. Revisit a previously attempted user: no new request. With permission
   already granted, confirm the helper preflights and exits without a dialog.
7. Exercise an unavailable GUI bootstrap or launch failure in a controlled test
   installation. Verify finite persisted retries and continued normal startup.
   Capture-only viewing at LoginWindow, remote input and other capture expansion
   are outside this acceptance scope.

Keep machine/account identifiers and diagnostic paths out of public PRs. Record
actual results and unresolved OS-specific behavior; do not infer them from unit
tests or from the shared launch builder.

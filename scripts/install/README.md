# One-liner installation diagnostics

Each authenticated fetch of `/api/agents/installer.sh` or
`/api/agents/installer.ps1` creates an installation attempt UUID and a
one-hour, write-only reporting capability. The generated script reports
straight to HTTP, independently of NATS, agent registration and the issuing
user's session lifetime. The archive download still requires that user's
valid session. Each attempt permits one archive download; obtain a **fresh
one-liner** for every retry. Manually downloaded archives do not create
backend installation attempts.

Operators with the **Agent Install** page permission can select an attempt
in the page's **Installation logs** card. It refreshes every five seconds.
The authenticated APIs are:

- `GET /api/agents/install-attempts`: latest 100 attempts, issuer, platform,
  timestamps and status (never the reporting capability).
- `GET /api/agents/install-attempts/{id}`: ordered structured events with
  stage, outcome, exit code and backend receipt time.

Use a normal operator session's `Authorization: Bearer ...` header for both
APIs. Viewer accounts, including download-only accounts, cannot read logs;
Audit permission alone does not grant access.

`unreported` means the script has sent no event. `running` means progress
was received without a final result. After the one-hour reporting window,
either becomes `unknown`. Neither means failure: network loss, forced process
termination, or failure fetching the initial script can prevent reporting.
A failure fetching the initial script cannot have a script-generated event.
`succeeded` means the installer completed, **not** that backend registration
was observed. `failed` includes the last known stage and the original child
exit code; PowerShell exceptions (including UAC cancellation) use exit 1.
Placement and service-start stages are collected from a safe child-process
file; only complete lines are collected during polling and after exit.

Only enumerated stages/outcomes, bounded sequence numbers and integer exit
codes are accepted. No command output, exception text, transcript, config,
environment or source lines are uploaded. Invalid/extra fields are rejected
instead of storing and attempting to mask potentially secret content.
Reports are limited to 1 KiB per request and 128 sequence positions per
attempt; this also bounds stored event content. Identical retries are
idempotent; conflicting duplicates, backwards sequences and changes to
terminal outcomes are refused. Gaps are accepted so a lost progress request
does not block the final outcome. Reporting is best effort with a short
network timeout and cannot replace the installer's failure exit code.

The console prints the attempt ID at startup. On failure it also prints the location of
`progress.jsonl` in the private temporary extraction directory. If reporting
fails on a successful installation, that directory is retained too. On Unix,
failures before temporary-directory creation print the safe final event and
attempt ID to stderr. Windows has no local file before directory creation,
but prints the attempt ID. The Windows child also writes fixed safe outcome
labels to `install.log`; raw transcripts are deliberately disabled. The
retained archive/config contains provisioning credentials: share only the
safe event files, and delete the directory after investigation. Never share
the generated script or its embedded credentials.

Attempts/events are retained for 30 days. Expired data is removed on script
issuance and log listing; detail reads exclude events older than 30 days.
Data is local SQLite state, not re-projectable from NATS: a database wipe
also removes these diagnostics. HTTP ingress/access logs must not record
`X-Install-Key` or Authorization headers. The report capability cannot read
logs, download an archive, or report to another attempt.

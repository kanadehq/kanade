# kanade — macOS agent

Runs `kanade-agent` on a Mac as a **launchd daemon** (`com.kanade.agent`),
the counterpart of the Linux systemd unit and the Windows service. Only the
agent is supported on macOS; the backend + NATS stay on Linux/Windows.

**Apple Silicon (arm64) Macs only.** Intel (x86_64) Macs are not supported
— macOS 27 dropped Intel, and no `x86_64-apple-darwin` agent is built:
`kanade agent publish` rejects an x86_64 Mach-O, the installer endpoint
rejects `?os=macos&arch=x86_64`, and the one-liner refuses Intel Macs.

| File | Role |
| --- | --- |
| `setup-agent.sh` | Install/upgrade from a local bundle (run as root). Ships in the backend installer as `setup-agent-macos.sh` |
| `launchd/com.kanade.agent.plist` | The LaunchDaemon definition |

Both are copied verbatim into `crates/kanade-backend/assets/`
(`setup-agent-macos.sh`, `com.kanade.agent.plist`) — edit the originals here
and copy them across; `assets_match_the_workspace_originals` fails on drift.

## Install via the backend (recommended)

Publish a thin Apple Silicon agent binary first (`<version>-macos-aarch64`
key in the `agent_releases` Object Store — `kanade agent publish` detects
the Mach-O arch; universal/fat binaries are rejected, so build
`aarch64-apple-darwin` or `lipo -thin arm64` a universal one). Then either:

- **One-liner** — the SPA Agent Install page's `curl … | sudo bash`
  command. The generated `installer.sh` picks `os=macos` from `uname -s`
  and checks the hardware with `sysctl -n hw.optional.arm64` (not
  `uname -m`, which reports `x86_64` in a Rosetta shell on Apple Silicon);
  on an Intel Mac it exits with an error.
- **Tarball** — `GET /api/agents/installer?os=macos&arch=aarch64` (`arch`
  may be omitted on macOS; `x86_64` is rejected with a 400), then:

  ```bash
  mkdir kanade-agent-installer && cd kanade-agent-installer
  tar xzf ../kanade-agent-installer-<key>.tar.gz
  sudo ./install.sh
  ```

  `install.sh` exports the NATS token configured under Settings → server
  settings (`agent_install`) and hands over to `setup-agent-macos.sh`.

## Install by hand

Lay out a bundle directory the way the backend tarball does and run the
script from its root:

```
bin/kanade-agent                  # the thin Apple Silicon (arm64) binary
etc/agent.toml                    # configs/agent.toml, nats_url set
launchd/com.kanade.agent.plist    # this directory's plist
setup-agent.sh                    # this directory's script
```

```bash
sudo KANADE_NATS_TOKEN=<the deployment's token> bash ./setup-agent.sh
# override the broker baked into etc/agent.toml:
sudo KANADE_NATS_URL=wss://nats.kanade.example.com \
     KANADE_NATS_TOKEN=<the deployment's token> bash ./setup-agent.sh
```

## What it installs

| Path | Notes |
| --- | --- |
| `/usr/local/bin/kanade-agent` | root:wheel 0755, quarantine xattr cleared |
| `/etc/kanade/agent.toml` | root:wheel 0644; an existing `nats_url` is preserved on redeploy unless `KANADE_NATS_URL` is set |
| `/etc/kanade/agent.env` | root:wheel **0600**, `KANADE_NATS_TOKEN=…` — `KANADE_NATS_TOKEN` → existing file → hard fail |
| `/Library/LaunchDaemons/com.kanade.agent.plist` | root:wheel 0644 (world-readable, so it never holds the token — its `/bin/sh` launcher reads `agent.env` and `exec`s the agent) |
| `/var/lib/kanade-agent` | root 0700 data dir (`KANADE_AGENT_DATA_DIR`) |
| `/var/log/kanade/agent.<date>.log` | the agent's own rotated log; `kanade-agent.launchd.log` catches pre-logging startup failures and panics |

The daemon runs as root with `RunAtLoad` and `KeepAlive.SuccessfulExit =
false`: any non-zero exit — a crash, or the self-update swap's `exit(64)` —
restarts it (throttled to 10 s); a clean `exit(0)` stays down. Re-running
the script upgrades in place (`launchctl bootout` → copy →
`bootstrap` → `kickstart -k`).

```bash
sudo launchctl print system/com.kanade.agent
tail -f /var/log/kanade/agent.*.log
# uninstall:
sudo launchctl bootout system/com.kanade.agent
sudo rm -rf /Library/LaunchDaemons/com.kanade.agent.plist /usr/local/bin/kanade-agent \
     "/Library/Application Support/Kanade"
```

launchd's default daemon PATH is only `/usr/bin:/bin:/usr/sbin:/sbin` (no
Homebrew, no `/usr/local/bin`). The plist does not change that: the agent
itself gives **every** job — `run_as: system` included — the job PATH
described below and resolves the host (`pwsh`, `sh`) against it, so an
install keeps working however old its plist is.

## Job identity (`run_as`)

| `run_as` | Runs as | Bootstrap (GUI / Keychain) | Environment |
| --- | --- | --- | --- |
| `system` (default) | root | system — no GUI, no login Keychain | the daemon's own, inherited, except `PATH` (below) |
| `user` | the console user | the user's GUI session | built from scratch (below) |
| `system_gui` | root | the console user's GUI session | built from scratch, root's account |

`user` and `system_gui` launch the job's host as

```text
user:        /bin/launchctl asuser <uid> /usr/bin/sudo -n -u <name> -H -- /usr/bin/env -i <env> <host> <args…>
system_gui:  /bin/launchctl asuser <uid> /usr/bin/env -i <env> <host> <args…>
```

- **Console user** = the owner of `/dev/console`. When it is root (the
  login window) or unreadable, nobody is logged in: the job is **not run**
  and the command handler fails with `no console user logged in … run_as:
  user / system_gui needs a logged-in user` — the same outcome as a Windows
  agent with no active console session.
- **Environment** is exactly `HOME`, `USER`, `LOGNAME`, `SHELL` (from the
  passwd entry of the user, or of root for `system_gui`), `LANG` (the
  daemon's, else `en_US.UTF-8`) and `PATH`. Nothing else crosses over — in
  particular not the daemon's `KANADE_*` variables or the NATS token.
  There is no `TMPDIR`; tools fall back to `/tmp`.
- **PATH** — for every `run_as`, `system` too — is
  `/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin`, then `/etc/paths`,
  then each file in `/etc/paths.d` in name order (what `path_helper` gives a
  login shell), duplicates dropped. The host program is looked up on it.
- **`cwd`**: `~` / `~/…` expands to the target's home (the user's for
  `user`, `/var/root` otherwise — also for `system`). A missing directory
  fails the spawn, as for `system`. With no `cwd`, a `user` job starts in
  the user's home (the daemon's own working directory is its 0700 data
  dir); `system_gui` inherits the daemon's.
- **Staged scripts** (`powershell` / `pwsh` launchers) go to
  `/Library/Application Support/Kanade/agent-scripts/<uuid>/` (root, 0755
  dirs / 0644 files, so the user can read them but not change them) and are
  deleted when the run ends. `$PSScriptRoot` is read-only for a `user` job.
- **Kill / timeout** — for every `run_as` — signals the host's whole
  process group (the host is spawned as a session leader): `SIGTERM`, up to
  5 s for the host to exit, then `SIGKILL`. A clean exit signals nothing, so
  a daemon the script started (`nohup … &`, `Start-Process`) keeps running;
  output capture stops 2 s after the host exits even if that daemon still
  holds stdout/stderr.

## Caveats

- **Gatekeeper**: the binary is not notarized. The script strips
  `com.apple.quarantine`, and launchd runs an unsigned command-line binary
  without a prompt. Apple Silicon still needs at least an ad-hoc signature,
  which the Rust linker applies — re-sign with `codesign --force --sign -`
  if the binary is modified after the build.
- **Command signing**: the keyring lives in `/etc/kanade/command-keys.json`
  (root:wheel, 0600) and enforcement in `/etc/kanade/require-signed-commands`,
  both written by `setup-agent-macos.sh` from `KANADE_COMMAND_KEYS` (JSON
  array) and `KANADE_REQUIRE_SIGNED_COMMANDS` (`1` enforce, `0` stop, unset
  keep) — the same inputs and semantics as the Linux agent, see
  [`deploy/linux/README.md`](../linux/README.md). The backend-generated
  tarball passes the backend's own public key automatically.

## What does not work on macOS yet

A schedule's targets can span operating systems, so none of this is
rejected when the schedule is created — the agent decides at run time.

- **`constraints.require` gates** `ac_power`, `idle` and `network` have no
  sensor on macOS (`cpu_below` works). A schedule that sets any of them
  **fails closed**: the job is not run and no per-pc completion is recorded
  (so it still runs once the gate is supported). The agent logs a WARN and
  publishes one synthetic skipped result (exit 122) per schedule / job
  version per agent run, e.g. `skipped: constraints.require.idle cannot be
  evaluated on macos — not running (fail-closed)`; later ticks only
  debug-log.
- **`when.on: [logon, lock, unlock, network_change]`** never fire — macOS
  has no source for them (`startup` works). When the agent loads such a
  schedule it warns once and publishes one synthetic skipped result (exit
  122), e.g. `skipped: when.on [unlock] never fires on macos — this OS has
  no unlock source`.
- **Idle / presence and Windows event log (winlog) swimlanes** on the
  per-PC timeline stay empty — both samplers are Windows-only.
- **`last_logon`** (last signed-in user / display name) is empty — it is
  read from the Windows registry.
- **Client App features** (the KLP listener: notifications, self-service
  job catalog, Health tab, support unlock, desktop shortcut) are
  Windows-only.

# kanade — macOS agent

Runs `kanade-agent` on a Mac as a **launchd daemon** (`com.kanade.agent`),
the counterpart of the Linux systemd unit and the Windows service. Only the
agent is supported on macOS; the backend + NATS stay on Linux/Windows.

| File | Role |
| --- | --- |
| `setup-agent.sh` | Install/upgrade from a local bundle (run as root). Ships in the backend installer as `setup-agent-macos.sh` |
| `launchd/com.kanade.agent.plist` | The LaunchDaemon definition |

Both are copied verbatim into `crates/kanade-backend/assets/`
(`setup-agent-macos.sh`, `com.kanade.agent.plist`) — edit the originals here
and copy them across; `assets_match_the_workspace_originals` fails on drift.

## Install via the backend (recommended)

Publish a thin per-arch agent binary first (`<version>-macos-x86_64` /
`<version>-macos-aarch64` keys in the `agent_releases` Object Store —
`kanade agent publish` detects the Mach-O arch; universal/fat binaries are
rejected, so build or `lipo -thin` one per arch). Then either:

- **One-liner** — the SPA Agent Install page's `curl … | sudo bash`
  command. The generated `installer.sh` picks `os=macos` from `uname -s`
  and the arch from `uname -m` (`arm64` → `aarch64`).
- **Tarball** — `GET /api/agents/installer?os=macos&arch=x86_64|aarch64`,
  then:

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
bin/kanade-agent                  # the thin binary for this Mac's arch
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
sudo rm /Library/LaunchDaemons/com.kanade.agent.plist /usr/local/bin/kanade-agent
```

## Caveats

- **Gatekeeper**: the binary is not notarized. The script strips
  `com.apple.quarantine`, and launchd runs an unsigned command-line binary
  without a prompt. Apple Silicon still needs at least an ad-hoc signature,
  which the Rust linker applies — re-sign with `codesign --force --sign -`
  if the binary is modified after the build.
- **Command signing**: keyring provisioning is Windows-only today, so
  signed-command verification is inactive on macOS agents (the #1165 gap).

# kanade — Linux (ARM64) production-like deployment

Stands up the backend + NATS + Caddy on a single Ubuntu ARM64 VM
(e.g. Oracle Cloud **Always Free** Ampere), internet-exposed behind
Caddy's automatic HTTPS. This is the "one company = one deployment"
shape from #1172, used here as a production-like validation environment.

## Model: collect artifacts, then install — the target never builds or fetches

This mirrors the Windows model (CI produces artifacts → the target only
installs them) and works in **closed networks**: you assemble a
self-contained bundle on a machine that *has* internet, copy it to the
server, and install entirely from local files. The deployment server needs
**no external access** — no `apt`, no `curl`, no toolchain.

```
[build/staging box, has internet]          [server, closed network]
  bundle.sh  ──assembles──▶  tarball  ──scp──▶  tar -xzf ; sudo ./setup.sh
   ├─ backend  (prebuilt aarch64 artifact)
   ├─ nats-server (aarch64, checksum-verified)
   └─ caddy       (aarch64, checksum-verified)
```

The backend already targets Linux: `default_paths.rs` resolves
`/etc/kanade`, `/var/lib/kanade`, `/var/log/kanade`; the Windows SCM
module is `#[cfg]`-gated off; the registry secret reader is a
`None`-returning stub off Windows, so every secret falls back to an env
var. CI builds + tests it on `ubuntu-latest` every PR. What has **never**
been exercised is a real Linux *deployment* — expect to iron out first-run
details (systemd cwd, file perms, JetStream provisioning).

## Layout

| File | Where it runs | Role |
| --- | --- | --- |
| `bundle.sh` | build/staging box, **Linux/mac** (internet) | Collect backend + nats-server + caddy + configs into one offline tarball |
| `bundle.ps1` | build/staging box, **Windows** (internet) | Same, PowerShell-native — no Git Bash needed |
| `setup.sh` | server (offline) | Install everything from the bundle; generate secrets; enable services |
| `build-aarch64.sh` | build box (interim) | Produce the aarch64 backend artifact until the release target lands |
| `nats-server.conf` | — | NATS: localhost `:4222` for the backend + localhost WS `:8443` for agents behind Caddy; token from env; JetStream under `/var/lib/kanade` |
| `Caddyfile` | — | `:443` auto-HTTPS → backend; `nats.<domain>` → NATS WebSocket |
| `systemd/*.service` | — | backend / nats-server / caddy units; `Restart=on-failure` replicates the Windows self-restart |

## Getting the backend binary

The backend artifact should come from a GitHub Release, the same way the
Windows binaries do — but `release.yml` currently builds **x86_64-linux**
only, so an **aarch64-linux release target still needs adding** (tracked
separately). Until then, build the artifact once on any ARM64 build box:

```bash
./deploy/linux/build-aarch64.sh          # SPA + backend → target/release/kanade-backend
```

`web/dist` is gitignored and CI artifacts are x86_64, which is why an
interim build step exists at all. Once the release target lands, point
`bundle.sh --backend` at the downloaded Release asset instead.

## Quick path: one command (symmetric with the Windows `build-release.ps1`)

`build-release.sh` (and its Windows twin `build-release.ps1`) is the
role-parameterized collector — the counterpart of the Windows
`scripts/build-release.ps1`. It downloads the kanade role binaries from
Releases for the target arch and packs a bundle per role, in one call:

```bash
# Both roles, version from Cargo.toml, arch from `uname -m`:
./deploy/linux/build-release.sh
# → dist/kanade-linux-<arch>-bundle.tar.gz  (backend + nats + caddy)
#   dist/kanade-linux-agent-bundle.tar.gz   (agent)

# Pin a version / target an x86_64 box / one role:
./deploy/linux/build-release.sh --roles backend --version 0.44.35 --arch x86_64
```

`fleet-deploy.sh` takes it the rest of the way — the counterpart of the
Windows `scripts/fleet-deploy.ps1 -Role`: collect → scp → install over
ssh, in one command (it probes the target's arch itself):

```bash
# Backend (needs the public domain):
./deploy/linux/fleet-deploy.sh --role backend --host ubuntu@1.2.3.4 \
    --domain kanade.example.com --identity ~/.ssh/key

# Agent, co-located with a backend (reuses its token):
./deploy/linux/fleet-deploy.sh --role agent --host ubuntu@1.2.3.4 --identity ~/.ssh/key
```

### First deploy vs. update — same split as Windows

Like the Windows `fleet-deploy.ps1`, the agent has two paths:

- **`--mode install`** (default, shown above) — the first deploy: bundle →
  scp → `setup-agent.sh` over ssh.
- **`--mode update`** — 2nd-deploy-onwards over **NATS**, no ssh (the twin
  of the Windows agent rollout): publishes the agent ELF to the release
  object store and flips `target_version` on a scope; the on-box agents
  self-update on their next watch tick and systemd restarts each onto the
  new binary.

```bash
# Roll a version out to a scope over NATS (needs `kanade` on PATH, pointed
# at the deployment via KANADE_NATS_URL / KANADE_NATS_TOKEN):
./deploy/linux/fleet-deploy.sh --role agent --mode update --arch x86_64 \
    --version 0.44.36 --group canary --jitter 5m       # or --pc <id> / --all
```

This works because the agent's self-update (`self_update.rs`) is
OS-neutral — a three-step binary swap then `exit(64)`, which the
`kanade-agent.service` `Restart`/`RestartForceExitStatus=64` turns into a
restart, exactly as SCM failure-actions do on Windows. The one Linux-only
enabler is `kanade agent publish --version` (an ELF has no PE VERSIONINFO
to auto-label).

The rest of this doc is the **manual, step-by-step** path the one-command
tools automate — useful for closed networks where the collector can't
reach the target directly.

## 1. Assemble the bundle (on a box with internet)

**Linux / macOS:**

```bash
./deploy/linux/bundle.sh --backend target/release/kanade-backend
# → kanade-linux-aarch64-bundle.tar.gz (+ .sha256)
# amd64 box? add --arch x86_64  (→ kanade-linux-x86_64-bundle.tar.gz)
```

**Windows** (PowerShell 7, no Git Bash needed):

```powershell
.\deploy\linux\bundle.ps1 -Backend C:\path\to\kanade-backend   # -Arch x86_64 for amd64
```

Either downloads `nats-server` and `caddy` for the selected arch (default
aarch64), verifies each against its official checksums, and packs them
with the backend, the configs, the systemd units and `setup.sh`. Both
produce the same tarball.

## 2. Install on the server (offline)

```bash
# (aarch64 shown; swap for kanade-linux-x86_64-bundle on an amd64 box)
scp kanade-linux-aarch64-bundle.tar.gz  server:
ssh server
tar -xzf kanade-linux-aarch64-bundle.tar.gz
cd kanade-linux-aarch64-bundle
# `bash ./setup.sh` (not `./setup.sh`) so it runs regardless of whether the
# archive carried the exec bit — a Windows-built bundle may not.
sudo KANADE_DOMAIN=kanade.example.com bash ./setup.sh
```

`setup.sh` (no network):
1. creates the `kanade` and `caddy` system users and the `/etc/kanade`,
   `/var/lib/kanade`, `/var/log/kanade`, `/var/lib/caddy`, `/etc/caddy` dirs;
2. installs the three binaries from the bundle into `/usr/local/bin`;
3. installs `backend.toml`, binding the backend to `127.0.0.1:8080` (so
   Caddy is the only public surface) and setting
   `server.public_url = https://$KANADE_DOMAIN` (Host-header hardening);
4. **generates** the NATS token, JWT secret, static token and bootstrap
   admin password — the token into a broker-only `nats.env` (least
   privilege) and the full set into `kanade.env`, both mode `0600`. This is
   the #1172 floor: real per-deployment secrets, never the `dev`/placeholder;
5. installs the NATS conf, Caddyfile (with your domain) and the three
   systemd units, then enables and starts everything.

Point two DNS records at the server's public IP first (Caddy needs them to
issue certs): `kanade.example.com` and `nats.kanade.example.com`.

### Firewall — open 80/443, keep everything else private

DNS is not enough: Caddy's ACME challenge and all public traffic need
inbound **TCP 80 and 443**. On many clouds (Oracle especially) there are
**two** layers and both must allow them — the cloud Security List / NSG
**and** the instance's own firewall (Oracle's Ubuntu images ship with a
restrictive `iptables`):

```bash
# instance-level (Oracle Ubuntu images block by default). Insert BEFORE the
# REJECT rule — on these images it's at line 5, so a hardcoded `-I INPUT 6`
# would land after it and 80/443 would stay blocked (ACME then fails).
rej=$(sudo iptables -L INPUT --line-numbers | awk '$1 ~ /^[0-9]+$/ && /REJECT/{print $1; exit}')
sudo iptables -I INPUT "${rej:-1}" -p tcp --dport 443 -j ACCEPT
sudo iptables -I INPUT "${rej:-1}" -p tcp --dport 80  -j ACCEPT
sudo netfilter-persistent save
```

Keep NATS `:4222`, its monitoring `:8222`, the WebSocket `:8443`, and the
backend `:8080` **private** — they all bind localhost and are reached only
through Caddy, so nothing but 80/443 should be open at either layer.

Agents then connect with:

```toml
nats_url = "wss://nats.kanade.example.com"
```

## The one critical secret: `KANADE_JWT_SECRET`

`auth.rs` warns that with no JWT secret it uses a **hard-coded dev
fallback (NEVER in production)** — anyone could forge admin JWTs. On an
internet-exposed box that is game over. `setup.sh` generates it into
`kanade.env`; do not remove it. This is why secret generation is part of
the floor, not an afterthought.

## First-run checks

```bash
systemctl status nats-server kanade-backend caddy
journalctl -u kanade-backend -f
# Validate TLS for real — no -k, so a failed cert/ACME/DNS shows up here
# instead of being hidden:
curl https://kanade.example.com/               # SPA
# log in as admin; read the password from the root-only env file:
sudo sed -n 's/^KANADE_BOOTSTRAP_ADMIN_PASSWORD=//p' /etc/kanade/kanade.env
```

## Deploying a Linux agent

Same collect-then-install model, for the **agent** (`bundle-agent.sh` /
`bundle-agent.ps1` → tarball → `setup-agent.sh` → a `kanade-agent`
systemd service). Unlike the backend bundle there is nothing to fetch
(no NATS/Caddy), so it is **arch-agnostic** — you pass the `kanade-agent`
binary matching the target's architecture
(`kanade-agent-<arch>-unknown-linux-musl` from a Release).

```bash
# On a box with internet — collect the agent binary + config + unit:
./bundle-agent.sh --agent /path/to/kanade-agent
# → kanade-linux-agent-bundle.tar.gz (+ .sha256)   (bundle-agent.ps1 on Windows)

# On the target — extract and install. Use `bash ./setup-agent.sh` (not
# `./setup-agent.sh`): a Windows-built bundle's tar may not carry the
# exec bit, so a bare invocation fails with "command not found".
tar -xzf kanade-linux-agent-bundle.tar.gz
cd kanade-linux-agent-bundle

# Co-located with a backend on the same box — reuses the backend's
# /etc/kanade/nats.env token and the local broker (nats://127.0.0.1:4222):
sudo bash ./setup-agent.sh

# Standalone agent box talking to a remote broker over wss:
sudo KANADE_NATS_URL=wss://nats.kanade.example.com \
     KANADE_NATS_TOKEN=<the deployment's token> bash ./setup-agent.sh
```

The agent runs as **root** (so `kanade run` / jobs can manage the box —
the Linux analog of the Windows LocalSystem agent) with an isolated data
dir at `/var/lib/kanade-agent`. It appears in the SPA fleet under the
host's name. `setup-agent.sh` ends with `systemctl restart`, so a
re-deploy swaps the running binary. Check it with:

```bash
systemctl status kanade-agent
journalctl -u kanade-agent -f
```

### Easier path: the backend-generated installer tarball

The backend can also generate the Linux installer for you — the
counterpart of the Windows Agent Install ZIP: `GET
/api/agents/installer?os=linux&arch=x86_64|aarch64` (or the SPA Agent
Install page) returns a tar.gz with the same layout as the manual bundle
above (`bin/`, `etc/`, `systemd/`, `setup-agent.sh`) plus a generated
`install.sh` that bakes in the NATS token configured under Settings →
server settings (`agent_install`). Extract and `sudo ./install.sh` — no
`KANADE_NATS_TOKEN` to pass by hand. The release it bundles comes from
the `agent_releases` Object Store (`<version>-linux-<arch>` keys,
published with `kanade agent publish --version …`); the manual
bundle-agent.sh flow above remains the way to install a binary that was
never published to the store. One caveat: command-signing keyring
provisioning is Windows-only today, so agents installed this way run
with signature verification inactive (the #1165 gap).

Note: `sh` / `pwsh` command execution on the Linux agent needs #1198
(older agent builds can register and be monitored but fail exec at
`spawn powershell`).

## Open items

- **aarch64-linux release target** (add to `release.yml`) so the backend
  artifact ships from a Release like the Windows binaries — tracked
  separately; `build-aarch64.sh` is the interim producer.
- This bundle is the #1172 floor (TLS, real secrets, one public surface).
  The fast-follow — command signing (#1165), per-agent identity (#1162),
  update signing — is not wired here.
- `Restart=on-failure` maps the Windows "exit(1) → SCM recovery" self-
  restart onto systemd. The boot-sentinel quarantine (#582) is Windows-
  deploy-script logic and has no Linux equivalent yet.

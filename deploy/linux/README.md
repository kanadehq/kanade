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

## 1. Assemble the bundle (on a box with internet)

**Linux / macOS:**

```bash
./deploy/linux/bundle.sh --backend target/release/kanade-backend
# → kanade-linux-arm64-bundle.tar.gz (+ .sha256)
```

**Windows** (PowerShell 7, no Git Bash needed):

```powershell
.\deploy\linux\bundle.ps1 -Backend C:\path\to\kanade-backend
```

Either downloads `nats-server` and `caddy` (aarch64), verifies each against
its official checksums, and packs them with the backend, the configs, the
systemd units and `setup.sh`. Both produce the same tarball.

## 2. Install on the server (offline)

```bash
scp kanade-linux-arm64-bundle.tar.gz  server:
ssh server
tar -xzf kanade-linux-arm64-bundle.tar.gz
cd kanade-linux-arm64-bundle
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

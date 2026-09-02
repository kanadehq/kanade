#!/usr/bin/env bash
# Install the kanade DEMO stack on a small internet-connected VM: Caddy
# (auto-HTTPS) + demo-api (Bun, the mock backend from
# crates/kanade-backend/web/demo/) + the built SPA.
#
# Unlike ../setup.sh (the real deployment), this box HAS internet, so this
# script fetches Caddy + Bun itself rather than requiring a pre-built
# offline bundle. There is no NATS, no kanade-backend, no real auth — see
# crates/kanade-backend/web/demo/README.md for what that means.
#
# Usage (run on the demo box, as root, from this directory after copying
# the bundle — see "assemble + copy" below):
#   sudo KANADE_DEMO_DOMAIN=132.226.85.186.sslip.io bash ./setup-demo.sh
#
# Assemble + copy (from a dev box with the repo + bun). Every member is
# archived with its full repo-relative path — this script resolves its
# inputs from `$repo_root` (see below), so flattening `demo/` with a
# second `-C` would put server.ts where nothing looks for it.
#   cd crates/kanade-backend/web && bun run build && cd ../../..
#   tar -czf /tmp/kanade-demo-bundle.tar.gz \
#     deploy/linux/demo \
#     deploy/linux/systemd/caddy.service \
#     crates/kanade-backend/web/dist \
#     crates/kanade-backend/web/demo/server.ts \
#     crates/kanade-backend/web/demo/fleet.ts \
#     crates/kanade-backend/web/demo/remote-desktop.jpg
#   scp -i <key> /tmp/kanade-demo-bundle.tar.gz ubuntu@<ip>:
#   ssh -i <key> ubuntu@<ip> 'tar -xzf kanade-demo-bundle.tar.gz && \
#     sudo KANADE_DEMO_DOMAIN=<ip>.sslip.io bash ./deploy/linux/demo/setup-demo.sh'
set -euo pipefail

: "${KANADE_DEMO_DOMAIN:?set KANADE_DEMO_DOMAIN=<public-ip>.sslip.io (or a real domain you point at this box)}"
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

# Validate the domain as a DNS hostname before it is templated into the
# Caddyfile. Anything else (spaces, sed delimiters, control chars) would
# corrupt that file instead of failing loudly — same check, and same
# reason, as ../setup.sh applies to KANADE_DOMAIN.
if ! printf '%s' "$KANADE_DEMO_DOMAIN" \
	| grep -Eq '^([a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,}$'; then
	echo "KANADE_DEMO_DOMAIN='${KANADE_DEMO_DOMAIN}' is not a valid DNS hostname" >&2
	exit 1
fi

CADDY_VERSION="2.10.2"
BUN_VERSION="1.3.14"
bundle="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$bundle/../../.." && pwd)"

echo "==> Verifying bundle contents"
# Every member the script later installs, checked before anything on the
# host is touched — a preflight that misses a file just moves the failure
# to after the box has been modified.
for f in "$repo_root/crates/kanade-backend/web/dist/index.html" \
	"$repo_root/crates/kanade-backend/web/demo/server.ts" \
	"$repo_root/crates/kanade-backend/web/demo/fleet.ts" \
	"$repo_root/crates/kanade-backend/web/demo/remote-desktop.jpg" \
	"$repo_root/deploy/linux/systemd/caddy.service" \
	"$bundle/Caddyfile" "$bundle/systemd/kanade-demo-api.service"; do
	[ -e "$f" ] || { echo "missing required bundle member: $f" >&2; exit 1; }
done

echo "==> Creating users and directories"
id -u kanade-demo >/dev/null 2>&1 || useradd --system --home /var/lib/kanade-demo --shell /usr/sbin/nologin kanade-demo
id -u caddy       >/dev/null 2>&1 || useradd --system --home /var/lib/caddy       --shell /usr/sbin/nologin caddy
install -d -o kanade-demo -g kanade-demo /var/lib/kanade-demo
install -d -o caddy       -g caddy       /var/lib/caddy /var/www/kanade-demo
install -d /etc/caddy

echo "==> Installing Bun ${BUN_VERSION} (checksum-verified; runs demo-api's TS directly, no build step)"
# Pinned + verified rather than `curl https://bun.sh/install | bash`: this
# script runs as root, so an unverified installer response would execute
# arbitrary code as root. Same treatment the Caddy download below gets.
if [ ! -x /usr/local/bin/bun ]; then
	command -v unzip >/dev/null 2>&1 \
		|| { echo "unzip is required to install bun (apt-get install -y unzip)" >&2; exit 1; }
	case "$(uname -m)" in
		x86_64) bunarch=x64 ;;
		aarch64) bunarch=aarch64 ;;
		*) echo "unsupported arch for bun: $(uname -m)" >&2; exit 1 ;;
	esac
	tmp="$(mktemp -d)"
	bbase="bun-linux-${bunarch}.zip"
	brel="https://github.com/oven-sh/bun/releases/download/bun-v${BUN_VERSION}"
	curl -fsSL "$brel/$bbase" -o "$tmp/$bbase"
	# Bun publishes one SHASUMS256.txt covering every asset in the release.
	curl -fsSL "$brel/SHASUMS256.txt" -o "$tmp/SHASUMS256.txt"
	( cd "$tmp" && sha256sum --check --ignore-missing SHASUMS256.txt ) \
		|| { echo "bun checksum FAILED" >&2; exit 1; }
	unzip -q "$tmp/$bbase" -d "$tmp"
	install -m 0755 "$tmp/bun-linux-${bunarch}/bun" /usr/local/bin/bun
	rm -rf "$tmp"
fi
[ -x /usr/local/bin/bun ] || { echo "bun did not land at /usr/local/bin/bun" >&2; exit 1; }

echo "==> Installing Caddy ${CADDY_VERSION} (checksum-verified)"
if [ ! -x /usr/local/bin/caddy ]; then
	arch="$(uname -m)"
	case "$arch" in
		x86_64) dlarch=amd64 ;;
		aarch64) dlarch=arm64 ;;
		*) echo "unsupported arch: $arch" >&2; exit 1 ;;
	esac
	tmp="$(mktemp -d)"
	cbase="caddy_${CADDY_VERSION}_linux_${dlarch}.tar.gz"
	crel="https://github.com/caddyserver/caddy/releases/download/v${CADDY_VERSION}"
	curl -fsSL "$crel/$cbase" -o "$tmp/$cbase"
	curl -fsSL "$crel/caddy_${CADDY_VERSION}_checksums.txt" -o "$tmp/caddy_checksums.txt"
	( cd "$tmp" && sha512sum --check --ignore-missing caddy_checksums.txt ) \
		|| { echo "caddy checksum FAILED" >&2; exit 1; }
	tar -xzf "$tmp/$cbase" -C "$tmp"
	install -m 0755 "$tmp/caddy" /usr/local/bin/caddy
	rm -rf "$tmp"
fi

echo "==> Installing demo-api (server.ts + fleet.ts + remote-desktop.jpg)"
install -o kanade-demo -g kanade-demo -m 0644 \
	"$repo_root/crates/kanade-backend/web/demo/server.ts" \
	"$repo_root/crates/kanade-backend/web/demo/fleet.ts" \
	"$repo_root/crates/kanade-backend/web/demo/remote-desktop.jpg" \
	/var/lib/kanade-demo/

echo "==> Installing the built SPA"
rm -rf /var/www/kanade-demo/*
cp -r "$repo_root/crates/kanade-backend/web/dist/." /var/www/kanade-demo/
chown -R caddy:caddy /var/www/kanade-demo

echo "==> Caddyfile + systemd units"
sed "s|__KANADE_DEMO_DOMAIN__|${KANADE_DEMO_DOMAIN}|g" "$bundle/Caddyfile" > /etc/caddy/Caddyfile
chmod 0644 /etc/caddy/Caddyfile
install -m 0644 "$bundle/systemd/kanade-demo-api.service" /etc/systemd/system/kanade-demo-api.service

# caddy.service is shared with the real deployment's shape; install it here
# too if this box doesn't already have one (a demo-only box).
if [ ! -e /etc/systemd/system/caddy.service ]; then
	install -m 0644 "$repo_root/deploy/linux/systemd/caddy.service" /etc/systemd/system/caddy.service
fi

echo "==> Enabling services"
systemctl daemon-reload
systemctl enable kanade-demo-api.service caddy.service
# `enable --now` only *starts* a unit — it does NOT restart one that is
# already active, so on a re-deploy the running Bun process would keep
# serving the server.ts this script just replaced (and Caddy the old
# Caddyfile). Restart unconditionally: both are stateless here, and
# `restart` starts a stopped unit too, so this covers first run as well.
systemctl restart kanade-demo-api.service
systemctl restart caddy.service

echo
echo "==> Done. Checks:"
echo "    systemctl status kanade-demo-api caddy"
echo "    journalctl -u kanade-demo-api -f"
echo "    curl https://${KANADE_DEMO_DOMAIN}/api/version"
echo "    curl https://${KANADE_DEMO_DOMAIN}/          (SPA; log in with ANY username/password)"

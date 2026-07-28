#!/usr/bin/env bash
# Assemble a fully-offline ARM64 deployment bundle.
#
# Run this on a machine WITH internet (a build / staging box, CI, your
# laptop) — NOT the deployment server. It collects the three binaries and
# the config/units/scripts into one tarball. Copy that tarball to the
# closed-network server, extract it, and run ./setup.sh there — the server
# needs no external access at all.
#
# The backend binary is not built here: point --backend at a prebuilt
# aarch64 kanade-backend (a GitHub Release asset once the aarch64-linux
# target lands — see the tracking issue — or, until then, the output of
# build-aarch64.sh run on any ARM64 build box).
#
# Usage:
#   ./bundle.sh --backend /path/to/kanade-backend [--out DIR] \
#               [--nats-version v2.14.3] [--caddy-version 2.10.2]
set -euo pipefail

NATS_VERSION="v2.14.3"
CADDY_VERSION="2.10.2"
BACKEND_BIN=""
OUT_DIR="."

while [ $# -gt 0 ]; do
	case "$1" in
		--backend) BACKEND_BIN="$2"; shift 2 ;;
		--out) OUT_DIR="$2"; shift 2 ;;
		--nats-version) NATS_VERSION="$2"; shift 2 ;;
		--caddy-version) CADDY_VERSION="$2"; shift 2 ;;
		*) echo "unknown arg: $1" >&2; exit 1 ;;
	esac
done

[ -n "$BACKEND_BIN" ] || { echo "--backend <path to aarch64 kanade-backend> is required" >&2; exit 1; }
[ -x "$BACKEND_BIN" ] || { echo "backend binary not found/executable: $BACKEND_BIN" >&2; exit 1; }

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
here="$repo_root/deploy/linux"

stage="$(mktemp -d)"
root="$stage/kanade-linux-arm64-bundle"
mkdir -p "$root/bin" "$root/etc" "$root/systemd"
trap 'rm -rf "$stage"' EXIT

echo "==> backend: $BACKEND_BIN"
install -m 0755 "$BACKEND_BIN" "$root/bin/kanade-backend"

echo "==> nats-server ${NATS_VERSION} (linux-arm64), checksum-verified"
tmp="$(mktemp -d)"
nbase="nats-server-${NATS_VERSION}-linux-arm64.tar.gz"
nrel="https://github.com/nats-io/nats-server/releases/download/${NATS_VERSION}"
curl -fsSL "$nrel/$nbase" -o "$tmp/$nbase"
curl -fsSL "$nrel/SHA256SUMS" -o "$tmp/SHA256SUMS"
( cd "$tmp" && sha256sum --check --ignore-missing SHA256SUMS ) \
	|| { echo "nats-server checksum FAILED" >&2; exit 1; }
tar -xzf "$tmp/$nbase" -C "$tmp"
install -m 0755 "$tmp"/nats-server-*/nats-server "$root/bin/nats-server"

echo "==> caddy ${CADDY_VERSION} (linux-arm64), checksum-verified"
cbase="caddy_${CADDY_VERSION}_linux_arm64.tar.gz"
crel="https://github.com/caddyserver/caddy/releases/download/v${CADDY_VERSION}"
curl -fsSL "$crel/$cbase" -o "$tmp/$cbase"
# Caddy publishes a per-release checksums file covering every asset.
curl -fsSL "$crel/caddy_${CADDY_VERSION}_checksums.txt" -o "$tmp/caddy_checksums.txt"
( cd "$tmp" && sha256sum --check --ignore-missing caddy_checksums.txt ) \
	|| { echo "caddy checksum FAILED" >&2; exit 1; }
tar -xzf "$tmp/$cbase" -C "$tmp"
install -m 0755 "$tmp/caddy" "$root/bin/caddy"
rm -rf "$tmp"

echo "==> configs, units, installer"
install -m 0644 "$here/nats-server.conf"          "$root/etc/nats-server.conf"
install -m 0644 "$here/Caddyfile"                 "$root/etc/Caddyfile"
install -m 0644 "$repo_root/configs/backend.toml" "$root/etc/backend.toml"
install -m 0644 "$here/systemd/nats-server.service"    "$root/systemd/nats-server.service"
install -m 0644 "$here/systemd/kanade-backend.service" "$root/systemd/kanade-backend.service"
install -m 0644 "$here/systemd/caddy.service"          "$root/systemd/caddy.service"
install -m 0755 "$here/setup.sh"                  "$root/setup.sh"
install -m 0644 "$here/README.md"                 "$root/README.md"

mkdir -p "$OUT_DIR"
out="$(cd "$OUT_DIR" && pwd)/kanade-linux-arm64-bundle.tar.gz"
tar -C "$stage" -czf "$out" kanade-linux-arm64-bundle
sha256sum "$out" | tee "${out}.sha256"

echo
echo "==> Bundle: $out"
echo "    Copy to the server, then:"
echo "      tar -xzf kanade-linux-arm64-bundle.tar.gz"
echo "      cd kanade-linux-arm64-bundle"
echo "      sudo KANADE_DOMAIN=kanade.example.com ./setup.sh"

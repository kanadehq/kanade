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
#   ./bundle.sh --backend /path/to/kanade-backend [--arch x86_64|aarch64] \
#               [--out DIR] [--nats-version v2.14.3] [--caddy-version 2.10.2]
#
# --arch selects which nats-server / caddy binaries to fetch (default
# aarch64). It must match the architecture of the --backend binary you
# pass; there is no way to verify that here, so the caller is
# responsible (build-release.sh keeps them in lockstep).
set -euo pipefail

NATS_VERSION="v2.14.3"
CADDY_VERSION="2.10.2"
BACKEND_BIN=""
OUT_DIR="."
ARCH="aarch64"

while [ $# -gt 0 ]; do
	case "$1" in
		--backend) BACKEND_BIN="$2"; shift 2 ;;
		--arch) ARCH="$2"; shift 2 ;;
		--out) OUT_DIR="$2"; shift 2 ;;
		--nats-version) NATS_VERSION="$2"; shift 2 ;;
		--caddy-version) CADDY_VERSION="$2"; shift 2 ;;
		*) echo "unknown arg: $1" >&2; exit 1 ;;
	esac
done

[ -n "$BACKEND_BIN" ] || { echo "--backend <path to kanade-backend> is required" >&2; exit 1; }
[ -x "$BACKEND_BIN" ] || { echo "backend binary not found/executable: $BACKEND_BIN" >&2; exit 1; }

# Normalise the user-facing arch token (x86_64 / aarch64, matching kanade
# release-target names and `uname -m`) to the nats/caddy asset arch
# (amd64 / arm64).
case "$ARCH" in
	aarch64 | arm64) ARCH="aarch64"; DL_ARCH="arm64" ;;
	x86_64 | amd64)  ARCH="x86_64";  DL_ARCH="amd64" ;;
	*) echo "unsupported --arch '$ARCH' (use x86_64 or aarch64)" >&2; exit 1 ;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
here="$repo_root/deploy/linux"

stage="$(mktemp -d)"
bundle_name="kanade-linux-${ARCH}-bundle"
root="$stage/$bundle_name"
mkdir -p "$root/bin" "$root/etc" "$root/systemd"
trap 'rm -rf "$stage"' EXIT

echo "==> backend: $BACKEND_BIN"
install -m 0755 "$BACKEND_BIN" "$root/bin/kanade-backend"

echo "==> nats-server ${NATS_VERSION} (linux-${DL_ARCH}), checksum-verified"
tmp="$(mktemp -d)"
nbase="nats-server-${NATS_VERSION}-linux-${DL_ARCH}.tar.gz"
nrel="https://github.com/nats-io/nats-server/releases/download/${NATS_VERSION}"
curl -fsSL "$nrel/$nbase" -o "$tmp/$nbase"
curl -fsSL "$nrel/SHA256SUMS" -o "$tmp/SHA256SUMS"
( cd "$tmp" && sha256sum --check --ignore-missing SHA256SUMS ) \
	|| { echo "nats-server checksum FAILED" >&2; exit 1; }
tar -xzf "$tmp/$nbase" -C "$tmp"
install -m 0755 "$tmp"/nats-server-*/nats-server "$root/bin/nats-server"

echo "==> caddy ${CADDY_VERSION} (linux-${DL_ARCH}), checksum-verified"
cbase="caddy_${CADDY_VERSION}_linux_${DL_ARCH}.tar.gz"
crel="https://github.com/caddyserver/caddy/releases/download/v${CADDY_VERSION}"
curl -fsSL "$crel/$cbase" -o "$tmp/$cbase"
# Caddy publishes a per-release checksums file covering every asset.
# It is SHA-512 (NATS's SHA256SUMS above is SHA-256) — using sha256sum here
# fails with "no properly formatted SHA256 checksum lines found".
curl -fsSL "$crel/caddy_${CADDY_VERSION}_checksums.txt" -o "$tmp/caddy_checksums.txt"
( cd "$tmp" && sha512sum --check --ignore-missing caddy_checksums.txt ) \
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
out_dir="$(cd "$OUT_DIR" && pwd)"
name="${bundle_name}.tar.gz"
tar -C "$stage" -czf "$out_dir/$name" "$bundle_name"
out="$out_dir/$name"
# Record the basename so `sha256sum -c` works after the archive is copied
# to the target (an absolute build path would not).
( cd "$out_dir" && sha256sum "$name" | tee "${name}.sha256" )

echo
echo "==> Bundle: $out"
echo "    Copy to the server, then:"
echo "      tar -xzf ${name}"
echo "      cd ${bundle_name}"
echo "      sudo KANADE_DOMAIN=kanade.example.com bash ./setup.sh"

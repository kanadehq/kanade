#!/usr/bin/env bash
# Assemble a fully-offline kanade AGENT deployment bundle.
#
# Run this on a machine WITH internet (build box, CI, your laptop) — NOT
# the deployment target. It collects the agent binary, its config, the
# systemd unit, and the installer into one tarball. Copy that tarball to
# the target, extract it, and run ./setup-agent.sh there — the target
# needs no external access at all.
#
# The agent binary is not built here: point --agent at a prebuilt Linux
# kanade-agent — a GitHub Release asset
# (kanade-agent-<arch>-unknown-linux-musl.tar.gz, extracted) matching the
# TARGET's architecture. The bundle is arch-agnostic (no nats/caddy to
# fetch), so this same script works for x86_64 and aarch64 — you pick the
# binary.
#
# Usage:
#   ./bundle-agent.sh --agent /path/to/kanade-agent [--out DIR]
set -euo pipefail

AGENT_BIN=""
OUT_DIR="."

while [ $# -gt 0 ]; do
	case "$1" in
		--agent) AGENT_BIN="$2"; shift 2 ;;
		--out) OUT_DIR="$2"; shift 2 ;;
		*) echo "unknown arg: $1" >&2; exit 1 ;;
	esac
done

[ -n "$AGENT_BIN" ] || { echo "--agent <path to a prebuilt linux kanade-agent> is required" >&2; exit 1; }
# -f as well as -x: a directory satisfies -x, and would be staged as a
# bogus "binary" that setup-agent.sh only rejects after it has begun
# changing the target.
[ -f "$AGENT_BIN" ] && [ -x "$AGENT_BIN" ] || { echo "agent binary not a regular executable file: $AGENT_BIN" >&2; exit 1; }

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
here="$repo_root/deploy/linux"

stage="$(mktemp -d)"
root="$stage/kanade-linux-agent-bundle"
mkdir -p "$root/bin" "$root/etc" "$root/systemd"
trap 'rm -rf "$stage"' EXIT

echo "==> agent: $AGENT_BIN"
install -m 0755 "$AGENT_BIN" "$root/bin/kanade-agent"

echo "==> config, unit, installer"
install -m 0644 "$repo_root/configs/agent.toml"        "$root/etc/agent.toml"
install -m 0644 "$here/systemd/kanade-agent.service"   "$root/systemd/kanade-agent.service"
install -m 0755 "$here/setup-agent.sh"                 "$root/setup-agent.sh"

mkdir -p "$OUT_DIR"
out_dir="$(cd "$OUT_DIR" && pwd)"
name="kanade-linux-agent-bundle.tar.gz"
tar -C "$stage" -czf "$out_dir/$name" kanade-linux-agent-bundle
out="$out_dir/$name"
# Record the basename (not the absolute build path) so `sha256sum -c
# kanade-linux-agent-bundle.tar.gz.sha256` works after the tarball is
# copied to the target.
( cd "$out_dir" && sha256sum "$name" | tee "${name}.sha256" )

echo
echo "==> Bundle: $out"
echo "    Copy to the target, then:"
echo "      tar -xzf kanade-linux-agent-bundle.tar.gz"
echo "      cd kanade-linux-agent-bundle"
echo "      sudo bash ./setup-agent.sh                  # co-located with a backend"
echo "      # or, standalone against a remote broker:"
echo "      sudo KANADE_NATS_URL=wss://nats.kanade.example.com \\"
echo "           KANADE_NATS_TOKEN=<token> bash ./setup-agent.sh"

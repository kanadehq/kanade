#!/usr/bin/env bash
# Install a kanade AGENT as a systemd service FROM A LOCAL BUNDLE — no
# network access required (closed-network friendly). Assemble the bundle
# on a machine with internet using bundle-agent.sh (or bundle-agent.ps1),
# copy it to the target, extract, then run this from the bundle root:
#
#   # Co-located with a backend on the same box (reuses its NATS token):
#   sudo bash ./setup-agent.sh
#
#   # Standalone agent box talking to a remote broker:
#   sudo KANADE_NATS_URL=wss://nats.kanade.example.com \
#        KANADE_NATS_TOKEN=<the deployment's token> bash ./setup-agent.sh
#
# Invoke via `bash ./setup-agent.sh` (not `./setup-agent.sh`): a
# Windows-built bundle's tar may not carry the exec bit, so a bare
# `./setup-agent.sh` fails with "command not found".
#
# This mirrors the Windows model and setup.sh: CI/build produces the
# artifacts, the target only installs them — it never builds or fetches.
#
# Idempotent: re-running keeps an existing /etc/kanade/agent.env (so the
# token is stable), overwrites the binary + config + unit, and RESTARTS
# the service so a re-deploy actually swaps the running binary.
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

# The bundle root is this script's directory. Everything is installed
# from here; nothing is downloaded.
bundle="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "==> Verifying bundle contents"
for f in bin/kanade-agent etc/agent.toml systemd/kanade-agent.service; do
	[ -e "$bundle/$f" ] || { echo "bundle is missing $f — rebuild it with bundle-agent.sh" >&2; exit 1; }
done

echo "==> Creating user and directories"
# Reuse the `kanade` service user if a backend already made it; the agent
# itself runs as root (see the unit), but the data/log dirs are owned by
# kanade for parity with the backend's layout.
id -u kanade >/dev/null 2>&1 || useradd --system --home /var/lib/kanade --shell /usr/sbin/nologin kanade
install -d -o kanade -g kanade /etc/kanade /var/log/kanade
# The agent runs as root; keep its data dir root-owned (0700) so the
# shared, lower-privileged `kanade` account can't read or tamper with
# agent state.
install -d -o root -g root -m 0700 /var/lib/kanade-agent

echo "==> Installing agent binary (from bundle, offline)"
install -m 0755 "$bundle/bin/kanade-agent" /usr/local/bin/kanade-agent

echo "==> Agent config (/etc/kanade/agent.toml)"
# agent.toml templates its Linux paths via teravars is_windows() at
# startup and defaults to nats://127.0.0.1:4222 (co-located broker).
#
# Preserve an existing deployment's broker across a redeploy: capture the
# current nats_url BEFORE overwriting, so a standalone agent (deployed
# once with KANADE_NATS_URL) doesn't silently revert to the bundle's
# localhost default when re-run without it. An explicit KANADE_NATS_URL
# still wins.
prev_url=""
[ -f /etc/kanade/agent.toml ] && \
	prev_url="$(sed -n "s/^ *nats_url *= *['\"]\\(.*\\)['\"].*/\\1/p" /etc/kanade/agent.toml | head -n1)"
# Root-owned (not the shared `kanade` user): the agent runs as root, so a
# config the lower-privileged backend account could rewrite would let it
# redirect the root agent to an attacker-controlled broker.
install -o root -g root -m 0644 "$bundle/etc/agent.toml" /etc/kanade/agent.toml
url="${KANADE_NATS_URL:-$prev_url}"
if [ -n "$url" ]; then
	# Reject characters that would break the TOML single-quoted literal or
	# be mangled by awk's -v backslash processing.
	case "$url" in
		*"'"*) echo "KANADE_NATS_URL must not contain a single quote" >&2; exit 1 ;;
		*'"'*) echo "KANADE_NATS_URL must not contain a double quote" >&2; exit 1 ;;
		*'\'*) echo "KANADE_NATS_URL must not contain a backslash" >&2; exit 1 ;;
	esac
	# awk with -v (not `sed s///`): the URL is passed as data, so `&` (from
	# query params) and `|` are never treated as replacement
	# metacharacters. `cat >` back into the file keeps its root:root 0644.
	tmpf="$(mktemp)"
	awk -v url="$url" -v q="'" '
		$0 ~ /^[[:space:]]*nats_url[[:space:]]*=/ { print "nats_url = " q url q; next }
		{ print }
	' /etc/kanade/agent.toml > "$tmpf" && cat "$tmpf" > /etc/kanade/agent.toml
	rm -f "$tmpf"
	echo "    nats_url -> ${url}"
fi

echo "==> Token (/etc/kanade/agent.env — root-only)"
# Resolve the NATS token, in priority order:
#   1. an explicit KANADE_NATS_TOKEN (standalone / override)
#   2. an existing /etc/kanade/nats.env from a co-located backend
#   3. an already-installed /etc/kanade/agent.env (idempotent re-run)
# Anything else is a hard error — never fall back to a dev/placeholder
# token (#1172 floor).
if [ -n "${KANADE_NATS_TOKEN:-}" ]; then
	token="$KANADE_NATS_TOKEN"
elif [ -f /etc/kanade/nats.env ]; then
	token="$(sed -n 's/^KANADE_NATS_TOKEN=//p' /etc/kanade/nats.env | head -n1)"
	echo "    reusing the co-located backend's token from /etc/kanade/nats.env"
elif [ -f /etc/kanade/agent.env ]; then
	token="$(sed -n 's/^KANADE_NATS_TOKEN=//p' /etc/kanade/agent.env | head -n1)"
	echo "    keeping the existing /etc/kanade/agent.env token"
else
	echo "no NATS token: set KANADE_NATS_TOKEN=... (or run on a box that already has /etc/kanade/nats.env)" >&2
	exit 1
fi
[ -n "$token" ] || { echo "resolved an empty NATS token — aborting" >&2; exit 1; }
umask 077
printf 'KANADE_NATS_TOKEN=%s\n' "$token" > /etc/kanade/agent.env
# Root-owned, not the shared `kanade` account — a compromised backend
# process must not be able to read the root agent's token file.
chown root:root /etc/kanade/agent.env
chmod 0600 /etc/kanade/agent.env

echo "==> systemd unit (from bundle)"
install -m 0644 "$bundle/systemd/kanade-agent.service" /etc/systemd/system/kanade-agent.service

echo "==> Enabling + (re)starting the agent"
systemctl daemon-reload
systemctl enable kanade-agent.service
# `restart` (not `enable --now`): on a re-deploy the unit is already
# running with the OLD binary, and `enable --now` would leave it running.
# restart swaps to the just-installed binary (and starts it if stopped).
systemctl restart kanade-agent.service

echo
echo "==> Done. Checks:"
echo "    systemctl status kanade-agent"
echo "    journalctl -u kanade-agent -f"
echo "    # the agent should appear in the SPA fleet as this host's name"

#!/usr/bin/env bash
# Install a kanade AGENT as a launchd daemon FROM A LOCAL BUNDLE — no
# network access required. The usual source is the backend's installer
# tarball (`GET /api/agents/installer?os=macos&arch=…`, or the SPA's Agent
# Install page one-liner), which ships this script as
# `setup-agent-macos.sh` next to the layout it expects:
#
#   bin/kanade-agent                   the agent binary (thin, per-arch)
#   etc/agent.toml                     the agent config
#   launchd/com.kanade.agent.plist     the LaunchDaemon definition
#
# Run it from the extracted bundle root:
#
#   sudo KANADE_NATS_TOKEN=<the deployment's token> bash ./setup-agent-macos.sh
#
#   # Override the broker baked into etc/agent.toml:
#   sudo KANADE_NATS_URL=wss://nats.kanade.example.com \
#        KANADE_NATS_TOKEN=<the deployment's token> bash ./setup-agent-macos.sh
#
# macOS counterpart of deploy/linux/setup-agent.sh. Written for the bash
# 3.2 macOS ships (no bash 4 features) and BSD userland (no `sed -i`, no
# useradd — the agent runs as root, like LocalSystem / the systemd unit).
#
# Idempotent: re-running keeps an existing /etc/kanade/agent.env (so the
# token is stable) and the deployed broker URL, overwrites the binary +
# config + plist, and RESTARTS the daemon so a re-deploy actually swaps
# the running binary.
set -euo pipefail

[ "$(id -u)" -eq 0 ] || { echo "run as root (sudo)" >&2; exit 1; }
[ "$(uname -s)" = "Darwin" ] || { echo "this installer is for macOS — use deploy/linux/setup-agent.sh on Linux" >&2; exit 1; }

label="com.kanade.agent"
plist_dst="/Library/LaunchDaemons/${label}.plist"

# The bundle root is this script's directory. Everything is installed
# from here; nothing is downloaded.
bundle="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "==> Verifying bundle contents"
for f in bin/kanade-agent etc/agent.toml "launchd/${label}.plist"; do
	[ -e "$bundle/$f" ] || { echo "bundle is missing $f — re-download the macOS installer" >&2; exit 1; }
done

# Resolve and validate every input BEFORE the running agent is booted
# out below: a redeploy with a bad KANADE_NATS_URL or no resolvable token
# must fail with the old agent still running, not leave the box without
# one.
echo "==> Resolving broker URL and token"
# Preserve an existing deployment's broker across a redeploy: capture the
# current nats_url BEFORE the config is overwritten, so an agent deployed
# once with KANADE_NATS_URL doesn't silently revert to the bundle's
# default when re-run without it. An explicit KANADE_NATS_URL still wins.
prev_url=""
[ -f /etc/kanade/agent.toml ] && \
	prev_url="$(sed -n "s/^ *nats_url *= *['\"]\\(.*\\)['\"].*/\\1/p" /etc/kanade/agent.toml | head -n1)"
url="${KANADE_NATS_URL:-$prev_url}"
# Reject characters that would break the TOML single-quoted literal or
# be mangled by awk's -v backslash processing.
case "$url" in
	*"'"*) echo "KANADE_NATS_URL must not contain a single quote" >&2; exit 1 ;;
	*'"'*) echo "KANADE_NATS_URL must not contain a double quote" >&2; exit 1 ;;
	*'\'*) echo "KANADE_NATS_URL must not contain a backslash" >&2; exit 1 ;;
esac
# Resolve the NATS token, in priority order:
#   1. an explicit KANADE_NATS_TOKEN (fresh install / override)
#   2. an already-installed /etc/kanade/agent.env (idempotent re-run)
# Anything else is a hard error — never fall back to a dev/placeholder
# token (#1172 floor).
if [ -n "${KANADE_NATS_TOKEN:-}" ]; then
	token="$KANADE_NATS_TOKEN"
elif [ -f /etc/kanade/agent.env ]; then
	token="$(sed -n 's/^KANADE_NATS_TOKEN=//p' /etc/kanade/agent.env | head -n1)"
	echo "    keeping the existing /etc/kanade/agent.env token"
else
	echo "no NATS token: set KANADE_NATS_TOKEN=... (or re-run on a box that already has /etc/kanade/agent.env)" >&2
	exit 1
fi
[ -n "$token" ] || { echo "resolved an empty NATS token — aborting" >&2; exit 1; }
case "$token" in
	*"
"*) echo "the NATS token must not contain a newline" >&2; exit 1 ;;
esac

echo "==> Stopping any running agent"
# bootout before touching files so a re-deploy never races the old
# process. "Not loaded" on a fresh install is expected — ignore it.
launchctl bootout "system/${label}" 2>/dev/null || true
# bootout returns before the service is fully torn down; bootstrapping
# while it still exists fails with "Bootstrap failed: 5". Wait it out.
i=0
while launchctl print "system/${label}" >/dev/null 2>&1; do
	i=$((i + 1))
	[ "$i" -le 20 ] || { echo "the previous ${label} is still loaded after 10s — aborting" >&2; exit 1; }
	sleep 0.5
done

echo "==> Creating directories"
# No `kanade` service account on macOS: the agent runs as root, so
# everything it reads or writes is root:wheel.
install -d -o root -g wheel -m 0755 /etc/kanade /var/log/kanade /usr/local/bin
# Root-only data dir (0700), same as Linux: agent state is not for other
# local accounts to read or tamper with.
install -d -o root -g wheel -m 0700 /var/lib/kanade-agent

echo "==> Installing agent binary (from bundle, offline)"
install -o root -g wheel -m 0755 "$bundle/bin/kanade-agent" /usr/local/bin/kanade-agent
# A browser-downloaded (or AirDropped) bundle carries the quarantine
# xattr, which would get the binary blocked on first exec. Harmless when
# absent (curl never sets it).
xattr -d com.apple.quarantine /usr/local/bin/kanade-agent 2>/dev/null || true

echo "==> Agent config (/etc/kanade/agent.toml)"
# agent.toml templates its non-Windows paths via teravars is_windows() at
# startup and ships the broker the backend baked in; $url (resolved and
# validated above) overrides it.
#
# Root-owned: a config another account could rewrite would let it
# redirect the root agent to an attacker-controlled broker.
install -o root -g wheel -m 0644 "$bundle/etc/agent.toml" /etc/kanade/agent.toml
if [ -n "$url" ]; then
	# awk with -v (not `sed s///`, and never BSD `sed -i ''`): the URL is
	# passed as data, so `&` (from query params) and `|` are never treated
	# as replacement metacharacters. `cat >` back into the file keeps its
	# root:wheel 0644.
	tmpf="$(mktemp)"
	awk -v url="$url" -v q="'" '
		$0 ~ /^[[:space:]]*nats_url[[:space:]]*=/ { print "nats_url = " q url q; next }
		{ print }
	' /etc/kanade/agent.toml > "$tmpf" && cat "$tmpf" > /etc/kanade/agent.toml
	rm -f "$tmpf"
	echo "    nats_url -> ${url}"
fi

echo "==> Token (/etc/kanade/agent.env — root-only)"
# launchd has no EnvironmentFile, and the plist is world-readable (0644 is
# mandatory for a LaunchDaemon), so the token must NOT go into the plist.
# It lives here, 0600 root:wheel; the plist's launcher reads it at start.
umask 077
printf 'KANADE_NATS_TOKEN=%s\n' "$token" > /etc/kanade/agent.env
chown root:wheel /etc/kanade/agent.env
chmod 0600 /etc/kanade/agent.env
umask 022

echo "==> LaunchDaemon (${plist_dst})"
# launchd refuses a daemon plist that isn't root-owned and non-writable
# by group/other.
install -o root -g wheel -m 0644 "$bundle/launchd/${label}.plist" "$plist_dst"

echo "==> Enabling + (re)starting the agent"
# enable BEFORE bootstrap: a label left disabled (e.g. by a manual
# `launchctl disable`) fails to bootstrap with "Service is disabled".
launchctl enable "system/${label}"
launchctl bootstrap system "$plist_dst"
# RunAtLoad already started it; kickstart -k guarantees the process now
# running is the just-installed binary even if the bootout above was a
# no-op for some reason.
launchctl kickstart -k "system/${label}"

echo
echo "==> Done. Checks:"
echo "    sudo launchctl print system/${label}"
echo "    tail -f /var/log/kanade/agent.*.log"
echo "    # startup failures before logging is up land in /var/log/kanade/kanade-agent.launchd.log"
echo "    # the agent should appear in the SPA fleet as this host's name"

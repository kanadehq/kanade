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
#   # Command signing (optional): trust the backend's signing key, and refuse
#   # commands that do not verify against it:
#   sudo KANADE_COMMAND_KEYS='[{"kid":"backend-1","public_key":"<base64>"}]' \
#        KANADE_REQUIRE_SIGNED_COMMANDS=1 bash ./setup-agent-macos.sh
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

# Command-signing keyring + enforcement (optional). Validated here, before
# anything on the box is stopped or changed, so a bad ring fails the run with
# the old agent still in place.
#
#   KANADE_COMMAND_KEYS                 JSON array of {kid, public_key[, label,
#                                       max_age_secs, audit_every_use]}; replaces
#                                       the host's keyring. Unset keeps the
#                                       existing one.
#   KANADE_REQUIRE_SIGNED_COMMANDS      1 = refuse unsigned/unverified commands
#                                       (needs a non-empty keyring), 0 = stop
#                                       enforcing, unset = keep as is.
#
# Both land in root-only 0600 files the agent reads (command-keys.json and
# require-signed-commands) — local config, deliberately not something a NATS
# message or the agent_config KV can change.
keys_file=/etc/kanade/command-keys.json
enforce_file=/etc/kanade/require-signed-commands
# A browser-downloaded (or AirDropped) bundle carries the quarantine xattr,
# which would block the bundled binary on first exec — and it is run below to
# validate the keyring, before it is installed.
xattr -d com.apple.quarantine "$bundle/bin/kanade-agent" 2>/dev/null || true
case "${KANADE_REQUIRE_SIGNED_COMMANDS:-}" in
	""|0|1) ;;
	*) echo "KANADE_REQUIRE_SIGNED_COMMANDS must be 1 (enforce), 0 (stop enforcing) or unset (keep the current setting)" >&2; exit 1 ;;
esac
keys_stage=""
trap '[ -z "$keys_stage" ] || rm -f "$keys_stage"' EXIT
# Validation is the agent's own parser (same strict rules it applies at run
# time, plus the deploy-agent.ps1 checks: JSON array, non-empty, string kid +
# public_key on every entry, no duplicate kid) so nothing can be accepted here
# that the agent would then reject, and no jq is needed.
#
# Run the bundled binary in place when it is executable, so a noexec temp
# directory is never in the way; otherwise from a private executable copy, so a
# bundle extracted from a tar that lost the exec bit still validates. Exit 3 is
# "ring rejected"; any other failure means the checker itself could not run (an
# older agent without the flag, a binary for another architecture, or no place
# to exec from), which is reported as that rather than as a bad ring.
check_keys() {
	if [ -x "$bundle/bin/kanade-agent" ]; then
		"$bundle/bin/kanade-agent" --check-command-keys "$1"
		return $?
	fi
	_chk="$(umask 077; mktemp "${TMPDIR:-/tmp}/kanade-agent-check.XXXXXX")"
	_rc=0
	{ cp "$bundle/bin/kanade-agent" "$_chk" && chmod 0700 "$_chk" && "$_chk" --check-command-keys "$1"; } || _rc=$?
	rm -f "$_chk"
	return "$_rc"
}
check_failed() {
	if [ "$1" -eq 3 ]; then
		echo "$2 rejected — nothing was changed" >&2
	else
		echo "could not run the bundled agent's keyring check (exit $1): its kanade-agent is probably older than this script or built for another architecture — rebuild the bundle, or make bin/kanade-agent executable and re-run. Nothing was changed" >&2
	fi
	exit 1
}
if [ -n "${KANADE_COMMAND_KEYS:-}" ]; then
	keys_stage="$(umask 077; mktemp "${TMPDIR:-/tmp}/kanade-command-keys.XXXXXX")"
	printf '%s\n' "$KANADE_COMMAND_KEYS" > "$keys_stage"
	keys_kids="$(check_keys "$keys_stage")" || check_failed "$?" "KANADE_COMMAND_KEYS"
elif [ -f "$keys_file" ] && [ "${KANADE_REQUIRE_SIGNED_COMMANDS:-}" = "1" ]; then
	check_keys "$keys_file" >/dev/null || check_failed "$?" "the existing $keys_file (pass KANADE_COMMAND_KEYS to replace it)"
fi
if [ "${KANADE_REQUIRE_SIGNED_COMMANDS:-}" = "1" ] && [ -z "$keys_stage" ] && [ ! -f "$keys_file" ]; then
	echo "KANADE_REQUIRE_SIGNED_COMMANDS=1 but this host has no keyring (and none was passed). Enforcing with an empty ring is inert — the agent declines to enforce — so pass KANADE_COMMAND_KEYS too." >&2
	exit 1
fi

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

echo "==> Command-signing keyring (root-only)"
# Write to a temp file beside the target, fix owner/mode, then rename: the
# agent re-reads these while running, and a rename means it never sees a
# half-written ring (which it would reject and keep its old one, but there is
# no reason to offer it the chance).
put_root_file() {
	_tmp="$(umask 077; mktemp /etc/kanade/.kanade-cfg.XXXXXX)"
	cat "$1" > "$_tmp"
	chown root:wheel "$_tmp"
	chmod 0600 "$_tmp"
	mv -f "$_tmp" "$2"
}
if [ -n "$keys_stage" ]; then
	put_root_file "$keys_stage" "$keys_file"
	echo "    $keys_file provisioned, kids: $keys_kids"
elif [ -f "$keys_file" ]; then
	echo "    keeping the existing $keys_file"
else
	echo "    none provisioned (pass KANADE_COMMAND_KEYS to enable signature verification)"
fi
case "${KANADE_REQUIRE_SIGNED_COMMANDS:-}" in
	1)
		_one="$(umask 077; mktemp "${TMPDIR:-/tmp}/kanade-enforce.XXXXXX")"
		echo 1 > "$_one"
		put_root_file "$_one" "$enforce_file"
		rm -f "$_one"
		echo "    enforcing: unsigned/unverified commands will be refused"
		;;
	0)
		rm -f "$enforce_file"
		echo "    enforcement off"
		;;
esac

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

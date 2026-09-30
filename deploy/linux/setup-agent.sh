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
#   # Command signing (optional): trust the backend's signing key, and refuse
#   # commands that do not verify against it:
#   sudo KANADE_COMMAND_KEYS='[{"kid":"backend-1","public_key":"<base64>"}]' \
#        KANADE_REQUIRE_SIGNED_COMMANDS=1 bash ./setup-agent.sh
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
check_keys() {
	"$bundle/bin/kanade-agent" --check-command-keys "$1"
}
if [ -n "${KANADE_COMMAND_KEYS:-}" ]; then
	keys_stage="$(umask 077; mktemp "${TMPDIR:-/tmp}/kanade-command-keys.XXXXXX")"
	printf '%s\n' "$KANADE_COMMAND_KEYS" > "$keys_stage"
	keys_kids="$(check_keys "$keys_stage")" || { echo "KANADE_COMMAND_KEYS rejected — nothing was changed" >&2; exit 1; }
elif [ -f "$keys_file" ] && [ "${KANADE_REQUIRE_SIGNED_COMMANDS:-}" = "1" ]; then
	check_keys "$keys_file" >/dev/null || { echo "the existing $keys_file is not a valid keyring — pass KANADE_COMMAND_KEYS to replace it; nothing was changed" >&2; exit 1; }
fi
if [ "${KANADE_REQUIRE_SIGNED_COMMANDS:-}" = "1" ] && [ -z "$keys_stage" ] && [ ! -f "$keys_file" ]; then
	echo "KANADE_REQUIRE_SIGNED_COMMANDS=1 but this host has no keyring (and none was passed). Enforcing with an empty ring is inert — the agent declines to enforce — so pass KANADE_COMMAND_KEYS too." >&2
	exit 1
fi

echo "==> Creating user and directories"
# Reuse the `kanade` service user if a backend already made it; the agent
# itself runs as root (see the unit), but the data/log dirs are owned by
# kanade for parity with the backend's layout.
id -u kanade >/dev/null 2>&1 || useradd --system --home /var/lib/kanade --shell /usr/sbin/nologin kanade
install -d -o kanade -g kanade /var/log/kanade
# /etc/kanade is root-owned: the keyring and enforcement files in it are the
# agent's trust root, and a directory the shared `kanade` account owns would
# let it delete or replace them (renaming needs directory write, not file
# write). A redeploy also puts this back if an earlier install left it
# kanade-owned.
install -d -o root -g root -m 0755 /etc/kanade
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

echo "==> Command-signing keyring (root-only)"
# Write to a temp file beside the target, fix owner/mode, then rename: the
# agent re-reads these while running, and a rename means it never sees a
# half-written ring (which it would reject and keep its old one, but there is
# no reason to offer it the chance).
put_root_file() {
	_tmp="$(umask 077; mktemp /etc/kanade/.kanade-cfg.XXXXXX)"
	cat "$1" > "$_tmp"
	chown root:root "$_tmp"
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

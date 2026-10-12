#!/usr/bin/env bash
# Mint the bcrypt hash that the broker's users configuration takes in place of
# a plaintext password, without echoing the password, putting it on a command
# line, or writing it to a file.
#
#   scripts/ops/nats-password-hash.sh [cost]      # default 8; prompts twice, prints the hash
#
# Only the hash goes to stdout; the plaintext stays with the role's own hosts,
# where deploy-agent.ps1 / deploy-backend.ps1 write it. It wraps
# `nats server passwd` (the nats CLI, https://github.com/nats-io/natscli): that
# is the tool nats-server's own documentation points at, so no hashing
# dependency is added here. The CLI only prompts when its output is a
# terminal, which would defeat capturing the hash, so this script does the
# silent prompt itself and hands the password over in the child's environment
# (`PASSWORD`), which is neither in shell history nor in the process argument
# list. The CLI refuses passwords under 10 characters.
#
# Cost 8 is recommended with a long random password; costs below 10 warn
# about short or obviously non-random inputs, without adding a refusal.
# See book/src/operations/nats-user-switch.md for broker burst measurements.
# The heuristic cannot prove randomness or guarantee password strength.
#
# The result is checked to be a `$2a$` bcrypt hash, the only form nats-server
# recognises as one. Feed it to deploy-nats.ps1 -AgentPasswordHash ... or to
# setup.sh as KANADE_NATS_<ROLE>_PASSWORD_HASH.
set -euo pipefail

cost="${1:-8}"
case "$cost" in
	''|*[!0-9]*) echo "cost must be a number (bcrypt cost, default 8)" >&2; exit 2 ;;
esac
command -v nats >/dev/null 2>&1 || {
	echo "the nats CLI is required (it provides 'nats server passwd'):" >&2
	echo "  https://github.com/nats-io/natscli/releases  (or: go install github.com/nats-io/natscli/nats@latest)" >&2
	exit 1
}

read -rsp "Password: " pw1 >&2 || true; echo >&2
read -rsp "Again:    " pw2 >&2 || true; echo >&2
if [ -z "$pw1" ] || [ "$pw1" != "$pw2" ]; then
	echo "passwords are empty or do not match" >&2
	exit 1
fi

# Strip leading zeroes before the numeric comparison (avoid octal parsing).
normalized_cost="$cost"
while [[ "$normalized_cost" == 0* && ${#normalized_cost} -gt 1 ]]; do
	normalized_cost="${normalized_cost#0}"
done
weak_run='[Pp][Aa][Ss][Ss][Ww][Oo][Rr][Dd]|[Qq][Ww][Ee][Rr][Tt][Yy]|[Ll][Ee][Tt][Mm][Ee][Ii][Nn]|[Ww][Ee][Ll][Cc][Oo][Mm][Ee]|[Aa][Dd][Mm][Ii][Nn]|[Kk][Aa][Nn][Aa][Dd][Ee]'
if [[ ${#normalized_cost} -eq 1 ]] && (( normalized_cost < 10 )) &&
	[[ ${#pw1} -lt 24 || "$pw1" =~ ^[a-zA-Z]+$ || "$pw1" =~ ^[0-9]+$ || "$pw1" =~ $weak_run ]]; then
	echo "warning: a low bcrypt cost is only appropriate for long random passwords (at least 24 characters). Mint a random password, for example:" >&2
	echo '  openssl rand -base64 36 | tr -dc A-Za-z0-9 | head -c 32' >&2
fi

hash="$(PASSWORD="$pw1" nats server passwd --cost "$cost")"
unset pw1 pw2
re='^\$2a\$[0-9]{2}\$[./A-Za-z0-9]{53}$'
[[ "$hash" =~ $re ]] || { echo "nats server passwd did not return a bcrypt hash" >&2; exit 1; }
printf '%s\n' "$hash"

#!/usr/bin/env bash
# Mint the bcrypt hash that the broker's users configuration takes in place of
# a plaintext password, without echoing the password, putting it on a command
# line, or writing it to a file.
#
#   scripts/ops/nats-password-hash.sh [cost]      # prompts twice, prints the hash
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
# The result is checked to be a `$2a$` bcrypt hash, the only form nats-server
# recognises as one. Feed it to deploy-nats.ps1 -AgentPasswordHash ... or to
# setup.sh as KANADE_NATS_<ROLE>_PASSWORD_HASH.
set -euo pipefail

cost="${1:-11}"
case "$cost" in
	''|*[!0-9]*) echo "cost must be a number (bcrypt cost, default 11)" >&2; exit 2 ;;
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

hash="$(PASSWORD="$pw1" nats server passwd --cost "$cost")"
unset pw1 pw2
re='^\$2a\$[0-9]{2}\$[./A-Za-z0-9]{53}$'
[[ "$hash" =~ $re ]] || { echo "nats server passwd did not return a bcrypt hash" >&2; exit 1; }
printf '%s\n' "$hash"

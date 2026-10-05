#!/usr/bin/env bash
# Install a single-VM kanade deployment FROM A LOCAL BUNDLE — no network
# access required (closed-network friendly). Assemble the bundle on a
# machine with internet using bundle.sh, copy it to the server, extract,
# then run this from the extracted bundle root:
#
#   sudo KANADE_DOMAIN=kanade.example.com ./setup.sh
#
# Optional: give the backend its own NATS user beside the generated token
# (both or neither; the token stays and the client chooses between them):
#
#   sudo KANADE_DOMAIN=kanade.example.com \
#        KANADE_NATS_USER=<backend-user> KANADE_NATS_PASSWORD=<backend-password> \
#        ./setup.sh
#
# It is written to /etc/kanade/kanade.env only -- never nats.env, which the
# broker process reads. Re-running without them leaves an installed pair
# alone; new values replace both halves. Nothing here switches the broker
# from the token to users.
#
# Optional, explicit: run the broker on the three role users (agent, backend,
# breakglass) instead of the single shared token. Nothing selects this unless
# asked, and what was asked is remembered:
#
#   sudo KANADE_DOMAIN=kanade.example.com KANADE_NATS_AUTH_MODE=users \
#        KANADE_NATS_AGENT_PASSWORD_HASH='$2a$...' \
#        KANADE_NATS_BACKEND_PASSWORD_HASH='$2a$...' \
#        KANADE_NATS_BREAKGLASS_PASSWORD_HASH='$2a$...' \
#        ./setup.sh
#
# KANADE_NATS_AUTH_MODE is `token` (the default, today's configuration) or
# `users`. The mode is recorded in /etc/kanade/nats-auth-mode (root only), so
# a later plain re-run keeps installing the recorded configuration instead of
# silently reverting it; setting the variable again changes it, and
# KANADE_NATS_AUTH_MODE=token is the revert. `users` installs the token
# config with its `authorization` block replaced by an include of
# /etc/kanade/nats-server.users.conf (the shipped template). The three values
# are bcrypt hashes (scripts/ops/nats-password-hash.sh mints one), never
# plaintext: they go into nats.env, the broker's own env file, mode 0600,
# next to the token and nowhere else. Once recorded, a re-run reuses the
# hashes already in nats.env, so they need not be supplied again. Anything
# that is not a `$2a$` bcrypt hash is refused before a file changes.
#
# WARNING: the switch is atomic. A config cannot carry both a token and
# users, and a token is rejected once users exist, so every agent, backend
# and CLI host must ALREADY hold a user pair before the broker flips (see the
# readiness procedure in the NATS operations chapter of the book).
#
# Restart behaviour is unchanged: this script enables the unit with
# `enable --now`, which does not restart a broker that is already running.
# After a mode change the new configuration takes effect only when you run
# `systemctl restart nats-server` yourself; the end of the run says so.
#
# This mirrors the Windows model: CI/build produces the artifacts, the
# target only installs them — it never builds or fetches.
#
# Idempotent-ish: re-running keeps an existing /etc/kanade/kanade.env
# (so secrets are stable) and overwrites config + unit files.
set -euo pipefail

: "${KANADE_DOMAIN:?set KANADE_DOMAIN=your.domain (A records for it AND nats.<domain> must point here)}"
[ "$(id -u)" -eq 0 ] || { echo "run as root" >&2; exit 1; }

# Validate the domain as a DNS hostname before it is templated into the
# Caddyfile and backend.toml. Anything else (spaces, sed delimiters,
# control chars) would corrupt those files.
if ! printf '%s' "$KANADE_DOMAIN" \
	| grep -Eq '^([a-zA-Z0-9]([a-zA-Z0-9-]{0,61}[a-zA-Z0-9])?\.)+[a-zA-Z]{2,}$'; then
	echo "KANADE_DOMAIN='${KANADE_DOMAIN}' is not a valid DNS hostname." >&2
	exit 1
fi

# >>> nats-user
nats_user="${KANADE_NATS_USER:-}"
nats_pass="${KANADE_NATS_PASSWORD:-}"
# systemd parses EnvironmentFile values itself, so a raw `'`, `"` or `\` in a
# password would be read as quoting. Written inside double quotes, only the
# four characters `\ " $` and the backtick are special, so those are
# backslash-escaped; everything else (spaces, single quotes) stays literal.
env_quote() {
	printf '%s' "$1" | sed -e 's/[\\"$`]/\\&/g'
}
# Both or neither, checked before anything is written: the client treats a
# half pair as a configuration error.
if [ -n "$nats_user" ] || [ -n "$nats_pass" ]; then
	if [ -z "$nats_user" ] || [ -z "$nats_pass" ]; then
		echo "KANADE_NATS_USER and KANADE_NATS_PASSWORD must be set together (or neither) — nothing was changed" >&2
		exit 1
	fi
	case "$nats_user$nats_pass" in
		*$'\n'*|*$'\r'*) echo "KANADE_NATS_USER / KANADE_NATS_PASSWORD must not contain a line break — nothing was changed" >&2; exit 1 ;;
	esac
fi
# <<< nats-user

# >>> nats-auth-input
# Which broker configuration to install. Precedence: the variable, then what
# an earlier run recorded, then `token`. All of it is validated here, before
# any file changes.
auth_mode_file=/etc/kanade/nats-auth-mode
recorded_mode=""
if [ -f "$auth_mode_file" ]; then
	recorded_mode="$(head -n 1 "$auth_mode_file")"
fi
auth_mode_set="${KANADE_NATS_AUTH_MODE:-}"
auth_mode="${auth_mode_set:-${recorded_mode:-token}}"
case "$auth_mode" in
	users|token) ;;
	*) echo "KANADE_NATS_AUTH_MODE / ${auth_mode_file} must be 'users' or 'token' — nothing was changed" >&2; exit 1 ;;
esac
# The format nats-server treats as a hash is `$2a$` only; `$2b$` / `$2y$` would
# be read as a plaintext password equal to the hash text. Quotes, backslashes
# and line breaks are outside the character set, so nothing needs escaping.
hash_re='^\$2a\$[0-9]{2}\$[./A-Za-z0-9]{53}$'
nats_env_hash() { # ROLE: the hash already in nats.env, if any
	[ -f /etc/kanade/nats.env ] || return 0
	sed -n "s/^KANADE_NATS_$1_PASSWORD_HASH='\"\(.*\)\"'\$/\1/p" /etc/kanade/nats.env | head -n 1
}
if [ "$auth_mode" = users ]; then
	for role in AGENT BACKEND BREAKGLASS; do
		var="KANADE_NATS_${role}_PASSWORD_HASH"
		val="${!var:-}"
		if [ -n "$val" ]; then
			[[ "$val" =~ $hash_re ]] || { echo "$var is not a bcrypt \$2a\$ hash (mint one with scripts/ops/nats-password-hash.sh; never pass the plaintext) — nothing was changed" >&2; exit 1; }
		else
			val="$(nats_env_hash "$role")"
			[[ "$val" =~ $hash_re ]] || { echo "users mode needs $var (no usable hash is recorded in /etc/kanade/nats.env yet) — nothing was changed" >&2; exit 1; }
		fi
		printf -v "nats_hash_$role" '%s' "$val"
	done
fi
# <<< nats-auth-input

# The bundle root is this script's directory. Everything is installed from
# here; nothing is downloaded.
bundle="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "==> Verifying bundle contents"
for f in bin/kanade-backend bin/nats-server bin/caddy \
	etc/nats-server.conf etc/Caddyfile etc/backend.toml \
	systemd/kanade-backend.service systemd/nats-server.service systemd/caddy.service; do
	[ -e "$bundle/$f" ] || { echo "bundle is missing $f — rebuild it with bundle.sh" >&2; exit 1; }
done

if [ "$auth_mode" = users ] && [ ! -e "$bundle/etc/nats-server.users.conf" ]; then
	echo "bundle is missing etc/nats-server.users.conf — rebuild it with bundle.sh" >&2
	exit 1
fi

echo "==> Creating users and directories"
id -u kanade >/dev/null 2>&1 || useradd --system --home /var/lib/kanade --shell /usr/sbin/nologin kanade
id -u caddy  >/dev/null 2>&1 || useradd --system --home /var/lib/caddy  --shell /usr/sbin/nologin caddy
install -d -o kanade -g kanade /var/lib/kanade /var/lib/kanade/nats/jetstream /var/log/kanade
# Root-owned so a co-located agent's keyring / enforcement files in here cannot
# be deleted or replaced by the shared `kanade` account.
install -d -o root -g root -m 0755 /etc/kanade
install -d -o caddy  -g caddy  /var/lib/caddy
install -d /etc/caddy

echo "==> Installing binaries (from bundle, offline)"
install -m 0755 "$bundle/bin/kanade-backend" /usr/local/bin/kanade-backend
install -m 0755 "$bundle/bin/nats-server"    /usr/local/bin/nats-server
install -m 0755 "$bundle/bin/caddy"          /usr/local/bin/caddy

echo "==> Backend config (/etc/kanade/backend.toml)"
# backend.toml already templates Linux paths via teravars is_windows(); we
# (a) bind the backend to localhost so Caddy is the only public surface,
# and (b) set public_url so email links + the forgot-password path use the
# real domain (Host-header hardening).
install -o kanade -g kanade -m 0644 "$bundle/etc/backend.toml" /etc/kanade/backend.toml
# Bind loopback only. The committed default is 0.0.0.0:8080, which would be
# reachable outside Caddy (bypassing TLS) on any host whose firewall lets
# :8080 through — do not depend on the cloud firewall alone.
sed -i "s|^\( *bind *= *\).*|\1'127.0.0.1:8080'|" /etc/kanade/backend.toml
if grep -q '^# *public_url' /etc/kanade/backend.toml; then
	sed -i "s|^# *public_url.*|public_url = 'https://${KANADE_DOMAIN}'|" /etc/kanade/backend.toml
elif ! grep -q '^public_url' /etc/kanade/backend.toml; then
	sed -i "/^\[server\]/a public_url = 'https://${KANADE_DOMAIN}'" /etc/kanade/backend.toml
fi

echo "==> Secrets — generated once, kept on re-run"
# Least privilege: nats-server only ever needs its token, so it gets its
# own env file (nats.env). The backend's fuller secret set — JWT secret,
# static token, bootstrap admin password — lives in kanade.env and is
# never handed to the broker process. Both files carry the SAME token
# value, generated once here.
if [ ! -f /etc/kanade/kanade.env ]; then
	gen() { head -c 32 /dev/urandom | base64 | tr -d '\n/+=' | cut -c1-40; }
	nats_token="$(gen)"
	admin_pw="$(gen)"
	umask 077

	cat > /etc/kanade/nats.env <<EOF
# The broker's only secret. Never the placeholder/dev token (#1172 floor).
KANADE_NATS_TOKEN=${nats_token}
EOF
	chown kanade:kanade /etc/kanade/nats.env
	chmod 0600 /etc/kanade/nats.env

	cat > /etc/kanade/kanade.env <<EOF
# Backend secrets. Same NATS token as nats.env (the backend connects to
# the broker too); plus secrets the broker must NOT see.
KANADE_NATS_TOKEN=${nats_token}
# REQUIRED — without it the backend uses an insecure hard-coded JWT
# fallback and anyone can forge admin tokens (auth.rs).
KANADE_JWT_SECRET=$(gen)
KANADE_AUTH_STATIC_TOKEN=$(gen)
KANADE_BOOTSTRAP_ADMIN_USER=admin
KANADE_BOOTSTRAP_ADMIN_PASSWORD=${admin_pw}
EOF
	chown kanade:kanade /etc/kanade/kanade.env
	chmod 0600 /etc/kanade/kanade.env
	# Do NOT echo the password: console / cloud-init / CI logs would retain
	# it, defeating the 0600 file. Point the operator at the protected file.
	echo "    Generated. Bootstrap admin user: admin"
	echo "    Password is in /etc/kanade/kanade.env (root-only):"
	echo "      sudo sed -n 's/^KANADE_BOOTSTRAP_ADMIN_PASSWORD=//p' /etc/kanade/kanade.env"
else
	echo "    Keeping existing /etc/kanade/kanade.env and nats.env"
fi

# The backend's own NATS user. Only kanade.env: nats.env is the broker's file
# and must never carry a backend credential. Any earlier pair is dropped and
# the new one appended in one rename, so both halves change together.
# >>> backend-env
env_stage=""
conf_stage=""
trap '[ -z "$env_stage" ] || rm -f "$env_stage"; [ -z "$conf_stage" ] || rm -rf "$conf_stage"' EXIT
if [ -n "$nats_user" ]; then
	env_stage="$(umask 077; mktemp /etc/kanade/.kanade-env.XXXXXX)"
	{
		grep -Ev '^KANADE_NATS_(USER|PASSWORD)=' /etc/kanade/kanade.env || true
		printf 'KANADE_NATS_USER="%s"\n' "$(env_quote "$nats_user")"
		printf 'KANADE_NATS_PASSWORD="%s"\n' "$(env_quote "$nats_pass")"
	} > "$env_stage"
	chown kanade:kanade "$env_stage"
	chmod 0600 "$env_stage"
	mv -f "$env_stage" /etc/kanade/kanade.env
	env_stage=""
	echo "    backend NATS user written to /etc/kanade/kanade.env"
fi
# <<< backend-env

echo "==> NATS config + Caddyfile + systemd units (from bundle)"
# >>> nats-auth-install
# The recorded mode, not the script, decides which broker config is installed,
# so a re-run no longer puts the token configuration back over a users one.
if [ "$auth_mode" = users ]; then
	# The shipped token config with its `authorization` block (a line starting
	# `authorization {` up to the first line that is a bare `}`) replaced by an
	# include of the users template; every other line is copied unchanged.
	conf_stage="$(umask 077; mktemp -d /etc/kanade/.nats-conf.XXXXXX)"
	awk -v inc='include "nats-server.users.conf"' '
		/^authorization[ \t]*\{/ { n++; if (n == 1) { skipping = 1 } }
		skipping { if ($0 ~ /^\}[ \t\r]*$/) { print inc; skipping = 0 } next }
		{ print }
		END { if (n != 1 || skipping) exit 3 }
	' "$bundle/etc/nats-server.conf" > "$conf_stage/nats-server.conf" \
		|| { echo "etc/nats-server.conf has no single authorization block to replace — nothing was changed" >&2; exit 1; }
	cp "$bundle/etc/nats-server.users.conf" "$conf_stage/nats-server.users.conf"
	# The broker's env file: every line but the old hashes is kept (the token
	# above all), the three hashes are appended, mode 0600, in one rename.
	# Double quotes inside single quotes: nats-server parses an environment
	# value as configuration, and an unquoted bcrypt string reads `$2a` as a
	# variable reference.
	env_stage="$(umask 077; mktemp /etc/kanade/.nats-env.XXXXXX)"
	{
		grep -Ev '^KANADE_NATS_(AGENT|BACKEND|BREAKGLASS)_PASSWORD_HASH=' /etc/kanade/nats.env 2>/dev/null || true
		printf "KANADE_NATS_AGENT_PASSWORD_HASH='\"%s\"'\n" "$nats_hash_AGENT"
		printf "KANADE_NATS_BACKEND_PASSWORD_HASH='\"%s\"'\n" "$nats_hash_BACKEND"
		printf "KANADE_NATS_BREAKGLASS_PASSWORD_HASH='\"%s\"'\n" "$nats_hash_BREAKGLASS"
	} > "$env_stage"
	# Let the real broker parse the candidate before anything is replaced; its
	# output is not shown because a parse error can quote the offending line.
	(
		export KANADE_NATS_AGENT_PASSWORD_HASH="\"$nats_hash_AGENT\""
		export KANADE_NATS_BACKEND_PASSWORD_HASH="\"$nats_hash_BACKEND\""
		export KANADE_NATS_BREAKGLASS_PASSWORD_HASH="\"$nats_hash_BREAKGLASS\""
		/usr/local/bin/nats-server -t -c "$conf_stage/nats-server.conf"
	) >/dev/null 2>&1 || { echo "nats-server rejected the users configuration — nothing was changed" >&2; exit 1; }
	chown kanade:kanade "$env_stage"
	chmod 0600 "$env_stage"
	mv -f "$env_stage" /etc/kanade/nats.env
	env_stage=""
	install -o kanade -g kanade -m 0644 "$conf_stage/nats-server.users.conf" /etc/kanade/nats-server.users.conf
	install -o kanade -g kanade -m 0644 "$conf_stage/nats-server.conf" /etc/kanade/nats-server.conf
	rm -rf "$conf_stage"
	conf_stage=""
	echo "    users mode: /etc/kanade/nats-server.conf includes nats-server.users.conf; hashes in nats.env"
else
	install -o kanade -g kanade -m 0644 "$bundle/etc/nats-server.conf" /etc/kanade/nats-server.conf
	# Only an explicit `token` or a recorded `users` earns the removal: an
	# unrecorded file next to a default run was never this script's to delete.
	if [ "$auth_mode_set" = token ] || [ "$recorded_mode" = users ]; then
		rm -f /etc/kanade/nats-server.users.conf
	fi
	# Back to the token: the broker's env file loses the hashes and keeps the
	# rest. Nothing is rewritten when there were none.
	if grep -Eq '^KANADE_NATS_(AGENT|BACKEND|BREAKGLASS)_PASSWORD_HASH=' /etc/kanade/nats.env 2>/dev/null; then
		env_stage="$(umask 077; mktemp /etc/kanade/.nats-env.XXXXXX)"
		grep -Ev '^KANADE_NATS_(AGENT|BACKEND|BREAKGLASS)_PASSWORD_HASH=' /etc/kanade/nats.env > "$env_stage" || true
		chown kanade:kanade "$env_stage"
		chmod 0600 "$env_stage"
		mv -f "$env_stage" /etc/kanade/nats.env
		env_stage=""
	fi
fi
# Record last: an interrupted run is simply repeated. Only written when the
# mode was asked for, so a deployment that never opts in has no such file.
auth_switched=0
if [ -n "$auth_mode_set" ] && [ "$auth_mode_set" != "$recorded_mode" ]; then
	printf '%s\n' "$auth_mode" | (umask 077; cat > "$auth_mode_file.tmp")
	chown root:root "$auth_mode_file.tmp"
	chmod 0600 "$auth_mode_file.tmp"
	mv -f "$auth_mode_file.tmp" "$auth_mode_file"
	[ "$auth_mode" = "${recorded_mode:-token}" ] || auth_switched=1
fi
# <<< nats-auth-install
sed "s|__KANADE_DOMAIN__|${KANADE_DOMAIN}|g" "$bundle/etc/Caddyfile" > /etc/caddy/Caddyfile
# The secret-generation block above set `umask 077`, which persists and would
# make this redirect create the Caddyfile mode 0600 — caddy runs as an
# unprivileged user and would fail to start with "permission denied". The
# Caddyfile carries no secrets (just the domain), so force it world-readable.
chmod 0644 /etc/caddy/Caddyfile
install -m 0644 "$bundle/systemd/nats-server.service"    /etc/systemd/system/nats-server.service
install -m 0644 "$bundle/systemd/kanade-backend.service" /etc/systemd/system/kanade-backend.service
install -m 0644 "$bundle/systemd/caddy.service"          /etc/systemd/system/caddy.service

echo "==> Enabling services"
systemctl daemon-reload
systemctl enable --now nats-server.service
# The binary is always present (verified above), so let a real start
# failure surface rather than swallowing it.
systemctl enable --now kanade-backend.service
systemctl enable --now caddy.service

echo
echo "==> Done. Checks:"
echo "    systemctl status nats-server kanade-backend caddy"
echo "    journalctl -u kanade-backend -f"
echo "    curl https://${KANADE_DOMAIN}/      (SPA; log in as admin)"
echo "    agents connect with: nats_url = wss://nats.${KANADE_DOMAIN}"
if [ "$auth_switched" -eq 1 ]; then
	echo
	echo "    NATS auth mode is now '${auth_mode}'. A running broker keeps its old configuration"
	echo "    until you restart it:  sudo systemctl restart nats-server"
	echo "    (every client must already hold a matching credential — the switch is atomic)"
fi

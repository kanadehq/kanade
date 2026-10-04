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

# The bundle root is this script's directory. Everything is installed from
# here; nothing is downloaded.
bundle="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

echo "==> Verifying bundle contents"
for f in bin/kanade-backend bin/nats-server bin/caddy \
	etc/nats-server.conf etc/Caddyfile etc/backend.toml \
	systemd/kanade-backend.service systemd/nats-server.service systemd/caddy.service; do
	[ -e "$bundle/$f" ] || { echo "bundle is missing $f — rebuild it with bundle.sh" >&2; exit 1; }
done

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
trap '[ -z "$env_stage" ] || rm -f "$env_stage"' EXIT
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
install -o kanade -g kanade -m 0644 "$bundle/etc/nats-server.conf" /etc/kanade/nats-server.conf
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

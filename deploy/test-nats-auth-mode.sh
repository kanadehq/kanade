#!/usr/bin/env bash
# Exercises the opt-in NATS auth mode (token / users) of the Linux setup.sh
# without root, systemd or a network: the marked regions are cut out of the
# real script and run against a scratch directory. When `nats-server` is in
# PATH the installed users configuration is also parsed by it.
#
#   bash deploy/test-nats-auth-mode.sh
set -u

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/.." && pwd)"
setup="$here/linux/setup.sh"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/nats-auth-mode-test.XXXXXX")" || { echo "cannot create a temporary directory" >&2; exit 1; }
[ -n "$tmp" ] && [ -d "$tmp" ] || { echo "cannot create a temporary directory" >&2; exit 1; }
trap 'rm -rf "$tmp"' EXIT
fail=0
check() { # name, ok(0/1)
	if [ "$2" -eq 0 ]; then echo "  PASS  $1"; else echo "  FAIL  $1"; fail=$((fail + 1)); fi
}

ha='$2a$04$hp8sSRHivlHlui1HEcNGee1fLUvKMbC9H3KPoVYuIltSr90aCzoOi'
hb='$2a$04$tPooi64/C8SMMIZ9Dr0i9eAduFcR9jOZ/Wl.upFfo4KdDFRqVLIzO'
hc='$2a$04$JM0AX5q9kVFyZDkRC5R9AuazrRXX8UeHwH8o760Ku5LXkn0OyW9eG'

mkdir -p "$tmp/etc" "$tmp/bundle/etc"
cp "$here/linux/nats-server.conf" "$tmp/bundle/etc/nats-server.conf"
cp "$repo/configs/nats-server.users.conf" "$tmp/bundle/etc/nats-server.users.conf"
nsbin="$(command -v nats-server || true)"
[ -n "$nsbin" ] || { nsbin="$tmp/nats-server-stub"; printf '#!/bin/sh\nexit 0\n' > "$nsbin"; chmod +x "$nsbin"; echo "  note: no nats-server in PATH, the parse step is a stub"; }

# The code between the markers, aimed at the scratch dir instead of /etc.
region() { # name
	sed -n "/^# >>> $1\$/,/^# <<< $1\$/p" "$setup" \
		| sed -e "s#/etc/kanade#$tmp/etc#g" -e "s#/usr/local/bin/nats-server#$nsbin#g" \
		      -e 's/-o kanade -g kanade //' -e 's/chown [a-z]*:[a-z]* /: /'
}
reset() { # fresh etc dir with the broker's env file as the first run leaves it
	rm -rf "$tmp/etc"; mkdir -p "$tmp/etc"
	printf 'KANADE_NATS_TOKEN=tok\n' > "$tmp/etc/nats.env"; chmod 0600 "$tmp/etc/nats.env"
}
run() { # env assignments...: validate then install; exit status is the script's
	( env "$@" bash -c '
		set -euo pipefail
		umask 022
		bundle="$1"; env_stage=""; conf_stage=""
		eval "$(cat "$2")"
		eval "$(cat "$3")"
	' _ "$tmp/bundle" <(region nats-auth-input) <(region nats-auth-install) ) >"$tmp/out" 2>"$tmp/err"
}
snapshot() { ( cd "$tmp/etc" && find . -type f | sort | while read -r f; do printf '%s %s\n' "$f" "$(cksum < "$f")"; done ); }
# GNU first: on Linux `stat -f` is filesystem status and succeeds with other text.
mode_of() { stat -c %a "$1" 2>/dev/null || stat -f %Lp "$1"; }
conf="$tmp/etc/nats-server.conf"; envf="$tmp/etc/nats.env"; modef="$tmp/etc/nats-auth-mode"; usersf="$tmp/etc/nats-server.users.conf"

# 1. Default: byte for byte what the unconditional install used to produce.
reset
run; rc=$?
check "default: exit 0" $rc
cmp -s "$conf" "$tmp/bundle/etc/nats-server.conf"; check "default: the token config is installed unchanged" $?
[ "$(cat "$envf")" = "KANADE_NATS_TOKEN=tok" ]; check "default: nats.env untouched" $?
[ ! -e "$modef" ] && [ ! -e "$usersf" ]; check "default: no mode file, no users file" $?
! grep -q 'restart' "$tmp/out"; check "default: nothing about a restart" $?

# 1b. A users file nobody asked this run about is left alone by a default run.
reset
printf 'operator-owned\n' > "$usersf"
run; rc=$?
[ "$rc" -eq 0 ] && [ "$(cat "$usersf")" = operator-owned ]; check "default: an existing users file is not deleted" $?

# 2. users without the three hashes, or with a malformed one: nothing changes.
reset
before="$(snapshot)"
run KANADE_NATS_AUTH_MODE=users "KANADE_NATS_AGENT_PASSWORD_HASH=$ha"; rc=$?
[ "$rc" -ne 0 ]; check "users with two hashes missing is refused" $?
[ "$before" = "$(snapshot)" ]; check "...and writes nothing" $?
for bad in 'hunter2' '$2y$04$hp8sSRHivlHlui1HEcNGee1fLUvKMbC9H3KPoVYuIltSr90aCzoOi' "$ha\"x" "${ha}\\x" "$(printf '%s\nextra' "$ha")"; do
	run KANADE_NATS_AUTH_MODE=users "KANADE_NATS_AGENT_PASSWORD_HASH=$bad" "KANADE_NATS_BACKEND_PASSWORD_HASH=$hb" "KANADE_NATS_BREAKGLASS_PASSWORD_HASH=$hc"; rc=$?
	[ "$rc" -ne 0 ] && [ "$before" = "$(snapshot)" ] && ! grep -qF -- "$bad" "$tmp/err"; check "malformed hash refused, nothing written, value not echoed: ${bad:0:12}" $?
done
run KANADE_NATS_AUTH_MODE=bogus; rc=$?
[ "$rc" -ne 0 ] && [ "$before" = "$(snapshot)" ]; check "an unknown mode is refused" $?

# 3. users: installed, recorded, hashes only in the broker's env file.
reset
users_env=(KANADE_NATS_AUTH_MODE=users "KANADE_NATS_AGENT_PASSWORD_HASH=$ha" "KANADE_NATS_BACKEND_PASSWORD_HASH=$hb" "KANADE_NATS_BREAKGLASS_PASSWORD_HASH=$hc")
run "${users_env[@]}"; rc=$?
check "users: exit 0" $rc
[ "$(cat "$modef")" = users ] && [ "$(mode_of "$modef")" = 600 ]; check "users: recorded, mode 0600" $?
[ "$(mode_of "$envf")" = 600 ]; check "users: nats.env is mode 0600" $?
grep -q '^KANADE_NATS_TOKEN=tok$' "$envf"; check "users: the token line is kept" $?
[ "$(grep -cE "^KANADE_NATS_[A-Z]+_PASSWORD_HASH='\"[\$]2a[\$]" "$envf")" -eq 3 ]; check "users: three quoted hashes in nats.env" $?
! grep -qE '^KANADE_NATS_(USER|PASSWORD)=' "$envf"; check "users: no plaintext pair or other credential in nats.env" $?
grep -qx 'include "nats-server.users.conf"' "$conf" && ! grep -q '^authorization' "$conf" && ! grep -q 'token:' "$conf"; check "users: main config includes the users file in place of the token block" $?
cmp -s "$usersf" "$tmp/bundle/etc/nats-server.users.conf"; check "users: the template is installed verbatim" $?
# every other line of the main config is the shipped one
diff <(grep -v '^include "nats-server.users.conf"$' "$conf") <(sed '/^authorization {/,/^}/d' "$tmp/bundle/etc/nats-server.conf") >/dev/null; check "users: the rest of the main config is unchanged" $?
if [ -n "$(command -v nats-server)" ]; then
	# the env file read the way systemd hands it to the broker
	( set -a; . "$envf"; set +a; cd "$tmp/etc"; mkdir -p js
	  sed -e "s#/var/lib/kanade/nats/jetstream#$tmp/etc/js#" "$conf" > "$tmp/etc/parse.conf"
	  nats-server -t -c "$tmp/etc/parse.conf" ) >"$tmp/out" 2>&1
	check "users: a real nats-server parses the installed set with nats.env's values" $?
	rm -f "$tmp/etc/parse.conf"
fi

# 4. A plain re-run keeps users (the unconditional overwrite is gone).
snap="$(snapshot)"
run; rc=$?
[ "$rc" -eq 0 ] && grep -qx 'include "nats-server.users.conf"' "$conf" && [ "$(cat "$modef")" = users ]; check "re-run without the variable keeps users" $?
[ "$snap" = "$(snapshot)" ]; check "...and reuses the recorded hashes unchanged" $?
run KANADE_NATS_AUTH_MODE=users; rc=$?
[ "$rc" -eq 0 ] && [ "$snap" = "$(snapshot)" ]; check "users requested again without hashes reuses nats.env" $?

# 5. Back to token.
run KANADE_NATS_AUTH_MODE=token; rc=$?
check "revert: exit 0" $rc
cmp -s "$conf" "$tmp/bundle/etc/nats-server.conf"; check "revert: the token config is restored byte for byte" $?
[ "$(cat "$envf")" = "KANADE_NATS_TOKEN=tok" ] && [ "$(mode_of "$envf")" = 600 ]; check "revert: hashes removed, token kept, still 0600" $?
[ ! -e "$usersf" ] && [ "$(cat "$modef")" = token ]; check "revert: users file gone, token recorded" $?
run; rc=$?
cmp -s "$conf" "$tmp/bundle/etc/nats-server.conf" && [ "$(cat "$modef")" = token ]; check "re-run after the revert stays on token" $?

# 6. The hash helper: only the hash on stdout, the password in the child's
#    environment rather than its argument list, a mismatch refused.
helper="$repo/scripts/ops/nats-password-hash.sh"
mkdir -p "$tmp/stub"
cat > "$tmp/stub/nats" <<STUB
#!/bin/sh
printf '%s\n' "\$*" > "$tmp/stub/argv"
[ -n "\${PASSWORD:-}" ] || exit 1
printf '%s\n' '$ha'
STUB
chmod +x "$tmp/stub/nats"
out="$(printf 'a-long-password-1\na-long-password-1\n' | PATH="$tmp/stub:$PATH" bash "$helper" 2>"$tmp/err")"; rc=$?
[ "$rc" -eq 0 ] && [ "$out" = "$ha" ]; check "helper prints only the hash" $?
! grep -qF 'a-long-password-1' "$tmp/stub/argv" "$tmp/err"; check "helper: the password is not on the command line or in the output" $?
printf 'a-long-password-1\nsomething-else-1\n' | PATH="$tmp/stub:$PATH" bash "$helper" >/dev/null 2>&1; rc=$?
[ "$rc" -ne 0 ]; check "helper refuses a mismatched confirmation" $?
printf 'a-long-password-1\na-long-password-1\n' | PATH="/usr/bin:/bin" bash "$helper" >/dev/null 2>"$tmp/err"; rc=$?
[ "$rc" -ne 0 ] && grep -q natscli "$tmp/err"; check "helper without the nats CLI says where to get it" $?

[ "$fail" -eq 0 ] || { echo "$fail check(s) failed"; exit 1; }
echo "all checks passed"

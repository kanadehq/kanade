#!/usr/bin/env bash
# Exercises the per-role NATS user handling (KANADE_NATS_USER /
# KANADE_NATS_PASSWORD) in the Linux and macOS setup scripts and the macOS
# launchd plist, without root, systemd or launchd: the marked regions are cut
# out of the real scripts and run against a scratch directory.
#
#   bash deploy/test-nats-user.sh
set -u

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/nats-user-test.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT
fail=0
check() { # name, ok(0/1)
	if [ "$2" -eq 0 ]; then echo "  PASS  $1"; else echo "  FAIL  $1"; fail=$((fail + 1)); fi
}
pw="it's a \$HOME \"q\" \\ \`x\` %s end"

# The code between the markers, aimed at the scratch dir instead of /etc.
region() { # file, name
	sed -n "/^# >>> $2\$/,/^# <<< $2\$/p" "$1" | sed "s#/etc/kanade#$tmp/etc#g"
}
# The agent.env writer: the brace group that redirects into agent.env.
writer() {
	sed -n '/^{$/,/^} > \/etc\/kanade\/agent.env$/p' "$1" | sed "s#/etc/kanade#$tmp/etc#g"
}
mkdir -p "$tmp/etc"

# One agent run: validate, then write agent.env. Prints nothing on success;
# the exit status is the script's would-be status.
run_agent() { # script, env assignments...
	local script="$1"; shift
	( set -euo pipefail
	  umask 022
	  env "$@" bash -c '
		set -euo pipefail
		eval "$(cat "$1")"
		token=tok
		umask 077
		eval "$(cat "$2")"
	  ' _ <(region "$script" nats-user) <(writer "$script") ) 2>"$tmp/err"
}

for script in "$here/linux/setup-agent.sh" "$here/macos/setup-agent.sh"; do
	name="$(basename "$(dirname "$script")")"
	envf="$tmp/etc/agent.env"
	rm -f "$envf"

	# neither: byte-for-byte what it wrote before (token line only)
	run_agent "$script"; rc=$?
	check "$name: no user supplied -> exit 0" $rc
	[ "$(cat "$envf")" = "KANADE_NATS_TOKEN=tok" ]; check "$name: no user supplied -> only the token line" $?

	# half pair: refused, nothing written, password not echoed
	rm -f "$envf"
	run_agent "$script" KANADE_NATS_USER=u1; rc=$?
	[ "$rc" -ne 0 ]; check "$name: user only is refused" $?
	[ ! -e "$envf" ]; check "$name: user only writes nothing" $?
	run_agent "$script" "KANADE_NATS_PASSWORD=$pw"; rc=$?
	[ "$rc" -ne 0 ]; check "$name: password only is refused" $?
	[ ! -e "$envf" ]; check "$name: password only writes nothing" $?
	! grep -qF -- "$pw" "$tmp/err"; check "$name: the refusal does not echo the password" $?
	run_agent "$script" "KANADE_NATS_USER=u1" "KANADE_NATS_PASSWORD=$(printf 'a\nb')"; rc=$?
	[ "$rc" -ne 0 ]; check "$name: a line break in the pair is refused" $?

	# both: written beside the token, mode 0600
	run_agent "$script" "KANADE_NATS_USER=agent user" "KANADE_NATS_PASSWORD=$pw"; rc=$?
	check "$name: pair supplied -> exit 0" $rc
	grep -q '^KANADE_NATS_TOKEN=tok$' "$envf"; check "$name: the token is kept beside the pair" $?
	[ "$(stat -f %Lp "$envf" 2>/dev/null || stat -c %a "$envf")" = "600" ]; check "$name: env file is mode 0600" $?
	first="$(cat "$envf")"

	# special characters survive the way each consumer reads the file
	if [ "$name" = linux ]; then
		# systemd's double-quoted form obeys the same escapes as sh
		line="$(grep '^KANADE_NATS_PASSWORD=' "$envf")"
		got="$(sh -c "$line; printf '%s' \"\$KANADE_NATS_PASSWORD\"")"
	else
		# the launcher reads it as data with sed
		got="$(sed -n 's/^KANADE_NATS_PASSWORD=//p' "$envf" | head -n 1)"
	fi
	[ "$got" = "$pw" ]; check "$name: password with ' \$ \" \\ backtick and spaces round-trips" $?
	if [ "$name" = linux ]; then
		uline="$(grep '^KANADE_NATS_USER=' "$envf")"
		gotu="$(sh -c "$uline; printf '%s' \"\$KANADE_NATS_USER\"")"
	else
		gotu="$(sed -n 's/^KANADE_NATS_USER=//p' "$envf")"
	fi
	[ "$gotu" = "agent user" ]; check "$name: user with a space round-trips" $?

	# same values again: identical file
	run_agent "$script" "KANADE_NATS_USER=agent user" "KANADE_NATS_PASSWORD=$pw"
	[ "$(cat "$envf")" = "$first" ]; check "$name: same values twice is idempotent" $?

	# no values: the pair already there is carried over
	run_agent "$script"; rc=$?
	check "$name: re-run without values -> exit 0" $rc
	[ "$(cat "$envf")" = "$first" ]; check "$name: re-run without values leaves the pair untouched" $?

	# new values replace both halves, no leftovers
	run_agent "$script" KANADE_NATS_USER=u2 KANADE_NATS_PASSWORD=p2
	[ "$(grep -c '^KANADE_NATS_USER=' "$envf")" = 1 ] && [ "$(grep -c '^KANADE_NATS_PASSWORD=' "$envf")" = 1 ]; check "$name: new values leave exactly one of each" $?
	! grep -qF -- "agent user" "$envf" && ! grep -qF -- "$pw" "$envf"; check "$name: new values replace both halves" $?
done

# --- the copies shipped in the backend crate are the same files ---
a="$here/../crates/kanade-backend/assets"
cmp -s "$here/linux/setup-agent.sh" "$a/setup-agent.sh"; check "assets/setup-agent.sh is the canonical copy" $?
cmp -s "$here/macos/setup-agent.sh" "$a/setup-agent-macos.sh"; check "assets/setup-agent-macos.sh is the canonical copy" $?
cmp -s "$here/macos/launchd/com.kanade.agent.plist" "$a/com.kanade.agent.plist"; check "assets/com.kanade.agent.plist is the canonical copy" $?

# --- the launchd launcher passes the variables through ---
plist="$here/macos/launchd/com.kanade.agent.plist"
launcher="$(sed -n 's/^[[:space:]]*<string>\(KANADE_NATS_TOKEN=.*\)<\/string>$/\1/p' "$plist")"
[ -n "$launcher" ]; check "plist: launcher command found" $?
launch() { # prints the environment the agent would see
	printf '%s\n' "$launcher" \
		| sed "s#/etc/kanade/agent.env#$tmp/etc/agent.env#g; s#exec /usr/local/bin/kanade-agent#exec env#" > "$tmp/launch.sh"
	env -i PATH="$PATH" /bin/sh "$tmp/launch.sh"
}
printf 'KANADE_NATS_TOKEN=tok\nKANADE_NATS_USER=agent user\nKANADE_NATS_PASSWORD=%s\n' "$pw" > "$tmp/etc/agent.env"
out="$(launch)"
printf '%s\n' "$out" | grep -qx 'KANADE_NATS_USER=agent user'; check "plist: user reaches the agent" $?
[ "$(launch | sed -n 's/^KANADE_NATS_PASSWORD=//p')" = "$pw" ]; check "plist: password with special characters reaches the agent intact" $?
printf '%s\n' "$out" | grep -qx 'KANADE_NATS_TOKEN=tok'; check "plist: the token still reaches the agent" $?
printf 'KANADE_NATS_TOKEN=tok\n' > "$tmp/etc/agent.env"
out="$(launch)"
! printf '%s\n' "$out" | grep -q 'KANADE_NATS_USER\|KANADE_NATS_PASSWORD'; check "plist: no user in the file -> no user variables exported" $?
printf '%s\n' "$out" | grep -qx 'KANADE_NATS_TOKEN=tok'; check "plist: token-only behaviour is unchanged" $?
! grep -q "$pw" "$plist"; check "plist: carries no credential" $?
python3 -c 'import plistlib,sys; plistlib.load(open(sys.argv[1],"rb"))' "$plist" 2>/dev/null; check "plist: still well-formed" $?

# --- backend (setup.sh) ---
setup="$here/linux/setup.sh"
vcheck() { # env assignments... ; runs only the validation region
	( env "$@" bash -c 'set -euo pipefail; eval "$(cat "$1")"' _ <(region "$setup" nats-user) ) 2>"$tmp/err" >/dev/null
}
vcheck; check "setup.sh: no user -> accepted" $?
vcheck KANADE_NATS_USER=b KANADE_NATS_PASSWORD=p; check "setup.sh: pair -> accepted" $?
vcheck KANADE_NATS_USER=b; [ $? -ne 0 ]; check "setup.sh: user only is refused" $?
vcheck "KANADE_NATS_PASSWORD=$pw"; [ $? -ne 0 ]; check "setup.sh: password only is refused" $?
! grep -qF -- "$pw" "$tmp/err"; check "setup.sh: the refusal does not echo the password" $?

kenv="$tmp/etc/kanade.env"
backend_write() { # env assignments...
	( env "$@" bash -c '
		set -euo pipefail
		eval "$(cat "$1")"
		chown() { :; }
		eval "$(cat "$2")"
	  ' _ <(region "$setup" nats-user) <(region "$setup" backend-env) ) >/dev/null 2>"$tmp/err"
}
printf 'KANADE_NATS_TOKEN=tok\nKANADE_JWT_SECRET=j\n' > "$kenv"
backend_write; first="$(cat "$kenv")"
[ "$first" = "$(printf 'KANADE_NATS_TOKEN=tok\nKANADE_JWT_SECRET=j')" ]; check "setup.sh: no user -> kanade.env untouched" $?
backend_write "KANADE_NATS_USER=backend user" "KANADE_NATS_PASSWORD=$pw"
got="$(sh -c "$(grep '^KANADE_NATS_PASSWORD=' "$kenv"); printf '%s' \"\$KANADE_NATS_PASSWORD\"")"
[ "$got" = "$pw" ]; check "setup.sh: backend password with special characters round-trips" $?
grep -q '^KANADE_JWT_SECRET=j$' "$kenv" && grep -q '^KANADE_NATS_TOKEN=tok$' "$kenv"; check "setup.sh: existing backend secrets and token are kept" $?
[ "$(stat -f %Lp "$kenv" 2>/dev/null || stat -c %a "$kenv")" = "600" ]; check "setup.sh: kanade.env is mode 0600" $?
second="$(cat "$kenv")"
backend_write "KANADE_NATS_USER=backend user" "KANADE_NATS_PASSWORD=$pw"
[ "$(cat "$kenv")" = "$second" ]; check "setup.sh: same values twice is idempotent" $?
backend_write
[ "$(cat "$kenv")" = "$second" ]; check "setup.sh: re-run without values leaves the pair untouched" $?
backend_write KANADE_NATS_USER=b2 KANADE_NATS_PASSWORD=p2
[ "$(grep -c '^KANADE_NATS_USER=' "$kenv")" = 1 ] && ! grep -qF -- "backend user" "$kenv"; check "setup.sh: new values replace both halves" $?
[ -z "$(ls -A "$tmp/etc" | grep '^\.kanade-env')" ]; check "setup.sh: no temp file left behind" $?
# nats.env is the broker's file: the pair must never be written there.
! sed -n '/^# >>> backend-env$/,/^# <<< backend-env$/p' "$setup" | grep -q 'nats\.env'; check "setup.sh: the pair never goes to nats.env" $?
! grep -n 'KANADE_NATS_USER\|KANADE_NATS_PASSWORD' "$here/linux/systemd/"*.service "$here/linux/bundle-agent.sh" "$here/linux/bundle.sh" >/dev/null 2>&1; check "bundles and units carry no credential" $?

if [ "$fail" -ne 0 ]; then echo; echo "$fail FAILED"; exit 1; fi
echo; echo "all checks passed"

#!/usr/bin/env bash
# One-command role-based deploy to a Linux host — the symmetric
# counterpart of the Windows scripts\fleet-deploy.ps1 (-Role backend|agent).
#
# The mechanism differs by platform: the Windows fleet-deploy drives an
# on-box agent's self-update/rollout over NATS, whereas the Linux fleet is
# the offline bundle model — so this collects a bundle (build-release.sh),
# copies it over ssh, and runs the matching installer (setup.sh /
# setup-agent.sh) on the target. Same UX (pick a role + a target, one
# command), different plumbing. A NATS-driven Linux agent rollout is a
# separate follow-up.
#
# Run from a box with internet + ssh access to the target.
#
# Usage:
#   # Backend (needs the public domain, A records for it AND nats.<domain>):
#   ./fleet-deploy.sh --role backend --host ubuntu@1.2.3.4 \
#       --domain kanade.example.com --identity ~/.ssh/key
#
#   # Agent, co-located with a backend on the same box (reuses its token):
#   ./fleet-deploy.sh --role agent --host ubuntu@1.2.3.4 --identity ~/.ssh/key
#
#   # Agent, standalone against a remote broker:
#   ./fleet-deploy.sh --role agent --host ubuntu@agentbox --identity ~/.ssh/key \
#       --nats-url wss://nats.kanade.example.com --nats-token <token>
set -euo pipefail

ROLE=""
HOST=""
IDENTITY=""
MODE="install"
VERSION=""
ARCH=""
DOMAIN=""
NATS_URL=""
NATS_TOKEN=""
OUT_DIR=""
BACKEND_BIN=""
AGENT_BIN=""
GITHUB_REPO="kanadehq/kanade"
# --mode update scope
PC=""
GROUP=""
ALL=false
JITTER=""

while [ $# -gt 0 ]; do
	case "$1" in
		--role) ROLE="$2"; shift 2 ;;
		--mode) MODE="$2"; shift 2 ;;
		--host) HOST="$2"; shift 2 ;;
		--identity) IDENTITY="$2"; shift 2 ;;
		--version) VERSION="$2"; shift 2 ;;
		--arch) ARCH="$2"; shift 2 ;;
		--domain) DOMAIN="$2"; shift 2 ;;
		--nats-url) NATS_URL="$2"; shift 2 ;;
		--nats-token) NATS_TOKEN="$2"; shift 2 ;;
		--out) OUT_DIR="$2"; shift 2 ;;
		--backend-bin) BACKEND_BIN="$2"; shift 2 ;;
		--agent-bin) AGENT_BIN="$2"; shift 2 ;;
		--github-repo) GITHUB_REPO="$2"; shift 2 ;;
		--pc) PC="$2"; shift 2 ;;
		--group) GROUP="$2"; shift 2 ;;
		--all) ALL=true; shift ;;
		--jitter) JITTER="$2"; shift 2 ;;
		*) echo "unknown arg: $1" >&2; exit 1 ;;
	esac
done

case "$ROLE" in
	backend | agent) ;;
	*) echo "--role backend|agent is required" >&2; exit 1 ;;
esac
case "$MODE" in
	install | update) ;;
	*) echo "--mode install|update (default install)" >&2; exit 1 ;;
esac

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# --------------------------------------------------------------------------
# --mode update: the NATS path (no ssh), symmetric with the Windows
# fleet-deploy.ps1 agent rollout. Publishes the agent ELF to the release
# object store and flips target_version on a scope; the on-box agents
# self-update on their next watch tick (self_update.rs) and systemd
# Restart brings each back on the new binary. `kanade` must be on PATH and
# pointed at the deployment (KANADE_NATS_URL / KANADE_NATS_TOKEN /
# KANADE_BACKEND_URL), exactly as the Windows script expects.
# --------------------------------------------------------------------------
if [ "$MODE" = "update" ]; then
	[ "$ROLE" = "agent" ] || {
		echo "--mode update is agent-only (backend update = re-run --mode install)" >&2
		exit 1
	}
	[ -n "$ARCH" ] || {
		echo "--mode update needs --arch x86_64|aarch64 (the fleet's arch — no --host to probe)" >&2
		exit 1
	}
	case "$ARCH" in
		aarch64 | arm64) ARCH="aarch64" ;;
		x86_64 | amd64)  ARCH="x86_64" ;;
		*) echo "unsupported --arch '$ARCH'" >&2; exit 1 ;;
	esac
	command -v kanade >/dev/null 2>&1 || {
		echo "'kanade' not on PATH — needed to publish + rollout over NATS" >&2
		exit 1
	}
	# Exactly one scope — reject an ambiguous combination rather than
	# silently picking a precedence (the Rust `kanade agent rollout` uses
	# conflicts_with_all + refuses a missing scope; match that here so the
	# two front-ends behave the same).
	n=0
	if $ALL; then n=$((n + 1)); fi
	if [ -n "$GROUP" ]; then n=$((n + 1)); fi
	if [ -n "$PC" ]; then n=$((n + 1)); fi
	if [ "$n" -ne 1 ]; then
		echo "--mode update needs EXACTLY one scope: --all | --group <name> | --pc <id>" >&2
		exit 1
	fi
	scope_args=()
	if $ALL; then scope_args=(--global)
	elif [ -n "$GROUP" ]; then scope_args=(--group "$GROUP")
	else scope_args=(--pc "$PC")
	fi
	[ -n "$JITTER" ] && scope_args+=(--jitter "$JITTER")

	ver="$VERSION"
	[ -n "$ver" ] || ver="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$here/../../Cargo.toml" | head -n1)"
	[ -n "$ver" ] || { echo "couldn't resolve a version — pass --version" >&2; exit 1; }
	ver="${ver#v}"
	tag="v${ver}"

	work="$(mktemp -d)"
	trap 'rm -rf "$work"' EXIT
	if [ -n "$AGENT_BIN" ]; then
		elf="$AGENT_BIN"
	else
		asset="kanade-agent-${ARCH}-unknown-linux-musl.tar.gz"
		url="https://github.com/${GITHUB_REPO}/releases/download/${tag}/${asset}"
		echo "==> downloading ${url}"
		curl -fsSL "$url" -o "$work/$asset" || { echo "download failed: $url" >&2; exit 1; }
		tar -xzf "$work/$asset" -C "$work"
		elf="$(find "$work" -maxdepth 2 -type f -name 'kanade-agent*' ! -name '*.tar.gz' | head -n1)"
		[ -n "$elf" ] || { echo "agent ELF not found inside $asset" >&2; exit 1; }
	fi

	echo "==> kanade agent publish --version ${ver} <elf>"
	kanade agent publish --version "$ver" "$elf"
	echo "==> kanade agent rollout ${ver} ${scope_args[*]}"
	kanade agent rollout "$ver" "${scope_args[@]}"
	echo ""
	echo "==> Done. Agent ${ver} rolled out over NATS. Agents self-update on their"
	echo "    next watch tick; systemd Restart brings each back on the new binary."
	exit 0
fi

# --------------------------------------------------------------------------
# --mode install (default): first-deploy via offline bundle over ssh.
# --------------------------------------------------------------------------
[ -n "$HOST" ] || { echo "--host [user@]host is required for --mode install" >&2; exit 1; }
if [ "$ROLE" = "backend" ] && [ -z "$DOMAIN" ]; then
	echo "--domain <your.domain> is required for --role backend (A records for it AND nats.<domain> must point at the host)" >&2
	exit 1
fi

# ssh/scp option array — shared so the identity + host-key policy stay in
# lockstep between the copy and the remote install.
ssh_opts=(-o StrictHostKeyChecking=accept-new)
[ -n "$IDENTITY" ] && ssh_opts+=(-i "$IDENTITY")

echo "==> Target ${HOST} — probing arch + preparing bundle"
# Detect the TARGET's arch over ssh (it may differ from this build box)
# unless the caller pinned --arch.
if [ -z "$ARCH" ]; then
	ARCH="$(ssh "${ssh_opts[@]}" "$HOST" 'uname -m')"
	echo "    remote arch: ${ARCH}"
fi
case "$ARCH" in
	aarch64 | arm64) ARCH="aarch64" ;;
	x86_64 | amd64)  ARCH="x86_64" ;;
	*) echo "unsupported target arch '$ARCH'" >&2; exit 1 ;;
esac

# Stage the bundle for just this role. When --out isn't given, use a temp
# dir and remove it on exit (build-release cleans its own downloads).
staging="$OUT_DIR"
if [ -z "$staging" ]; then
	staging="$(mktemp -d)"
	trap 'rm -rf "$staging"' EXIT
fi
mkdir -p "$staging"
br_args=(--roles "$ROLE" --arch "$ARCH" --out "$staging" --github-repo "$GITHUB_REPO")
[ -n "$VERSION" ] && br_args+=(--version "$VERSION")
[ -n "$BACKEND_BIN" ] && br_args+=(--backend-bin "$BACKEND_BIN")
[ -n "$AGENT_BIN" ] && br_args+=(--agent-bin "$AGENT_BIN")
bash "$here/build-release.sh" "${br_args[@]}"

if [ "$ROLE" = "backend" ]; then
	tarball="kanade-linux-${ARCH}-bundle.tar.gz"
	bundle_dir="kanade-linux-${ARCH}-bundle"
else
	tarball="kanade-linux-agent-bundle.tar.gz"
	bundle_dir="kanade-linux-agent-bundle"
fi

echo ""
echo "==> Copying ${tarball} to ${HOST}"
scp "${ssh_opts[@]}" "$staging/$tarball" "$HOST:~/"

# For a standalone agent, the broker URL + token go over in a mode-0600
# env file rather than on the install command line — a token spliced into
# the ssh command would be visible in `ps aux` / /proc/<pid>/cmdline on
# the target for the ssh session's lifetime. scp carries only filenames
# in argv, so the secret never appears there.
remote_envfile=""
if [ "$ROLE" = "agent" ] && { [ -n "$NATS_URL" ] || [ -n "$NATS_TOKEN" ]; }; then
	local_envfile="$staging/.kanade-agent-deploy.env"
	(
		umask 077
		: > "$local_envfile"
		[ -n "$NATS_URL" ]   && printf 'KANADE_NATS_URL=%s\n'   "$NATS_URL"   >> "$local_envfile"
		[ -n "$NATS_TOKEN" ] && printf 'KANADE_NATS_TOKEN=%s\n' "$NATS_TOKEN" >> "$local_envfile"
	)
	remote_envfile=".kanade-agent-deploy.env"
	scp "${ssh_opts[@]}" "$local_envfile" "$HOST:~/${remote_envfile}"
	# Shred the local copy immediately — it's only needed for the scp
	# above. Unconditional (not reliant on the staging-dir trap, which is
	# only armed when --out is omitted), so a `--out DIR` run doesn't leave
	# the plaintext token in DIR.
	shred -u "$local_envfile" 2>/dev/null || rm -f "$local_envfile"
fi

echo "==> Installing on ${HOST} (${ROLE})"
# Extract to a fresh dir and run the installer. `bash ./setup*.sh` (not
# `./setup*.sh`): a bundle built on Windows may lack the exec bit. Only
# non-secret values (bundle name, domain) are interpolated into the
# command; the agent's secrets arrive via the scp'd env file above,
# sourced inside the sudo'd installer and shredded afterward.
if [ "$ROLE" = "backend" ]; then
	ssh "${ssh_opts[@]}" "$HOST" \
		"set -e; rm -rf ~/${bundle_dir}; tar -xzf ~/${tarball}; cd ~/${bundle_dir}; sudo KANADE_DOMAIN=$(printf '%q' "$DOMAIN") bash ./setup.sh"
elif [ -n "$remote_envfile" ]; then
	# Standalone agent: source the scp'd 0600 secrets file inside the
	# sudo'd installer (absolute path, since ~ is root's home under sudo),
	# then shred it. Piped over stdin (bash -s) so the nested quoting stays
	# readable. Only ${bundle_dir}/${tarball}/${remote_envfile} interpolate
	# locally; \$HOME / \$ef resolve on the target.
	ssh "${ssh_opts[@]}" "$HOST" bash -s <<REMOTE
set -e
ef="\$HOME/${remote_envfile}"
# Shred on ANY exit — with set -e a failed install would otherwise skip a
# trailing shred and leave the plaintext token on the target.
trap 'shred -u "\$ef" 2>/dev/null || rm -f "\$ef"' EXIT
rm -rf ~/${bundle_dir}; tar -xzf ~/${tarball}; cd ~/${bundle_dir}
sudo bash -c 'set -a; . "'"\$ef"'"; set +a; bash ./setup-agent.sh'
REMOTE
else
	# Co-located agent: setup-agent reuses the backend's /etc/kanade/nats.env.
	ssh "${ssh_opts[@]}" "$HOST" \
		"set -e; rm -rf ~/${bundle_dir}; tar -xzf ~/${tarball}; cd ~/${bundle_dir}; sudo bash ./setup-agent.sh"
fi

echo ""
echo "==> Done. ${ROLE} deployed to ${HOST}."
if [ "$ROLE" = "backend" ]; then
	echo "    https://${DOMAIN}/    (SPA)"
else
	echo "    the agent should appear in the SPA fleet as the host's name."
fi

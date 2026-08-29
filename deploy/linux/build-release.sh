#!/usr/bin/env bash
# One role-parameterized collector for the Linux deploy bundles — the
# symmetric counterpart of the Windows `scripts/build-release.ps1`.
#
# Windows: `build-release.ps1 -Roles agent,backend,...` stages every role
# in one invocation, downloading the binaries from GitHub Releases by
# default (no toolchain). This does the same for Linux: it fetches the
# kanade role binaries for the target arch and hands each to the matching
# bundler (bundle.sh / bundle-agent.sh), producing one tarball per role.
#
# Run on a machine WITH internet (build box, CI, laptop) — NOT the
# deployment target. The tarballs it emits install offline via
# setup.sh / setup-agent.sh.
#
# Usage:
#   ./build-release.sh [--roles backend,agent] [--version 0.44.35] \
#                      [--arch x86_64|aarch64] [--out DIR] \
#                      [--backend-bin PATH] [--agent-bin PATH] \
#                      [--github-repo owner/repo] \
#                      [--nats-version v2.14.3] [--caddy-version 2.10.2]
#
# Defaults: both roles, version from the workspace Cargo.toml, arch from
# `uname -m`, out=dist. Pass --backend-bin / --agent-bin to use a locally
# built binary instead of downloading that role.
set -euo pipefail

ROLES="backend,agent"
VERSION=""
ARCH="$(uname -m)"
OUT_DIR="dist"
GITHUB_REPO="kanadehq/kanade"
BACKEND_BIN=""
AGENT_BIN=""
NATS_VERSION="v2.14.3"
CADDY_VERSION="2.10.2"

while [ $# -gt 0 ]; do
	case "$1" in
		--roles | --role) ROLES="$2"; shift 2 ;;
		--version) VERSION="$2"; shift 2 ;;
		--arch) ARCH="$2"; shift 2 ;;
		--out) OUT_DIR="$2"; shift 2 ;;
		--github-repo) GITHUB_REPO="$2"; shift 2 ;;
		--backend-bin) BACKEND_BIN="$2"; shift 2 ;;
		--agent-bin) AGENT_BIN="$2"; shift 2 ;;
		--nats-version) NATS_VERSION="$2"; shift 2 ;;
		--caddy-version) CADDY_VERSION="$2"; shift 2 ;;
		*) echo "unknown arg: $1" >&2; exit 1 ;;
	esac
done

case "$ARCH" in
	aarch64 | arm64) ARCH="aarch64" ;;
	x86_64 | amd64)  ARCH="x86_64" ;;
	*) echo "unsupported --arch '$ARCH' (use x86_64 or aarch64)" >&2; exit 1 ;;
esac

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"

if [ -z "$VERSION" ]; then
	VERSION="$(sed -n 's/^version[[:space:]]*=[[:space:]]*"\([^"]*\)".*/\1/p' "$repo_root/Cargo.toml" | head -n1)"
	[ -n "$VERSION" ] || { echo "couldn't read version from Cargo.toml — pass --version" >&2; exit 1; }
fi
tag="v${VERSION#v}"

mkdir -p "$OUT_DIR"
OUT_DIR="$(cd "$OUT_DIR" && pwd)"

# One work dir for all downloads, cleaned on exit. Created in THIS shell
# (not inside fetch_role_bin, which runs in a `$(...)` subshell whose
# variable writes wouldn't survive) so the trap always sees it — and the
# trap is a plain `rm -rf`, so the EXIT status stays 0 (an earlier version
# looped over an array whose last test returned 1, making the whole script
# exit 1 even on success).
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

# Download a role's release binary for $ARCH and echo the path to the
# extracted, executable binary. Files land under $work (on disk), so the
# `$(...)` subshell this runs in doesn't lose them.
fetch_role_bin() {
	local crate="$1"
	local target="${ARCH}-unknown-linux-musl"
	local asset="${crate}-${target}.tar.gz"
	local url="https://github.com/${GITHUB_REPO}/releases/download/${tag}/${asset}"
	local d="$work/$crate"
	mkdir -p "$d"
	echo "    downloading ${url}" >&2
	curl -fsSL "$url" -o "$d/$asset" || { echo "download failed: $url" >&2; return 1; }
	tar -xzf "$d/$asset" -C "$d"
	# The archived binary carries the target triple
	# (kanade-backend-x86_64-unknown-linux-musl), not the bare crate name —
	# match on the crate prefix, but exclude the .tar.gz asset itself
	# (which also starts with the crate name).
	local bin
	bin="$(find "$d" -maxdepth 2 -type f -name "${crate}*" ! -name '*.tar.gz' | head -n1)"
	[ -n "$bin" ] || { echo "no '${crate}*' binary found inside $asset" >&2; return 1; }
	chmod +x "$bin"
	printf '%s\n' "$bin"
}

IFS=',' read -r -a role_list <<< "$ROLES"
echo "==> build-release: roles=[${ROLES}] version=${tag} arch=${ARCH} out=${OUT_DIR}"

for role in "${role_list[@]}"; do
	role="$(printf '%s' "$role" | tr -d '[:space:]')"
	[ -n "$role" ] || continue
	echo ""
	echo "=== ${role} ==="
	case "$role" in
		backend)
			bin="$BACKEND_BIN"
			[ -n "$bin" ] || bin="$(fetch_role_bin kanade-backend)"
			bash "$here/bundle.sh" --backend "$bin" --arch "$ARCH" --out "$OUT_DIR" \
				--nats-version "$NATS_VERSION" --caddy-version "$CADDY_VERSION"
			;;
		agent)
			bin="$AGENT_BIN"
			[ -n "$bin" ] || bin="$(fetch_role_bin kanade-agent)"
			bash "$here/bundle-agent.sh" --agent "$bin" --out "$OUT_DIR"
			;;
		*)
			echo "unknown role '$role' (supported: backend, agent)" >&2; exit 1 ;;
	esac
done

echo ""
echo "==> Done. Bundles in ${OUT_DIR}:"
ls -1 "$OUT_DIR"/kanade-linux-*bundle.tar.gz 2>/dev/null || true

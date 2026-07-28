#!/usr/bin/env bash
# Produce an aarch64 kanade-backend artifact WITH the real SPA embedded.
#
# This is an INTERIM artifact producer, not part of the deployment. The
# proper source is a GitHub Release asset, once the aarch64-linux target
# is added to release.yml (tracked separately). Until then, run this on
# any ARM64 BUILD box (NOT the deployment server — the server installs a
# prebuilt binary offline via setup.sh) and feed the result to bundle.sh:
#
#   ./deploy/linux/build-aarch64.sh
#   ./deploy/linux/bundle.sh --backend target/release/kanade-backend
#
# Why a build step at all: CI release artifacts are currently x86_64 only,
# and web/dist is gitignored so a plain `cargo build` embeds a placeholder.
set -euo pipefail

# cargo builds for the host by default, so on an x86_64 box this would
# silently produce a binary the ARM64 VM cannot run. Refuse early.
case "$(uname -m)" in
	aarch64 | arm64) ;;
	*)
		echo "This script must run on an ARM64 host (uname -m = $(uname -m))." >&2
		echo "Run it on the Ampere VM itself." >&2
		exit 1
		;;
esac

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

echo "==> Installing build toolchains (rustup, bun, cargo-make)"
# Guard on cargo too, not only rustc: an apt-installed rustc without cargo
# would pass the rustc check and then fail at the build step below.
if ! command -v cargo >/dev/null 2>&1 || ! command -v rustc >/dev/null 2>&1; then
	curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
	# shellcheck disable=SC1091
	source "$HOME/.cargo/env"
fi
# shellcheck disable=SC1091
[ -f "$HOME/.cargo/env" ] && source "$HOME/.cargo/env"

if ! command -v bun >/dev/null 2>&1; then
	curl -fsSL https://bun.sh/install | bash
	export BUN_INSTALL="$HOME/.bun"
	export PATH="$BUN_INSTALL/bin:$PATH"
fi

command -v cargo-make >/dev/null 2>&1 || cargo install cargo-make --locked

echo "==> Building the SPA (cargo make web-build)"
# Populates crates/kanade-backend/web/dist with the real bundle that
# rust-embed picks up at compile time.
cargo make web-build

echo "==> Building kanade-backend (release, native aarch64)"
cargo build --release -p kanade-backend

bin="target/release/kanade-backend"
# Fail loudly if the build produced nothing (do not print an install
# command for a file that isn't there). `file` is only cosmetic, so keep
# it optional.
[ -x "$bin" ] || { echo "build did not produce $bin" >&2; exit 1; }
command -v file >/dev/null 2>&1 && file "$bin"
echo
echo "==> Built: $repo_root/$bin"
echo "    Install it with:"
echo "      sudo install -m 0755 $bin /usr/local/bin/kanade-backend"

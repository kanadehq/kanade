#!/bin/sh
# Local managed/test .pkg only: builds the app, wraps it in an unsigned payload-only
# package and validates it. Never runs installer, writes /Applications or touches the agent.
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -sm)" = "Darwin arm64" ] || { echo 'Requires an Apple Silicon Mac' >&2; exit 1; }
app='target/aarch64-apple-darwin/release/bundle/macos/Kanade Client.app'
[ "${SKIP_APP_BUILD:-0}" = 1 ] || sh scripts/build-client-macos.sh
python3 -I scripts/ci/client_macos_pkg.py build --app "$app" --out target/client-macos-pkg

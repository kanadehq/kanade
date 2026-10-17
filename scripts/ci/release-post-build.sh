#!/usr/bin/env bash
# Generic release hook, called by .github/workflows/release.yml:
#   release-post-build.sh sign         # after the build, before packaging
#   release-post-build.sh verify-dist  # after packaging, before upload
# Kept in the repo (release.yml is kata-managed and overwritten on apply)
# so project-specific release steps live outside the template.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mode="${1:?usage: release-post-build.sh <sign|verify-dist>}"

# macOS signing first (its own no-op for other targets), so the metadata
# check below runs against the artifact that actually ships.
case "${TARGET:-}" in
  *apple-darwin) "$here/macos-codesign.sh" "$mode" ;;
  *) echo "release-post-build: codesign nothing to do for target '${TARGET:-}'" ;;
esac

# The backend refuses to register a kanade-agent without its embedded
# release metadata (kanade_shared::agent_metadata), so a release artifact
# that lost it to optimisation / strip / signing / packaging must not ship.
# Read-only: the file is parsed, never run, re-signed or rewritten.
case "${TARGET:-}" in
  x86_64-*-windows-*) platform=windows-x86_64; ext=.exe ;;
  x86_64-*-linux-*) platform=linux-x86_64; ext= ;;
  aarch64-*-linux-*) platform=linux-aarch64; ext= ;;
  aarch64-apple-darwin) platform=macos-aarch64; ext= ;;
  *) echo "release-post-build: no agent platform for target '${TARGET:-}'"; exit 0 ;;
esac
version=$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[] | select(.name == "kanade-agent") | .version' | tr -d '\r')
case "$mode" in
  sign) artifact="target/$TARGET/release/kanade-agent$ext" ;;
  verify-dist)
    if [[ "$platform" == windows-* ]]; then
      artifact="dist/kanade-agent-$TARGET.zip"
    else
      artifact="dist/kanade-agent-$TARGET.tar.gz"
    fi ;;
esac
python3 "$here/verify-agent-dist.py" "$artifact" "$version" "$platform"

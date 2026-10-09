#!/usr/bin/env bash
# Generic release hook, called by .github/workflows/release.yml:
#   release-post-build.sh sign         # after the build, before packaging
#   release-post-build.sh verify-dist  # after packaging, before upload
# Kept in the repo (release.yml is kata-managed and overwritten on apply)
# so project-specific release steps live outside the template.
set -euo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
mode="${1:?usage: release-post-build.sh <sign|verify-dist>}"
case "${TARGET:-}" in
  *apple-darwin) exec "$here/macos-codesign.sh" "$mode" ;;
  *) echo "release-post-build: nothing to do for target '${TARGET:-}'" ;;
esac

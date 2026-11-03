#!/usr/bin/env bash
# Repo-owned release hook invoked by release.yml:
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

# The ELF machine type and release metadata do not establish CPU compatibility.
# Execute the final Linux archives on the documented x86-64-v1 CPU model before
# release.yml uploads them. Keep this in the hook, not the kata-managed workflow.
if [[ "$mode" == verify-dist && "${TARGET:-}" == x86_64-unknown-linux-musl ]]; then
  if [[ -z "${QEMU_X86_64:-}" ]] \
      && ! command -v qemu-x86_64 >/dev/null \
      && ! command -v qemu-x86_64-static >/dev/null; then
    if [[ "${GITHUB_ACTIONS:-}" == true ]]; then
      sudo apt-get update
      sudo apt-get install -y --no-install-recommends qemu-user
    else
      echo "release-post-build: install qemu-user or set QEMU_X86_64 to verify the CPU baseline" >&2
      exit 1
    fi
  fi
  python3 "$here/verify-linux-cpu.py" dist "$version"
fi

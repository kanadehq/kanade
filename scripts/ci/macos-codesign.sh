#!/usr/bin/env bash
# Sign the shipped macOS binaries with a STABLE self-signed code-signing
# identity so the designated requirement (and therefore TCC / PPPC grants)
# survives self-updates. See book/src/operations/macos-code-signing.md.
#
#   macos-codesign.sh sign         sign target/$TARGET/release/<bin>
#   macos-codesign.sh verify-dist  verify the binaries inside dist/*.tar.gz
#
# Env: TARGET, MACOS_SIGN_CERT_P12_BASE64, MACOS_SIGN_CERT_PASSWORD.
# No secrets  -> notice + exit 0 (forks / CI without secrets still build),
#                unless deploy/macos/signing-cert.sha1 pins the identity (then exit 1).
# Half-set or any import/sign/verify failure -> exit 1.
set -euo pipefail

mode="${1:?usage: macos-codesign.sh <sign|verify-dist>}"
TARGET="${TARGET:?TARGET is required}"
p12_b64="${MACOS_SIGN_CERT_P12_BASE64:-}"
p12_pass="${MACOS_SIGN_CERT_PASSWORD:-}"
sha_file="deploy/macos/signing-cert.sha1"

if [[ "$TARGET" != *apple-darwin ]]; then exit 0; fi

if [[ -z "$p12_b64" && -z "$p12_pass" ]]; then
  if [[ -f "$sha_file" ]]; then
    echo "::error::$sha_file pins the signing identity but MACOS_SIGN_CERT_P12_BASE64 / MACOS_SIGN_CERT_PASSWORD are not set; refusing to ship ad-hoc signed binaries"
    exit 1
  fi
  echo "::notice::macOS code signing skipped: MACOS_SIGN_CERT_P12_BASE64 / MACOS_SIGN_CERT_PASSWORD not set (binaries keep the linker ad-hoc signature)"
  exit 0
fi
if [[ -z "$p12_b64" || -z "$p12_pass" ]]; then
  echo "::error::only one of MACOS_SIGN_CERT_P12_BASE64 / MACOS_SIGN_CERT_PASSWORD is set"
  exit 1
fi

bins=$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[].targets[] | select(.kind | index("bin")) | .name' \
  | tr -d '\r' | sort -u)

ident_for() { if [[ "$1" == kanade-agent ]]; then echo com.kanade.agent; else echo "com.kanade.$1"; fi; }

# Expected leaf SHA-1 (lowercase hex, as codesign prints it), "" when not pinned yet.
expected_sha=""
if [[ -f "$sha_file" ]]; then
  expected_sha=$(tr -d ' \t\r\n:' < "$sha_file" | tr 'A-F' 'a-f')
fi

# verify_bin <path> <identifier> <sha1>
verify_bin() {
  local f="$1" id="$2" sha="$3" dr dr_l
  codesign --verify --strict -v "$f"
  codesign -dv "$f"
  dr=$(codesign -dr - "$f" 2>&1)
  echo "$dr"
  dr_l=$(printf '%s' "$dr" | tr 'A-F' 'a-f')
  if [[ "$dr_l" != *"identifier \"$id\""* || "$dr_l" != *"certificate leaf = H\"$sha\""* ]]; then
    echo "::error::$f: designated requirement lacks identifier \"$id\" / certificate leaf H\"$sha\" (ad-hoc or wrong identity?)"
    return 1
  fi
}

# Resolve the SHA-1 to check against: the pinned one, else the one of
# the imported certificate (first-time setup).
resolve_sha() {
  local imported="$1"
  if [[ -n "$expected_sha" ]]; then
    if [[ -n "$imported" && "$imported" != "$expected_sha" ]]; then
      echo "::error::imported certificate SHA-1 $imported != $sha_file ($expected_sha)"
      return 1
    fi
    echo "$expected_sha"
  else
    echo "::notice::$sha_file not committed; certificate SHA-1 is $imported (commit it to pin the identity)" >&2
    echo "$imported"
  fi
}

work=$(mktemp -d)
kc="$work/kanade-sign.keychain-db"
cleanup() {
  security delete-keychain "$kc" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

printf '%s' "$p12_b64" | base64 --decode > "$work/cert.p12"
# Certificate SHA-1 straight from the p12 (no keychain needed).
imported_sha=$(openssl pkcs12 -in "$work/cert.p12" -passin env:MACOS_SIGN_CERT_PASSWORD -nokeys -clcerts -legacy 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha1 2>/dev/null \
  || openssl pkcs12 -in "$work/cert.p12" -passin env:MACOS_SIGN_CERT_PASSWORD -nokeys -clcerts 2>/dev/null \
  | openssl x509 -noout -fingerprint -sha1)
imported_sha=$(echo "${imported_sha#*=}" | tr -d ':' | tr 'A-F' 'a-f')
[[ -n "$imported_sha" ]] || { echo "::error::could not read certificate from p12"; exit 1; }
sha=$(resolve_sha "$imported_sha")

case "$mode" in
  sign)
    kcpass=$(uuidgen)
    security create-keychain -p "$kcpass" "$kc"
    security set-keychain-settings -lut 3600 "$kc"
    security unlock-keychain -p "$kcpass" "$kc"
    security import "$work/cert.p12" -k "$kc" -P "$p12_pass" -T /usr/bin/codesign
    security set-key-partition-list -S apple-tool:,apple: -s -k "$kcpass" "$kc" >/dev/null
    # shellcheck disable=SC2046
    security list-keychains -d user -s "$kc" $(security list-keychains -d user | tr -d '"')
    for bin in $bins; do
      f="target/$TARGET/release/$bin"
      [[ -f "$f" ]] || { echo "::error::missing $f"; exit 1; }
      id=$(ident_for "$bin")
      codesign --force --sign "$sha" --keychain "$kc" --identifier "$id" \
        -r="designated => identifier \"$id\" and certificate leaf = H\"$sha\"" \
        --timestamp=none "$f"
      verify_bin "$f" "$id" "$sha"
      echo "signed $f as $id (leaf $sha)"
    done
    ;;
  verify-dist)
    for bin in $bins; do
      a="dist/$bin-$TARGET.tar.gz"
      [[ -f "$a" ]] || { echo "::error::missing $a"; exit 1; }
      d="$work/x-$bin"; mkdir -p "$d"
      tar xzf "$a" -C "$d"
      verify_bin "$d/$bin-$TARGET" "$(ident_for "$bin")" "$sha"
    done
    echo "all archives carry the expected signature"
    ;;
  *) echo "unknown mode $mode"; exit 2 ;;
esac

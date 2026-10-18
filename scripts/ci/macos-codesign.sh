#!/usr/bin/env bash
# Sign the shipped macOS binaries with a STABLE self-signed code-signing
# identity so the designated requirement (and therefore TCC / PPPC grants)
# survives self-updates. See book/src/operations/macos-code-signing.md.
#
#   macos-codesign.sh sign         sign target/$TARGET/release/<bin>
#   macos-codesign.sh verify-dist  verify the binaries inside dist/*.tar.gz
#
# Env for sign: TARGET, MACOS_SIGN_CERT_P12_BASE64, MACOS_SIGN_CERT_PASSWORD.
# Env for verify-dist: TARGET (no secrets needed; validates against deploy/macos/signing-cert.sha1).
# Pinned identity in deploy/macos/signing-cert.sha1 is required for both modes.
# Half-set secrets, missing pinned cert, or any import/sign/verify failure -> exit 1.
set -euo pipefail

mode="${1:?usage: macos-codesign.sh <sign|verify-dist>}"
TARGET="${TARGET:?TARGET is required}"
p12_b64="${MACOS_SIGN_CERT_P12_BASE64:-}"
p12_pass="${MACOS_SIGN_CERT_PASSWORD:-}"
sha_file="deploy/macos/signing-cert.sha1"

if [[ "$TARGET" != *apple-darwin ]]; then exit 0; fi

if [[ ! -f "$sha_file" ]]; then
  echo "::error::$sha_file is missing; official release requires a pinned signing identity"
  exit 1
fi

expected_sha=$(tr -d ' \t\r\n:' < "$sha_file" | tr 'A-F' 'a-f')
if [[ -z "$expected_sha" || ${#expected_sha} -ne 40 ]]; then
  echo "::error::$sha_file does not contain a valid 40-character hex SHA-1 fingerprint"
  exit 1
fi

bins=$(cargo metadata --no-deps --format-version 1 \
  | jq -r '.packages[].targets[] | select(.kind | index("bin")) | .name' \
  | tr -d '\r' | sort -u)

ident_for() { if [[ "$1" == kanade-agent ]]; then echo com.kanade.agent; else echo "com.kanade.$1"; fi; }

# verify_bin <path> <identifier> <sha1>
verify_bin() {
  local f="$1" id="$2" sha="$3" dr dr_l
  codesign --verify --strict -v "$f"
  codesign -dv "$f"
  dr=$(codesign -dr - "$f" 2>&1)
  echo "$dr"
  dr_l=$(printf '%s' "$dr" | tr 'A-Z' 'a-z')
  if [[ "$dr_l" == *"cdhash"* ]]; then
    echo "::error::$f: designated requirement contains unstable cdhash constraint"
    return 1
  fi
  if [[ "$dr_l" != *"identifier \"$id\""* || "$dr_l" != *"certificate leaf = h\"$sha\""* ]]; then
    echo "::error::$f: designated requirement lacks identifier \"$id\" or certificate leaf H\"$sha\""
    return 1
  fi
}

work=$(mktemp -d)
kc="$work/kanade-sign.keychain-db"
cleanup() {
  security delete-keychain "$kc" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

case "$mode" in
  sign)
    if [[ -z "$p12_b64" && -z "$p12_pass" ]]; then
      echo "::error::$sha_file pins the signing identity but MACOS_SIGN_CERT_P12_BASE64 / MACOS_SIGN_CERT_PASSWORD are not set; refusing to ship ad-hoc signed binaries"
      exit 1
    fi
    if [[ -z "$p12_b64" || -z "$p12_pass" ]]; then
      echo "::error::only one of MACOS_SIGN_CERT_P12_BASE64 / MACOS_SIGN_CERT_PASSWORD is set"
      exit 1
    fi

    printf '%s' "$p12_b64" | base64 --decode > "$work/cert.p12"
    # Certificate SHA-1 straight from the p12 (no keychain needed).
    imported_sha=$(openssl pkcs12 -in "$work/cert.p12" -passin env:MACOS_SIGN_CERT_PASSWORD -nokeys -clcerts -legacy 2>/dev/null \
      | openssl x509 -noout -fingerprint -sha1 2>/dev/null \
      || openssl pkcs12 -in "$work/cert.p12" -passin env:MACOS_SIGN_CERT_PASSWORD -nokeys -clcerts 2>/dev/null \
      | openssl x509 -noout -fingerprint -sha1)
    imported_sha=$(echo "${imported_sha#*=}" | tr -d ':' | tr 'A-F' 'a-f')
    [[ -n "$imported_sha" ]] || { echo "::error::could not read certificate from p12"; exit 1; }

    if [[ "$imported_sha" != "$expected_sha" ]]; then
      echo "::error::imported certificate SHA-1 $imported_sha != $sha_file ($expected_sha)"
      exit 1
    fi

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
      codesign --force --sign "$expected_sha" --keychain "$kc" --identifier "$id" \
        -r="designated => identifier \"$id\" and certificate leaf = H\"$expected_sha\"" \
        --timestamp=none "$f"
      verify_bin "$f" "$id" "$expected_sha"
      echo "signed $f as $id (leaf $expected_sha)"
    done
    ;;
  verify-dist)
    for bin in $bins; do
      a="dist/$bin-$TARGET.tar.gz"
      [[ -f "$a" ]] || { echo "::error::missing $a"; exit 1; }
      d="$work/x-$bin"; mkdir -p "$d"
      tar xzf "$a" -C "$d"
      verify_bin "$d/$bin-$TARGET" "$(ident_for "$bin")" "$expected_sha"
    done
    echo "all archives carry the expected signature"
    ;;
  *) echo "unknown mode $mode"; exit 2 ;;
esac

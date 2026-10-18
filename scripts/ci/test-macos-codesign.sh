#!/usr/bin/env bash
# Integration test for scripts/ci/macos-codesign.sh
# Tests:
#   1. Signing a macOS binary with a self-signed test certificate.
#   2. Secretless verification of packaged archive (verify-dist).
#   3. Rejection of ad-hoc signatures (cdhash only).
#   4. Rejection of mismatched pinned certificate SHA-1.
#   5. Rejection of tampered binaries inside archive.
#   6. Rejection when deploy/macos/signing-cert.sha1 is missing.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "test-macos-codesign: macOS only; skipping on $(uname -s)"
  exit 0
fi

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "$here/../.." && pwd)"

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

echo "=== Generating test code-signing certificate ==="
cat > "$work/cs.cnf" <<'CNF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = Kanade Test Code Signing
[ext]
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
basicConstraints = critical, CA:false
CNF

openssl req -x509 -newkey rsa:2048 -nodes -keyout "$work/key.pem" -out "$work/cert.pem" -days 1 -config "$work/cs.cnf" 2>/dev/null
p12_pass="testpass"
openssl pkcs12 -export -legacy -inkey "$work/key.pem" -in "$work/cert.pem" -out "$work/test.p12" -passout pass:"$p12_pass" 2>/dev/null \
  || openssl pkcs12 -export -inkey "$work/key.pem" -in "$work/cert.pem" -out "$work/test.p12" -passout pass:"$p12_pass"

test_sha=$(openssl x509 -in "$work/cert.pem" -noout -fingerprint -sha1 | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f')
p12_b64=$(base64 -i "$work/test.p12")

echo "Test cert leaf SHA-1: $test_sha"

# Prepare mock workspace structure
mock_target="aarch64-apple-darwin"
mkdir -p "$work/repo/target/$mock_target/release"
mkdir -p "$work/repo/dist"
mkdir -p "$work/repo/deploy/macos"
mkdir -p "$work/repo/scripts/ci"
cp "$here/macos-codesign.sh" "$work/repo/scripts/ci/"

# Create a small valid Mach-O binary
echo 'int main(void) { return 0; }' | clang -x c - -target "$mock_target" -o "$work/repo/target/$mock_target/release/kanade-agent"

# Mock cargo metadata inside work directory
cat > "$work/repo/scripts/ci/mock_cargo" <<'MOCK'
#!/bin/sh
echo '{"packages":[{"targets":[{"kind":["bin"],"name":"kanade-agent"}]}]}'
MOCK
chmod +x "$work/repo/scripts/ci/mock_cargo"

run_codesign() {
  (
    cd "$work/repo"
    export PATH="$work/repo/scripts/ci:$PATH"
    # Override cargo to use our mock
    alias cargo="$work/repo/scripts/ci/mock_cargo"
    export TARGET="$mock_target"
    bash scripts/ci/macos-codesign.sh "$@"
  )
}

echo "=== Test 1: Missing signing-cert.sha1 must fail ==="
if run_codesign sign 2>/dev/null; then
  echo "FAIL: sign succeeded without signing-cert.sha1"
  exit 1
fi
echo "PASS: sign rejected missing signing-cert.sha1"

echo "$test_sha" > "$work/repo/deploy/macos/signing-cert.sha1"

echo "=== Test 2: Successful sign with matching certificate ==="
(
  cd "$work/repo"
  export TARGET="$mock_target"
  export MACOS_SIGN_CERT_P12_BASE64="$p12_b64"
  export MACOS_SIGN_CERT_PASSWORD="$p12_pass"
  # Mock cargo by prepending a wrapper script
  PATH="$work/bin:$PATH"
  mkdir -p "$work/bin"
  ln -sf "$work/repo/scripts/ci/mock_cargo" "$work/bin/cargo"
  bash scripts/ci/macos-codesign.sh sign
)
dr=$(codesign -dr - "$work/repo/target/$mock_target/release/kanade-agent" 2>&1)
echo "Designated requirement: $dr"
if [[ "$dr" != *"identifier \"com.kanade.agent\""* || "$dr" != *"certificate leaf = H\"$test_sha\""* ]]; then
  echo "FAIL: signed binary lacks expected DR"
  exit 1
fi
echo "PASS: binary signed and DR matches"

echo "=== Test 3: Secretless verify-dist against packaged archive ==="
# Package the signed binary into dist/
stem="kanade-agent-$mock_target"
(
  cd "$work/repo"
  cp "target/$mock_target/release/kanade-agent" "$stem"
  tar czf "dist/$stem.tar.gz" "$stem"
  rm "$stem"
)
(
  cd "$work/repo"
  export TARGET="$mock_target"
  unset MACOS_SIGN_CERT_P12_BASE64
  unset MACOS_SIGN_CERT_PASSWORD
  PATH="$work/bin:$PATH"
  bash scripts/ci/macos-codesign.sh verify-dist
)
echo "PASS: secretless verify-dist succeeded"

echo "=== Test 4: Rejection of ad-hoc (unsigned / cdhash-only) binary ==="
(
  cd "$work/repo"
  echo 'int main(void) { return 1; }' | clang -x c - -target "$mock_target" -o "$stem"
  codesign -s - "$stem" # ad-hoc sign
  tar czf "dist/$stem.tar.gz" "$stem"
  rm "$stem"
  export TARGET="$mock_target"
  unset MACOS_SIGN_CERT_P12_BASE64
  unset MACOS_SIGN_CERT_PASSWORD
  PATH="$work/bin:$PATH"
  if bash scripts/ci/macos-codesign.sh verify-dist 2>/dev/null; then
    echo "FAIL: verify-dist accepted ad-hoc signed binary"
    exit 1
  fi
)
echo "PASS: ad-hoc signature rejected by verify-dist"

echo "=== Test 5: Rejection of mismatched pinned SHA-1 on valid signed binary ==="
(
  cd "$work/repo"
  # Re-package the validly signed binary
  cp "target/$mock_target/release/kanade-agent" "$stem"
  tar czf "dist/$stem.tar.gz" "$stem"
  rm "$stem"
  echo "0000000000000000000000000000000000000000" > deploy/macos/signing-cert.sha1
  export TARGET="$mock_target"
  unset MACOS_SIGN_CERT_P12_BASE64
  unset MACOS_SIGN_CERT_PASSWORD
  PATH="$work/bin:$PATH"
  if bash scripts/ci/macos-codesign.sh verify-dist 2>/dev/null; then
    echo "FAIL: verify-dist accepted signed binary with mismatched pinned sha1"
    exit 1
  fi
)
echo "PASS: mismatched pinned sha1 rejected by verify-dist"

echo "=== Test 6: Rejection of tampered binary inside archive ==="
(
  cd "$work/repo"
  echo "$test_sha" > deploy/macos/signing-cert.sha1
  cp "target/$mock_target/release/kanade-agent" "$stem"
  # Corrupt the binary slightly
  printf '\xff' >> "$stem"
  tar czf "dist/$stem.tar.gz" "$stem"
  rm "$stem"
  export TARGET="$mock_target"
  unset MACOS_SIGN_CERT_P12_BASE64
  unset MACOS_SIGN_CERT_PASSWORD
  PATH="$work/bin:$PATH"
  if bash scripts/ci/macos-codesign.sh verify-dist 2>/dev/null; then
    echo "FAIL: verify-dist accepted tampered binary"
    exit 1
  fi
)
echo "PASS: tampered binary rejected by verify-dist"

echo "=== All test-macos-codesign checks passed! ==="

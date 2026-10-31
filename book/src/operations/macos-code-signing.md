# macOS code signing (stable identity)

macOS TCC (Files and Folders, Documents, Photos, Full Disk Access) keys
grants on the binary's code identity. The linker's ad-hoc signature uses
the cdhash as its identity — it changes on every build, so every
self-update looks like a brand-new binary and prompts for privacy permissions again.

The release workflow therefore signs `kanade-agent` (and any other shipped
macOS binary) with a **stable self-signed code-signing certificate**. The
designated requirement stays identical across versions, so a one-time grant
(or an MDM PPPC profile) survives updates. No Apple Developer ID or
notarization is involved; `spctl` still rejects the binary, which is fine
for a launchd-run command-line tool.

## How it works

- **Signing (`sign` hook)**: `scripts/ci/macos-codesign.sh sign` runs right after
  the release build and before artifact packaging. It imports the certificate
  from repo secrets (`MACOS_SIGN_CERT_P12_BASE64`, `MACOS_SIGN_CERT_PASSWORD`)
  into an ephemeral keychain and signs each binary:
  `codesign --force --sign <sha1> --keychain <kc> --identifier <id> -r="designated => identifier \"<id>\" and certificate leaf = H\"<sha1>\"" --timestamp=none <bin>`
- **Post-package verification (`verify-dist` hook)**: Runs after packaging and
  before uploading artifacts. This step is strictly **secretless**: it unpacks
  the shipped `dist/*.tar.gz` and verifies the binaries against the pinned
  fingerprint in `deploy/macos/signing-cert.sha1`.
- **Integrity checks**:
  - `codesign --verify --strict -v` ensures Mach-O code directory validity.
  - Designated requirement verification checks that the binary specifies
    `identifier "com.kanade.agent"` and `certificate leaf = H"<sha1>"`.
  - Rejects unstable `cdhash` constraints so ad-hoc or fallback signatures
    never pass unnoticed.
- **Strict enforcement**:
  - `deploy/macos/signing-cert.sha1` is required for official releases. Missing,
    invalid, or mismatched fingerprints fail the build.
  - Releases never fall back silently to ad-hoc linker signatures.
  - This policy also applies to fork and `workflow_dispatch` release builds:
    missing secrets fail the macOS matrix entry and block publication. There is
    no unsigned-release opt-out. Ordinary pull-request checks do not need the
    production secrets; the signing integration test uses a disposable test
    certificate and never publishes its binaries.

## One-time setup

### 1. Reuse the established certificate

Before creating anything, list secret **names only**:

```sh
gh secret list --repo kanadehq/kanade --json name --jq '.[].name'
```

Check for both `MACOS_SIGN_CERT_P12_BASE64` and `MACOS_SIGN_CERT_PASSWORD`.
Secret presence does not prove that the pinned fingerprint matches the stored
certificate. GitHub does not return secret values. The operator must derive the
public fingerprint from the securely archived established certificate, or supply
its public certificate for comparison. Do not export or log the private key,
P12 contents, or password to perform this check. Do not replace an existing
certificate merely because its private key cannot be read back from GitHub.

**Provisioning status:** both secret names (`MACOS_SIGN_CERT_P12_BASE64` and
`MACOS_SIGN_CERT_PASSWORD`) are present in the repository, checked by name only.
The current pin in `deploy/macos/signing-cert.sha1` has **not** been matched to
the certificate stored in that secret, because GitHub never returns secret
values. A syntactically valid pin is not evidence of a match. If it differs,
`macos-codesign.sh sign` fails on the imported-vs-pinned comparison and blocks
the release by design; that is an operator provisioning item, not something to
patch around. The operator must confirm the established certificate's public
SHA-1 (or replace the pin with it) before the next release.
### Only if no established identity exists: create the certificate

Creating a new certificate is an explicit operator provisioning decision. It
changes the TCC identity if a certificate was previously used. Reuse the
existing P12 and certificate whenever available.

Keychain Access: *Certificate Assistant → Create a Certificate…*, name
`Kanade Code Signing`, Identity Type *Self Signed Root*, Certificate Type
*Code Signing*. Set a long validity (e.g. 3650 days) and export it as `.p12`.

Or with openssl:

```sh
cat > cs.cnf <<'CNF'
[req]
distinguished_name = dn
x509_extensions = ext
prompt = no
[dn]
CN = Kanade Code Signing
[ext]
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
basicConstraints = critical, CA:false
CNF
openssl req -x509 -newkey rsa:2048 -nodes -keyout key.pem -out cert.pem -days 3650 -config cs.cnf
# -legacy: macOS `security import` rejects OpenSSL 3's default AES/PBKDF2 p12
openssl pkcs12 -export -legacy -inkey key.pem -in cert.pem -out kanade-sign.p12
```

### 2. Store it in the repo secrets

```sh
base64 -i kanade-sign.p12 | gh secret set MACOS_SIGN_CERT_P12_BASE64
gh secret set MACOS_SIGN_CERT_PASSWORD
```

Back up the `.p12` file in a secure password vault or encrypted backup;
losing the private key requires rotating the identity and invalidates all existing grants.

### 3. Pin the certificate SHA-1

```sh
openssl x509 -in cert.pem -noout -fingerprint -sha1 | sed 's/.*=//; s/://g' | tr 'A-F' 'a-f' \
  > deploy/macos/signing-cert.sha1
```

Commit `deploy/macos/signing-cert.sha1`. Official builds require this pinned
fingerprint to match the certificate used for signing.

## Granting access on devices

### MDM-managed Macs (PPPC profile)

```xml
<key>Services</key>
<dict>
  <key>SystemPolicyAllFiles</key>
  <array>
    <dict>
      <key>Identifier</key><string>com.kanade.agent</string>
      <key>IdentifierType</key><string>bundleID</string>
      <key>CodeRequirement</key>
      <string>identifier "com.kanade.agent" and certificate leaf = H"&lt;SHA1 OF THE CERT&gt;"</string>
      <key>Allowed</key><true/>
    </dict>
  </array>
</dict>
```

### Manual

System Settings → Privacy & Security → Full Disk Access → `+` → add
`/usr/local/bin/kanade-agent` (⇧⌘G to type the path). Once, per device.

## Migration and operational caveats

### One-time migration grant vs recurring update prompts

- **Expected one-time regrant**: Moving an installation from an ad-hoc signed
  binary (cdhash requirement) to the stable self-signed identity requires
  exactly one manual permission grant or PPPC profile deployment.
- **Recurring prompts are regressions**: If an update between two versions signed
  with the stable certificate triggers a privacy prompt, this indicates an identity
  regression (such as an accidental fallback to linker ad-hoc signing or a changed
  leaf fingerprint).
- **Do not use workarounds**: Never reset TCC permissions (`tccutil reset ...`) or
  re-sign running agent binaries on production devices as a workaround. Investigate
  the binary's code requirement with `codesign -dr - /usr/local/bin/kanade-agent`
  and verify CI release signing artifacts.

### Key storage, validity, and rotation

- The private key `.p12` must be archived securely.
- Certificate expiry must be tracked. Rotating the certificate changes the leaf
  SHA-1, requiring `deploy/macos/signing-cert.sha1` updates and re-granting
  permissions across all managed machines.
- The self-update mechanism (`crates/kanade-agent/src/self_update.rs`) copies
  downloaded Mach-O binaries verbatim and only alters permissions bits (`0o755`).
  Nothing on the client device strips or re-signs the executable.

### Kata template integration

- `.github/workflows/release.yml` is kata-managed. The post-build `sign` and
  post-package `verify-dist` hooks calling `scripts/ci/release-post-build.sh`
  must also be preserved in upstream templates (`yukimemi/pj-rust-workspace`
  and `yukimemi/pj-rust-cli`) so `kata apply` cannot overwrite or remove them.
- Prepared upstream patches and their exact source blob IDs are recorded in
  `scripts/ci/kata-signing/README.md`. These patches are pending upstream
  application, not evidence of completed integration. Do not mark the repair
  complete until the upstream changes are reviewed, applied, and the workspace
  source revision in `.kata/applied.toml` is updated by `kata apply`.

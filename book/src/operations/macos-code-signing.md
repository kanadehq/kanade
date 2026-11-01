# macOS code signing (stable identity)

macOS TCC (Files and Folders, Documents, Photos, Full Disk Access) uses the
binary's code identity. A build with only the linker's ad-hoc signature has
a cdhash that changes on every build; updates can invalidate grants or require
renewed approval. Verify grant persistence on the actual installed launch path.

The repository provides a helper to sign `kanade-agent` (and other built
macOS binaries) with a **stable self-signed code-signing certificate**.
**Current wiring:** `.github/workflows/release.yml` does not invoke
`scripts/ci/release-post-build.sh` or `macos-codesign.sh`. Configuring secrets
alone does not activate release signing. The procedure below describes the
helper; connecting it to the kata-managed release workflow requires upstream
template work. Check each artifact rather than assuming it was signed. The
designated requirement stays the same across versions, so a one-time grant
(or an MDM PPPC profile) survives updates. No Apple Developer ID or
notarization is involved; `spctl` still rejects the binary, which is fine
for a launchd-run command-line tool.

## How it works

- `scripts/ci/macos-codesign.sh` imports the certificate from the repo
  secrets into a temporary keychain, then runs
  `codesign --force --sign <sha1> --identifier com.kanade.agent --timestamp=none`
  on each bin (`com.kanade.<bin>` for the others).
- It then checks `codesign --verify --strict`, `codesign -dv`, and that the
  designated requirement contains `identifier "com.kanade.agent"` and
  `certificate leaf = H"<sha1>"`. A mere `--verify` would pass an ad-hoc
  signature, so the DR check is what fails an unsigned artifact.
- Invoking `verify-dist` checks the unpacked `.tar.gz`. A future workflow
  integration must run it before upload and propagate failure to the release
  gate; the current workflow does not run this check.
- When invoked without secrets and without a pinned certificate, the helper
  prints a notice and skips. A pinned certificate with missing secrets fails.
  With only one of the two secrets, or on any import/sign/verify failure,
  the helper returns nonzero.

## One-time setup

### 1. Create the certificate

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

Back up the `.p12` somewhere safe; losing it means rotating the identity.

### 3. Pin the certificate SHA-1

```sh
openssl x509 -in cert.pem -noout -fingerprint -sha1 | sed 's/.*=//; s/://g' \
  > deploy/macos/signing-cert.sha1
```

Commit `deploy/macos/signing-cert.sha1`. When invoked, the helper fails if the
imported certificate or a signed binary does not match it. Until the file
exists the helper only prints the imported SHA-1 and checks the binaries
against it.

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

The IdentifierType and how a bare (non-bundle) executable is matched should
be confirmed on a real device; test the profile on one Mac before rolling out.
Some MDMs want `IdentifierType` `path` with `/usr/local/bin/kanade-agent`.

### Manual

System Settings → Privacy & Security → Full Disk Access → `+` → add
`/usr/local/bin/kanade-agent` (⇧⌘G to type the path). Once, per device.

## Caveats

- Moving from an existing ad-hoc build to the first self-signed build needs
  one re-grant.
- Rotating the certificate changes the leaf SHA-1 and invalidates every PPPC
  profile and manual grant. Plan the expiry, keep the key backed up.
- The self-update path (`crates/kanade-agent/src/self_update.rs`) copies the
  downloaded Mach-O bytes verbatim and only changes the mode bits; nothing on
  the device re-signs or strips. Neither `setup-agent.sh` (xattr removal only)
  nor the release → object-store publish touches the binary.
- `release.yml` is kata-managed (`.kata/applied.toml`) and overwritten on
  `kata apply`. The missing hook steps for `scripts/ci/release-post-build.sh`
  must also exist in the upstream templates (`yukimemi/pj-rust-cli`,
  `yukimemi/pj-rust-workspace`); otherwise a re-apply silently drops signing.

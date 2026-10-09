# macOS code signing (stable identity)

macOS TCC (Files and Folders, Documents, Photos, Full Disk Access) keys
grants on the binary's code identity. The release build only carries the
linker's ad-hoc signature, whose identity is the cdhash — it changes on
every build, so every self-update looks like a new app and prompts again.

The release workflow therefore signs `kanade-agent` (and any other shipped
macOS binary) with a **stable self-signed code-signing certificate**. The
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
- After packaging, the same check runs against the unpacked `.tar.gz`, still
  inside the `build` job, so `release` / `publish` (`needs: build`) are blocked
  on failure.
- Without the secrets the step prints a notice and skips (forks keep
  building). With only one of the two secrets, or on any import/sign/verify
  failure, the build fails.

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

Commit `deploy/macos/signing-cert.sha1`. The release then fails if the
imported certificate or a signed binary does not match it. Until the file
exists the build only prints the imported SHA-1 and checks the binaries
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
  `kata apply`. The two hook steps calling `scripts/ci/release-post-build.sh`
  must also exist in the upstream templates (`yukimemi/pj-rust-cli`,
  `yukimemi/pj-rust-workspace`); otherwise a re-apply silently drops signing.

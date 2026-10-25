# macOS Client GUI (stage 2)

The production Client UI now builds on Apple Silicon for macOS 14 or later.
Windows retains its adapters and existing behavior; Linux and Intel macOS still
run the shim. This stage does not implement native notifications, URL registration
or full cold/warm deep-link handling, tray/close-to-hide, session launchers,
LaunchAgents, emergency fallback, pkg installation (see the package section
below: only an unsigned payload-only build artifact exists), deployment jobs or updates.

## Build a local app

On an Apple Silicon Mac with the pinned Rust toolchain, Xcode command-line tools,
Bun and Python 3.11+:

```sh
sh scripts/build-client-macos.sh
```

The script builds the SPA with the frozen lockfile, then builds the default
`custom-protocol` production binary with `MACOSX_DEPLOYMENT_TARGET=14.0`, assembles
`target/aarch64-apple-darwin/release/bundle/macos/Kanade Client.app`, and ad-hoc
signs and validates it. It needs neither a Tauri CLI nor a Vite server. Do not use
`--no-default-features` for a production bundle. The repository-owned workflow
`client-macos.yml` uses the same script and runs Client KLP/policy/storage tests;
Windows checks compile and exercise native capability and shared policy regressions. The existing template CI is intact.

The stable identifier remains `com.yukimemi.kanade-client`; the macOS bundle name
is `Kanade Client`. Cargo and Tauri versions must match. `build.rs` synchronizes
the common Tauri version on supported native GUI hosts and checks the actual
application target before running the helper. `tauri.macos.conf.json` isolates
macOS settings from Windows. The local assembler uses these settings and also
includes third-party notices; it does not build an installer or publish anything.

Ad-hoc signing provides no trusted distribution or notarization. On a managed
test Mac, use the OS's app-specific approval flow if Gatekeeper blocks this
locally built app (Privacy & Security → Open Anyway, according to local policy).
Never disable Gatekeeper globally. Developer ID and notarization remain deferred.

## Build the test .pkg (packaging slice, no activation)

```sh
sh scripts/build-client-macos-pkg.sh            # builds the app first
SKIP_APP_BUILD=1 sh scripts/build-client-macos-pkg.sh   # reuse an existing app
python3 -I scripts/ci/test_client_macos_pkg.py  # validator regressions
```

Output: `target/client-macos-pkg/client/<version>/macos-aarch64-darwin14.pkg`
(the shared `ArtifactIdentity` key `client/<version>/macos-aarch64-darwin14.pkg`)
plus `target/client-macos-pkg/build-record.json` (SHA-256, size, source commit,
identity). The record is a build note, not a signed package manifest. Only plain
numeric `x.y.z` versions are accepted; workspace, Tauri, `Info.plist`, package
and component versions must agree. Identifiers are fixed: bundle
`com.yukimemi.kanade-client`, component `com.yukimemi.kanade-client.pkg`, product
`com.yukimemi.kanade-client.product`.

The package is a `productbuild` distribution around a `pkgbuild` component. It is
**payload-only**: exactly `/Applications/Kanade Client.app`, owned `root:wheel`
(directories and the executable 0755, other files 0644), non-relocatable, with no
scripts, LaunchAgent, session helper or resources. The distribution requires
arm64 and macOS 14.0+, local-system domain only. Payload content is reproducible;
the `.pkg` bytes are not (timestamps). Before packaging, the app is validated
(identity, versions, every Mach-O's arch/deployment target/dependencies, resources,
ad-hoc signature, embedded SPA) and again after the payload is restored. Package
validation lists archive members and payload paths before expansion, then compares
BOM, `PackageInfo`, `Distribution` and the restored payload with the input app.
Rejected: extra members, scripts, symlinks, hard links, `..`/absolute paths,
duplicates, wrong owner/mode, tampered or unsigned bundles. AppleDouble `._*`
carriers (build hosts may stamp `com.apple.provenance`) are tolerated only for that
one xattr and never appear in the restored tree.

### Signing boundary

The app is ad-hoc signed; the **package is unsigned** (`pkgutil --check-signature`
reports no signature). An ad-hoc app signature does not make the pkg
Gatekeeper-trusted. Developer ID Application/Installer signing and notarization
are deferred and need paid credentials; nothing re-signs on the target and
Gatekeeper/quarantine are never disabled globally. Publishing as a release asset
needs upstream (kata-managed) release-workflow work; this repo only uploads a
7-day CI artifact from `client-macos.yml` and does not edit managed workflows.

### Ownership contract for the later typed agent handler

* Install target is fixed: `/Applications/Kanade Client.app`; receipt id
  `com.yukimemi.kanade-client.pkg`.
* The pkg cannot protect an unmanaged bundle: the installer's overwrite/version
  settings are explicit (`BundleOverwriteAction=upgrade`, version check off so an
  operator can reinstall an older version) but do not stop replacement of a
  foreign app with the same identifier. The handler must first compare receipt,
  bundle identifier, `Info.plist` and managed ownership, and refuse implicit
  takeover of any unmanaged or differently-identified bundle.
* Uninstall (later) removes only a verified managed bundle and its receipt. It
  must preserve every user's `Library/Application Support/Kanade/Client`,
  `Library/Logs/Kanade/Client`, caches and UI state, must not walk home
  directories, and must not stop or launch GUI sessions on its own.
* Verification after install reads the receipt and `Info.plist` version back.

### Not done (acceptance prerequisites)

* **Automatic startup is not implemented**: there is no LaunchAgent, session
  helper or opt-out store, so this package does not enable login start and must
  not be described as doing so. Later integration owns the Aqua `--tray`
  LaunchAgent, session helper and opt-out (SPEC 2.11.7.4).
* Not run in this change: real `installer` receipt generation, install on a clean
  macOS 14 VM, Gatekeeper behavior, GUI session, KLP against an installed agent,
  update/uninstall, session/opt-out. CI checks package structure only.

## Runtime boundaries

The existing connection supervisor and KLP client use the agent's Unix socket;
no NATS credentials or new transport is introduced. Manual startup shows the UI.
`--tray`, `--show-notification <id>` and `--resurface` start hidden using the shared
policy. The existing single-instance plugin forwards a repeated manual launch
to show the running window and keeps repeated hidden modes hidden. No stronger
UID/session instance contract or URL-open event queue is claimed in this stage.

No macOS notification plugin is registered or granted permissions. The Rust
capability command returns history-only using unknown shared session facts; the
SPA waits for it before considering automatic notifications and skips permission
requests, native toast calls and failure-driven reveals. The unavailable native
command returns an error as a backstop. Manual navigation, history, read/ack and
jobs continue through KLP independently of notification eligibility.

Account home comes from `getpwuid_r(geteuid())`, never inherited HOME. The macOS
storage adapter uses the shared Library paths and ownership policy, with pinned
directory descriptors, `O_NOFOLLOW`, 0700 private directories and 0600 regular
single-link files. Existing foreign ownership or unsafe modes are rejected,
not repaired or adopted. Logs go to `Library/Logs/Kanade/Client/client.log`;
size goes to `Library/Application Support/Kanade/Client/window-size.json` under
the resolved account home. Missing, corrupt or unwritable state never blocks GUI
startup; save/restore failures warn. Only logical dimensions are persisted.
Sizes use an atomic descriptor-relative temp-file rename. WKWebView uses an ephemeral datastore,
so no persistent WebKit data escapes into unmanaged identifier directories.
The SPA stores only its theme preference in localStorage; on macOS that choice
is session-only and the next launch follows the system theme. Cache removal loses
no read/ack state, which stays authoritative in the agent.
Log rotation and persistent WebKit storage remain later operational work.

## Manual acceptance (requires a GUI session and a running agent)

Use the locally built app in its build directory; do not replace an installed
application, restart the agent or run two test sessions against an existing
Client. Test only when the operator's Client is not already running, because
the stable identity and existing single-instance guard intentionally overlap.

```sh
open 'target/aarch64-apple-darwin/release/bundle/macos/Kanade Client.app'
# After quitting that test instance:
open 'target/aarch64-apple-darwin/release/bundle/macos/Kanade Client.app' --args --tray
```

With no Vite process listening, confirm the actual WKWebView shows the production
UI, KLP connects, notification history loads, and read/ack changes survive a
relaunch. Resize, quit and relaunch to verify logical size restoration. Confirm
manual startup is visible and each hidden mode stays hidden even with incoming
notifications or a failed toast capability. A repeated hidden launch must exit
promptly without revealing the first window; repeated manual launch should show
it. Quit only the test instance. Verify private file modes and retry with corrupt
or unwritable size state. Confirm cache deletion does not remove window size or
agent history. Test on macOS 14 itself before claiming minimum-OS acceptance;
a newer build host plus a deployment-target check does not prove that acceptance.

CI validates compilation and bundle structure, not a real GUI session, KLP access
to an installed agent, Gatekeeper approval, notification delivery or fleet rollout.

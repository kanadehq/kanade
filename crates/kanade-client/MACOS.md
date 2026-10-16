# macOS Client GUI (stage 2)

The production Client UI now builds on Apple Silicon for macOS 14 or later.
Windows retains its adapters and existing behavior; Linux and Intel macOS still
run the shim. This stage does not implement native notifications, URL registration
or full cold/warm deep-link handling, tray/close-to-hide, session launchers,
LaunchAgents, emergency fallback, pkg installation, deployment jobs or updates.

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

# macOS remote viewing (phase 1)

The browser Remote Screen page can view the logged-in user's **primary display
only**, on **macOS 14 Sonoma or later** (Apple Silicon and Intel). This uses
[ScreenCaptureKit's SCScreenshotManager](https://developer.apple.com/documentation/screencapturekit/scscreenshotmanager),
not the deprecated CGDisplayCreateImage API. Older macOS agents keep working for
other operations, but report remote viewing as unsupported.

Sessions are strictly read-only: no Accessibility permission is needed, no input
subject is subscribed, and requests enabling input or selecting a secondary
display are refused. The backend's existing operator role, Remote feature,
PC access and audit consent checks still apply.

## Deployment and permission

Update both backend and agent. Use the normal installed agent executable and
LaunchDaemon service, with an executable path and parent directories traversable
by the console user. A daemon running as root cannot capture the desktop itself:
it starts the **same installed executable** with `--session-capture`, through
`launchctl asuser` **and** `sudo -n -u` (`run_as: user`). The child receives a
fresh explicit user environment, never daemon credentials. A non-root development
agent may capture only when it is the current console owner. No logged-in user
means a refused session, not a silent capture of the login window.

On the target Mac, as the logged-in user:

1. Open **System Settings → Privacy & Security → Screen Recording**, or
   **Screen & System Audio Recording** on newer versions.
2. Enable the capture process macOS identifies. If necessary, use the `+` button
   and file chooser to select the installed `kanade-agent` executable. The
   refusal message includes the console UID and actual executable path.
3. Restart the installed agent service after changing permission, then stop and
   reconnect the browser viewer. Verify through the deployed LaunchDaemon, not
   just a Terminal-launched development binary.

The preflight check detects missing permission without waiting indefinitely for
an interactive dialog. Kanade cannot grant TCC permission remotely. TCC may
attribute capture to a responsible launcher depending on the launch context;
adding a fixed binary path is **not a guarantee** that a particular installed
launch chain will be authorized. Inspect the entry macOS actually presents.
Terminal permission alone does not establish that the LaunchDaemon path works.
Use the existing [stable code signing procedure](macos-code-signing.md) for
releases; ad-hoc development builds and identity/path changes may need new grants.
Do not change signing identity, entitlements or privacy database settings just to
bypass a refusal.

A capability in fresh ping/heartbeat replies distinguishes supported from
unsupported agents. It describes implementation/OS support, not permission or a
currently usable desktop. Older agents omit it: the backend attempts Start
rather than guessing. A missing remote responder can mean an older/unsupported
agent or a connection problem; it does not prove the agent is offline.

## Lifecycle and limits

The agent accepts Start only after the first valid JPEG tile, with an 8-second
initialization deadline (below NATS's 10-second request default and the backend's
15-second start deadline). Enumeration and screenshot operations each have a
3-second native timeout. Initialization errors return the actual permission,
missing-user/display or capture failure reason. Repeated Start with the same ID
is idempotent; a different session cannot take over. Stop must name the owning
session and is idempotent after shutdown.

Capture polls full screenshots at up to 10 fps, using the existing BGRA-to-JPEG
encoder, horizontal tile splitting, payload limits, NATS frame headers and browser
canvas. Framework image dimensions are used for actual transmitted geometry;
pixels are copied into owned tightly packed BGRA storage rather than assuming a
framework row stride. A static desktop still emits snapshots. This costs more
bandwidth/CPU than Windows dirty rectangles; the requested fps is a ceiling.

Stop, viewer disconnect, initialization failure, child exit, stream timeout,
publish failure and loss of the control subscription release the child. A
stdin lifetime pipe also stops capture if the daemon dies, even on a static
screen. The parent kills/reaps the dedicated child process group; an old
publisher's completion cannot clear a replacement session. Runtime capture
failure appears as a screen-unavailable gap with a reason: stop and reconnect.

No multiple-display selection, input control, Retina/native-pixel refinements,
HDR fidelity guarantees or broad lock/sleep/display-change recovery are included.
Protected content may be blank. Logout/user switch ends capture; reconnect in the
new logged-in session. Headless or locked/unavailable desktops are not promised
to work. Native SDK compilation requires Xcode Command Line Tools with a macOS
14+ SDK; the bridge is embedded in the agent, with no separately installed helper
or Swift runtime requirement.

## Manual acceptance procedure

Use a test Mac with a visible logged-in desktop, the installed signed agent and a
backend connected to the same broker. Do not treat a development-shell test as
proof of installed TCC behavior.

1. With Screen Recording denied for the actual installed capture context,
   connect from an authorized operator account and accept the audit notice.
   Expect a refusal describing Settings, console user, permission target,
   restart and reconnect. Other agent operations/ping should still work.
2. Grant permission as above and restart the service. Connect again. Confirm a
   changing clock, a moving window and text in all four corners appear right-side
   up with correct colors and no padded/corrupted rows. Check the reported size
   matches the canvas; test a noisy full-screen image to exercise tile splitting.
3. Confirm mouse/keyboard activity in the browser does not control the Mac. On
   a test broker/control client, Start with `allow_input: true` or
   `output_index: 1` must be refused without launching capture.
4. Open a second viewer: it must be refused while the first holds the display.
   Close/Stop the first, then reconnect the second. Retry Start/Stop with the
   same session ID, and send a Stop naming a different ID: it must not disrupt
   the owner. Repeat connect/disconnect and check Activity Monitor for orphaned
   `--session-capture` children or continuing CPU use.
5. On a static desktop, terminate the capture child, then kill/restart the agent
   service separately. Expect an unavailable reason after child failure, no
   orphan after parent failure, and successful reconnect after restarting.
   Log out/switch users: no pixels from another user's session should leak;
   reconnect only after login and permission checks succeed for that user.
6. Check a pre-14 Mac or another unsupported platform reports unsupported.
   Check an older agent without capability reporting still attempts Start and
   gives a responder/version/support/connection explanation when no subscriber
   exists. Disconnect an agent from NATS and verify the UI does not assert that
   every missing responder means offline.
7. Verify viewer-only accounts, accounts lacking the Remote feature or target PC
   access, and sockets without audit consent remain refused. Run the Windows
   viewer smoke test to check DXGI capture and reconnect behavior are unchanged.

Automated tests cover protocol compatibility, framing/JPEG tile splitting,
read-only/output refusal, startup failure and session ownership/cleanup. They do
not establish desktop capture quality, TCC attribution/grant persistence, the
installed LaunchDaemon launch chain, logout or sleep/lock behavior. Record the OS,
signing/deployment route and results of the steps above before rollout.

# Client App demo

```sh
cargo make demo-client        # browser  → http://localhost:1421
cargo make demo-client-app    # real window (Windows + `cargo install tauri-cli`)
```

Runs the **end-user Client App against fixtures** — no agent, no NATS,
no PC. Companion to `cargo make demo` (the operator SPA,
`crates/kanade-backend/web/demo/`).

Use the browser for most screenshots; it is instant and needs nothing
installed. Use the window for the two things a tab cannot show: the app
with no address bar, and **real OS toasts** — which is where a notice
actually reaches someone who is not looking at the app.

### What `demo-client-app` does to your machine

It registers `kanade-client://` for the current user, and removes it
again on exit.

That is not incidental. The native toast activates through that scheme
(#647), and on any machine with kanade installed it is registered
machine-wide to the shipped binary — so clicking the demo's own toast
opened the real client, in front of whoever was being shown the
product. Observed, not theorised.

The registration is written under HKCU (which wins over HKLM for this
user), carries a marker, and is only ever removed when that marker is
present — so it cannot silently delete a registration you put there
yourself. It is torn down in a `finally`, which covers a closed window
and Ctrl-C but not a hard kill. To check or clean up by hand:

```sh
pwsh scripts/demo-client-protocol.ps1            # status, changes nothing
pwsh scripts/demo-client-protocol.ps1 -Unregister
```

While the demo is not running, a leftover registration points at a dev
binary that needs the Vite server — clicking an old toast then opens a
window that cannot load. That is the reason the teardown is wired into
the task rather than left to the operator.

## How it is wired

The app's only contact with the outside world is `invoke()` and
`listen()` from `@tauri-apps/api` — there is no HTTP layer, so the SPA
demo's trick (repointing Vite's `/api` proxy) has no equivalent here.
The same idea applies one level down instead: `vite.config.ts` aliases
the Tauri modules to shims under `demo/` when `KANADE_CLIENT_DEMO=1`.

| module | shim |
| --- | --- |
| `@tauri-apps/api/core` | `demo/tauri-core.ts` — the 18 `invoke` commands |
| `@tauri-apps/api/event` | `demo/tauri-event.ts` — the 6 KLP events |
| `@tauri-apps/plugin-notification` | `demo/tauri-notification.ts` |

The app is **untouched and unaware**, and nothing demo-shaped can reach
the product bundle: the aliases are absent from the config unless the
env var is set, so a normal `bun run dev` (what `tauri dev` invokes)
resolves the real modules. The demo also serves on **:1421**, leaving
:1420 — pinned by `tauri.conf.json::build.devUrl` — free for a real
`tauri dev` alongside.

The notification plugin is aliased for a second reason worth knowing:
its own bundle imports `addPluginListener` from `@tauri-apps/api/core`,
so the core alias reached into a dependency's internals and broke the
build on a missing export. Shimming the plugin is a tidier boundary than
growing the core shim to satisfy whatever a third-party package happens
to import.

## What the demo actually does

Static fixtures would show the screens. These don't, because the
interesting part of this app is that pressing something changes what
happens next:

- **State is mutable.** 確認 / 取り消し really flip a notification's
  state; the panel and the unread badge follow.
- **Jobs stream.** Pressing 実行 pushes `jobs.progress` events on real
  timers, so output appears line by line — you can watch a run work
  rather than see a finished block appear at once.
- **A notification arrives while you watch.** Eight seconds after
  connect, one is pushed as if an operator had just sent it. That moment
  is the whole reason the notification feature exists, and it is the one
  thing a screenshot cannot show.
- **The support unlock rejects a wrong code**, and a correct one
  (`123456`) makes a gated job *appear* — the agent applies that gate
  when it builds the list, so a locked job is absent, not greyed out.
  Mirroring that is the difference between demoing the feature and
  demoing a disabled button.
- **Every call has ~60 ms of latency.** An app whose actions land in
  zero milliseconds photographs fine and demos badly: the spinners and
  disabled states never appear, so a viewer never sees the app handle
  them.

## Adding a fixture

An unmocked `invoke` logs one line and returns `null`:

```text
[demo-client] unmocked invoke("foo") — returning null
```

Add the case to `demo/tauri-core.ts`; the response shapes are the `type`
declarations near the top of `src/main.ts`.

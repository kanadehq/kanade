# Demo stack

```sh
cargo make demo        # → http://localhost:5173, sign in with ANY username/password
```

Brings the SPA up against a **mock backend** (`server.ts`, port 8082)
serving an **invented 248-PC fleet** (`fleet.ts`). No NATS, no SQLite,
no backend binary, no agent — one command, no infrastructure.

Use it for:

- **Screenshots** for docs / marketing / release notes, without real
  hostnames and sign-in accounts ending up in a public image.
- **UI work** on pages whose real data is awkward to produce locally
  (a fleet of 248, a half-adopted rollout, a compliance board with
  actual failures on it).
- **Showing the product** on a laptop with no deployment behind it.

## How it is wired

`cargo make demo` runs two tasks in parallel:

| task            | what                                                    |
| --------------- | ------------------------------------------------------- |
| `demo-api`      | `bun --watch demo/server.ts` on :8082                    |
| `web-dev-demo`  | Vite dev server on :5173, `BACKEND_PROXY=http://localhost:8082` |

The SPA is **untouched and unaware**: `vite.config.ts` already proxies
`/api` to `$BACKEND_PROXY`, so the demo swaps the backend out from
underneath it. There is no demo-mode branch in the product, and nothing
here ships in the binary.

Both halves hot-reload — edit `fleet.ts` or `server.ts` and the mock
restarts, the same way the SPA reloads on a `src/` edit.

Run either half on its own with `cargo make demo-api` /
`cargo make web-dev-demo` (e.g. to point a phone at the demo over
Tailscale, which `web-dev`'s `allowedHosts` already permits).

## Auth

Any username and password is accepted, and the session is an admin with
no feature restrictions. A demo that makes you find a password is a
demo nobody runs.

Which means **the login screen is not an authentication boundary** —
it is a screenshot of one. Anyone who can reach the port is an
administrator of the mock, so keep it on a trusted network: loopback,
or a tailnet as above. Do not put it on a public interface or behind a
reverse proxy that terminates on the open internet.

Nothing real is behind it — the mock holds invented data and talks to no
backend, NATS, or PC — so the exposure is of the fixtures, not of a
fleet. It is still worth being deliberate about, because this is the one
part of the demo that looks exactly like the product's real front door.

## The fixtures

`fleet.ts` seeds a PRNG, so the same PC keeps the same hostname, owner,
model, disk and agent version across restarts — screenshots taken a
week apart stay comparable. It carries *offsets* rather than absolute
timestamps; `server.ts` resolves them against the wall clock per
request, so "3m ago" and "in 2h" stay honest however long the demo has
been up.

The fleet is deliberately **not** perfect: 17 hosts are offline, the
rollout is at ~80%, and the compliance board has real failures with
real reasons. An all-green board photographs well and tells the viewer
nothing about what the product is for.

Three constraints are easy to break:

- **Cross-page agreement.** The OS pie, the Inventory OS column and the
  `os_eol` check counts are all *counted from the fleet*, not written
  by hand. Two screenshots that contradict each other is exactly the
  detail a viewer notices.
- **The Events page's default window.** `EVENT_PC_COUNT` and the event
  density are tuned so the swimlane's default view (200-row limit, 2
  days back to midnight) is *fully covered*. Raise either and the page
  opens on its truncation warning with half the strip hatched as "not
  fetched" — correct behaviour, terrible first impression.
- **Every host needs a boot or shutdown inside that window.**
  `buildLanes` decides whether a host has winlog at all by asking
  whether any power event is present in the fetched set
  (`hasPower`, `OperationalTimeline.tsx:800`). A host with none falls
  back to the #970 idle-sampler backfill, which paints power and
  session straight across the night. The laptop persona suspends
  overnight instead of shutting down, so it trips exactly that — hence
  the staggered every-other-night shutdown in `eventsForPc`.

## Adding a fixture

Unimplemented routes answer `[]` and log one line:

```text
[demo-api] no fixture for GET /api/foo — returning []
```

So a page that needs more than the demo covers says so in the terminal.
Add a `get(/^\/api\/foo$/, …)` next to its siblings in `server.ts`; the
response shapes are the `type` declarations at the top of the consuming
page in `src/pages/`.

Two things to know when adding one:

- **The `[]` fallback only saves list pages.** A *detail* route
  (`/api/results/{id}`) is read as an object, so falling through to `[]`
  crashes the page with `Cannot read properties of undefined`. Detail
  handlers here answer **404** on an unknown id instead, which is what
  the SPA's own not-found handling expects.

  This is the single most common way to ship a broken demo, and it was
  found three times by clicking — results, schedule coverage,
  notification detail — before anyone thought to look for it
  systematically. Don't wait to be told. Every route the SPA can build
  is discoverable from the source, so before shipping a change that
  adds pages or fixtures, sweep them:

  ```sh
  # every /api path the SPA can construct
  rg -o "[\`'\"](/api/[^\`'\"[:space:]]*)" -r '$1' src --no-filename | sort -u
  ```

  Substitute a real id for each `${…}` hole, GET them all against the
  running mock, and treat "200 with a body of exactly `[]`" as
  unimplemented. That flags POST-only routes too (`/api/run`,
  `/api/…/kill`), which are fine — the ones that matter are the GETs a
  page renders from. Then open those pages: a route that returns a
  *list* degrades to an empty table, which is acceptable; a route that
  returns an *object* takes the whole page down, which is not.
- **Literal routes beat parameterised ones**, whatever order you write
  them in — the dispatcher sorts on "does this pattern capture?" before
  matching. That is deliberate: relying on registration order meant
  `/api/agents/([^/]+)` answered `404 agent not found` for
  `/api/agents/releases`, a literal route defined 1,300 lines further
  down, and the Rollout page silently lost its release list. Write the
  route wherever it belongs; you don't have to think about this.

Detail rows are derived from the same builder as their list row
(`RESULTS`, `FLEET`) rather than regenerated. Clicking a row and landing
on a *different* job or host is the most obvious way for a demo to fall
apart in front of an audience.

`demo/` is inside `tsconfig.app.json`'s `include`, so `bun run build`
(and therefore CI) type-checks it. It never enters the bundle, because
Vite builds from `index.html` and nothing reachable from there imports
it — `noEmit` governs whether *tsc* writes files and has no say in what
Vite bundles. The include exists purely so a
mistake here fails the build instead of surfacing when the mock crashes
at run time. It earned its keep immediately: turning it on caught an
unused parameter that Bun's type-stripping had been happily ignoring.

# Screenshots

What the operator console looks like in use. Every image on this page was
captured from the demo stack (`cargo make demo`), which serves an invented
248-PC fleet from a mock backend — no NATS, no agents, no real hosts. Names,
hostnames and sign-in accounts are fiction; nothing here came from a
deployment.

Desktop captures are 1440×900, mobile 390×901.

The console is shown in English here; the Japanese edition of this page has
the same shots with the interface in Japanese. The *data* stays Japanese in
both, because the fixture is a Japanese company — widget titles, group names
and check descriptions are operator-authored, so they are whatever the
operator wrote, not something the interface translates.

## The fleet at a glance

The dashboard opens on the numbers an operator checks first: how many agents
are reporting, how many have gone quiet, whether the broker's streams are
within their limits, and how many job runs failed in the last day. Each figure
links through to the page that explains it.

![Dashboard](../images/screenshots/dashboard-light-en.jpg)

The pinned widgets below are operator-defined — they come from the same
analytics views you can build yourself, pinned to the dashboard from the
Analytics page.

## Agents

Liveness only: who is online, on which agent version, and when each host last
checked in. Detail belongs to Inventory; this page answers "is it there".

![Agents](../images/screenshots/agents-light-en.jpg)

The columns are yours to choose — the shot above hides the ones this fleet
does not need, which is what the columns picker is for.

## Inventory

Whatever your manifests collect. The probes are operator-authored PowerShell
tagged `inventory:` in a YAML job, so the columns reflect what you decided to
gather, not a fixed schema. Clicking a row opens that PC's full facts.

![Inventory](../images/screenshots/inventory-light-en.jpg)

## Compliance

Fleet-wide status for every health check, grouped per check with its own
counts. A check is just a job carrying a `check:` hint, so adding one is
writing a manifest — there is no hard-coded list of things kanade knows how to
verify.

![Compliance](../images/screenshots/compliance-light-en.jpg)

The detail column is the point: it says *why* a host is unhappy, in that
host's own terms.

## Uptime and activity

Power, session, sleep and active intervals reconstructed from Windows event
log entries, one lane per kind. A blank lane means no event of that kind in
the window — the machine was off, or idle.

![Events](../images/screenshots/events-light-en.jpg)

## Analytics

Aggregations over whatever your jobs emit. App-usage, browsing history and
inventory facts are shown here; the widgets are defined in YAML and can be
pinned to the dashboard.

![Analytics](../images/screenshots/analytics-light-en.jpg)

## Groups

Declared fleet groups. A dynamic group carries a query the backend
re-evaluates on a schedule, so membership follows the fleet as metadata
changes — stamp `site` and `department` onto an agent once, and a PC that
moves office lands in the right groups by itself. A static group carries a
literal member list, for membership no query can express.

![Groups](../images/screenshots/groups-light-en.jpg)

## Notifications

Send a notice to the Client App and track who confirmed it. Bodies are
Markdown — headings, lists, tables, links — so a fleet-wide notice can be a
document rather than a paragraph.

![Notifications](../images/screenshots/notifications-light-en.jpg)

## Jobs

The manifests themselves, grouped by tag. Everything kanade runs on an
endpoint is one of these.

![Jobs](../images/screenshots/jobs-light-en.jpg)

## Audit

Every dispatch and every operator action, with the payload behind each one.
Filterable by actor, action, target, and a text search into the payload
itself.

![Audit](../images/screenshots/audit-light-en.jpg)

## Accounts

Operator accounts and their page access. Three states are visible at once
here, because they are easy to confuse: unrestricted, a per-account
allow-list, and a shared permission group — where the group governs and the
account's own list, if any, no longer applies.

![Accounts](../images/screenshots/accounts-light-en.jpg)

## JetStream

The broker's own health: every stream, KV bucket and object store the backend
bootstraps, with usage against its limit.

![JetStream](../images/screenshots/jetstream-light-en.jpg)

## Dark theme

The console follows the operating system by default, and can be pinned to
light or dark per browser.

| | |
|---|---|
| ![Dashboard, dark](../images/screenshots/dashboard-dark-en.jpg) | ![Agents, dark](../images/screenshots/agents-dark-en.jpg) |
| ![Compliance, dark](../images/screenshots/compliance-dark-en.jpg) | ![Uptime, dark](../images/screenshots/events-dark-en.jpg) |

## On a phone

Below 1024px a data table stops being a table: each row becomes a card and
every value carries its column name. No horizontal scrolling, no pinch-zoom
to read a cell.

| | | |
|---|---|---|
| ![Dashboard on a phone](../images/screenshots/dashboard-mobile-en.jpg) | ![Agents on a phone](../images/screenshots/agents-mobile-en.jpg) | ![Compliance on a phone](../images/screenshots/compliance-mobile-en.jpg) |

## The Client App

The half that lands on the end user's desk. It is a small tray application
on the PC itself — the health tab they check, the jobs they can run without
raising a ticket, and the notice they have to confirm. It talks to the agent
over a named pipe on the same machine; it never reaches the backend directly.

These are shown at 880×560, the app's own window size, and in Japanese only:
the Client App has no i18n layer — its strings are written into the source —
so an English edition of these shots does not exist to capture.

Opening on the two things the user is being asked to look at:

![Client App home](../images/screenshots/client-home.jpg)

### Device health

The same checks the operator sees on the Compliance page, from the other
side. A warning carries the operator's own explanation and the remedy, so
the first move is not "open a ticket".

![Device health](../images/screenshots/client-health.jpg)

### Notices

Sent from the Notifications page, tracked back to who confirmed. Expanding
one renders the Markdown the operator wrote — headings, a numbered
procedure, a table, a quote, a link:

![Notification list](../images/screenshots/client-notifications.jpg)

![Notification, expanded](../images/screenshots/client-notification-detail.jpg)

### Self-service

Jobs an operator has published to the user, grouped by category. Each is an
ordinary manifest; what makes it appear here is the operator deciding it
should.

| | |
|---|---|
| ![Updates](../images/screenshots/client-updates.jpg) | ![Troubleshooting](../images/screenshots/client-troubleshoot.jpg) |

A job can ask before it runs — `confirm:` in the manifest, with the
operator's own wording — and the run streams its output as it goes, rather
than appearing complete after a silence:

| | |
|---|---|
| ![Confirmation](../images/screenshots/client-confirm.jpg) | ![A run in progress](../images/screenshots/client-running.jpg) |

The catalog is the same mechanism pointed at installs:

![Catalog](../images/screenshots/client-catalog.jpg)

## Running it yourself

```sh
cargo make demo         # the operator console
cargo make demo-client  # the Client App
```

Then open <http://localhost:5173> and sign in with any username and password.
Nothing else needs to be running. See
`crates/kanade-backend/web/demo/README.md` for what the mock covers and how to
add to it.

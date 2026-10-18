# Lost-device location

Lets an authorised operator see a managed endpoint's last known position and
ask for a fresh reading from the device detail page. It is **location
visibility only** (no remote lock or wipe) and it is off by default.

## Enabling

1. An administrator enables it: `PUT /api/server-settings/location` with
   `{"enabled": true, "retention_days": 7}` (admin only; there is no SPA toggle
   yet). `retention_days` is 1-30, default 7.
2. Accounts that need it must hold the `location` feature **and** the Operator
   role. Both `GET /api/agents/{pc_id}/location` and
   `POST /api/agents/{pc_id}/location/request` enforce both.
3. Use NATS user authentication with **unique credentials for each endpoint**.
   The shared `agent` role in `configs/nats-server.users.conf` deliberately has
   no location subscription permission. Provision a separate user per endpoint
   using the agent role's existing grants plus exactly
   `location.req.<pc_id>` in its subscribe allow list (preserve hostname casing).
   Set that user's `allow_responses` to `{ max: 1, expires: "15s" }` so a
   five-second OS acquisition can answer; NATS's default response grant expires
   after two seconds. Never grant `location.req.>` or another endpoint's
   subject. Keep the backend's publish grant and private `location.reply.*`
   subscription grant. Replies use this private namespace rather than the
   shared `_INBOX.>` namespace. Distribute the unique credentials to that
   endpoint and restart
   it with `KANADE_LOCATION_ENABLED=1` in the agent process environment
   (the listener is off otherwise); verify both its own subscription and
   rejection of a different endpoint's subscription at the broker before enabling this feature. Provisioning these
   users is currently manual; shared-token and shared-agent credentials are
   unsuitable for location because they cannot authenticate which endpoint
   answered. Without the exact grant, requests return unavailable or timeout.

Disabling the feature clears every stored coordinate before the setting is
saved; if clearing fails the save is refused and the feature stays enabled.
Re-enabling clears coordinates again, so nothing kept while disabled can
reappear. Deleting an endpoint clears its row. The cleanup sweep clears readings older than the
retention window, and the API withholds them even before the sweep runs.

## Behaviour

- Nothing is collected passively and heartbeats carry no coordinates. A reading
  exists only because an operator pressed *Request location*.
- The request is a core-NATS request/reply (not a persisted job) with a 15 s
  expiry; the agent refuses an expired request. One request per endpoint at a
  time, and at least 5 s between requests.
- The backend stores the **last successful reading** and the **latest request
  outcome** separately. A failure never overwrites the coordinates or their
  acquisition time.
- A position is shown as *current* only when the latest request succeeded and
  the reading is under 2 minutes old and the heartbeat is under 90 seconds
  old; otherwise it is labelled *last known*.
  An endpoint whose heartbeat is older than 90 s is not asked at all and is
  recorded as `offline`. Readings dated before the request, or in the future,
  are rejected as `unavailable` (30 seconds of clock skew tolerated). OS fix
  timestamps are preserved; receiving a cached fix does not renew its age.
  The detail card refreshes status every 10 seconds and expires its current
  label locally if the data or refresh becomes stale.
- Outcomes are explicit: `success`, `unsupported`, `permission_denied`,
  `location_disabled`, `no_user_session`, `unavailable`, `timeout`, `expired`,
  `offline`.
- Coordinates, accuracy, acquisition time and source are shown. Windows
  reports satellite, Wi-Fi, cellular or IP (labelled as coarse); macOS does not
  say, so its source is `unknown`. An OpenStreetMap link opens only on click
  and sends the coordinates to that third party. kanade adds no IP geolocation
  and uses no paid service.
- Views are audited once per caller and endpoint in each five-minute window
  (polls and tabs are coalesced; anonymous callers share a window per endpoint).
  Each view event includes `dedup_window_seconds`. Collection requests and
  setting changes are audited individually; collection responses do not emit
  extra view events. The in-memory deduplication cache is bounded to 4096
  caller/endpoint pairs; eviction or a backend restart can produce an extra
  view event within a window. **Coordinates are never put
  in audit records**, the normal agent API, search or export. Ad-hoc SQL and
  SQL-backed widgets/groups cannot access location records, including via views.

## Platform prerequisites and limits

| Platform | Mechanism | Notes |
|---|---|---|
| Windows | WinRT `Geolocator` | Location must be enabled and an interactive console session must exist. |
| macOS | Core Location | Location Services on, authorisation granted to the agent, a console user. |
| Linux | none | Reported `unsupported`. A GeoClue2 path needs a desktop session, an agent policy and a positioning backend; none is verified. |

Limitations to expect:

- OS location APIs are tied to a user session and a per-app grant. The agent
  runs as a service/daemon, so a grant may not be attributable to it; the
  result is then `permission_denied`, `no_user_session` or `unavailable`.
  **This was not verified on real Windows or macOS hardware in this change**
  (the Windows path was not even compiled on this host). A grant to a
  development Terminal does not establish daemon authorisation; verify on the
  installed launch route before relying on it.
- Without GPS hardware, desktops get Wi-Fi or IP-derived positions that can be
  kilometres off; the accuracy field is what the OS reported.
- A powered-off, offline, logged-out or permission-revoked device cannot answer.
  Only a previous reading (with its age) is available, and the device may have
  moved since. This is an aid for lost-device response, not a recovery guarantee.

Core Location runs on a bounded worker RunLoop, rather than blocking delegate
callbacks on the async executor. A missing grant (`notDetermined`) is reported
as unavailable with authorization instructions; denied/restricted grants report
permission_denied. A five-second wait without a fix reports timeout.

API references: [Apple Core Location](https://developer.apple.com/documentation/corelocation/cllocationmanager)
and [Windows coordinate timestamp](https://learn.microsoft.com/en-us/uwp/api/windows.devices.geolocation.geocoordinate.timestamp).

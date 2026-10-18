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
3. The broker must run the role-level user config that allows `location.req.>`
   (`configs/nats-server.users.conf`): the backend publishes, an agent may
   subscribe only to its own `location.req.<pc_id>`.

Disabling the feature clears every stored coordinate immediately. Deleting an
endpoint clears its row. The cleanup sweep clears readings older than the
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
  the reading is under 2 minutes old; otherwise it is labelled *last known*.
  An endpoint whose heartbeat is older than 90 s is not asked at all and is
  recorded as `offline`. Readings dated before the request, or in the future,
  are rejected as `unavailable`.
- Outcomes are explicit: `success`, `unsupported`, `permission_denied`,
  `location_disabled`, `no_user_session`, `unavailable`, `timeout`, `expired`,
  `offline`.
- Coordinates, accuracy, acquisition time and source (`unknown` when the OS
  does not say) are shown, plus an OpenStreetMap link that opens only on click
  and sends the coordinates to that third party. kanade adds no IP geolocation
  and uses no paid service.
- Views, requests and setting changes are audited. **Coordinates are never put
  in audit records**, the normal agent API, search or export.

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

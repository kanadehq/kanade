-- Backend-raised notices: conditions the backend itself observed and wants an
-- operator to see, as opposed to anything an agent or operator published.
--
-- One row per (kind, subject). A condition that persists updates its row
-- rather than adding one, and a condition that clears is marked resolved
-- rather than deleted, so a recurrence reopens the same row. That keeps the
-- table bounded by the number of distinct (kind, subject) pairs, however
-- many connections or hosts feed a notice.
--
--   kind          -- namespaced source + finding, e.g. 'nats_auth.broker_open'
--   subject       -- what the notice is about, from a fixed vocabulary
--   severity      -- 'info' | 'warning' | 'critical'
--   count         -- how many things (connections) the notice currently covers
--   sample_json   -- JSON array of a bounded sample of registered host names
--   first_seen_at -- when this occurrence was first raised (reset on reopen)
--   updated_at    -- when count / sample / severity last changed. Not bumped
--                    by an unchanged poll, so a steady state writes nothing.
--   resolved_at   -- NULL while open
CREATE TABLE backend_notices (
    kind          TEXT NOT NULL,
    subject       TEXT NOT NULL,
    severity      TEXT NOT NULL,
    count         INTEGER NOT NULL DEFAULT 0,
    sample_json   TEXT NOT NULL DEFAULT '[]',
    first_seen_at TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    resolved_at   TEXT,
    PRIMARY KEY (kind, subject)
);

-- State of the broker-authentication audit's last poll, so the health
-- response can say whether "no findings" is a fresh answer or a blind spot.
-- A single row (id = 1).
--
--   status    -- 'ok' | 'incomplete' | 'endpoint_unreadable' |
--                'settings_unreadable'
--   polled_at -- when that status was recorded
CREATE TABLE nats_auth_audit_state (
    id        INTEGER PRIMARY KEY CHECK (id = 1),
    status    TEXT NOT NULL,
    polled_at TEXT NOT NULL
);

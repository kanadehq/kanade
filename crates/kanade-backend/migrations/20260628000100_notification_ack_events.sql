-- Append-only audit log of every confirmation lifecycle event (ack and
-- unack) for a notification. Distinct from `notification_acks`, which is
-- the *current-state* read model the SPA roster aggregates: this table
-- is the source of truth the operator can audit — "this user confirmed
-- at 09:00, retracted at 09:05, re-confirmed at 09:10". A user who later
-- claims "I never saw it" after retracting is contradicted here.
--
-- The notification-acks projector appends one row per
-- `events.notifications.{acked,unacked}.>` event it processes, in
-- STREAM_EVENTS sequence order (single consumer, max_ack_pending=1), so
-- the log preserves the true ordering of toggles.
--
-- `occurred_at` is the agent-stamped instant from the event body (when
-- the user clicked); `recorded_at` is the JetStream publish time
-- (re-projection-stable, matching the other projectors). The UNIQUE
-- constraint makes redelivery / re-projection idempotent: the same
-- (recipient, kind, occurred_at) tuple inserts once.
CREATE TABLE IF NOT EXISTS notification_ack_events (
    notification_id TEXT NOT NULL,
    pc_id           TEXT NOT NULL,
    user_sid        TEXT NOT NULL,
    -- 'acked' | 'unacked'
    kind            TEXT NOT NULL,
    occurred_at     TIMESTAMP NOT NULL,
    recorded_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    account         TEXT,
    UNIQUE (notification_id, pc_id, user_sid, kind, occurred_at)
);

-- The audit timeline for one notification ("show every confirm/retract
-- for notif X, oldest→newest") — the lookup the detail page makes.
CREATE INDEX IF NOT EXISTS idx_notification_ack_events_notif
    ON notification_ack_events (notification_id, occurred_at);

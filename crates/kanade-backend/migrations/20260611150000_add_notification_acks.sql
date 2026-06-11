-- Phase E (KLP notifications): per-recipient ack ledger. The
-- notification-acks projector consumes
-- `events.notifications.acked.>` off STREAM_EVENTS and inserts one
-- row per (notification_id, pc_id, user_sid) so the SPA's
-- `GET /api/notifications/{id}/ack_status` can list who confirmed a
-- notification and when. `{user_sid}` is part of the PK so the same
-- PC's concurrent users (Fast User Switching / RDP) each get their
-- own ack row.
--
-- `acked_at` is the agent-stamped confirmation instant carried in the
-- event body; `recorded_at` is the projection time (re-projection-
-- stable: bound to the message's JetStream publish time, like the
-- other projectors).
CREATE TABLE IF NOT EXISTS notification_acks (
    notification_id TEXT NOT NULL,
    pc_id           TEXT NOT NULL,
    user_sid        TEXT NOT NULL,
    acked_at        TIMESTAMP NOT NULL,
    recorded_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    PRIMARY KEY (notification_id, pc_id, user_sid)
);

-- `notification_id`-leading index for the ack_status lookup. It is
-- already the leading column of the PK, so `WHERE notification_id = ?`
-- uses the PK B-tree — this explicit index is redundant and omitted.

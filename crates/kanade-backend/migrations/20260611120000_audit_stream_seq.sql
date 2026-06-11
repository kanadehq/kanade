-- #501: idempotency key for the audit projector. The plain INSERT
-- meant a JetStream redelivery (ack lost / ack_wait expiry) inserted
-- a duplicate audit row — the permanent record was the only
-- non-idempotent projection. `stream_seq` carries the message's
-- stream sequence; the partial UNIQUE index dedups redeliveries
-- while pre-#501 rows (NULL) stay untouched.
ALTER TABLE audit_log ADD COLUMN stream_seq INTEGER;
CREATE UNIQUE INDEX IF NOT EXISTS idx_audit_log_stream_seq
    ON audit_log (stream_seq) WHERE stream_seq IS NOT NULL;

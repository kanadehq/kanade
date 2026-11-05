-- Only broker-produced named identities carry an expiry. Legacy TCP behavior
-- remains unchanged. APIs must not expose a proven name past this deadline.
ALTER TABLE agents ADD COLUMN nats_identity_expires_at TEXT;
CREATE INDEX nats_identity_expiry ON agents(nats_identity_expires_at) WHERE nats_identity_expires_at IS NOT NULL;

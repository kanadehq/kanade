-- Additive: old notices have no recovered connection evidence.
CREATE TABLE nats_auth_occurrences (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    kind TEXT NOT NULL,
    subject TEXT NOT NULL,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    resolved_at TEXT,
    count INTEGER NOT NULL,
    last_poll_complete INTEGER NOT NULL DEFAULT 1,
    sampled_out INTEGER NOT NULL DEFAULT 0,
    evidence_json TEXT NOT NULL DEFAULT '[]' CHECK(length(CAST(evidence_json AS BLOB)) <= 32768)
);
CREATE UNIQUE INDEX nats_auth_occurrences_open ON nats_auth_occurrences(kind, subject) WHERE resolved_at IS NULL;
CREATE INDEX nats_auth_occurrences_resolved ON nats_auth_occurrences(resolved_at);

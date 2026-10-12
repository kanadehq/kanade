CREATE TABLE install_attempts (
    id TEXT PRIMARY KEY,
    issuer TEXT,
    os TEXT NOT NULL,
    key_hash TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL,
    status TEXT NOT NULL DEFAULT 'unreported',
    download_claimed INTEGER NOT NULL DEFAULT 0,
    seq INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE install_events (
    attempt_id TEXT NOT NULL REFERENCES install_attempts(id) ON DELETE CASCADE,
    seq INTEGER NOT NULL,
    stage TEXT NOT NULL,
    outcome TEXT NOT NULL,
    exit_code INTEGER NOT NULL,
    received_at INTEGER NOT NULL,
    PRIMARY KEY (attempt_id, seq)
);
CREATE INDEX install_attempts_created ON install_attempts(created_at);

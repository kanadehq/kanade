-- #770 account email onboarding: optional email per account + one-time
-- password setup/reset token links.
--
-- `users.email` is nullable — email is opt-in. When set, account
-- creation (or an admin "send link" / self-service "forgot password")
-- mails a one-time URL the user follows to set their own password, so a
-- plaintext password never rides in email.
ALTER TABLE users ADD COLUMN email TEXT;

-- One-time password setup/reset tokens. The emailed URL carries the raw
-- token; only its SHA-256 hash is stored here, so a read of this table
-- can't reconstruct a live link. Single-use (the row is deleted on a
-- successful set), and `username` is UNIQUE so issuing a new token for a
-- user replaces (invalidates) any outstanding one.
--
-- Unlike `users`, this table is NOT preserved across `-WipeDb`
-- (wipe_projector snapshots only `users`): a wipe expires every
-- outstanding link, which is acceptable for short-lived (72h) tokens —
-- an admin re-sends. `expires_at` is an absolute timestamp so a replay
-- with scrambled `recorded_at` can't resurrect a stale token.
CREATE TABLE IF NOT EXISTS password_setup_tokens (
    token_hash  TEXT PRIMARY KEY,            -- hex SHA-256 of the raw token
    username    TEXT NOT NULL UNIQUE
                REFERENCES users(username) ON DELETE CASCADE,
    purpose     TEXT NOT NULL CHECK (purpose IN ('setup', 'reset')),
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    expires_at  TIMESTAMP NOT NULL
);

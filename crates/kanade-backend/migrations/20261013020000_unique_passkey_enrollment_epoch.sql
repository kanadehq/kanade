-- The empty legacy default must never identify multiple account incarnations.
-- Backfill existing accounts, preserving tokens already rotated by recovery.
UPDATE users SET passkey_enrollment_epoch = lower(hex(randomblob(32)))
WHERE passkey_enrollment_epoch = '';

-- SQLite cannot add a column with a nonconstant DEFAULT via ALTER TABLE.
-- Assign a fresh generation inside every INSERT statement instead, covering
-- admin creation, bootstrap, and restores without caller-specific logic.
CREATE TRIGGER users_passkey_enrollment_epoch
AFTER INSERT ON users
WHEN NEW.passkey_enrollment_epoch = ''
BEGIN
    UPDATE users SET passkey_enrollment_epoch = lower(hex(randomblob(32)))
    WHERE username = NEW.username;
END;

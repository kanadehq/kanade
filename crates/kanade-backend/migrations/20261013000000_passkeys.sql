ALTER TABLE users ADD COLUMN passkey_enrollment_epoch TEXT NOT NULL DEFAULT '';
UPDATE users SET passkey_enrollment_epoch = lower(hex(randomblob(32)));
CREATE TABLE user_passkeys (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    username TEXT NOT NULL,
    user_uuid TEXT NOT NULL,
    credential_id TEXT NOT NULL UNIQUE,
    passkey TEXT NOT NULL,
    name TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP,
    last_used_at TEXT
);
CREATE INDEX user_passkeys_username ON user_passkeys(username);
CREATE TRIGGER users_passkey_epoch AFTER INSERT ON users
WHEN NEW.passkey_enrollment_epoch = '' BEGIN
    UPDATE users SET passkey_enrollment_epoch = lower(hex(randomblob(32))) WHERE username = NEW.username;
END;
-- Writer connections intentionally disable foreign keys. Keep account
-- deletion and credential revocation atomic even on those connections.
CREATE TRIGGER users_passkey_delete AFTER DELETE ON users BEGIN
    DELETE FROM user_passkeys WHERE username = OLD.username;
END;
-- Never let an account recreated under an old name inherit orphaned keys.
CREATE TRIGGER users_passkey_create BEFORE INSERT ON users
WHEN NOT EXISTS (SELECT 1 FROM users WHERE username = NEW.username) BEGIN
    DELETE FROM user_passkeys WHERE username = NEW.username;
END;

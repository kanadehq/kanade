-- A proof applies only to the password/TOTP snapshot it verified. Rotate
-- the enrollment generation in the same statement/transaction as any factor
-- change, including admin resets and the password-setup/reset-link paths.
-- Unchanged assignments do not invalidate a proof; NULL-safe comparisons
-- cover MFA activation, rotation and removal.
CREATE TRIGGER users_passkey_factor_epoch
AFTER UPDATE OF password_hash, totp_secret ON users
WHEN OLD.password_hash IS NOT NEW.password_hash
    OR OLD.totp_secret IS NOT NEW.totp_secret
BEGIN
    UPDATE users SET passkey_enrollment_epoch = lower(hex(randomblob(32)))
    WHERE username = NEW.username;
END;

-- v0.44 — RBAC accounts. Human operators authenticate against this
-- table (username + argon2id password) and the backend mints a short-
-- lived HS256 JWT carrying their role. The middleware re-reads this
-- row on every request so `disabled` / `role` changes take effect
-- immediately rather than waiting for the JWT's exp.
--
-- role is a single tier (not a set): admin ⊇ operator ⊇ viewer.
--   viewer   — read-only (GET /api/*)
--   operator — viewer + fleet mutations (exec / kill / schedules / …)
--   admin    — operator + account management
--
-- The shared static token (service token) and KANADE_AUTH_DISABLE
-- bypass this table entirely; they are admin-equivalent by design and
-- carry no row here.

CREATE TABLE IF NOT EXISTS users (
    username       TEXT PRIMARY KEY,
    password_hash  TEXT NOT NULL,                 -- argon2id PHC string
    role           TEXT NOT NULL CHECK(role IN ('viewer','operator','admin')),
    disabled       INTEGER NOT NULL DEFAULT 0,
    must_change_pw INTEGER NOT NULL DEFAULT 0,    -- forced reset on first login
    created_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at     TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

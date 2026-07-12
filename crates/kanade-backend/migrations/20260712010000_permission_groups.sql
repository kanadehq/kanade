-- Named permission groups (#1008 Phase 3): a reusable, live-referenced page
-- allow-list that many accounts can share. Assigning an account to a group
-- means its effective page access IS the group's — editing the group updates
-- every member at once (resolved live in `auth::verify`, not copied).
--
-- `features` is a JSON array of feature keys (same vocabulary as
-- `users.allowed_features`; see `kanade_shared::feature::Feature`). Unlike a
-- per-user allow-list there is no "unrestricted" sentinel here: a group is
-- always a concrete set (an empty array = commons only). Un-restricting an
-- account is done by clearing its group + `allowed_features`, not by a group.
CREATE TABLE IF NOT EXISTS permission_groups (
    name       TEXT PRIMARY KEY,
    features    TEXT NOT NULL,               -- JSON array of feature keys
    created_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,
    updated_at  TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP
);

-- Account → group membership. NULL = not in a group (fall back to the
-- account's own `allowed_features`, which itself may be NULL = unrestricted).
-- A set `permission_group` takes precedence over `allowed_features`. No FK
-- constraint (the pool doesn't run with `PRAGMA foreign_keys = ON`); the API
-- refuses to delete a group that still has members, so a dangling reference
-- shouldn't arise, and `auth` falls back safely if one ever does.
ALTER TABLE users ADD COLUMN permission_group TEXT;

-- `permission_group` is read on the auth hot path (the `LEFT JOIN` in
-- `auth::lookup_user`, run on every authenticated request) and scanned by the
-- group-delete member guard (`WHERE permission_group = ?`). Index it so both
-- stay O(log n) as the account count grows.
CREATE INDEX IF NOT EXISTS idx_users_permission_group ON users(permission_group);

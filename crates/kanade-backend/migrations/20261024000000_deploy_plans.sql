-- Stage 4 of the typed deployment migration (SPEC §2.13): deployment plan
-- records, their single-use application ledger, and the per-agent capability
-- evidence the preflight reads.
--
-- Plans are *authorization state*, not a re-derivable projection: nothing here
-- can be rebuilt from JetStream, and a lost database must never be repaired by
-- guessing which plans had been approved. A plan is immutable once created
-- (no content-edit path); a change is a new plan. `state` / `revision` move only
-- through conditional UPDATEs in the API layer, and the CHECKs below make a
-- contradictory approval state unrepresentable even for a hand-edited row.

CREATE TABLE deploy_plans (
    plan_id         TEXT PRIMARY KEY CHECK (length(plan_id) BETWEEN 1 AND 128),
    record_version  INTEGER NOT NULL CHECK (record_version = 1),
    -- Lowercase hex SHA-256 over the canonical `content` (domain separated).
    plan_digest     TEXT NOT NULL CHECK (length(plan_digest) = 64),
    -- Canonical JSON of the plan content the digest covers.
    content         TEXT NOT NULL,
    -- The typed job manifest the plan was registered with (for the reviewer).
    job_manifest    TEXT NOT NULL,
    job_id          TEXT NOT NULL,
    role            TEXT NOT NULL CHECK (role IN ('backend', 'client', 'cli')),
    -- Unix seconds. Expiry is evaluated against the clock on every use; it is
    -- not a stored state.
    not_before      INTEGER NOT NULL,
    expires_at      INTEGER NOT NULL CHECK (expires_at > not_before),
    state           TEXT NOT NULL CHECK (state IN ('pending', 'approved', 'revoked')),
    -- Bumped by every state transition; conditional updates compare it.
    revision        INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    created_by      TEXT NOT NULL CHECK (length(created_by) > 0),
    created_at      INTEGER NOT NULL,
    approved_by     TEXT,
    approved_at     INTEGER,
    revoked_by      TEXT,
    revoked_at      INTEGER,
    revoke_reason   TEXT,
    CHECK ((approved_by IS NULL) = (approved_at IS NULL)),
    CHECK ((revoked_by IS NULL) = (revoked_at IS NULL)),
    CHECK (approved_by IS NULL OR length(approved_by) > 0),
    CHECK (revoked_by IS NULL OR length(revoked_by) > 0),
    CHECK (
        (state = 'pending'  AND approved_by IS NULL AND revoked_by IS NULL)
     OR (state = 'approved' AND approved_by IS NOT NULL AND revoked_by IS NULL)
     OR (state = 'revoked'  AND revoked_by IS NOT NULL)
    ),
    CHECK (revoke_reason IS NULL OR revoked_by IS NOT NULL)
);
CREATE INDEX deploy_plans_created ON deploy_plans(created_at);

-- One row per (plan, target) ever consumed. The primary key is the replay
-- guard: a second application of the same plan to the same pc_id cannot insert.
CREATE TABLE deploy_plan_applications (
    plan_id     TEXT NOT NULL REFERENCES deploy_plans(plan_id),
    pc_id       TEXT NOT NULL,
    exec_id     TEXT NOT NULL,
    applied_at  INTEGER NOT NULL,
    PRIMARY KEY (plan_id, pc_id)
);

-- Typed deployment capability evidence, taken from ONE heartbeat. All columns
-- are NULL until an agent reports `deploy_capabilities`; a heartbeat that omits
-- the field (older agent, ping reply) never touches them, so it can neither
-- refresh nor clear evidence — freshness comes only from a real report.
--
--   deploy_capabilities           NULL = never reported (unknown, NOT empty)
--                                 JSON {"schema_version":1,"handlers":[..]};
--                                 "handlers":[] = reporting, supports nothing.
--   deploy_evidence_reported_at   the agent's heartbeat time (unix ms); a report
--                                 older than the stored one is ignored.
--   deploy_evidence_received_at   backend receipt time (unix s); the freshness
--                                 clock, immune to agent clock skew.
--   deploy_evidence_agent_version agent_version of that heartbeat; a different
--                                 current version invalidates the evidence.
--   deploy_evidence_protocols / _keys / _os_family
--                                 command_protocols / command_keys / os_family
--                                 of that same heartbeat.
ALTER TABLE agents ADD COLUMN deploy_capabilities TEXT;
ALTER TABLE agents ADD COLUMN deploy_evidence_reported_at INTEGER;
ALTER TABLE agents ADD COLUMN deploy_evidence_received_at INTEGER;
ALTER TABLE agents ADD COLUMN deploy_evidence_agent_version TEXT;
ALTER TABLE agents ADD COLUMN deploy_evidence_protocols TEXT;
ALTER TABLE agents ADD COLUMN deploy_evidence_keys TEXT;
ALTER TABLE agents ADD COLUMN deploy_evidence_os_family TEXT;

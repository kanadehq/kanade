-- Lost-device location visibility: last known successful location and latest request outcome.
CREATE TABLE IF NOT EXISTS device_locations (
    pc_id               TEXT PRIMARY KEY NOT NULL,
    -- Last known successful reading (retained across subsequent failed requests)
    last_latitude       REAL,
    last_longitude      REAL,
    last_accuracy       REAL,
    last_source         TEXT,
    last_acquired_at    TEXT,
    -- Latest on-demand request outcome
    last_request_id     TEXT,
    last_request_at     TEXT,
    last_request_status TEXT,
    last_request_reason TEXT,
    updated_at          TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_device_locations_acquired_at
    ON device_locations (last_acquired_at);

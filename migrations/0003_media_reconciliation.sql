ALTER TABLE cameras ADD COLUMN deleted_at TEXT;

CREATE TABLE media_desired_states (
    camera_id TEXT PRIMARY KEY REFERENCES cameras(id) ON DELETE RESTRICT,
    generation INTEGER NOT NULL CHECK (generation > 0),
    desired_present INTEGER NOT NULL CHECK (desired_present IN (0, 1)),
    main_path TEXT NOT NULL,
    sub_path TEXT,
    record_enabled INTEGER NOT NULL CHECK (record_enabled IN (0, 1)),
    updated_at TEXT NOT NULL
);

CREATE TABLE media_operations (
    id TEXT PRIMARY KEY,
    camera_id TEXT NOT NULL REFERENCES cameras(id) ON DELETE RESTRICT,
    generation INTEGER NOT NULL CHECK (generation > 0),
    kind TEXT NOT NULL CHECK (kind = 'reconcile_camera'),
    state TEXT NOT NULL CHECK (
        state IN ('pending', 'running', 'succeeded', 'failed', 'unknown')
    ),
    reason TEXT NOT NULL CHECK (
        reason IN ('camera_created', 'camera_updated', 'camera_deleted', 'drift_detected')
    ),
    requested_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    created_at TEXT NOT NULL,
    started_at TEXT,
    finished_at TEXT,
    retry_at TEXT,
    lease_owner TEXT,
    lease_expires_at TEXT,
    result_json TEXT,
    error_code TEXT,
    error_message TEXT
);

CREATE INDEX media_operations_queue_idx
    ON media_operations (state, retry_at, created_at);
CREATE INDEX media_operations_camera_idx
    ON media_operations (camera_id, generation, created_at DESC);
CREATE UNIQUE INDEX media_operations_active_generation_idx
    ON media_operations (camera_id, generation)
    WHERE state IN ('pending', 'running', 'unknown');

CREATE TABLE media_actual_paths (
    path_name TEXT PRIMARY KEY,
    camera_id TEXT NOT NULL REFERENCES cameras(id) ON DELETE RESTRICT,
    profile TEXT NOT NULL CHECK (profile IN ('main', 'sub')),
    present INTEGER NOT NULL CHECK (present IN (0, 1)),
    ready INTEGER NOT NULL CHECK (ready IN (0, 1)),
    publisher_active INTEGER NOT NULL CHECK (publisher_active IN (0, 1)),
    recording_active INTEGER NOT NULL CHECK (recording_active IN (0, 1)),
    source_digest BLOB CHECK (source_digest IS NULL OR length(source_digest) = 32),
    source_on_demand INTEGER CHECK (source_on_demand IS NULL OR source_on_demand IN (0, 1)),
    record_configured INTEGER CHECK (record_configured IS NULL OR record_configured IN (0, 1)),
    applied_generation INTEGER,
    last_operation_id TEXT REFERENCES media_operations(id) ON DELETE SET NULL,
    observed_at TEXT NOT NULL
);

CREATE INDEX media_actual_paths_camera_idx
    ON media_actual_paths (camera_id, profile);

CREATE TABLE media_reconciler_leases (
    scope TEXT PRIMARY KEY CHECK (scope = 'global'),
    lease_owner TEXT,
    lease_expires_at TEXT,
    updated_at TEXT NOT NULL
);

INSERT INTO media_reconciler_leases (scope, updated_at)
VALUES ('global', datetime('now'));

INSERT INTO media_desired_states (
    camera_id,
    generation,
    desired_present,
    main_path,
    sub_path,
    record_enabled,
    updated_at
)
SELECT
    id,
    1,
    enabled,
    'cam_' || LOWER(REPLACE(id, '-', '')) || '_main',
    CASE
        WHEN sub_stream_url_enc IS NOT NULL
        THEN 'cam_' || LOWER(REPLACE(id, '-', '')) || '_sub'
    END,
    record_enabled,
    updated_at
FROM cameras;

INSERT INTO media_operations (
    id,
    camera_id,
    generation,
    kind,
    state,
    reason,
    attempt,
    created_at,
    retry_at
)
SELECT
    LOWER(HEX(RANDOMBLOB(16))),
    camera_id,
    generation,
    'reconcile_camera',
    'pending',
    'drift_detected',
    0,
    updated_at,
    updated_at
FROM media_desired_states;

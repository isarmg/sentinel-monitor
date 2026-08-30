CREATE TABLE product_metadata (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    application TEXT NOT NULL,
    application_version TEXT NOT NULL,
    schema_revision INTEGER NOT NULL,
    schema_sha256 TEXT NOT NULL
);

CREATE TABLE users (
    id TEXT PRIMARY KEY,
    email TEXT NOT NULL,
    password_hash TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('admin', 'operator', 'viewer')),
    active INTEGER NOT NULL DEFAULT 1,
    last_login_at TEXT,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    session_version INTEGER NOT NULL DEFAULT 1
);

CREATE UNIQUE INDEX users_email_lower_idx ON users (LOWER(email));

CREATE TABLE cameras (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    location TEXT NOT NULL DEFAULT '',
    main_stream_url_enc BLOB NOT NULL,
    sub_stream_url_enc BLOB,
    onvif_url TEXT,
    username TEXT,
    password_enc BLOB,
    enabled INTEGER NOT NULL DEFAULT 1,
    record_enabled INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'online', 'offline', 'disabled', 'error')),
    last_seen_at TEXT,
    created_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    deleted_at TEXT
);

CREATE INDEX cameras_status_idx ON cameras (status);
CREATE INDEX cameras_enabled_idx ON cameras (enabled);

CREATE TABLE events (
    id TEXT PRIMARY KEY,
    camera_id TEXT REFERENCES cameras(id) ON DELETE SET NULL,
    kind TEXT NOT NULL,
    severity TEXT NOT NULL CHECK (severity IN ('info', 'warning', 'critical')),
    message TEXT NOT NULL,
    details TEXT NOT NULL DEFAULT '{}',
    acknowledged_at TEXT,
    acknowledged_by TEXT REFERENCES users(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX events_created_at_idx ON events (created_at DESC);
CREATE INDEX events_camera_id_idx ON events (camera_id, created_at DESC);
CREATE INDEX events_unacknowledged_idx ON events (created_at DESC) WHERE acknowledged_at IS NULL;

CREATE TABLE audit_logs (
    id TEXT PRIMARY KEY,
    user_id TEXT REFERENCES users(id) ON DELETE SET NULL,
    action TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT,
    details TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);

CREATE INDEX audit_logs_created_at_idx ON audit_logs (created_at DESC);

CREATE TABLE browser_sessions (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    token_digest BLOB NOT NULL UNIQUE CHECK (length(token_digest) = 32),
    csrf_digest BLOB NOT NULL CHECK (length(csrf_digest) = 32),
    session_version INTEGER NOT NULL,
    created_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    idle_expires_at TEXT NOT NULL,
    absolute_expires_at TEXT NOT NULL,
    revoked_at TEXT,
    CHECK (idle_expires_at <= absolute_expires_at)
);

CREATE INDEX browser_sessions_user_idx
    ON browser_sessions (user_id, absolute_expires_at DESC);
CREATE INDEX browser_sessions_expiry_idx
    ON browser_sessions (idle_expires_at, absolute_expires_at)
    WHERE revoked_at IS NULL;

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

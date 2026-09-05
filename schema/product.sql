CREATE TABLE cameras (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    location TEXT NOT NULL DEFAULT '',
    main_stream_url_enc BLOB NOT NULL,
    sub_stream_url_enc BLOB,
    onvif_url TEXT,
    username_enc BLOB,
    password_enc BLOB,
    enabled INTEGER NOT NULL DEFAULT 1,
    record_enabled INTEGER NOT NULL DEFAULT 1,
    status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'online', 'offline', 'disabled', 'error')),
    last_seen_at TEXT,
    created_by TEXT REFERENCES _sarmg_administrators(administrator_id) ON DELETE SET NULL,
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
    acknowledged_by TEXT REFERENCES _sarmg_administrators(administrator_id) ON DELETE SET NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX events_created_at_idx ON events (created_at DESC);
CREATE INDEX events_camera_id_idx ON events (camera_id, created_at DESC);
CREATE INDEX events_unacknowledged_idx ON events (created_at DESC) WHERE acknowledged_at IS NULL;

CREATE TABLE audit_logs (
    id TEXT PRIMARY KEY,
    user_id TEXT REFERENCES _sarmg_administrators(administrator_id) ON DELETE SET NULL,
    action TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id TEXT,
    details TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);

CREATE INDEX audit_logs_created_at_idx ON audit_logs (created_at DESC);

CREATE TABLE media_desired_states (
    camera_id TEXT PRIMARY KEY REFERENCES cameras(id) ON DELETE RESTRICT,
    generation INTEGER NOT NULL CHECK (generation > 0),
    desired_present INTEGER NOT NULL CHECK (desired_present IN (0, 1)),
    main_path TEXT NOT NULL,
    sub_path TEXT,
    record_enabled INTEGER NOT NULL CHECK (record_enabled IN (0, 1)),
    updated_at TEXT NOT NULL
);

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
    last_operation_id TEXT REFERENCES _sarmg_operations(operation_id) ON DELETE SET NULL,
    observed_at TEXT NOT NULL
);

CREATE INDEX media_actual_paths_camera_idx
    ON media_actual_paths (camera_id, profile);

CREATE TABLE media_reconciler_leases (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    lease_owner TEXT,
    lease_expires_at TEXT,
    updated_at TEXT NOT NULL,
    CHECK ((lease_owner IS NULL) = (lease_expires_at IS NULL)),
    CHECK (julianday(updated_at) IS NOT NULL),
    CHECK (
        lease_owner IS NULL OR (
            length(lease_owner) = 36
            AND lease_owner = lower(lease_owner)
            AND substr(lease_owner, 9, 1) = '-'
            AND substr(lease_owner, 14, 1) = '-'
            AND substr(lease_owner, 15, 1) = '4'
            AND substr(lease_owner, 19, 1) = '-'
            AND substr(lease_owner, 20, 1) GLOB '[89ab]'
            AND substr(lease_owner, 24, 1) = '-'
            AND lease_owner NOT GLOB '*[^0-9a-f-]*'
            AND length(replace(lease_owner, '-', '')) = 32
        )
    ),
    CHECK (
        lease_expires_at IS NULL OR (
            julianday(lease_expires_at) IS NOT NULL
            AND julianday(lease_expires_at) > julianday(updated_at)
        )
    )
);

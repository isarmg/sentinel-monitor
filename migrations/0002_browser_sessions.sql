ALTER TABLE users ADD COLUMN session_version INTEGER NOT NULL DEFAULT 1;

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

-- An attendance activity, like an assignment: it has an open and a close time.
CREATE TABLE activities (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    course_id   TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    lecturer_id TEXT    NOT NULL,
    open_at     INTEGER NOT NULL,           -- unix seconds
    close_at    INTEGER NOT NULL,           -- unix seconds
    closed      INTEGER NOT NULL DEFAULT 0  -- lecturer closed it early
);

-- One-time codes. Only the SHA-256 hash is stored, never the code itself.
CREATE TABLE tokens (
    token_hash  TEXT PRIMARY KEY,
    activity_id INTEGER NOT NULL REFERENCES activities(id),
    issued_at   INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    used_by     TEXT,
    used_at     INTEGER
);
CREATE INDEX idx_tokens_activity ON tokens(activity_id);

CREATE TABLE attendance (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    activity_id  INTEGER NOT NULL REFERENCES activities(id),
    student_id   TEXT    NOT NULL,
    student_name TEXT    NOT NULL,
    device_id    TEXT    NOT NULL,
    token_hash   TEXT    NOT NULL,
    marked_at    INTEGER NOT NULL,
    UNIQUE (activity_id, student_id),  -- one record per student
    UNIQUE (activity_id, device_id)    -- one phone cannot mark several people
);

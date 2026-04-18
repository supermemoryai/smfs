-- supermemoryfs local cache schema.
-- Adapted from AgentFS with nanosecond timestamp precision.

-- Inode metadata. Every file, directory, and symlink gets a row here.
-- ino is AUTOINCREMENT so inode numbers are never reused.
-- dirty_since: epoch-ms when the user last wrote this inode locally; pull reconciler
-- skips an inode whose dirty_since is newer than the remote updatedAt (local wins).
CREATE TABLE IF NOT EXISTS fs_inode (
    ino          INTEGER PRIMARY KEY AUTOINCREMENT,
    mode         INTEGER NOT NULL,
    nlink        INTEGER NOT NULL DEFAULT 0,
    uid          INTEGER NOT NULL DEFAULT 0,
    gid          INTEGER NOT NULL DEFAULT 0,
    size         INTEGER NOT NULL DEFAULT 0,
    atime        INTEGER NOT NULL,
    mtime        INTEGER NOT NULL,
    ctime        INTEGER NOT NULL,
    rdev         INTEGER NOT NULL DEFAULT 0,
    atime_nsec   INTEGER NOT NULL DEFAULT 0,
    mtime_nsec   INTEGER NOT NULL DEFAULT 0,
    ctime_nsec   INTEGER NOT NULL DEFAULT 0,
    dirty_since  INTEGER
);

-- Directory entries: maps (parent_ino, name) → child ino.
CREATE TABLE IF NOT EXISTS fs_dentry (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT    NOT NULL,
    parent_ino INTEGER NOT NULL,
    ino        INTEGER NOT NULL,
    UNIQUE(parent_ino, name)
);
CREATE INDEX IF NOT EXISTS idx_dentry_parent ON fs_dentry(parent_ino, name);

-- Chunked file data. Files are split into fixed-size chunks (default 4096).
CREATE TABLE IF NOT EXISTS fs_data (
    ino         INTEGER NOT NULL,
    chunk_index INTEGER NOT NULL,
    data        BLOB    NOT NULL,
    PRIMARY KEY (ino, chunk_index)
);

-- Symlink targets.
CREATE TABLE IF NOT EXISTS fs_symlink (
    ino    INTEGER PRIMARY KEY,
    target TEXT NOT NULL
);

-- Key-value configuration (chunk_size, schema_version, etc.).
CREATE TABLE IF NOT EXISTS fs_config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Remote document ID tracking. Maps local inode → Supermemory API document ID.
-- Populated on first successful flush (POST) and on pull reconciliation. Used
-- for subsequent updates (PATCH) and for delta-pull version comparison via
-- mirrored_updated_at.
CREATE TABLE IF NOT EXISTS fs_remote (
    ino                  INTEGER PRIMARY KEY,
    remote_id            TEXT    NOT NULL,
    mirrored_updated_at  INTEGER,
    last_status          TEXT,
    last_status_at       INTEGER
);

-- Persistent push queue. One row per filepath enforces latest-wins coalescing:
-- if a write arrives while another write for the same filepath is queued (but
-- not yet inflight), the new write replaces it. If the earlier write IS
-- inflight, the new write sits in the pending_* columns and promotes once
-- the inflight op finishes.
CREATE TABLE IF NOT EXISTS push_queue (
    filepath             TEXT PRIMARY KEY,
    op                   TEXT NOT NULL,
    content_ino          INTEGER,
    rename_to            TEXT,
    inflight_remote_id   TEXT,
    inflight_started_at  INTEGER,
    pending_op           TEXT,
    pending_content_ino  INTEGER,
    pending_rename_to    TEXT,
    last_error           TEXT,
    attempt              INTEGER NOT NULL DEFAULT 0,
    updated_at           INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_push_queue_updated ON push_queue(updated_at);

-- General KV for sync timestamps and ID-set snapshots.
--   last_seen_updated_at   — watermark for delta pull (loop A)
--   last_scan_total_items  — for skip-if-unchanged deletion scan (loop C)
CREATE TABLE IF NOT EXISTS sync_meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- supermemoryfs local cache schema.
-- Adapted from AgentFS with nanosecond timestamp precision.

-- Inode metadata. Every file, directory, and symlink gets a row here.
-- ino is AUTOINCREMENT so inode numbers are never reused.
CREATE TABLE IF NOT EXISTS fs_inode (
    ino        INTEGER PRIMARY KEY AUTOINCREMENT,
    mode       INTEGER NOT NULL,
    nlink      INTEGER NOT NULL DEFAULT 0,
    uid        INTEGER NOT NULL DEFAULT 0,
    gid        INTEGER NOT NULL DEFAULT 0,
    size       INTEGER NOT NULL DEFAULT 0,
    atime      INTEGER NOT NULL,
    mtime      INTEGER NOT NULL,
    ctime      INTEGER NOT NULL,
    rdev       INTEGER NOT NULL DEFAULT 0,
    atime_nsec INTEGER NOT NULL DEFAULT 0,
    mtime_nsec INTEGER NOT NULL DEFAULT 0,
    ctime_nsec INTEGER NOT NULL DEFAULT 0
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
-- Populated on first successful flush (POST), used for subsequent updates (PATCH).
CREATE TABLE IF NOT EXISTS fs_remote (
    ino       INTEGER PRIMARY KEY,
    remote_id TEXT    NOT NULL
);

-- Append-only revision payloads shared by descendant AWACS roots.
--
-- Operational databases retain revision metadata, watches, leases, snapshots,
-- and recovery state. These tables contain only immutable checkpoint/overlay
-- rows, keyed by globally allocated revision ids.
CREATE TABLE revision_ids (
    id INTEGER PRIMARY KEY AUTOINCREMENT
);

CREATE TABLE checkpoint_objects (
    revision_id     INTEGER NOT NULL,
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    generation      BLOB NOT NULL CHECK (length(generation) = 8),
    mode            INTEGER NOT NULL,
    nlink           INTEGER NOT NULL,
    uid             BLOB NOT NULL CHECK (length(uid) = 8),
    gid             BLOB NOT NULL CHECK (length(gid) = 8),
    rdev            BLOB NOT NULL CHECK (length(rdev) = 8),
    privilege_flags INTEGER NOT NULL,
    security_xattr_hash BLOB NOT NULL CHECK (length(security_xattr_hash) = 32),
    PRIMARY KEY (revision_id, ino)
) WITHOUT ROWID;

CREATE TABLE checkpoint_refs (
    revision_id     INTEGER NOT NULL,
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    parent_ino      BLOB NOT NULL CHECK (length(parent_ino) = 8),
    name            BLOB NOT NULL,
    PRIMARY KEY (revision_id, ino, parent_ino, name)
) WITHOUT ROWID;

CREATE INDEX checkpoint_refs_by_parent
ON checkpoint_refs(revision_id, parent_ino, name, ino);

CREATE UNIQUE INDEX checkpoint_one_child_per_name
ON checkpoint_refs(revision_id, parent_ino, name);

CREATE TABLE checkpoint_owner_counts (
    revision_id     INTEGER NOT NULL,
    uid             BLOB NOT NULL CHECK (length(uid) = 8),
    object_count    INTEGER NOT NULL CHECK (object_count > 0),
    PRIMARY KEY (revision_id, uid)
) WITHOUT ROWID;

CREATE TABLE object_overrides (
    revision_id     INTEGER NOT NULL,
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    present         INTEGER NOT NULL CHECK (present IN (0, 1)),
    generation      BLOB CHECK (generation IS NULL OR length(generation) = 8),
    mode            INTEGER,
    nlink           INTEGER,
    uid             BLOB CHECK (uid IS NULL OR length(uid) = 8),
    gid             BLOB CHECK (gid IS NULL OR length(gid) = 8),
    rdev            BLOB CHECK (rdev IS NULL OR length(rdev) = 8),
    privilege_flags INTEGER,
    security_xattr_hash BLOB CHECK
                    (security_xattr_hash IS NULL
                     OR length(security_xattr_hash) = 32),
    PRIMARY KEY (revision_id, ino),
    CHECK ((present = 0 AND generation IS NULL AND mode IS NULL
                         AND nlink IS NULL AND uid IS NULL AND gid IS NULL
                         AND rdev IS NULL AND privilege_flags IS NULL
                         AND security_xattr_hash IS NULL)
        OR (present = 1 AND generation IS NOT NULL AND mode IS NOT NULL
                        AND nlink IS NOT NULL AND uid IS NOT NULL
                        AND gid IS NOT NULL AND rdev IS NOT NULL
                        AND privilege_flags IS NOT NULL
                        AND security_xattr_hash IS NOT NULL))
) WITHOUT ROWID;

CREATE TABLE ref_overrides (
    revision_id     INTEGER NOT NULL,
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    parent_ino      BLOB NOT NULL CHECK (length(parent_ino) = 8),
    name            BLOB NOT NULL,
    present         INTEGER NOT NULL CHECK (present IN (0, 1)),
    PRIMARY KEY (revision_id, ino, parent_ino, name)
) WITHOUT ROWID;

CREATE INDEX ref_overrides_by_parent
ON ref_overrides(revision_id, parent_ino, name, ino);

CREATE TABLE owner_count_overrides (
    revision_id     INTEGER NOT NULL,
    uid             BLOB NOT NULL CHECK (length(uid) = 8),
    object_count    INTEGER NOT NULL CHECK (object_count >= 0),
    PRIMARY KEY (revision_id, uid)
) WITHOUT ROWID;

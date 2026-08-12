CREATE TABLE service_metadata (
    singleton       INTEGER PRIMARY KEY CHECK (singleton = 1),
    store_uuid      BLOB NOT NULL UNIQUE CHECK (length(store_uuid) = 16),
    clock_hmac_key  BLOB NOT NULL CHECK (length(clock_hmac_key) = 32),
    clock_format_version INTEGER NOT NULL CHECK (clock_format_version > 0),
    last_boot_id    BLOB NOT NULL CHECK (length(last_boot_id) = 16),
    created_ns      INTEGER NOT NULL
);

CREATE TABLE filesystems (
    id              INTEGER PRIMARY KEY,
    fs_uuid         BLOB NOT NULL UNIQUE CHECK (length(fs_uuid) = 16)
);

CREATE TABLE topology_leases (
    filesystem_id   INTEGER PRIMARY KEY REFERENCES filesystems(id),
    lease_owner     BLOB,
    lease_fence     INTEGER NOT NULL DEFAULT 0,
    lease_expires_ns INTEGER
);

CREATE TABLE snapshots (
    id              INTEGER PRIMARY KEY,
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    subvol_uuid     BLOB NOT NULL CHECK (length(subvol_uuid) = 16),
    parent_uuid     BLOB CHECK (parent_uuid IS NULL OR length(parent_uuid) = 16),
    received_uuid   BLOB CHECK (received_uuid IS NULL OR length(received_uuid) = 16),
    root_id         BLOB NOT NULL CHECK (length(root_id) = 8),
    ctransid        BLOB NOT NULL CHECK (length(ctransid) = 8),
    otransid        BLOB NOT NULL CHECK (length(otransid) = 8),
    path            BLOB NOT NULL,
    readonly        INTEGER NOT NULL CHECK (readonly = 1),
    physical_state  TEXT NOT NULL CHECK
                    (physical_state IN
                     ('creating', 'present', 'deleting', 'deleted', 'lost')),
    created_ns      INTEGER NOT NULL,
    deleted_ns      INTEGER,
    UNIQUE (filesystem_id, subvol_uuid)
);

CREATE UNIQUE INDEX snapshots_live_path
ON snapshots(filesystem_id, path)
WHERE physical_state IN ('creating', 'present', 'deleting');

CREATE TABLE revisions (
    id              INTEGER PRIMARY KEY,
    snapshot_id     INTEGER NOT NULL UNIQUE REFERENCES snapshots(id),
    storage_base_revision_id INTEGER REFERENCES revisions(id),
    provenance_comparison_id INTEGER REFERENCES comparisons(id),
    delta_depth     INTEGER NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('building', 'ready', 'failed')),
    builder_owner   BLOB,
    builder_fence   INTEGER NOT NULL,
    builder_expires_ns INTEGER,
    object_count    INTEGER,
    ref_count       INTEGER,
    state_hash      BLOB,
    single_owner_uid BLOB CHECK
                    (single_owner_uid IS NULL OR length(single_owner_uid) = 8),
    privileged_metadata_count INTEGER,
    security_state_hash BLOB CHECK
                    (security_state_hash IS NULL OR length(security_state_hash) = 32),
    owner_cardinality INTEGER,
    owner_uid_xor  BLOB CHECK
                    (owner_uid_xor IS NULL OR length(owner_uid_xor) = 8),
    summary_version INTEGER NOT NULL CHECK (summary_version IN (1, 2)),
    created_ns      INTEGER NOT NULL,
    CHECK ((summary_version = 1)
        OR (summary_version = 2 AND owner_cardinality > 0
            AND owner_uid_xor IS NOT NULL))
);

-- A checkpoint is a full materialization of an immutable revision. The first
-- revision is a checkpoint; later revisions may gain one during compaction.
CREATE TABLE revision_checkpoints (
    revision_id     INTEGER PRIMARY KEY REFERENCES revisions(id),
    state           TEXT NOT NULL CHECK (state IN ('building', 'ready')),
    builder_owner   BLOB,
    builder_fence   INTEGER NOT NULL,
    builder_expires_ns INTEGER,
    object_count    INTEGER,
    ref_count       INTEGER,
    state_hash      BLOB
);

CREATE TABLE checkpoint_objects (
    revision_id     INTEGER NOT NULL REFERENCES revision_checkpoints(revision_id),
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
    revision_id     INTEGER NOT NULL REFERENCES revision_checkpoints(revision_id),
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
    revision_id     INTEGER NOT NULL REFERENCES revision_checkpoints(revision_id),
    uid             BLOB NOT NULL CHECK (length(uid) = 8),
    object_count    INTEGER NOT NULL CHECK (object_count > 0),
    PRIMARY KEY (revision_id, uid)
) WITHOUT ROWID;

-- Target-state overrides relative to storage_base_revision_id.
CREATE TABLE object_overrides (
    revision_id     INTEGER NOT NULL REFERENCES revisions(id),
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
    CHECK ((present = 0 AND generation IS NULL AND mode IS NULL AND nlink IS NULL
                         AND uid IS NULL AND gid IS NULL AND rdev IS NULL
                         AND privilege_flags IS NULL
                         AND security_xattr_hash IS NULL)
        OR (present = 1 AND generation IS NOT NULL
                        AND mode IS NOT NULL AND nlink IS NOT NULL
                        AND uid IS NOT NULL AND gid IS NOT NULL
                        AND rdev IS NOT NULL AND privilege_flags IS NOT NULL
                        AND security_xattr_hash IS NOT NULL))
) WITHOUT ROWID;

CREATE TABLE ref_overrides (
    revision_id     INTEGER NOT NULL REFERENCES revisions(id),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    parent_ino      BLOB NOT NULL CHECK (length(parent_ino) = 8),
    name            BLOB NOT NULL,
    present         INTEGER NOT NULL CHECK (present IN (0, 1)),
    PRIMARY KEY (revision_id, ino, parent_ino, name)
) WITHOUT ROWID;

CREATE INDEX ref_overrides_by_parent
ON ref_overrides(revision_id, parent_ino, name, ino);

-- Absolute per-UID counts relative to storage_base_revision_id. Only UIDs
-- touched by this delta need rows; zero is retained to mask an inherited UID.
CREATE TABLE owner_count_overrides (
    revision_id     INTEGER NOT NULL REFERENCES revisions(id),
    uid             BLOB NOT NULL CHECK (length(uid) = 8),
    object_count    INTEGER NOT NULL CHECK (object_count >= 0),
    PRIMARY KEY (revision_id, uid)
) WITHOUT ROWID;

-- summary_version 2 makes state_hash and security_state_hash incrementally
-- composable: each is the XOR accumulator of domain-separated SHA-256 entry
-- digests keyed by inode or reference identity. Replacing an entry XORs out
-- its old digest and XORs in its new digest. Object/reference counts and the
-- privileged-metadata count use checked signed deltas. Owner cardinality is
-- exact, not inferred from single_owner_uid: checkpoint rows contain every
-- positive UID count, overlays contain absolute counts only for touched UIDs,
-- and zero masks an inherited owner. owner_uid_xor changes only on a
-- zero/nonzero transition; with exact cardinality it recovers the sole UID.
-- Version-1 summaries are checkpointed before another delta is published.

CREATE TABLE comparisons (
    id              INTEGER PRIMARY KEY,
    from_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    to_snapshot_id  INTEGER NOT NULL REFERENCES snapshots(id),
    comparison_kind TEXT NOT NULL CHECK
                    (comparison_kind IN ('incremental', 'full_fresh')),
    algorithm_version INTEGER NOT NULL,
    state           TEXT NOT NULL CHECK
                    (state IN ('claimed', 'manifest_ready', 'index_ready', 'failed')),
    lease_owner     BLOB,
    lease_fence     INTEGER NOT NULL DEFAULT 0,
    lease_expires_ns INTEGER,
    manifest_hash   BLOB,
    raw_ref_adds    INTEGER,
    raw_ref_deletes INTEGER,
    UNIQUE (from_snapshot_id, to_snapshot_id,
            comparison_kind, algorithm_version),
    UNIQUE (id, from_snapshot_id, to_snapshot_id)
);

CREATE TABLE comparison_objects (
    comparison_id   INTEGER NOT NULL REFERENCES comparisons(id),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    old_generation  BLOB CHECK (old_generation IS NULL OR length(old_generation) = 8),
    new_generation  BLOB CHECK (new_generation IS NULL OR length(new_generation) = 8),
    change_mask     INTEGER NOT NULL,
    PRIMARY KEY (comparison_id, ino)
) WITHOUT ROWID;

CREATE TABLE comparison_refs (
    comparison_id   INTEGER NOT NULL REFERENCES comparisons(id),
    operation       INTEGER NOT NULL CHECK (operation IN (-1, 1)),
    ino             BLOB NOT NULL CHECK (length(ino) = 8),
    parent_ino      BLOB NOT NULL CHECK (length(parent_ino) = 8),
    name            BLOB NOT NULL,
    PRIMARY KEY (comparison_id, operation, ino, parent_ino, name)
) WITHOUT ROWID;

CREATE TABLE change_events (
    comparison_id   INTEGER NOT NULL REFERENCES comparisons(id),
    ordinal         INTEGER NOT NULL,
    event_kind      TEXT NOT NULL,
    ino             BLOB CHECK (ino IS NULL OR length(ino) = 8),
    old_generation  BLOB CHECK (old_generation IS NULL OR length(old_generation) = 8),
    new_generation  BLOB CHECK (new_generation IS NULL OR length(new_generation) = 8),
    change_mask     INTEGER NOT NULL,
    old_path        BLOB,
    new_path        BLOB,
    PRIMARY KEY (comparison_id, ordinal)
) WITHOUT ROWID;

CREATE TABLE watches (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    live_subvol_uuid BLOB NOT NULL CHECK (length(live_subvol_uuid) = 16),
    live_path       BLOB NOT NULL,
    indexed_revision_id INTEGER REFERENCES revisions(id),
    indexed_seq     INTEGER,
    last_cut_snapshot_id INTEGER REFERENCES snapshots(id),
    last_cut_seq    INTEGER,
    cut_owner       BLOB,
    cut_fence       INTEGER NOT NULL DEFAULT 0,
    cut_expires_ns  INTEGER,
    clock_epoch     BLOB NOT NULL CHECK (length(clock_epoch) = 16),
    replay_floor_seq INTEGER,
    fsmonitor_owner_grant_id BLOB
                    CHECK (fsmonitor_owner_grant_id IS NULL
                           OR length(fsmonitor_owner_grant_id) = 16),
    fsmonitor_root  BLOB,
    mount_ns_dev    BLOB CHECK (mount_ns_dev IS NULL OR length(mount_ns_dev) = 8),
    mount_ns_ino    BLOB CHECK (mount_ns_ino IS NULL OR length(mount_ns_ino) = 8),
    view_root_dev   BLOB CHECK (view_root_dev IS NULL OR length(view_root_dev) = 8),
    view_root_ino   BLOB CHECK (view_root_ino IS NULL OR length(view_root_ino) = 8),
    view_root_mnt_id BLOB CHECK
                    (view_root_mnt_id IS NULL OR length(view_root_mnt_id) = 8),
    view_monitor_session_id BLOB CHECK
                    (view_monitor_session_id IS NULL
                     OR length(view_monitor_session_id) = 16),
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    guard_head_seq  INTEGER,
    guard_replay_floor_seq INTEGER,
    fsmonitor_state TEXT NOT NULL CHECK
                    (fsmonitor_state IN
                     ('disabled', 'snapshot_only', 'guard_arming',
                      'guard_active', 'guard_gapped')),
    state           TEXT NOT NULL CHECK
                    (state IN ('initializing', 'active', 'blocked', 'deleted')),
    CHECK (
        (state = 'initializing'
         AND indexed_revision_id IS NULL AND indexed_seq IS NULL
         AND last_cut_snapshot_id IS NULL AND last_cut_seq IS NULL
         AND replay_floor_seq IS NULL)
        OR
        (state IN ('active', 'blocked')
         AND indexed_revision_id IS NOT NULL AND indexed_seq IS NOT NULL
         AND last_cut_snapshot_id IS NOT NULL AND last_cut_seq IS NOT NULL
         AND replay_floor_seq IS NOT NULL
         AND replay_floor_seq <= indexed_seq
         AND indexed_seq <= last_cut_seq)
        OR
        (state = 'deleted'
         AND indexed_revision_id IS NULL AND indexed_seq IS NULL
         AND last_cut_snapshot_id IS NULL AND last_cut_seq IS NULL
         AND replay_floor_seq IS NULL)
    ),
    CHECK (
        (fsmonitor_state = 'disabled'
         AND fsmonitor_owner_grant_id IS NULL AND fsmonitor_root IS NULL
         AND mount_ns_dev IS NULL AND mount_ns_ino IS NULL
         AND view_root_dev IS NULL AND view_root_ino IS NULL
         AND view_root_mnt_id IS NULL
         AND view_monitor_session_id IS NULL
         AND guard_epoch IS NULL AND guard_head_seq IS NULL
         AND guard_replay_floor_seq IS NULL)
        OR
        (fsmonitor_state = 'snapshot_only'
         AND state = 'active'
         AND fsmonitor_owner_grant_id IS NOT NULL
         AND fsmonitor_root IS NOT NULL
         AND mount_ns_dev IS NOT NULL AND mount_ns_ino IS NOT NULL
         AND view_root_dev IS NOT NULL AND view_root_ino IS NOT NULL
         AND view_root_mnt_id IS NOT NULL
         AND view_monitor_session_id IS NOT NULL
         AND guard_epoch IS NULL AND guard_head_seq IS NULL
         AND guard_replay_floor_seq IS NULL)
        OR
        (fsmonitor_state IN
         ('guard_arming', 'guard_active', 'guard_gapped')
         AND state = 'active'
         AND fsmonitor_owner_grant_id IS NOT NULL
         AND fsmonitor_root IS NOT NULL
         AND mount_ns_dev IS NOT NULL AND mount_ns_ino IS NOT NULL
         AND view_root_dev IS NOT NULL AND view_root_ino IS NOT NULL
         AND view_root_mnt_id IS NOT NULL
         AND view_monitor_session_id IS NOT NULL
         AND guard_epoch IS NOT NULL AND guard_head_seq IS NOT NULL
         AND guard_replay_floor_seq IS NOT NULL
         AND guard_replay_floor_seq <= guard_head_seq)
    ),
    FOREIGN KEY (fsmonitor_owner_grant_id, id)
        REFERENCES watch_grants(id, watch_id)
);

CREATE UNIQUE INDEX watches_live_subvolume
ON watches(filesystem_id, live_subvol_uuid)
WHERE state IN ('initializing', 'active', 'blocked');

CREATE TABLE watch_grants (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    principal_kind  TEXT NOT NULL CHECK
                    (principal_kind IN ('uid', 'service')),
    principal_id    BLOB NOT NULL,
    -- 0x01 READ, 0x02 CUT, 0x10 RETAIN, 0x20 ADMIN.
    -- Unknown bits are rejected.
    permissions     INTEGER NOT NULL CHECK
                    (permissions > 0 AND (permissions & ~51) = 0),
    state           TEXT NOT NULL CHECK (state IN ('active', 'revoked')),
    created_ns      INTEGER NOT NULL,
    revoked_ns      INTEGER,
    CHECK ((state = 'active' AND revoked_ns IS NULL)
        OR (state = 'revoked' AND revoked_ns IS NOT NULL)),
    UNIQUE (id, watch_id)
);

CREATE UNIQUE INDEX watch_grants_one_active_principal
ON watch_grants(watch_id, principal_kind, principal_id)
WHERE state = 'active';

-- Conservative mutation hints between immutable cuts. The guard producer
-- emits two path rows for a rename. A NULL path is permitted only for a
-- whole-tree invalidation marker.
CREATE TABLE mutation_events (
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    guard_epoch     BLOB NOT NULL CHECK (length(guard_epoch) = 16),
    sequence        INTEGER NOT NULL CHECK (sequence >= 0),
    event_kind      TEXT NOT NULL CHECK
                    (event_kind IN ('path', 'directory-prefix',
                                    'object', 'full-invalidation')),
    path            BLOB,
    ino             BLOB CHECK (ino IS NULL OR length(ino) = 8),
    generation      BLOB CHECK (generation IS NULL OR length(generation) = 8),
    observed_ns     INTEGER NOT NULL,
    PRIMARY KEY (watch_id, guard_epoch, sequence),
    CHECK ((event_kind = 'full-invalidation' AND path IS NULL)
        OR (event_kind != 'full-invalidation' AND path IS NOT NULL))
) WITHOUT ROWID;

CREATE TABLE operations (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    kind            TEXT NOT NULL CHECK (kind IN ('initialize', 'cut')),
    state           TEXT NOT NULL CHECK
                    (state IN ('planned', 'fs_started', 'fs_created', 'uuid_recorded',
                               'manifest_ready', 'index_committed',
                               'done', 'failed')),
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    sequence        INTEGER,
    source_subvol_uuid BLOB NOT NULL CHECK (length(source_subvol_uuid) = 16),
    base_snapshot_id INTEGER REFERENCES snapshots(id),
    expected_parent_uuid BLOB NOT NULL CHECK (length(expected_parent_uuid) = 16),
    requested_readonly INTEGER NOT NULL CHECK (requested_readonly IN (0, 1)),
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    guard_sequence  INTEGER,
    requester_uid   INTEGER NOT NULL,
    requester_gid   INTEGER NOT NULL,
    authorization_id BLOB NOT NULL CHECK (length(authorization_id) = 16),
    reserved_path   BLOB NOT NULL,
    discovered_uuid BLOB CHECK
                    (discovered_uuid IS NULL OR length(discovered_uuid) = 16),
    lease_owner     BLOB,
    lease_fence     INTEGER NOT NULL,
    lease_expires_ns INTEGER,
    error           TEXT,
    updated_ns      INTEGER NOT NULL,
    UNIQUE (watch_id, sequence),
    UNIQUE (id, watch_id),
    UNIQUE (id, watch_id, sequence),
    FOREIGN KEY (authorization_id, watch_id)
        REFERENCES watch_grants(id, watch_id),
    CHECK ((guard_epoch IS NULL) = (guard_sequence IS NULL))
);

CREATE UNIQUE INDEX operations_active_reserved_path
ON operations(filesystem_id, reserved_path)
WHERE state NOT IN ('done', 'failed');

-- A compatibility request joins a cut only through this writer-serialized
-- record. This closes the read/check versus fs_started race.
CREATE TABLE cut_admissions (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    operation_id    BLOB NOT NULL,
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    authorization_id BLOB NOT NULL CHECK (length(authorization_id) = 16),
    requester_session_id BLOB NOT NULL
                    CHECK (length(requester_session_id) = 16),
    request_kind    TEXT NOT NULL CHECK
                    (request_kind IN ('clock', 'query')),
    state           TEXT NOT NULL CHECK
                    (state IN ('waiting', 'fulfilled', 'abandoned')),
    admitted_ns     INTEGER NOT NULL,
    expires_ns      INTEGER NOT NULL,
    FOREIGN KEY (operation_id, watch_id)
        REFERENCES operations(id, watch_id),
    FOREIGN KEY (authorization_id, watch_id)
        REFERENCES watch_grants(id, watch_id)
);

CREATE TABLE watch_cuts (
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    sequence        INTEGER NOT NULL,
    operation_id    BLOB NOT NULL UNIQUE,
    base_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    target_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    comparison_id   INTEGER REFERENCES comparisons(id),
    comparison_from_snapshot_id INTEGER REFERENCES snapshots(id),
    state           TEXT NOT NULL CHECK
                    (state IN ('created', 'comparing', 'ready', 'failed')),
    fresh_instance  INTEGER NOT NULL DEFAULT 0 CHECK (fresh_instance IN (0, 1)),
    PRIMARY KEY (watch_id, sequence),
    UNIQUE (watch_id, target_snapshot_id),
    UNIQUE (watch_id, sequence, target_snapshot_id, operation_id),
    FOREIGN KEY (operation_id, watch_id, sequence)
        REFERENCES operations(id, watch_id, sequence),
    FOREIGN KEY (comparison_id, comparison_from_snapshot_id, target_snapshot_id)
        REFERENCES comparisons(id, from_snapshot_id, to_snapshot_id)
) WITHOUT ROWID;

CREATE INDEX watch_cuts_ready_range
ON watch_cuts(watch_id, sequence, comparison_id)
WHERE state = 'ready';

-- One committed external-clock boundary per fsmonitor-visible cut. Guard fields are an
-- optional precision cursor, not part of the coarse dirty-witness proof.
CREATE TABLE fsmonitor_boundaries (
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    cut_sequence    INTEGER NOT NULL CHECK (cut_sequence > 0),
    target_snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
    boundary_kind   TEXT NOT NULL CHECK (boundary_kind = 'cut'),
    cut_operation_id BLOB NOT NULL,
    clock_epoch     BLOB NOT NULL CHECK (length(clock_epoch) = 16),
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    guard_sequence  INTEGER CHECK (guard_sequence IS NULL OR guard_sequence >= 0),
    guard_complete  INTEGER NOT NULL CHECK (guard_complete IN (0, 1)),
    PRIMARY KEY (watch_id, cut_sequence),
    UNIQUE (watch_id, target_snapshot_id),
    FOREIGN KEY (watch_id, cut_sequence, target_snapshot_id, cut_operation_id)
        REFERENCES watch_cuts(watch_id, sequence,
                              target_snapshot_id, operation_id),
    CHECK ((guard_complete = 0
            AND guard_epoch IS NULL AND guard_sequence IS NULL)
        OR (guard_complete = 1
            AND guard_epoch IS NOT NULL AND guard_sequence IS NOT NULL))
) WITHOUT ROWID;

CREATE TABLE query_leases (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    authorization_id BLOB NOT NULL CHECK (length(authorization_id) = 16),
    clock_epoch     BLOB NOT NULL CHECK (length(clock_epoch) = 16),
    from_cut_sequence INTEGER,
    to_cut_sequence INTEGER NOT NULL,
    guard_epoch     BLOB CHECK (guard_epoch IS NULL OR length(guard_epoch) = 16),
    from_guard_sequence INTEGER,
    to_guard_sequence INTEGER,
    lease_owner     BLOB NOT NULL,
    lease_fence     INTEGER NOT NULL,
    lease_expires_ns INTEGER NOT NULL,
    state           TEXT NOT NULL CHECK (state IN ('active', 'released')),
    FOREIGN KEY (authorization_id, watch_id)
        REFERENCES watch_grants(id, watch_id),
    CHECK (from_cut_sequence IS NULL
        OR from_cut_sequence <= to_cut_sequence),
    CHECK (from_guard_sequence IS NULL
        OR from_guard_sequence <= to_guard_sequence),
    CHECK ((guard_epoch IS NULL
            AND from_guard_sequence IS NULL AND to_guard_sequence IS NULL)
        OR (guard_epoch IS NOT NULL
            AND from_guard_sequence IS NOT NULL
            AND to_guard_sequence IS NOT NULL))
);

CREATE TABLE query_revision_pins (
    query_id        BLOB NOT NULL REFERENCES query_leases(id),
    revision_id     INTEGER NOT NULL REFERENCES revisions(id),
    PRIMARY KEY (query_id, revision_id)
) WITHOUT ROWID;

CREATE TABLE query_comparison_pins (
    query_id        BLOB NOT NULL REFERENCES query_leases(id),
    comparison_id   INTEGER NOT NULL REFERENCES comparisons(id),
    PRIMARY KEY (query_id, comparison_id)
) WITHOUT ROWID;

-- Physical GC is manager policy, not an unfenced ioctl issued directly from a
-- caller request. Caller ADMIN operations may change retention/watch state;
-- this independent durable intent owns the eventual privileged deletion.
CREATE TABLE snapshot_delete_operations (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    snapshot_id     INTEGER NOT NULL UNIQUE REFERENCES snapshots(id),
    filesystem_id   INTEGER NOT NULL REFERENCES filesystems(id),
    state           TEXT NOT NULL CHECK
                    (state IN ('planned', 'fs_started', 'fs_deleted',
                               'delete_durable', 'done', 'failed')),
    lease_owner     BLOB,
    lease_fence     INTEGER NOT NULL DEFAULT 0,
    lease_expires_ns INTEGER,
    error           TEXT,
    updated_ns      INTEGER NOT NULL,
    UNIQUE (id, snapshot_id)
);

-- Caller-controlled retention is revocable/expiring authorization state, not
-- an untyped permanent pin. Internal topology/job pins remain separate.
CREATE TABLE retention_leases (
    id              BLOB NOT NULL PRIMARY KEY CHECK (length(id) = 16),
    watch_id        BLOB NOT NULL REFERENCES watches(id),
    authorization_id BLOB NOT NULL CHECK (length(authorization_id) = 16),
    snapshot_id     INTEGER NOT NULL REFERENCES snapshots(id),
    state           TEXT NOT NULL CHECK
                    (state IN ('active', 'released', 'revoked', 'expired')),
    lease_fence     INTEGER NOT NULL DEFAULT 0,
    expires_ns      INTEGER NOT NULL,
    created_ns      INTEGER NOT NULL,
    FOREIGN KEY (authorization_id, watch_id)
        REFERENCES watch_grants(id, watch_id),
    UNIQUE (id, snapshot_id)
);

CREATE TABLE snapshot_pins (
    snapshot_id     INTEGER NOT NULL REFERENCES snapshots(id),
    owner_kind      TEXT NOT NULL CHECK
                    (owner_kind IN ('watch-indexed-head', 'watch-last-cut',
                                    'operation', 'comparison',
                                    'retention-lease', 'consumer-baseline')),
    owner_id        BLOB NOT NULL,
    reason          TEXT NOT NULL,
    PRIMARY KEY (snapshot_id, owner_kind, owner_id, reason)
) WITHOUT ROWID;

CREATE TRIGGER snapshot_pins_only_present
BEFORE INSERT ON snapshot_pins
WHEN (SELECT physical_state FROM snapshots WHERE id = NEW.snapshot_id)
     IS NOT 'present'
BEGIN
    SELECT RAISE(ABORT, 'cannot pin a non-present snapshot');
END;

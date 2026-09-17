use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const CONNECTION_SCHEMA: &str = include_str!("store_connection.sql");
const MANAGER_SCHEMA: &str = include_str!("store_schema.sql");
const SCHEMA_VERSION: i64 = 14;
const MANAGER_APPLICATION_ID: i64 = 0x4241_5731; // BAW1
const MANAGER_WAL_SIZE_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceMetadata {
    pub store_uuid: [u8; 16],
    pub clock_format_version: u32,
    pub last_boot_id: [u8; 16],
    pub created_ns: i64,
}

impl ServiceMetadata {
    pub fn generate(last_boot_id: [u8; 16], created_ns: i64) -> Result<Self, StoreError> {
        Ok(Self {
            store_uuid: *Uuid::new_v4().as_bytes(),
            clock_format_version: 1,
            last_boot_id,
            created_ns,
        })
    }
}

#[derive(Debug)]
pub struct Store {
    path: PathBuf,
    connection: Connection,
}

impl Drop for Store {
    fn drop(&mut self) {
        let mut wal_path = self.path.as_os_str().to_os_string();
        wal_path.push("-wal");
        let Ok(metadata) = fs::metadata(wal_path) else {
            return;
        };
        if metadata.len() <= MANAGER_WAL_SIZE_LIMIT {
            return;
        }
        // Last-connection close can discard the shared WAL index, including
        // checkpoint progress. A sequence of short callers must still reclaim
        // an oversized WAL even if none writes twice after a checkpoint.
        // Never wait for a reader to release its snapshot on this path.
        let result = self.connection.busy_timeout(Duration::ZERO).and_then(|()| {
            self.connection
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })
        });
        match result {
            Ok((busy, log_frames, checkpointed_frames)) => tracing::debug!(
                wal_bytes = metadata.len(),
                busy,
                log_frames,
                checkpointed_frames,
                "oversized manager WAL checkpoint"
            ),
            Err(error) => tracing::warn!(%error, "defer oversized manager WAL checkpoint"),
        }
    }
}

#[derive(Debug)]
pub struct StoreError {
    message: String,
}

impl StoreError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::new(error.to_string())
    }
}

impl Store {
    pub fn create(path: &Path, metadata: &ServiceMetadata) -> Result<Self, StoreError> {
        create_private_file(path)?;
        let result = (|| {
            let mut connection = open_connection(path)?;
            configure_manager_connection(&connection)?;
            install_manager_schema(&mut connection, metadata)?;
            Ok(Self {
                path: path.to_owned(),
                connection,
            })
        })();
        if result.is_err() {
            cleanup_new_database(path);
        }
        result
    }

    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let mut connection = open_connection(path)?;
        configure_manager_connection(&connection)?;
        migrate_manager_schema(&mut connection)?;
        verify_database_header(&connection, MANAGER_APPLICATION_ID)?;
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM service_metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        if count != 1 {
            return Err(StoreError::new(
                "manager database does not contain exactly one service metadata row",
            ));
        }
        Ok(Self {
            path: path.to_owned(),
            connection,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Creates a fresh child operational database. The child publishes its
    /// own immutable baseline identity; no parent path graph is copied.
    pub fn create_descendant_seed(
        parent: &mut Store,
        path: &Path,
        metadata: &ServiceMetadata,
        seed_revision_id: i64,
        now_ns: i64,
        inert_snapshot_path_prefix: &[u8],
    ) -> Result<Self, StoreError> {
        let _ = (parent, seed_revision_id, now_ns, inert_snapshot_path_prefix);
        Self::create(path, metadata)
    }

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub fn metadata(&self) -> Result<ServiceMetadata, StoreError> {
        self.connection
            .query_row(
                "SELECT store_uuid, clock_format_version, \
                        last_boot_id, created_ns \
                 FROM service_metadata WHERE singleton = 1",
                [],
                |row| {
                    Ok(ServiceMetadata {
                        store_uuid: fixed_blob(row.get_ref(0)?.as_blob()?, "store_uuid")?,
                        clock_format_version: row.get(1)?,
                        last_boot_id: fixed_blob(row.get_ref(2)?.as_blob()?, "last_boot_id")?,
                        created_ns: row.get(3)?,
                    })
                },
            )
            .map_err(StoreError::from)
    }

    pub fn foreign_key_violations(&self) -> Result<Vec<String>, StoreError> {
        let mut statement = self.connection.prepare("PRAGMA foreign_key_check")?;
        let rows = statement.query_map([], |row| {
            let table: String = row.get(0)?;
            let rowid: Option<i64> = row.get(1)?;
            let parent: String = row.get(2)?;
            let fk: i64 = row.get(3)?;
            Ok(format!("{table} row {rowid:?} -> {parent} constraint {fk}"))
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::from)
    }
}

pub fn encode_u64(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

pub fn decode_u64(value: &[u8]) -> Result<u64, StoreError> {
    let value: [u8; 8] = value
        .try_into()
        .map_err(|_| StoreError::new(format!("U64 BLOB has length {}, expected 8", value.len())))?;
    Ok(u64::from_be_bytes(value))
}

fn create_private_file(path: &Path) -> Result<(), StoreError> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map(|_| ())
        .map_err(|error| StoreError::new(format!("create {}: {error}", path.display())))
}

fn open_connection(path: &Path) -> Result<Connection, StoreError> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_URI
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|error| StoreError::new(format!("open {}: {error}", path.display())))
}

fn configure_manager_connection(connection: &Connection) -> Result<(), StoreError> {
    connection.busy_timeout(Duration::from_secs(30))?;
    // Direct callers are short lived. Keep the committed WAL between calls
    // instead of checkpointing, unlinking and recreating it for every hook.
    // FULL still synchronizes every commit; passive automatic checkpoints
    // amortize database writes without waiting for an active reader.
    connection.set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)?;
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;\n\
         PRAGMA synchronous = FULL;\n\
         PRAGMA wal_autocheckpoint = 1000;\n\
         PRAGMA temp_store = FILE;\n\
         PRAGMA trusted_schema = OFF;",
    )?;
    connection.pragma_update(None, "journal_size_limit", MANAGER_WAL_SIZE_LIMIT)?;
    Ok(())
}

fn install_manager_schema(
    connection: &mut Connection,
    metadata: &ServiceMetadata,
) -> Result<(), StoreError> {
    connection.execute_batch(CONNECTION_SCHEMA)?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(MANAGER_SCHEMA)?;
    transaction.execute_batch(
        "CREATE TABLE schema_migrations (\n\
             version INTEGER PRIMARY KEY,\n\
             name TEXT NOT NULL UNIQUE,\n\
             applied_ns INTEGER NOT NULL\n\
         );",
    )?;
    transaction.execute(
        "INSERT INTO service_metadata(\
             singleton, store_uuid, clock_format_version,\
             last_boot_id, created_ns\
         ) VALUES (1, ?1, ?2, ?3, ?4)",
        params![
            metadata.store_uuid.as_slice(),
            metadata.clock_format_version,
            metadata.last_boot_id.as_slice(),
            metadata.created_ns,
        ],
    )?;
    transaction.execute(
        "INSERT INTO schema_migrations(version, name, applied_ns) \
         VALUES (14, 'observed-git-cursors-v14', ?1)",
        [metadata.created_ns],
    )?;
    transaction.pragma_update(None, "application_id", MANAGER_APPLICATION_ID)?;
    transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    transaction.commit()?;
    verify_database_header(connection, MANAGER_APPLICATION_ID)
}

fn migrate_manager_schema(connection: &mut Connection) -> Result<(), StoreError> {
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if application_id != MANAGER_APPLICATION_ID {
        return Err(StoreError::new(format!(
            "unexpected SQLite application_id {application_id:#x}"
        )));
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if matches!(version, 11..=13) {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if version == 11 {
            transaction.execute_batch("ALTER TABLE service_metadata DROP COLUMN clock_hmac_key")?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, name, applied_ns) \
                 SELECT 12, 'unkeyed-cursor-integrity-v12', created_ns \
                   FROM service_metadata WHERE singleton = 1",
                [],
            )?;
        }
        if version <= 12 {
            // Old publishers could only commit an adjacent cut. Unpublished
            // attempts have no logical predecessor sequence yet: recovery obtains
            // it from the exact still-current indexed snapshot when publishing.
            transaction.execute_batch(
                "ALTER TABLE watch_cuts ADD COLUMN base_sequence INTEGER
                     CHECK (base_sequence >= 0 AND base_sequence < sequence);
                 UPDATE watch_cuts SET base_sequence = sequence - 1 WHERE state = 'ready';",
            )?;
            transaction.execute(
                "INSERT INTO schema_migrations(version, name, applied_ns) \
                 SELECT 13, 'published-cut-predecessors-v13', created_ns \
                   FROM service_metadata WHERE singleton = 1",
                [],
            )?;
        }
        // Issued boundaries do not prove that Git ever queried a cursor.
        // Leave observations empty and preserve the existing replay floor.
        transaction.execute_batch(
            "CREATE TABLE git_cursor_observations (
                 watch_id BLOB NOT NULL REFERENCES watches(id),
                 cut_sequence INTEGER NOT NULL CHECK (cut_sequence > 0),
                 last_queried_ns INTEGER NOT NULL,
                 retain_until_ns INTEGER,
                 PRIMARY KEY (watch_id, cut_sequence),
                 FOREIGN KEY (watch_id, cut_sequence)
                     REFERENCES fsmonitor_boundaries(watch_id, cut_sequence),
                 CHECK (retain_until_ns IS NULL OR retain_until_ns >= last_queried_ns)
             ) WITHOUT ROWID;
             CREATE UNIQUE INDEX git_cursor_observations_head
                 ON git_cursor_observations(watch_id) WHERE retain_until_ns IS NULL;
             CREATE INDEX git_cursor_observations_expiry
                 ON git_cursor_observations(watch_id, retain_until_ns, cut_sequence)
                 WHERE retain_until_ns IS NOT NULL;",
        )?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, applied_ns) \
             SELECT 14, 'observed-git-cursors-v14', created_ns \
               FROM service_metadata WHERE singleton = 1",
            [],
        )?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        return Ok(());
    }
    if (1..SCHEMA_VERSION).contains(&version) {
        return Err(StoreError::new(format!(
            "AWACS root state uses legacy schema version {version}, but this binary requires \
             schema version {SCHEMA_VERSION}; please run \
             `jj util subvolume enable --rebuild-baseline` from this worktree"
        )));
    }
    Err(StoreError::new(format!(
        "unsupported manager database schema version {version}"
    )))
}
fn verify_database_header(
    connection: &Connection,
    expected_application_id: i64,
) -> Result<(), StoreError> {
    let application_id: i64 =
        connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
    if application_id != expected_application_id {
        return Err(StoreError::new(format!(
            "unexpected SQLite application_id {application_id:#x}"
        )));
    }
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(StoreError::new(format!(
            "unsupported database schema version {version}"
        )));
    }
    let migration: Option<i64> = connection
        .query_row(
            "SELECT version FROM schema_migrations WHERE version = ?1",
            [SCHEMA_VERSION],
            |row| row.get(0),
        )
        .optional()?;
    if migration != Some(SCHEMA_VERSION) {
        return Err(StoreError::new("database migration record is missing"));
    }
    Ok(())
}

fn fixed_blob<const N: usize>(value: &[u8], field: &str) -> rusqlite::Result<[u8; N]> {
    value.try_into().map_err(|_| {
        rusqlite::Error::FromSqlConversionFailure(
            value.len(),
            rusqlite::types::Type::Blob,
            Box::new(StoreError::new(format!(
                "{field} has length {}, expected {N}",
                value.len()
            ))),
        )
    })
}

fn cleanup_new_database(path: &Path) {
    for candidate in [
        path.to_owned(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        let _ = fs::remove_file(candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    fn metadata() -> ServiceMetadata {
        ServiceMetadata {
            store_uuid: [1; 16],
            clock_format_version: 1,
            last_boot_id: [3; 16],
            created_ns: 123,
        }
    }

    #[test]
    fn creates_and_reopens_normative_manager_schema() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        let store = Store::create(&path, &metadata()).unwrap();
        assert_eq!(store.metadata().unwrap(), metadata());
        assert!(store.foreign_key_violations().unwrap().is_empty());
        let table_count: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM sqlite_schema WHERE type = 'table'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(table_count >= 18);
        let legacy_trigger_tables: i64 = store
            .connection()
            .query_row(
                "SELECT count(*) FROM sqlite_schema \
                 WHERE type = 'table' AND name = 'watchman_triggers'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_trigger_tables, 0);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(store);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.metadata().unwrap(), metadata());
    }

    #[test]
    fn retains_committed_wal_between_direct_callers() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manager.sqlite3");
        let wal = temp.path().join("manager.sqlite3-wal");
        let store = Store::create(&path, &metadata()).unwrap();
        assert_eq!(
            store
                .connection()
                .query_row("PRAGMA synchronous", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            2,
        );
        assert_eq!(
            store
                .connection()
                .query_row("PRAGMA wal_autocheckpoint", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            1000,
        );
        store
            .connection()
            .execute("UPDATE service_metadata SET created_ns = 456", [])
            .unwrap();
        let wal_size = fs::metadata(&wal).unwrap().len();
        assert!(wal_size > 32);
        drop(store);
        assert_eq!(fs::metadata(&wal).unwrap().len(), wal_size);
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.metadata().unwrap().created_ns, 456);
        assert!(reopened.foreign_key_violations().unwrap().is_empty());
        assert_eq!(
            fs::metadata(&wal).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn committed_wal_survives_process_exit_without_connection_cleanup() {
        const CHILD_PATH: &str = "AWACS_TEST_COMMITTED_WAL_EXIT_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let store = Store::create(Path::new(&path), &metadata()).unwrap();
            store
                .connection()
                .execute("UPDATE service_metadata SET created_ns = 789", [])
                .unwrap();
            // Exit without dropping the connection: committed WAL frames must
            // be usable after a process crash, before any checkpoint runs.
            std::process::exit(0);
        }
        let temp = tempdir().unwrap();
        let path = temp.path().join("manager.sqlite3");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store::tests::committed_wal_survives_process_exit_without_connection_cleanup",
            ])
            .env(CHILD_PATH, &path)
            .status()
            .unwrap();
        assert!(status.success());
        let reopened = Store::open(&path).unwrap();
        assert_eq!(reopened.metadata().unwrap().created_ns, 789);
        assert!(reopened.foreign_key_violations().unwrap().is_empty());
    }

    #[test]
    fn automatic_checkpoints_resume_after_a_reader_releases_its_snapshot() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manager.sqlite3");
        let writer = Store::create(&path, &metadata()).unwrap();
        // Exercise the same automatic checkpoint policy with fewer pages.
        writer
            .connection()
            .pragma_update(None, "wal_autocheckpoint", 1)
            .unwrap();
        let reader = Store::open(&path).unwrap();
        reader.connection().execute_batch("BEGIN").unwrap();
        assert_eq!(reader.metadata().unwrap().created_ns, 123);
        writer
            .connection()
            .execute("UPDATE service_metadata SET created_ns = 456", [])
            .unwrap();
        assert_eq!(reader.metadata().unwrap().created_ns, 123);
        writer.connection().busy_timeout(Duration::ZERO).unwrap();
        let busy: i64 = writer
            .connection()
            .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| row.get(0))
            .unwrap();
        assert_eq!(busy, 1);
        reader.connection().execute_batch("COMMIT").unwrap();
        writer
            .connection()
            .execute("UPDATE service_metadata SET created_ns = 789", [])
            .unwrap();
        // Opening the main database as immutable ignores the WAL and proves
        // the threshold-triggered checkpoint copied the latest committed data.
        let checkpointed = Connection::open_with_flags(
            format!("file:{}?immutable=1", path.display()),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .unwrap();
        let created: i64 = checkpointed
            .query_row("SELECT created_ns FROM service_metadata", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(created, 789);
    }

    #[test]
    fn checkpoints_recycle_large_wals_across_direct_callers() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manager.sqlite3");
        let wal = temp.path().join("manager.sqlite3-wal");
        let store = Store::create(&path, &metadata()).unwrap();
        let reader = Store::open(&path).unwrap();
        reader.connection().execute_batch("BEGIN").unwrap();
        assert_eq!(reader.metadata().unwrap().created_ns, 123);
        store
            .connection()
            .execute_batch(
                "CREATE TABLE checkpoint_test(data BLOB);\n\
             WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x < 1100)\n\
             INSERT INTO checkpoint_test SELECT zeroblob(8192) FROM n;",
            )
            .unwrap();
        drop(store);
        // The close-time checkpoint must defer instead of waiting on a reader.
        assert!(fs::metadata(&wal).unwrap().len() > MANAGER_WAL_SIZE_LIMIT);
        reader.connection().execute_batch("COMMIT").unwrap();
        drop(reader);
        for created_ns in [456, 789] {
            let store = Store::open(&path).unwrap();
            store
                .connection()
                .execute("UPDATE service_metadata SET created_ns = ?1", [created_ns])
                .unwrap();
        }
        // The limit applies when the WAL can be recycled, not while a reader
        // pins old frames or a single large transaction is still in progress.
        assert!(fs::metadata(&wal).unwrap().len() <= MANAGER_WAL_SIZE_LIMIT);
        assert_eq!(
            Store::open(&path).unwrap().metadata().unwrap().created_ns,
            789
        );
    }

    #[test]
    fn migrates_v11_manager_without_rebuilding_or_retaining_the_old_key() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        let store = Store::create(&path, &metadata()).unwrap();
        store
            .connection()
            .execute_batch(
                "ALTER TABLE service_metadata ADD COLUMN clock_hmac_key BLOB;
                 UPDATE service_metadata SET clock_hmac_key = zeroblob(32);
                 DROP TABLE git_cursor_observations;
                 ALTER TABLE watch_cuts DROP COLUMN base_sequence;
                 DELETE FROM schema_migrations WHERE version >= 12;
                 INSERT INTO schema_migrations(version, name, applied_ns)
                     VALUES (11, 'atomic-published-cut-heads-v11', 123);
                 PRAGMA user_version = 11;",
            )
            .unwrap();
        drop(store);

        let migrated = Store::open(&path).unwrap();
        assert_eq!(migrated.metadata().unwrap(), metadata());
        let old_key_columns: i64 = migrated
            .connection()
            .query_row(
                "SELECT count(*) FROM pragma_table_info('service_metadata') \
                  WHERE name = 'clock_hmac_key'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(old_key_columns, 0);
        assert_eq!(
            migrated
                .connection()
                .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                .unwrap(),
            SCHEMA_VERSION
        );
    }

    #[test]
    fn refuses_to_overwrite_an_existing_database() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("state.sqlite3");
        Store::create(&path, &metadata()).unwrap();
        assert!(
            Store::create(&path, &metadata())
                .unwrap_err()
                .to_string()
                .contains("create")
        );
        assert_eq!(Store::open(&path).unwrap().metadata().unwrap(), metadata());
    }

    #[test]
    fn rejects_v1_manager_with_rebuild_guidance() {
        let temp = tempdir().unwrap();
        let manager_path = temp.path().join("manager.sqlite3");
        let manager = Store::create(&manager_path, &metadata()).unwrap();
        manager
            .connection()
            .execute("DELETE FROM schema_migrations WHERE version >= 2", [])
            .unwrap();
        manager
            .connection()
            .pragma_update(None, "user_version", 1)
            .unwrap();
        drop(manager);
        assert!(
            Store::open(&manager_path)
                .unwrap_err()
                .to_string()
                .contains("please run `jj util subvolume enable --rebuild-baseline`")
        );
    }

    #[test]
    fn rejects_v4_manager_with_rebuild_guidance() {
        let temp = tempdir().unwrap();
        let path = temp.path().join("manager.sqlite3");
        let store = Store::create(&path, &metadata()).unwrap();
        store
            .connection()
            .execute("CREATE TABLE watchman_triggers (legacy INTEGER)", [])
            .unwrap();
        store
            .connection()
            .execute("DELETE FROM schema_migrations WHERE version = 5", [])
            .unwrap();
        store
            .connection()
            .execute("DELETE FROM schema_migrations WHERE version = 6", [])
            .unwrap();
        store
            .connection()
            .pragma_update(None, "user_version", 4)
            .unwrap();
        drop(store);

        let error = Store::open(&path).unwrap_err().to_string();
        assert!(error.contains("legacy schema version 4"));
        assert!(error.contains(&format!("requires schema version {SCHEMA_VERSION}")));
        assert!(error.contains("please run `jj util subvolume enable --rebuild-baseline`"));
    }

    #[test]
    fn stores_unsigned_values_in_sortable_big_endian_blobs() {
        let values = [0, 1, i64::MAX as u64, i64::MAX as u64 + 1, u64::MAX];
        let mut blobs: Vec<_> = values.into_iter().map(encode_u64).collect();
        blobs.sort();
        assert_eq!(
            blobs
                .iter()
                .map(|blob| decode_u64(blob).unwrap())
                .collect::<Vec<_>>(),
            values
        );
        assert!(decode_u64(&[0; 7]).is_err());
    }
}

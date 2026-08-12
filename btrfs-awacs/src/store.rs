use getrandom::fill as random_fill;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::fmt;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;

const CONNECTION_SCHEMA: &str = include_str!("store_connection.sql");
const MANAGER_SCHEMA: &str = include_str!("store_schema.sql");
const SCHEMA_VERSION: i64 = 6;
const MANAGER_APPLICATION_ID: i64 = 0x4241_5731; // BAW1

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceMetadata {
    pub store_uuid: [u8; 16],
    pub clock_hmac_key: [u8; 32],
    pub clock_format_version: u32,
    pub last_boot_id: [u8; 16],
    pub created_ns: i64,
}

impl ServiceMetadata {
    pub fn generate(last_boot_id: [u8; 16], created_ns: i64) -> Result<Self, StoreError> {
        let mut clock_hmac_key = [0; 32];
        random_fill(&mut clock_hmac_key)
            .map_err(|error| StoreError::new(format!("obtain random HMAC key: {error}")))?;
        Ok(Self {
            store_uuid: *Uuid::new_v4().as_bytes(),
            clock_hmac_key,
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

    pub fn connection(&self) -> &Connection {
        &self.connection
    }

    pub fn connection_mut(&mut self) -> &mut Connection {
        &mut self.connection
    }

    pub fn metadata(&self) -> Result<ServiceMetadata, StoreError> {
        self.connection
            .query_row(
                "SELECT store_uuid, clock_hmac_key, clock_format_version, \
                        last_boot_id, created_ns \
                 FROM service_metadata WHERE singleton = 1",
                [],
                |row| {
                    Ok(ServiceMetadata {
                        store_uuid: fixed_blob(row.get_ref(0)?.as_blob()?, "store_uuid")?,
                        clock_hmac_key: fixed_blob(row.get_ref(1)?.as_blob()?, "clock_hmac_key")?,
                        clock_format_version: row.get(2)?,
                        last_boot_id: fixed_blob(row.get_ref(3)?.as_blob()?, "last_boot_id")?,
                        created_ns: row.get(4)?,
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
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;\n\
         PRAGMA synchronous = FULL;\n\
         PRAGMA temp_store = FILE;\n\
         PRAGMA trusted_schema = OFF;",
    )?;
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
             singleton, store_uuid, clock_hmac_key, clock_format_version,\
             last_boot_id, created_ns\
         ) VALUES (1, ?1, ?2, ?3, ?4, ?5)",
        params![
            metadata.store_uuid.as_slice(),
            metadata.clock_hmac_key.as_slice(),
            metadata.clock_format_version,
            metadata.last_boot_id.as_slice(),
            metadata.created_ns,
        ],
    )?;
    transaction.execute(
        "INSERT INTO schema_migrations(version, name, applied_ns) \
         VALUES (1, 'normative-v1', ?1)",
        [metadata.created_ns],
    )?;
    transaction.execute(
        "INSERT INTO schema_migrations(version, name, applied_ns) \
         VALUES (2, 'recovery-v2', ?1)",
        [metadata.created_ns],
    )?;
    transaction.execute(
        "INSERT INTO schema_migrations(version, name, applied_ns) \
         VALUES (3, 'composable-summaries-v3', ?1)",
        [metadata.created_ns],
    )?;
    transaction.execute(
        "INSERT INTO schema_migrations(version, name, applied_ns) \
         VALUES (4, 'generic-watchman-trigger-argv-v4', ?1)",
        [metadata.created_ns],
    )?;
    transaction.execute(
        "INSERT INTO schema_migrations(version, name, applied_ns) \
         VALUES (5, 'remove-watchman-triggers-v5', ?1)",
        [metadata.created_ns],
    )?;
    transaction.execute(
        "INSERT INTO schema_migrations(version, name, applied_ns) \
         VALUES (6, 'consumer-baseline-pins-v6', ?1)",
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
    let mut version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version == SCHEMA_VERSION {
        return Ok(());
    }
    if !(1..=SCHEMA_VERSION).contains(&version) {
        return Err(StoreError::new(format!(
            "unsupported manager database schema version {version}"
        )));
    }
    if version == 1 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, applied_ns) \
             VALUES (2, 'recovery-v2', 0)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 2)?;
        transaction.commit()?;
        version = 2;
    }
    if version == 2 {
        let has_summary_version: i64 = connection.query_row(
            "SELECT count(*) FROM pragma_table_info('revisions') \
             WHERE name = 'summary_version'",
            [],
            |row| row.get(0),
        )?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let has_owner_cardinality: i64 = transaction.query_row(
            "SELECT count(*) FROM pragma_table_info('revisions') \
             WHERE name = 'owner_cardinality'",
            [],
            |row| row.get(0),
        )?;
        let has_owner_uid_xor: i64 = transaction.query_row(
            "SELECT count(*) FROM pragma_table_info('revisions') \
             WHERE name = 'owner_uid_xor'",
            [],
            |row| row.get(0),
        )?;
        if has_summary_version == 0 {
            transaction.execute_batch(
                "ALTER TABLE revisions ADD COLUMN summary_version INTEGER NOT NULL \
                 DEFAULT 1 CHECK (summary_version IN (1, 2));",
            )?;
        }
        if has_owner_cardinality == 0 {
            transaction
                .execute_batch("ALTER TABLE revisions ADD COLUMN owner_cardinality INTEGER;")?;
        }
        if has_owner_uid_xor == 0 {
            transaction.execute_batch(
                "ALTER TABLE revisions ADD COLUMN owner_uid_xor BLOB \
                 CHECK (owner_uid_xor IS NULL OR length(owner_uid_xor) = 8);",
            )?;
        }
        transaction.execute_batch(
            r#"CREATE TABLE IF NOT EXISTS checkpoint_owner_counts (
                   revision_id INTEGER NOT NULL REFERENCES revision_checkpoints(revision_id),
                   uid BLOB NOT NULL CHECK (length(uid) = 8),
                   object_count INTEGER NOT NULL CHECK (object_count > 0),
                   PRIMARY KEY (revision_id, uid)
               ) WITHOUT ROWID;
               CREATE TABLE IF NOT EXISTS owner_count_overrides (
                   revision_id INTEGER NOT NULL REFERENCES revisions(id),
                   uid BLOB NOT NULL CHECK (length(uid) = 8),
                   object_count INTEGER NOT NULL CHECK (object_count >= 0),
                   PRIMARY KEY (revision_id, uid)
               ) WITHOUT ROWID;"#,
        )?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, applied_ns) \
             VALUES (3, 'composable-summaries-v3', 0)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 3)?;
        transaction.commit()?;
        version = 3;
    }
    if version == 3 {
        // Version 4 previously widened a dormant trigger table. The table is
        // gone in version 5, so an older database only needs to discard any
        // legacy copy before recording the historical version-4 step.
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch("DROP TABLE IF EXISTS watchman_triggers;")?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, applied_ns) VALUES (4, 'generic-watchman-trigger-argv-v4', 0)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 4)?;
        transaction.commit()?;
        version = 4;
    }
    if version == 4 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch("DROP TABLE IF EXISTS watchman_triggers;")?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, applied_ns) VALUES (5, 'remove-watchman-triggers-v5', 0)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 5)?;
        transaction.commit()?;
        version = 5;
    }
    if version == 5 {
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute_batch(
            r#"CREATE TABLE snapshot_pins_v6 (
                   snapshot_id INTEGER NOT NULL REFERENCES snapshots(id),
                   owner_kind TEXT NOT NULL CHECK
                       (owner_kind IN ('watch-indexed-head', 'watch-last-cut',
                                       'operation', 'comparison',
                                       'retention-lease', 'consumer-baseline')),
                   owner_id BLOB NOT NULL,
                   reason TEXT NOT NULL,
                   PRIMARY KEY (snapshot_id, owner_kind, owner_id, reason)
               ) WITHOUT ROWID;
               INSERT INTO snapshot_pins_v6 SELECT * FROM snapshot_pins;
               DROP TABLE snapshot_pins;
               ALTER TABLE snapshot_pins_v6 RENAME TO snapshot_pins;
               CREATE TRIGGER snapshot_pins_only_present
               BEFORE INSERT ON snapshot_pins
               WHEN (SELECT physical_state FROM snapshots WHERE id = NEW.snapshot_id)
                    IS NOT 'present'
               BEGIN
                   SELECT RAISE(ABORT, 'cannot pin a non-present snapshot');
               END;"#,
        )?;
        transaction.execute(
            "INSERT INTO schema_migrations(version, name, applied_ns) VALUES (6, 'consumer-baseline-pins-v6', 0)",
            [],
        )?;
        transaction.pragma_update(None, "user_version", 6)?;
        transaction.commit()?;
    }
    Ok(())
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
            clock_hmac_key: [2; 32],
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
        assert!(table_count >= 25);
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
    fn migrates_v1_manager_through_current_schema() {
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
        let manager = Store::open(&manager_path).unwrap();
        let manager_version: i64 = manager
            .connection()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(manager_version, SCHEMA_VERSION);
    }

    #[test]
    fn migrates_v4_manager_by_dropping_legacy_trigger_table() {
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

        let migrated = Store::open(&path).unwrap();
        let legacy_trigger_tables: i64 = migrated
            .connection()
            .query_row(
                "SELECT count(*) FROM sqlite_schema \
                 WHERE type = 'table' AND name = 'watchman_triggers'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_trigger_tables, 0);
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

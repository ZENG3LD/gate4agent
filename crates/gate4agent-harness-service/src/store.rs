use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use std::path::Path;
use thiserror::Error;

pub const HARNESS_STORE_SCHEMA_VERSION: i64 = 1;
pub const MAX_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_ENTITY_BYTES: usize = 1024 * 1024;
pub const MAX_OPERATION_BYTES: usize = 1024 * 1024;
pub const HARNESS_OPERATION_TAIL_MAX: usize = 4_096;
pub const HARNESS_ENTITY_ROWS_MAX: usize = 16_384;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedEntity {
    pub id: String,
    pub revision: u64,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedOperation {
    pub id: String,
    pub revision: u64,
    pub request_digest: String,
    pub state: String,
    pub payload: Vec<u8>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PersistedHarnessState {
    pub checkpoint: Vec<u8>,
    pub tasks: Vec<PersistedEntity>,
    pub runs: Vec<PersistedEntity>,
    pub grants: Vec<PersistedEntity>,
    pub deliveries: Vec<PersistedEntity>,
    pub continuations: Vec<PersistedEntity>,
    pub dispatches: Vec<PersistedEntity>,
    pub harness_mcp_reservations: Vec<PersistedEntity>,
    pub operations: Vec<PersistedOperation>,
}

pub struct HarnessStore {
    connection: Connection,
}

impl HarnessStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, HarnessStoreError> {
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.busy_timeout(std::time::Duration::ZERO)?;

        // user_version must be checked before any pragma or DDL that may rewrite
        // a database created by a newer binary.
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 0 && version != HARNESS_STORE_SCHEMA_VERSION {
            return Err(HarnessStoreError::UnsupportedSchema(version));
        }

        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "locking_mode", "EXCLUSIVE")?;
        connection.execute_batch(
            "BEGIN EXCLUSIVE;
             CREATE TABLE IF NOT EXISTS harness_checkpoint (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                checkpoint_version INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_tasks (
                entity_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_runs (
                entity_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_grants (
                entity_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_operations (
                operation_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                request_digest TEXT NOT NULL,
                state TEXT NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_deliveries (
                entity_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_dispatch_contexts (
                entity_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_continuations (
                entity_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_mcp_reservations (
                entity_id TEXT PRIMARY KEY,
                revision INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS harness_operation_tail (
                position INTEGER PRIMARY KEY AUTOINCREMENT,
                operation_id TEXT NOT NULL,
                revision INTEGER NOT NULL,
                request_digest TEXT NOT NULL,
                state TEXT NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS harness_operation_tail_id
                ON harness_operation_tail(operation_id, revision);
             COMMIT;",
        )?;
        if version == 0 {
            connection.pragma_update(None, "user_version", HARNESS_STORE_SCHEMA_VERSION)?;
        }
        Ok(Self { connection })
    }

    pub fn load_checkpoint(&self) -> Result<Option<Vec<u8>>, HarnessStoreError> {
        let stored = self.connection.query_row(
            "SELECT checkpoint_version, payload FROM harness_checkpoint WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        ).optional()?;
        let Some((version, payload)) = stored else {
            return Ok(None);
        };
        if version != 1 {
            return Err(HarnessStoreError::UnsupportedCheckpoint(version));
        }
        ensure_bound("checkpoint", payload.len(), MAX_CHECKPOINT_BYTES)?;
        Ok(Some(payload))
    }

    pub fn load_harness_mcp_reservations(
        &self,
    ) -> Result<Vec<PersistedEntity>, HarnessStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT entity_id, revision, payload
             FROM harness_mcp_reservations ORDER BY entity_id",
        )?;
        let rows = statement.query_map([], |row| Ok(PersistedEntity {
            id: row.get(0)?,
            revision: row.get(1)?,
            payload: row.get(2)?,
        }))?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        validate_entities("harness_mcp_reservations", &rows)?;
        Ok(rows)
    }

    pub fn load_continuations(&self) -> Result<Vec<PersistedEntity>, HarnessStoreError> {
        let mut statement = self.connection.prepare(
            "SELECT entity_id, revision, payload
             FROM harness_continuations ORDER BY entity_id",
        )?;
        let rows = statement.query_map([], |row| Ok(PersistedEntity {
            id: row.get(0)?,
            revision: row.get(1)?,
            payload: row.get(2)?,
        }))?;
        let rows = rows.collect::<Result<Vec<_>, _>>()?;
        validate_entities("continuations", &rows)?;
        Ok(rows)
    }

    pub fn commit(
        &mut self,
        state: &PersistedHarnessState,
        tail: &PersistedOperation,
    ) -> Result<(), HarnessStoreError> {
        state.validate()?;
        tail.validate()?;
        let transaction = self.connection.transaction()?;
        let write_result = (|| -> Result<(), rusqlite::Error> {
            transaction.execute(
                "INSERT INTO harness_checkpoint(singleton, checkpoint_version, payload)
                 VALUES (1, 1, ?1)
                 ON CONFLICT(singleton) DO UPDATE SET
                    checkpoint_version = excluded.checkpoint_version,
                    payload = excluded.payload",
                params![&state.checkpoint],
            )?;
            replace_entities(&transaction, "harness_tasks", &state.tasks)?;
            replace_entities(&transaction, "harness_runs", &state.runs)?;
            replace_entities(&transaction, "harness_grants", &state.grants)?;
            replace_entities(&transaction, "harness_deliveries", &state.deliveries)?;
            replace_entities(&transaction, "harness_continuations", &state.continuations)?;
            replace_entities(&transaction, "harness_dispatch_contexts", &state.dispatches)?;
            replace_entities(
                &transaction,
                "harness_mcp_reservations",
                &state.harness_mcp_reservations,
            )?;
            transaction.execute("DELETE FROM harness_operations", [])?;
            for operation in &state.operations {
                transaction.execute(
                    "INSERT INTO harness_operations(
                        operation_id, revision, request_digest, state, payload
                     ) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        &operation.id,
                        operation.revision,
                        &operation.request_digest,
                        &operation.state,
                        &operation.payload,
                    ],
                )?;
            }
            transaction.execute(
                "INSERT INTO harness_operation_tail(
                    operation_id, revision, request_digest, state, payload
                 ) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    &tail.id,
                    tail.revision,
                    &tail.request_digest,
                    &tail.state,
                    &tail.payload,
                ],
            )?;
            transaction.execute(
                "DELETE FROM harness_operation_tail
                 WHERE position NOT IN (
                    SELECT position FROM harness_operation_tail
                    ORDER BY position DESC LIMIT ?1
                 )",
                params![HARNESS_OPERATION_TAIL_MAX],
            )?;
            Ok(())
        })();
        if let Err(error) = write_result {
            return Err(HarnessStoreError::Sqlite(error));
        }
        transaction.commit().map_err(HarnessStoreError::CommitAmbiguous)
    }

    pub fn flush(&mut self) -> Result<(), HarnessStoreError> {
        let (busy, _, _): (i64, i64, i64) = self.connection.query_row(
            "PRAGMA wal_checkpoint(FULL)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if busy != 0 {
            return Err(HarnessStoreError::WalCheckpointBusy);
        }
        Ok(())
    }

    pub fn close(mut self) -> Result<(), HarnessStoreError> {
        self.flush()?;
        self.connection.close().map_err(|(_, error)| error)?;
        Ok(())
    }
}

fn replace_entities(
    transaction: &rusqlite::Transaction<'_>,
    table: &'static str,
    rows: &[PersistedEntity],
) -> Result<(), rusqlite::Error> {
    let delete = format!("DELETE FROM {table}");
    transaction.execute(&delete, [])?;
    let insert = format!(
        "INSERT INTO {table}(entity_id, revision, payload) VALUES (?1, ?2, ?3)"
    );
    for row in rows {
        transaction.execute(&insert, params![&row.id, row.revision, &row.payload])?;
    }
    Ok(())
}

impl PersistedHarnessState {
    fn validate(&self) -> Result<(), HarnessStoreError> {
        ensure_bound("checkpoint", self.checkpoint.len(), MAX_CHECKPOINT_BYTES)?;
        validate_entities("tasks", &self.tasks)?;
        validate_entities("runs", &self.runs)?;
        validate_entities("grants", &self.grants)?;
        validate_entities("deliveries", &self.deliveries)?;
        validate_entities("continuations", &self.continuations)?;
        validate_entities("dispatches", &self.dispatches)?;
        validate_entities("harness_mcp_reservations", &self.harness_mcp_reservations)?;
        validate_operations(&self.operations)?;
        Ok(())
    }
}

fn validate_entities(
    kind: &'static str,
    rows: &[PersistedEntity],
) -> Result<(), HarnessStoreError> {
    if rows.len() > HARNESS_ENTITY_ROWS_MAX {
        return Err(HarnessStoreError::RowsTooMany {
            kind,
            actual: rows.len(),
            max: HARNESS_ENTITY_ROWS_MAX,
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        row.validate()?;
        if !ids.insert(&row.id) {
            return Err(HarnessStoreError::DuplicateId { kind });
        }
    }
    Ok(())
}

fn validate_operations(rows: &[PersistedOperation]) -> Result<(), HarnessStoreError> {
    if rows.len() > HARNESS_ENTITY_ROWS_MAX {
        return Err(HarnessStoreError::RowsTooMany {
            kind: "operations",
            actual: rows.len(),
            max: HARNESS_ENTITY_ROWS_MAX,
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        row.validate()?;
        if !ids.insert(&row.id) {
            return Err(HarnessStoreError::DuplicateId { kind: "operations" });
        }
    }
    Ok(())
}

impl PersistedEntity {
    fn validate(&self) -> Result<(), HarnessStoreError> {
        if self.id.is_empty() {
            return Err(HarnessStoreError::Corrupt("empty persisted entity id"));
        }
        ensure_bound("entity", self.payload.len(), MAX_ENTITY_BYTES)
    }
}

impl PersistedOperation {
    fn validate(&self) -> Result<(), HarnessStoreError> {
        if self.id.is_empty() || self.request_digest.is_empty() || self.state.is_empty() {
            return Err(HarnessStoreError::Corrupt("empty persisted operation identity"));
        }
        ensure_bound("operation", self.payload.len(), MAX_OPERATION_BYTES)
    }
}

fn ensure_bound(
    kind: &'static str,
    actual: usize,
    max: usize,
) -> Result<(), HarnessStoreError> {
    if actual > max {
        Err(HarnessStoreError::PayloadTooLarge { kind, actual, max })
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum HarnessStoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("SQLite commit result is ambiguous: {0}")]
    CommitAmbiguous(rusqlite::Error),
    #[error("unsupported harness store schema version {0}")]
    UnsupportedSchema(i64),
    #[error("unsupported harness checkpoint version {0}")]
    UnsupportedCheckpoint(i64),
    #[error("persisted {kind} payload is {actual} bytes; maximum is {max}")]
    PayloadTooLarge {
        kind: &'static str,
        actual: usize,
        max: usize,
    },
    #[error("harness WAL checkpoint could not complete because the database is busy")]
    WalCheckpointBusy,
    #[error("corrupt harness store: {0}")]
    Corrupt(&'static str),
    #[error("persisted {kind} has {actual} rows; maximum is {max}")]
    RowsTooMany {
        kind: &'static str,
        actual: usize,
        max: usize,
    },
    #[error("persisted {kind} contains duplicate ids")]
    DuplicateId { kind: &'static str },
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::{SystemTime, UNIX_EPOCH}};

    #[test]
    fn harness_operation_tail_retention_is_bounded() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-harness-tail-{}-{nonce}.sqlite",
            std::process::id(),
        ));
        let mut store = HarnessStore::open(&path).unwrap();
        let state = PersistedHarnessState {
            checkpoint: vec![1],
            ..PersistedHarnessState::default()
        };
        for position in 0..=HARNESS_OPERATION_TAIL_MAX {
            let tail = PersistedOperation {
                id: format!("hop_{position:024x}"),
                revision: 1,
                request_digest: format!("{position:064x}"),
                state: "Succeeded".to_owned(),
                payload: vec![1],
            };
            store.commit(&state, &tail).unwrap();
        }
        store.close().unwrap();
        let connection = Connection::open(&path).unwrap();
        let count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM harness_operation_tail",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, HARNESS_OPERATION_TAIL_MAX as i64);
        connection.close().unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }
}

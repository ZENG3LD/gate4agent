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

/// A write-only introspection mirror of one `HarnessMailMessageV1` --
/// durability comes from `checkpoint` (the engine's own `mail_messages`
/// field), exactly like `harness_tasks`/`harness_runs`/etc already work for
/// their entities; this row exists so an operator can query the mailbox by
/// recipient or task without decoding the checkpoint blob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedMailMessage {
    pub message_id: String,
    pub to_record_id: Option<String>,
    pub to_task_id: Option<String>,
    pub created_at_unix_ms: i64,
    pub payload: Vec<u8>,
}

/// A write-only introspection mirror of one `HarnessMailAckV1`, keyed the
/// same way the engine dedups acks: `(message_id, reader_record_id)`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PersistedMailAck {
    pub message_id: String,
    pub reader_record_id: String,
    pub acked_at_unix_ms: i64,
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
    pub mail_messages: Vec<PersistedMailMessage>,
    pub mail_acks: Vec<PersistedMailAck>,
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
             -- `harness_operations` used to mirror the engine's whole
             -- operation history, one row per operation, rewritten wholesale
             -- (DELETE + re-INSERT every row) on every single commit. Nothing
             -- in this crate or any consumer ever SELECTed from it -- the
             -- authoritative history lives in the `harness_checkpoint`
             -- payload blob (`HarnessEngineCheckpointV1::operations`, wholly
             -- serialized into `checkpoint.payload`), and bounded
             -- introspection is already served by `harness_operation_tail`.
             -- Dropping it here is safe on both a fresh database (the table
             -- never existed) and an existing one (its rows duplicate the
             -- checkpoint and were never read).
             DROP TABLE IF EXISTS harness_operations;
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
             CREATE TABLE IF NOT EXISTS harness_mail_messages (
                message_id TEXT PRIMARY KEY,
                to_record_id TEXT,
                to_task_id TEXT,
                created_at_unix_ms INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE INDEX IF NOT EXISTS harness_mail_messages_to_record
                ON harness_mail_messages(to_record_id, created_at_unix_ms);
             CREATE INDEX IF NOT EXISTS harness_mail_messages_to_task
                ON harness_mail_messages(to_task_id, created_at_unix_ms);
             CREATE TABLE IF NOT EXISTS harness_mail_acks (
                message_id TEXT NOT NULL,
                reader_record_id TEXT NOT NULL,
                acked_at_unix_ms INTEGER NOT NULL,
                payload BLOB NOT NULL,
                PRIMARY KEY (message_id, reader_record_id)
             );
             CREATE INDEX IF NOT EXISTS harness_mail_acks_message_reader
                ON harness_mail_acks(message_id, reader_record_id);
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
            replace_mail_messages(&transaction, &state.mail_messages)?;
            replace_mail_acks(&transaction, &state.mail_acks)?;
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

fn replace_mail_messages(
    transaction: &rusqlite::Transaction<'_>,
    rows: &[PersistedMailMessage],
) -> Result<(), rusqlite::Error> {
    transaction.execute("DELETE FROM harness_mail_messages", [])?;
    for row in rows {
        transaction.execute(
            "INSERT INTO harness_mail_messages(
                message_id, to_record_id, to_task_id, created_at_unix_ms, payload
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &row.message_id,
                &row.to_record_id,
                &row.to_task_id,
                row.created_at_unix_ms,
                &row.payload,
            ],
        )?;
    }
    Ok(())
}

fn replace_mail_acks(
    transaction: &rusqlite::Transaction<'_>,
    rows: &[PersistedMailAck],
) -> Result<(), rusqlite::Error> {
    transaction.execute("DELETE FROM harness_mail_acks", [])?;
    for row in rows {
        transaction.execute(
            "INSERT INTO harness_mail_acks(
                message_id, reader_record_id, acked_at_unix_ms, payload
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                &row.message_id,
                &row.reader_record_id,
                row.acked_at_unix_ms,
                &row.payload,
            ],
        )?;
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
        validate_mail_messages(&self.mail_messages)?;
        validate_mail_acks(&self.mail_acks)?;
        Ok(())
    }
}

fn validate_mail_messages(rows: &[PersistedMailMessage]) -> Result<(), HarnessStoreError> {
    if rows.len() > HARNESS_ENTITY_ROWS_MAX {
        return Err(HarnessStoreError::RowsTooMany {
            kind: "mail_messages",
            actual: rows.len(),
            max: HARNESS_ENTITY_ROWS_MAX,
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        if row.message_id.is_empty() {
            return Err(HarnessStoreError::Corrupt("empty persisted mail message id"));
        }
        ensure_bound("mail message", row.payload.len(), MAX_ENTITY_BYTES)?;
        if !ids.insert(&row.message_id) {
            return Err(HarnessStoreError::DuplicateId { kind: "mail_messages" });
        }
    }
    Ok(())
}

fn validate_mail_acks(rows: &[PersistedMailAck]) -> Result<(), HarnessStoreError> {
    if rows.len() > HARNESS_ENTITY_ROWS_MAX {
        return Err(HarnessStoreError::RowsTooMany {
            kind: "mail_acks",
            actual: rows.len(),
            max: HARNESS_ENTITY_ROWS_MAX,
        });
    }
    let mut ids = std::collections::BTreeSet::new();
    for row in rows {
        if row.message_id.is_empty() || row.reader_record_id.is_empty() {
            return Err(HarnessStoreError::Corrupt("empty persisted mail ack identity"));
        }
        ensure_bound("mail ack", row.payload.len(), MAX_ENTITY_BYTES)?;
        if !ids.insert((&row.message_id, &row.reader_record_id)) {
            return Err(HarnessStoreError::DuplicateId { kind: "mail_acks" });
        }
    }
    Ok(())
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

    #[test]
    fn harness_mail_tables_round_trip_and_are_queryable_by_recipient() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-harness-mail-{}-{nonce}.sqlite",
            std::process::id(),
        ));
        let mut store = HarnessStore::open(&path).unwrap();
        let state = PersistedHarnessState {
            checkpoint: vec![1],
            mail_messages: vec![
                PersistedMailMessage {
                    message_id: "hmail_1".to_owned(),
                    to_record_id: Some("record-recipient".to_owned()),
                    to_task_id: None,
                    created_at_unix_ms: 20,
                    payload: vec![1, 2, 3],
                },
                PersistedMailMessage {
                    message_id: "hmail_2".to_owned(),
                    to_record_id: None,
                    to_task_id: Some("htask_1".to_owned()),
                    created_at_unix_ms: 21,
                    payload: vec![4, 5, 6],
                },
            ],
            mail_acks: vec![PersistedMailAck {
                message_id: "hmail_1".to_owned(),
                reader_record_id: "record-recipient".to_owned(),
                acked_at_unix_ms: 25,
                payload: vec![7],
            }],
            ..PersistedHarnessState::default()
        };
        let tail = PersistedOperation {
            id: format!("hop_{}", "a".repeat(24)),
            revision: 1,
            request_digest: "b".repeat(64),
            state: "Succeeded".to_owned(),
            payload: vec![1],
        };
        store.commit(&state, &tail).unwrap();
        store.close().unwrap();

        let connection = Connection::open(&path).unwrap();
        let by_record: String = connection.query_row(
            "SELECT message_id FROM harness_mail_messages WHERE to_record_id = ?1",
            params!["record-recipient"],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(by_record, "hmail_1");
        let by_task: String = connection.query_row(
            "SELECT message_id FROM harness_mail_messages WHERE to_task_id = ?1",
            params!["htask_1"],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(by_task, "hmail_2");
        let ack_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM harness_mail_acks WHERE message_id = ?1 AND reader_record_id = ?2",
            params!["hmail_1", "record-recipient"],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(ack_count, 1);
        connection.close().unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn existing_harness_operations_table_is_dropped_on_open_and_store_still_commits() {
        // Simulates a database written by a binary that still had the
        // write-only `harness_operations` mirror: create it by hand with
        // rows, exactly as an old `commit` would have left it, then open it
        // through `HarnessStore::open` and confirm the table is gone and the
        // store still commits afterward.
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-harness-drop-operations-{}-{nonce}.sqlite",
            std::process::id(),
        ));
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute_batch(
                "CREATE TABLE harness_operations (
                    operation_id TEXT PRIMARY KEY,
                    revision INTEGER NOT NULL,
                    request_digest TEXT NOT NULL,
                    state TEXT NOT NULL,
                    payload BLOB NOT NULL
                 );",
            ).unwrap();
            for position in 0..32 {
                connection.execute(
                    "INSERT INTO harness_operations(
                        operation_id, revision, request_digest, state, payload
                     ) VALUES (?1, 1, ?2, 'Succeeded', X'01')",
                    params![format!("hop_{position:024x}"), format!("{position:064x}")],
                ).unwrap();
            }
            connection.close().unwrap();
        }

        let mut store = HarnessStore::open(&path).unwrap();
        let state = PersistedHarnessState {
            checkpoint: vec![1],
            ..PersistedHarnessState::default()
        };
        let tail = PersistedOperation {
            id: format!("hop_{}", "c".repeat(24)),
            revision: 1,
            request_digest: "d".repeat(64),
            state: "Succeeded".to_owned(),
            payload: vec![1],
        };
        store.commit(&state, &tail).unwrap();
        store.close().unwrap();

        let connection = Connection::open(&path).unwrap();
        let table_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'harness_operations'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(table_count, 0);
        connection.close().unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn commit_does_not_create_or_write_harness_operations_table() {
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-harness-no-operations-{}-{nonce}.sqlite",
            std::process::id(),
        ));
        let mut store = HarnessStore::open(&path).unwrap();
        let state = PersistedHarnessState {
            checkpoint: vec![1],
            ..PersistedHarnessState::default()
        };
        let tail = PersistedOperation {
            id: format!("hop_{}", "e".repeat(24)),
            revision: 1,
            request_digest: "f".repeat(64),
            state: "Succeeded".to_owned(),
            payload: vec![1],
        };
        store.commit(&state, &tail).unwrap();
        store.close().unwrap();

        let connection = Connection::open(&path).unwrap();
        let table_count: i64 = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'harness_operations'",
            [],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(table_count, 0);
        connection.close().unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }

    #[test]
    fn operation_history_far_exceeding_the_old_row_cap_now_commits() {
        // Before this change, operation history was ALSO mirrored row-per-
        // operation into `PersistedHarnessState::operations`, and
        // `validate_operations` refused the commit once that count passed
        // `HARNESS_ENTITY_ROWS_MAX`. Operation history now lives only inside
        // the `checkpoint` blob, which is bounded by `MAX_CHECKPOINT_BYTES`
        // (bytes), not by a row count -- so a checkpoint sized as if it held
        // far more than `HARNESS_ENTITY_ROWS_MAX` operations must still
        // commit, where the old row-count gate would have rejected it.
        let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!(
            "gate4agent-harness-operations-uncapped-{}-{nonce}.sqlite",
            std::process::id(),
        ));
        let mut store = HarnessStore::open(&path).unwrap();
        let synthetic_operation_count = HARNESS_ENTITY_ROWS_MAX + 1;
        let checkpoint = vec![7u8; synthetic_operation_count * 64];
        assert!(checkpoint.len() < MAX_CHECKPOINT_BYTES);
        let state = PersistedHarnessState {
            checkpoint,
            ..PersistedHarnessState::default()
        };
        let tail = PersistedOperation {
            id: format!("hop_{}", "1".repeat(24)),
            revision: 1,
            request_digest: "2".repeat(64),
            state: "Succeeded".to_owned(),
            payload: vec![1],
        };
        store.commit(&state, &tail).unwrap();
        store.close().unwrap();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(format!("{}-wal", path.display()));
        let _ = fs::remove_file(format!("{}-shm", path.display()));
    }
}

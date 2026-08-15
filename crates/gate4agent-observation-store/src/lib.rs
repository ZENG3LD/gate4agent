//! SQLite durability for validated, monitoring-only observation operations.

use gate4agent_observation_api::{
    ObservationIngressEnvelope, ObservationRecordInventory, ObservationResyncBatch,
};
use gate4agent_observation_engine::ObservationEngineCheckpointV1;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use thiserror::Error;

pub const OBSERVATION_STORE_SCHEMA_VERSION: i64 = 1;
pub const DEFAULT_TAIL_OPERATION_LIMIT: usize = 256;
pub const DEFAULT_TAIL_BYTES_LIMIT: usize = 8 * 1024 * 1024;
pub const MAX_OPERATION_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_CHECKPOINT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ObservationStoreLimits {
    pub tail_operations: usize,
    pub tail_bytes: usize,
}

impl Default for ObservationStoreLimits {
    fn default() -> Self {
        Self {
            tail_operations: DEFAULT_TAIL_OPERATION_LIMIT,
            tail_bytes: DEFAULT_TAIL_BYTES_LIMIT,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "operation", rename_all = "kebab-case", deny_unknown_fields)]
pub enum StoredObservationOperation {
    Ingress { envelope: ObservationIngressEnvelope },
    Resync { batch: ObservationResyncBatch },
    RecordInventory { inventory: ObservationRecordInventory },
}

impl StoredObservationOperation {
    pub fn validate(&self) -> Result<(), ObservationStoreError> {
        match self {
            Self::Ingress { envelope } => envelope.validate()?,
            Self::Resync { batch } => batch.validate()?,
            Self::RecordInventory { inventory } => inventory.validate()?,
        }
        Ok(())
    }

    pub fn encode(&self) -> Result<Vec<u8>, ObservationStoreError> {
        self.validate()?;
        let encoded = serde_json::to_vec(self)?;
        if encoded.len() > MAX_OPERATION_BYTES {
            return Err(ObservationStoreError::OperationTooLarge {
                actual: encoded.len(),
                max: MAX_OPERATION_BYTES,
            });
        }
        Ok(encoded)
    }
}

pub struct ObservationStore {
    connection: Connection,
    limits: ObservationStoreLimits,
    tail_operations: usize,
    tail_bytes: usize,
}

impl ObservationStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ObservationStoreError> {
        Self::open_with_limits(path, ObservationStoreLimits::default())
    }

    pub fn open_with_limits(
        path: impl AsRef<Path>,
        limits: ObservationStoreLimits,
    ) -> Result<Self, ObservationStoreError> {
        if limits.tail_operations == 0 || limits.tail_bytes == 0 {
            return Err(ObservationStoreError::InvalidLimits);
        }
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != 0 && version != OBSERVATION_STORE_SCHEMA_VERSION {
            return Err(ObservationStoreError::UnsupportedSchema(version));
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS observation_checkpoint (
                singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
                checkpoint_version INTEGER NOT NULL,
                payload BLOB NOT NULL
             );
             CREATE TABLE IF NOT EXISTS observation_tail (
                position INTEGER PRIMARY KEY AUTOINCREMENT,
                payload BLOB NOT NULL
             );",
        )?;
        if version == 0 {
            connection.pragma_update(None, "user_version", OBSERVATION_STORE_SCHEMA_VERSION)?;
        }
        let (tail_operations, tail_bytes) = connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length(payload)), 0) FROM observation_tail",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let tail_operations = usize::try_from(tail_operations)
            .map_err(|_| ObservationStoreError::Corrupt("negative tail count"))?;
        let tail_bytes = usize::try_from(tail_bytes)
            .map_err(|_| ObservationStoreError::Corrupt("negative tail byte count"))?;
        if tail_operations > limits.tail_operations || tail_bytes > limits.tail_bytes {
            return Err(ObservationStoreError::Corrupt("durable tail exceeds configured bounds"));
        }
        Ok(Self { connection, limits, tail_operations, tail_bytes })
    }

    pub fn load(
        &self,
    ) -> Result<(Option<ObservationEngineCheckpointV1>, Vec<StoredObservationOperation>), ObservationStoreError> {
        let checkpoint = self.connection.query_row(
            "SELECT checkpoint_version, payload FROM observation_checkpoint WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
        ).optional()?;
        let checkpoint = checkpoint.map(|(stored_version, encoded)| {
            if encoded.len() > MAX_CHECKPOINT_BYTES {
                return Err(ObservationStoreError::CheckpointTooLarge {
                    actual: encoded.len(),
                    max: MAX_CHECKPOINT_BYTES,
                });
            }
            let decoded: ObservationEngineCheckpointV1 = serde_json::from_slice(&encoded)?;
            if stored_version != i64::from(decoded.version) {
                return Err(ObservationStoreError::Corrupt(
                    "stored checkpoint version does not match decoded checkpoint",
                ));
            }
            Ok(decoded)
        }).transpose()?;

        let mut statement = self.connection.prepare(
            "SELECT payload FROM observation_tail ORDER BY position ASC",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
        let mut operations = Vec::with_capacity(self.tail_operations);
        for row in rows {
            let encoded = row?;
            if encoded.len() > MAX_OPERATION_BYTES {
                return Err(ObservationStoreError::OperationTooLarge {
                    actual: encoded.len(),
                    max: MAX_OPERATION_BYTES,
                });
            }
            let operation: StoredObservationOperation = serde_json::from_slice(&encoded)?;
            operation.validate()?;
            operations.push(operation);
        }
        Ok((checkpoint, operations))
    }

    pub fn should_checkpoint_after(&self, encoded_operation_bytes: usize) -> bool {
        self.tail_operations.saturating_add(1) >= self.limits.tail_operations
            || self.tail_bytes.saturating_add(encoded_operation_bytes) >= self.limits.tail_bytes
    }

    pub fn commit_operation(
        &mut self,
        operation: &StoredObservationOperation,
        checkpoint: Option<&ObservationEngineCheckpointV1>,
    ) -> Result<(), ObservationStoreError> {
        let encoded_operation = operation.encode()?;
        let encoded_checkpoint = checkpoint.map(|checkpoint| {
            checkpoint.validate().map_err(ObservationStoreError::Engine)?;
            let encoded = serde_json::to_vec(checkpoint)?;
            if encoded.len() > MAX_CHECKPOINT_BYTES {
                return Err(ObservationStoreError::CheckpointTooLarge {
                    actual: encoded.len(),
                    max: MAX_CHECKPOINT_BYTES,
                });
            }
            Ok(encoded)
        }).transpose()?;

        let transaction = self.connection.transaction()?;
        transaction.execute(
            "INSERT INTO observation_tail(payload) VALUES (?1)",
            params![&encoded_operation],
        )?;
        if let Some(encoded_checkpoint) = encoded_checkpoint.as_deref() {
            transaction.execute(
                "INSERT INTO observation_checkpoint(singleton, checkpoint_version, payload)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT(singleton) DO UPDATE SET
                    checkpoint_version = excluded.checkpoint_version,
                    payload = excluded.payload",
                params![gate4agent_observation_engine::OBSERVATION_ENGINE_CHECKPOINT_VERSION_V1, encoded_checkpoint],
            )?;
            transaction.execute("DELETE FROM observation_tail", [])?;
        }
        transaction.commit()?;
        if checkpoint.is_some() {
            self.tail_operations = 0;
            self.tail_bytes = 0;
        } else {
            self.tail_operations = self.tail_operations.saturating_add(1);
            self.tail_bytes = self.tail_bytes.saturating_add(encoded_operation.len());
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<(), ObservationStoreError> {
        let (busy, _, _): (i64, i64, i64) = self.connection.query_row(
            "PRAGMA wal_checkpoint(FULL)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        if busy != 0 {
            return Err(ObservationStoreError::WalCheckpointBusy);
        }
        Ok(())
    }

    pub fn close(mut self) -> Result<(), ObservationStoreError> {
        self.flush()?;
        self.connection.close().map_err(|(_, error)| error)?;
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ObservationStoreError {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Api(#[from] gate4agent_observation_api::ObservationApiError),
    #[error(transparent)]
    Engine(#[from] gate4agent_observation_engine::ObservationEngineError),
    #[error("unsupported observation store schema version {0}")]
    UnsupportedSchema(i64),
    #[error("observation store limits must be non-zero")]
    InvalidLimits,
    #[error("stored observation operation is {actual} bytes; maximum is {max}")]
    OperationTooLarge { actual: usize, max: usize },
    #[error("observation checkpoint is {actual} bytes; maximum is {max}")]
    CheckpointTooLarge { actual: usize, max: usize },
    #[error("observation WAL checkpoint could not complete because the database is busy")]
    WalCheckpointBusy,
    #[error("corrupt observation store: {0}")]
    Corrupt(&'static str),
}

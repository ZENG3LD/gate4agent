//! Durable, content-addressed store for exported `NodeContextPack`s.
//!
//! A simplified sibling of [`super::bundle_delivery::DeliveryStore`]: a pack
//! is produced whole, in-process, and already bounded at
//! `MAX_CONTEXT_PACK_BYTES`, so there is no stage/blob-chunk-upload
//! machinery here — just a flat commit-only content-addressed store, one
//! JSON file per pack, keyed by the pack's own digest.

use crate::context_pack::{NodeContextPack, MAX_CONTEXT_PACK_CATALOG_ENTRIES};
use crate::protocol::{ResolvedContextPackReceipt, SpawnContextDigest, SpawnContextId, MAX_CONTEXT_PACK_BYTES};
use crate::session_environment::{
    ensure_materialization_root, secure_create_directory, secure_create_file,
    validate_secure_directory, validate_secure_file, verify_or_create_exact_file,
    MaterializationRootLock,
};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

use super::bundle_delivery::publish_temporary_create_new;

const ROOT_MARKER_NAME: &str = ".gate4agent-context-pack-store-root";
const ROOT_MARKER: &[u8] = b"gate4agent-node-context-pack-store-v1\n";
const ROOT_LOCK_NAME: &str = ".gate4agent-context-pack-store-lock";
const PACKS_DIRECTORY: &str = "packs";
const CONTEXT_PACK_STORE_SCHEMA: u16 = 1;
// Headroom over the raw pack byte cap: the persisted record re-embeds the
// pack's own JSON text as an escaped JSON string (worst case close to 2x for
// quote/backslash-heavy content) plus the small receipt and wrapper fields.
const MAX_PERSISTED_CONTEXT_PACK_BYTES: u64 = (MAX_CONTEXT_PACK_BYTES as u64) * 2 + 4_096;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedContextPackV1 {
    schema_version: u16,
    receipt: ResolvedContextPackReceipt,
    document: String,
    committed_at_unix_ms: u64,
}

pub(crate) struct ContextPackStore {
    root: PathBuf,
    packs: BTreeMap<SpawnContextId, ()>,
    _lock: MaterializationRootLock,
}

#[derive(Debug, Error)]
pub(crate) enum ContextPackStoreError {
    #[error("context pack store is unavailable")]
    Unavailable,
    #[error("context pack store capacity is exhausted")]
    Capacity,
    #[error("context pack conflicts with previously committed content")]
    Conflict,
    #[error("context pack store contents failed validation")]
    Corrupt,
    #[error("context pack store operation failed")]
    Storage(#[source] io::Error),
}

impl ContextPackStore {
    pub(crate) fn open(root: PathBuf) -> Result<(Self, Vec<NodeContextPack>), ContextPackStoreError> {
        if !root.is_absolute() {
            return Err(ContextPackStoreError::Unavailable);
        }
        ensure_materialization_root(&root).map_err(ContextPackStoreError::Storage)?;
        let lock = MaterializationRootLock::acquire(&root.join(ROOT_LOCK_NAME))
            .map_err(ContextPackStoreError::Storage)?;
        verify_or_create_exact_file(&root.join(ROOT_MARKER_NAME), ROOT_MARKER)
            .map_err(ContextPackStoreError::Storage)?;
        let packs_directory = root.join(PACKS_DIRECTORY);
        match secure_create_directory(&packs_directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                validate_secure_directory(&packs_directory)
                    .map_err(ContextPackStoreError::Storage)?;
            }
            Err(error) => return Err(ContextPackStoreError::Storage(error)),
        }

        let mut store = Self {
            root,
            packs: BTreeMap::new(),
            _lock: lock,
        };
        let packs = store.reload_packs()?;
        Ok((store, packs))
    }

    pub(crate) fn commit(&mut self, pack: &NodeContextPack) -> Result<(), ContextPackStoreError> {
        let receipt = pack.receipt();
        let path = self.pack_path(&receipt.digest);
        if path.exists() {
            let existing = self.read_pack_file(&path)?;
            if existing.schema_version != CONTEXT_PACK_STORE_SCHEMA
                || &existing.receipt != receipt
                || existing.document.as_bytes() != pack.bytes()
            {
                return Err(ContextPackStoreError::Conflict);
            }
            self.packs.insert(receipt.id.clone(), ());
            return Ok(());
        }
        if !self.packs.contains_key(&receipt.id)
            && self.packs.len() >= MAX_CONTEXT_PACK_CATALOG_ENTRIES
        {
            return Err(ContextPackStoreError::Capacity);
        }
        let document = String::from_utf8(pack.bytes().to_vec())
            .map_err(|_| ContextPackStoreError::Corrupt)?;
        let record = PersistedContextPackV1 {
            schema_version: CONTEXT_PACK_STORE_SCHEMA,
            receipt: receipt.clone(),
            document,
            committed_at_unix_ms: unix_time_ms(),
        };
        let bytes =
            serde_json::to_vec(&record).map_err(|_| ContextPackStoreError::Corrupt)?;
        let temporary = self
            .packs_directory()
            .join(format!(".tmp-{}", random_hex_16()?));
        secure_create_file(&temporary, &bytes).map_err(ContextPackStoreError::Storage)?;
        match publish_temporary_create_new(&temporary, &path) {
            Ok(()) => {}
            Err(_error) if path.exists() => {}
            Err(error) => return Err(ContextPackStoreError::Storage(error)),
        }
        let persisted = self.read_pack_file(&path)?;
        if persisted.schema_version != CONTEXT_PACK_STORE_SCHEMA
            || &persisted.receipt != receipt
            || persisted.document.as_bytes() != pack.bytes()
        {
            return Err(ContextPackStoreError::Conflict);
        }
        self.packs.insert(receipt.id.clone(), ());
        Ok(())
    }

    fn reload_packs(&mut self) -> Result<Vec<NodeContextPack>, ContextPackStoreError> {
        let directory = self.packs_directory();
        validate_secure_directory(&directory).map_err(ContextPackStoreError::Storage)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&directory).map_err(ContextPackStoreError::Storage)? {
            if entries.len() == MAX_CONTEXT_PACK_CATALOG_ENTRIES {
                return Err(ContextPackStoreError::Corrupt);
            }
            entries.push(entry.map_err(ContextPackStoreError::Storage)?);
        }
        entries.sort_by_key(|entry| entry.file_name());
        let mut packs = Vec::with_capacity(entries.len());
        for entry in entries {
            let path = entry.path();
            let file_name = entry.file_name();
            if is_owned_temporary_name(&file_name) {
                validate_secure_file(&path).map_err(ContextPackStoreError::Storage)?;
                fs::remove_file(&path).map_err(ContextPackStoreError::Storage)?;
                continue;
            }
            if !is_canonical_pack_file_name(&file_name) {
                return Err(ContextPackStoreError::Corrupt);
            }
            let record = self.read_pack_file(&path)?;
            if record.schema_version != CONTEXT_PACK_STORE_SCHEMA
                || pack_file_name(&record.receipt.digest) != file_name.to_string_lossy()
            {
                return Err(ContextPackStoreError::Corrupt);
            }
            let pack =
                NodeContextPack::from_materialized(record.receipt, record.document.into_bytes())
                    .map_err(|_| ContextPackStoreError::Corrupt)?;
            if self.packs.insert(pack.receipt().id.clone(), ()).is_some() {
                return Err(ContextPackStoreError::Corrupt);
            }
            packs.push(pack);
        }
        Ok(packs)
    }

    fn read_pack_file(&self, path: &Path) -> Result<PersistedContextPackV1, ContextPackStoreError> {
        validate_secure_file(path).map_err(ContextPackStoreError::Storage)?;
        read_bounded_persisted_pack(path)
    }

    fn packs_directory(&self) -> PathBuf {
        self.root.join(PACKS_DIRECTORY)
    }

    fn pack_path(&self, digest: &SpawnContextDigest) -> PathBuf {
        self.packs_directory().join(pack_file_name(digest))
    }
}

fn pack_file_name(digest: &SpawnContextDigest) -> String {
    format!("{}.json", digest_hex(digest))
}

fn digest_hex(digest: &SpawnContextDigest) -> &str {
    digest
        .as_str()
        .strip_prefix("sha256:")
        .expect("spawn context digest is validated")
}

fn is_owned_temporary_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(".tmp-"))
        .map_or(false, |nonce| is_lowercase_hex(nonce, 32))
}

fn is_canonical_pack_file_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_suffix(".json"))
        .map_or(false, |digest| is_lowercase_hex(digest, 64))
}

fn is_lowercase_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn random_hex_16() -> Result<String, ContextPackStoreError> {
    let mut nonce = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| ContextPackStoreError::Unavailable)?;
    let mut value = String::with_capacity(32);
    for byte in nonce {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(value)
}

fn read_bounded_persisted_pack(path: &Path) -> Result<PersistedContextPackV1, ContextPackStoreError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(ContextPackStoreError::Storage)?
        .take(MAX_PERSISTED_CONTEXT_PACK_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(ContextPackStoreError::Storage)?;
    if bytes.len() as u64 > MAX_PERSISTED_CONTEXT_PACK_BYTES {
        return Err(ContextPackStoreError::Corrupt);
    }
    serde_json::from_slice(&bytes).map_err(|_| ContextPackStoreError::Corrupt)
}

fn unix_time_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{ContextPackLineageReceipt, NodeId, SessionAddress, SessionKey, WorkspaceId};
    use crate::session_environment::secure_replace_file;
    use gate4agent_types::{
        AgentId, AgentInstanceId, HistoryMessageRecord, HistoryMessageRole, HistorySessionRecord,
        SessionGeneration,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "gate4agent-context-pack-store-{label}-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed),
            )))
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            if self.0.exists() {
                fs::remove_dir_all(&self.0).unwrap();
            }
        }
    }

    fn lineage(instance_id: u64) -> ContextPackLineageReceipt {
        ContextPackLineageReceipt {
            source_node_id: NodeId::new("node-local").unwrap(),
            source_session: SessionAddress {
                workspace_id: WorkspaceId::new("source").unwrap(),
                session: SessionKey {
                    instance_id: AgentInstanceId(instance_id),
                    generation: SessionGeneration(1),
                },
            },
            source_provider: AgentId::new("qwen-code").unwrap(),
        }
    }

    fn history(text: &str) -> HistorySessionRecord {
        HistorySessionRecord {
            session_id: "vendor-session".to_owned(),
            title: None,
            cwd: None,
            model: None,
            message_count: 1,
            completed_turn_count: None,
            total_tokens: 0,
            messages: vec![HistoryMessageRecord {
                role: HistoryMessageRole::User,
                text: text.to_owned(),
            }],
        }
    }

    #[test]
    fn context_pack_store_commit_survives_reopen_and_reseeds_the_catalog() {
        let root = TestRoot::new("reopen");
        let pack = NodeContextPack::export(lineage(1), &history("hello")).unwrap();
        {
            let (mut store, seeded) = ContextPackStore::open(root.0.clone()).unwrap();
            assert!(seeded.is_empty());
            store.commit(&pack).unwrap();
        }

        let (_reopened, seeded) = ContextPackStore::open(root.0.clone()).unwrap();
        assert_eq!(seeded, vec![pack]);
    }

    #[test]
    fn context_pack_store_commit_is_idempotent_for_identical_content() {
        let root = TestRoot::new("idempotent");
        let (mut store, _) = ContextPackStore::open(root.0.clone()).unwrap();
        let pack = NodeContextPack::export(lineage(2), &history("hello again")).unwrap();
        store.commit(&pack).unwrap();
        store.commit(&pack).unwrap();
        let entries = fs::read_dir(root.0.join(PACKS_DIRECTORY))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn context_pack_store_corrupt_file_fails_closed_on_reopen() {
        let root = TestRoot::new("corrupt");
        let pack = NodeContextPack::export(lineage(3), &history("corrupt me")).unwrap();
        let pack_path = root.0.join(PACKS_DIRECTORY).join(pack_file_name(&pack.receipt().digest));
        {
            let (mut store, _) = ContextPackStore::open(root.0.clone()).unwrap();
            store.commit(&pack).unwrap();
        }
        secure_replace_file(&pack_path, b"not a persisted context pack").unwrap();
        assert!(matches!(
            ContextPackStore::open(root.0.clone()),
            Err(ContextPackStoreError::Corrupt),
        ));
    }

    #[test]
    fn context_pack_store_capacity_cap_rejects_extra_packs() {
        let root = TestRoot::new("capacity");
        let (mut store, _) = ContextPackStore::open(root.0.clone()).unwrap();
        for index in 0..MAX_CONTEXT_PACK_CATALOG_ENTRIES as u64 {
            let pack = NodeContextPack::export(lineage(index), &history("filler")).unwrap();
            store.commit(&pack).unwrap();
        }
        let overflow = NodeContextPack::export(
            lineage(MAX_CONTEXT_PACK_CATALOG_ENTRIES as u64),
            &history("overflow"),
        )
        .unwrap();
        assert!(matches!(
            store.commit(&overflow),
            Err(ContextPackStoreError::Capacity),
        ));
    }
}

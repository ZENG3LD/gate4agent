use crate::bundle_catalog::{
    digest_delivery_blob, digest_delivery_manifest, NodeBundle, NodeBundleError,
    MAX_BUNDLE_CATALOG_ENTRIES,
};
use crate::protocol::{
    DeliveryBlobDigestV1, DeliveryBlobReceiptV1, DeliveryBundleManifestV2,
    DeliveryCommitReceiptV1, DeliveryManifestDigestV2, DeliveryStageId,
    MAX_DELIVERY_FILES, MAX_DELIVERY_TOTAL_BYTES,
};
use crate::session_environment::{
    ensure_materialization_root, remove_tree_no_links, secure_create_directory,
    secure_create_file, secure_replace_file, validate_secure_directory,
    validate_secure_file, verify_or_create_exact_file, MaterializationRootLock,
};
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

pub(crate) const MAX_ACTIVE_DELIVERY_STAGES: usize = 16;
pub(crate) const MAX_ACTIVE_DELIVERY_BYTES: u64 =
    (MAX_DELIVERY_TOTAL_BYTES as u64) * 4;
pub(crate) const DELIVERY_STAGE_TTL: Duration = Duration::from_secs(15 * 60);

const ROOT_MARKER_NAME: &str = ".gate4agent-delivery-store-root";
const ROOT_MARKER: &[u8] = b"gate4agent-node-delivery-store-v1\n";
const ROOT_LOCK_NAME: &str = ".gate4agent-delivery-store-lock";
const STAGES_DIRECTORY: &str = "stages";
const BLOBS_DIRECTORY: &str = "blobs";
const COMMITS_DIRECTORY: &str = "commits";
const STAGE_MANIFEST_NAME: &str = "manifest.json";
const COMMIT_SCHEMA: u16 = 1;
const MAX_PERSISTED_MANIFEST_BYTES: u64 = 256 * 1024;
const MAX_DELIVERY_CAS_BLOBS: usize = MAX_BUNDLE_CATALOG_ENTRIES * MAX_DELIVERY_FILES;

pub(crate) struct DeliveryStore {
    root: PathBuf,
    stages: BTreeMap<DeliveryStageId, DeliveryStage>,
    completed: BTreeMap<DeliveryStageId, CompletedStage>,
    commits: BTreeMap<DeliveryManifestDigestV2, CommittedDelivery>,
    _lock: MaterializationRootLock,
    #[cfg(test)]
    fail_after_partial_create_once: bool,
    #[cfg(test)]
    fail_blob_parent_sync_once: std::cell::Cell<bool>,
    #[cfg(test)]
    fail_commit_parent_sync_once: std::cell::Cell<bool>,
    #[cfg(test)]
    fail_commit_before_rename_once: std::cell::Cell<bool>,
    #[cfg(test)]
    fail_stage_cleanup_once: std::cell::Cell<bool>,
}

struct DeliveryStage {
    manifest: DeliveryBundleManifestV2,
    expected: BTreeMap<DeliveryBlobDigestV1, DeliveryBlobReceiptV1>,
    received: BTreeMap<DeliveryBlobDigestV1, Vec<u8>>,
    reserved_bytes: u64,
    created_at_unix_ms: u64,
}

#[derive(Clone)]
struct CompletedStage {
    receipt: DeliveryCommitReceiptV1,
}

#[derive(Clone)]
struct CommittedDelivery {
    stage_id: DeliveryStageId,
    manifest: DeliveryBundleManifestV2,
    receipt: DeliveryCommitReceiptV1,
    bundle: NodeBundle,
}

pub(crate) struct PreparedDeliveryCommit {
    stage_id: DeliveryStageId,
    manifest: DeliveryBundleManifestV2,
    receipt: DeliveryCommitReceiptV1,
    bundle: NodeBundle,
}

impl PreparedDeliveryCommit {
    pub(crate) fn bundle(&self) -> &NodeBundle {
        &self.bundle
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeliveryStageBegun {
    pub(crate) stage_id: DeliveryStageId,
    pub(crate) manifest_digest: DeliveryManifestDigestV2,
    pub(crate) missing_blobs: Vec<DeliveryBlobDigestV1>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeliveryChunkAccepted {
    pub(crate) stage_id: DeliveryStageId,
    pub(crate) blob_digest: DeliveryBlobDigestV1,
    pub(crate) next_offset: u64,
}

#[derive(Debug, Error)]
pub(crate) enum DeliveryStoreError {
    #[error("delivery store is unavailable")]
    Unavailable,
    #[error("delivery manifest is invalid")]
    InvalidManifest,
    #[error("delivery staging capacity is exhausted")]
    Capacity,
    #[error("delivery stage does not exist")]
    UnknownStage,
    #[error("delivery stage conflicts with committed content")]
    StageConflict,
    #[error("delivery blob is not expected by the stage")]
    UnexpectedBlob,
    #[error("delivery chunk is out of order")]
    ChunkOutOfOrder,
    #[error("delivery chunk exceeds the declared blob length")]
    ChunkOverflow,
    #[error("delivery blob digest does not match its receipt")]
    BlobDigestMismatch,
    #[error("delivery bundle digest does not match its manifest")]
    BundleDigestMismatch,
    #[error("delivery stage is incomplete")]
    StageIncomplete,
    #[error("delivery store contents failed validation")]
    Corrupt,
    #[error("delivery storage operation failed")]
    Storage(#[source] io::Error),
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PersistedCommit {
    schema_version: u16,
    stage_id: DeliveryStageId,
    manifest: DeliveryBundleManifestV2,
    receipt: DeliveryCommitReceiptV1,
    completed_at_unix_ms: u64,
}

impl DeliveryStore {
    pub(crate) fn open(root: PathBuf) -> Result<(Self, Vec<NodeBundle>), DeliveryStoreError> {
        if !root.is_absolute() {
            return Err(DeliveryStoreError::Unavailable);
        }
        let root_missing = match fs::symlink_metadata(&root) {
            Ok(_) => false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(error) => return Err(DeliveryStoreError::Storage(error)),
        };
        ensure_materialization_root(&root).map_err(DeliveryStoreError::Storage)?;
        if root_missing {
            sync_parent_directory(&root).map_err(DeliveryStoreError::Storage)?;
        }
        let lock = MaterializationRootLock::acquire(&root.join(ROOT_LOCK_NAME))
            .map_err(DeliveryStoreError::Storage)?;
        verify_or_create_exact_file(&root.join(ROOT_MARKER_NAME), ROOT_MARKER)
            .map_err(DeliveryStoreError::Storage)?;
        sync_directory(&root).map_err(DeliveryStoreError::Storage)?;
        for name in [STAGES_DIRECTORY, BLOBS_DIRECTORY, COMMITS_DIRECTORY] {
            ensure_child_directory(&root.join(name))?;
        }

        let mut store = Self {
            root,
            stages: BTreeMap::new(),
            completed: BTreeMap::new(),
            commits: BTreeMap::new(),
            _lock: lock,
            #[cfg(test)]
            fail_after_partial_create_once: false,
            #[cfg(test)]
            fail_blob_parent_sync_once: std::cell::Cell::new(false),
            #[cfg(test)]
            fail_commit_parent_sync_once: std::cell::Cell::new(false),
            #[cfg(test)]
            fail_commit_before_rename_once: std::cell::Cell::new(false),
            #[cfg(test)]
            fail_stage_cleanup_once: std::cell::Cell::new(false),
        };
        store.discard_interrupted_stages()?;
        store.reload_commits()?;
        store.garbage_collect_blobs()?;
        store.expire(unix_time_ms())?;
        let bundles = store
            .commits
            .values()
            .map(|committed| committed.bundle.clone())
            .collect();
        Ok((store, bundles))
    }

    pub(crate) fn begin(
        &mut self,
        manifest: DeliveryBundleManifestV2,
    ) -> Result<DeliveryStageBegun, DeliveryStoreError> {
        let now = unix_time_ms();
        self.expire(now)?;
        manifest
            .validate()
            .map_err(|_| DeliveryStoreError::InvalidManifest)?;
        if digest_delivery_manifest(&manifest) != manifest.manifest_digest {
            return Err(DeliveryStoreError::InvalidManifest);
        }
        if let Some((stage_id, stage)) = self
            .stages
            .iter()
            .find(|(_, stage)| stage.manifest.manifest_digest == manifest.manifest_digest)
        {
            if stage.manifest != manifest {
                return Err(DeliveryStoreError::StageConflict);
            }
            return Ok(DeliveryStageBegun {
                stage_id: stage_id.clone(),
                manifest_digest: manifest.manifest_digest,
                missing_blobs: stage
                    .expected
                    .iter()
                    .filter(|(digest, receipt)| {
                        stage
                            .received
                            .get(*digest)
                            .map_or(true, |bytes| bytes.len() as u64 != receipt.byte_len)
                    })
                    .map(|(digest, _)| digest.clone())
                    .collect(),
            });
        }
        if let Some(committed) = self.commits.get(&manifest.manifest_digest) {
            if committed.manifest != manifest {
                return Err(DeliveryStoreError::StageConflict);
            }
            return Ok(DeliveryStageBegun {
                stage_id: committed.stage_id.clone(),
                manifest_digest: manifest.manifest_digest,
                missing_blobs: Vec::new(),
            });
        }
        if self.stages.len() == MAX_ACTIVE_DELIVERY_STAGES {
            return Err(DeliveryStoreError::Capacity);
        }

        let expected = unique_blob_receipts(&manifest)?;
        let mut missing = BTreeMap::new();
        for (digest, receipt) in &expected {
            if receipt.byte_len == 0 {
                if digest_delivery_blob(&[]) != *digest {
                    return Err(DeliveryStoreError::InvalidManifest);
                }
                self.publish_blob(digest, &[])?;
            } else if !self.validate_existing_blob(receipt)? {
                missing.insert(digest.clone(), receipt.clone());
            }
        }
        let reserved_bytes = missing.values().map(|receipt| receipt.byte_len).sum::<u64>();
        let active_bytes = self
            .stages
            .values()
            .map(|stage| stage.reserved_bytes)
            .sum::<u64>();
        if active_bytes.saturating_add(reserved_bytes) > MAX_ACTIVE_DELIVERY_BYTES {
            return Err(DeliveryStoreError::Capacity);
        }

        let stage_id = self.allocate_stage_id()?;
        let stage_root = self.stage_root(&stage_id);
        secure_create_directory(&stage_root).map_err(DeliveryStoreError::Storage)?;
        sync_parent_directory(&stage_root).map_err(DeliveryStoreError::Storage)?;
        let manifest_bytes = serde_json::to_vec(&manifest)
            .map_err(|_| DeliveryStoreError::InvalidManifest)?;
        secure_create_file(&stage_root.join(STAGE_MANIFEST_NAME), &manifest_bytes)
            .map_err(DeliveryStoreError::Storage)?;
        sync_directory(&stage_root).map_err(DeliveryStoreError::Storage)?;
        self.stages.insert(
            stage_id.clone(),
            DeliveryStage {
                manifest: manifest.clone(),
                expected: missing,
                received: BTreeMap::new(),
                reserved_bytes,
                created_at_unix_ms: now,
            },
        );
        let missing_blobs = self
            .stages
            .get(&stage_id)
            .expect("newly inserted delivery stage is present")
            .expected
            .keys()
            .cloned()
            .collect();
        Ok(DeliveryStageBegun {
            stage_id,
            manifest_digest: manifest.manifest_digest,
            missing_blobs,
        })
    }

    pub(crate) fn put_chunk(
        &mut self,
        stage_id: &DeliveryStageId,
        blob_digest: &DeliveryBlobDigestV1,
        offset: u64,
        chunk: &[u8],
    ) -> Result<DeliveryChunkAccepted, DeliveryStoreError> {
        #[cfg(test)]
        let fail_after_partial_create =
            std::mem::take(&mut self.fail_after_partial_create_once);
        let now = unix_time_ms();
        self.expire(now)?;
        let stage = self
            .stages
            .get_mut(stage_id)
            .ok_or(DeliveryStoreError::UnknownStage)?;
        let receipt = stage
            .expected
            .get(blob_digest)
            .ok_or(DeliveryStoreError::UnexpectedBlob)?;
        let received = stage.received.entry(blob_digest.clone()).or_default();
        if offset < received.len() as u64 {
            let end = offset
                .checked_add(chunk.len() as u64)
                .ok_or(DeliveryStoreError::ChunkOverflow)?;
            if end <= received.len() as u64
                && received[offset as usize..end as usize] == *chunk
            {
                return Ok(DeliveryChunkAccepted {
                    stage_id: stage_id.clone(),
                    blob_digest: blob_digest.clone(),
                    next_offset: end,
                });
            }
            return Err(DeliveryStoreError::ChunkOutOfOrder);
        }
        if offset != received.len() as u64 {
            return Err(DeliveryStoreError::ChunkOutOfOrder);
        }
        let next_offset = offset
            .checked_add(chunk.len() as u64)
            .ok_or(DeliveryStoreError::ChunkOverflow)?;
        if next_offset > receipt.byte_len {
            return Err(DeliveryStoreError::ChunkOverflow);
        }
        let previous_len = received.len();
        received.extend_from_slice(chunk);
        let partial_path = stage_blob_path(&self.root, stage_id, blob_digest);
        #[cfg(test)]
        let persisted = if fail_after_partial_create && offset == 0 {
            let partial_len = received.len().min(1);
            secure_create_file(&partial_path, &received[..partial_len]).and_then(|_| {
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected failure after partial delivery file creation",
                ))
            })
        } else if offset == 0 {
            secure_create_file(&partial_path, received)
        } else {
            secure_replace_file(&partial_path, received)
        };
        #[cfg(not(test))]
        let persisted = if offset == 0 {
            secure_create_file(&partial_path, received)
        } else {
            secure_replace_file(&partial_path, received)
        };
        if let Err(error) = persisted {
            received.truncate(previous_len);
            if let Err(reconcile_error) = reconcile_partial_write(&partial_path, received) {
                self.stages.remove(stage_id);
                return Err(DeliveryStoreError::Storage(reconcile_error));
            }
            return Err(DeliveryStoreError::Storage(error));
        }
        if next_offset == receipt.byte_len && digest_delivery_blob(received) != *blob_digest {
            received.clear();
            let _ = fs::remove_file(&partial_path);
            return Err(DeliveryStoreError::BlobDigestMismatch);
        }
        Ok(DeliveryChunkAccepted {
            stage_id: stage_id.clone(),
            blob_digest: blob_digest.clone(),
            next_offset,
        })
    }

    #[cfg(test)]
    fn fail_after_partial_create_once(&mut self) {
        self.fail_after_partial_create_once = true;
    }

    pub(crate) fn prepare_commit(
        &mut self,
        stage_id: &DeliveryStageId,
    ) -> Result<PreparedDeliveryCommit, DeliveryStoreError> {
        let now = unix_time_ms();
        self.expire(now)?;
        if let Some(completed) = self.completed.get(stage_id) {
            let committed = self
                .commits
                .values()
                .find(|commit| commit.receipt == completed.receipt)
                .ok_or(DeliveryStoreError::Corrupt)?;
            return Ok(PreparedDeliveryCommit {
                stage_id: stage_id.clone(),
                manifest: committed.manifest.clone(),
                receipt: completed.receipt.clone(),
                bundle: committed.bundle.clone(),
            });
        }
        if let Some(committed) = self
            .commits
            .values()
            .find(|committed| &committed.stage_id == stage_id)
        {
            return Ok(PreparedDeliveryCommit {
                stage_id: stage_id.clone(),
                manifest: committed.manifest.clone(),
                receipt: committed.receipt.clone(),
                bundle: committed.bundle.clone(),
            });
        }
        let stage = self
            .stages
            .get(stage_id)
            .ok_or(DeliveryStoreError::UnknownStage)?;
        for (digest, receipt) in &stage.expected {
            let bytes = stage
                .received
                .get(digest)
                .ok_or(DeliveryStoreError::StageIncomplete)?;
            if bytes.len() as u64 != receipt.byte_len {
                return Err(DeliveryStoreError::StageIncomplete);
            }
            if digest_delivery_blob(bytes) != *digest {
                return Err(DeliveryStoreError::BlobDigestMismatch);
            }
        }
        let blobs = self.collect_manifest_blobs(&stage.manifest, Some(&stage.received))?;
        let bundle = NodeBundle::from_delivery(stage.manifest.clone(), &blobs)
            .map_err(map_bundle_error)?;
        let receipt = DeliveryCommitReceiptV1 {
            bundle_id: stage.manifest.bundle_id.clone(),
            revision: stage.manifest.revision.clone(),
            bundle_digest: stage.manifest.bundle_digest.clone(),
            manifest_digest: stage.manifest.manifest_digest.clone(),
            blobs: unique_blob_receipts(&stage.manifest)?.into_values().collect(),
        };
        Ok(PreparedDeliveryCommit {
            stage_id: stage_id.clone(),
            manifest: stage.manifest.clone(),
            receipt,
            bundle,
        })
    }

    pub(crate) fn publish_commit(
        &mut self,
        prepared: PreparedDeliveryCommit,
    ) -> Result<DeliveryCommitReceiptV1, DeliveryStoreError> {
        if let Some(completed) = self.completed.get(&prepared.stage_id) {
            if completed.receipt == prepared.receipt {
                return Ok(completed.receipt.clone());
            }
            return Err(DeliveryStoreError::StageConflict);
        }
        for component in &prepared.manifest.components {
            let bytes = self
                .stages
                .get(&prepared.stage_id)
                .and_then(|stage| stage.received.get(&component.blob.digest))
                .cloned()
                .or_else(|| self.read_blob(&component.blob).ok())
                .ok_or(DeliveryStoreError::StageIncomplete)?;
            self.publish_blob(&component.blob.digest, &bytes)?;
        }
        match self.commits.get(&prepared.manifest.manifest_digest) {
            Some(existing) if existing.manifest == prepared.manifest => {}
            Some(_) => return Err(DeliveryStoreError::StageConflict),
            None => {
                let completed_at_unix_ms = unix_time_ms();
                self.persist_commit(&prepared, completed_at_unix_ms)?;
                self.commits.insert(
                    prepared.manifest.manifest_digest.clone(),
                    CommittedDelivery {
                        stage_id: prepared.stage_id.clone(),
                        manifest: prepared.manifest.clone(),
                        receipt: prepared.receipt.clone(),
                        bundle: prepared.bundle.clone(),
                    },
                );
            }
        }
        self.completed.insert(
            prepared.stage_id.clone(),
            CompletedStage {
                receipt: prepared.receipt.clone(),
            },
        );
        if self.stages.remove(&prepared.stage_id).is_some() {
            let _ = self.remove_stage_directory(&prepared.stage_id);
        }
        Ok(prepared.receipt)
    }

    #[cfg(test)]
    fn persist_commit_authority_before_reply(
        &self,
        prepared: &PreparedDeliveryCommit,
    ) -> Result<(), DeliveryStoreError> {
        for component in &prepared.manifest.components {
            let bytes = self
                .stages
                .get(&prepared.stage_id)
                .and_then(|stage| stage.received.get(&component.blob.digest))
                .cloned()
                .or_else(|| self.read_blob(&component.blob).ok())
                .ok_or(DeliveryStoreError::StageIncomplete)?;
            self.publish_blob(&component.blob.digest, &bytes)?;
        }
        self.persist_commit(prepared, unix_time_ms())
    }

    pub(crate) fn abort(
        &mut self,
        stage_id: &DeliveryStageId,
    ) -> Result<(), DeliveryStoreError> {
        self.expire(unix_time_ms())?;
        if self.completed.contains_key(stage_id) {
            return Ok(());
        }
        if self.stages.remove(stage_id).is_none() {
            return Err(DeliveryStoreError::UnknownStage);
        }
        self.remove_stage_directory(stage_id)
    }

    fn collect_manifest_blobs(
        &self,
        manifest: &DeliveryBundleManifestV2,
        staged: Option<&BTreeMap<DeliveryBlobDigestV1, Vec<u8>>>,
    ) -> Result<BTreeMap<DeliveryBlobDigestV1, Vec<u8>>, DeliveryStoreError> {
        let mut blobs = BTreeMap::new();
        for receipt in unique_blob_receipts(manifest)?.into_values() {
            let bytes = staged
                .and_then(|staged| staged.get(&receipt.digest).cloned())
                .map(Ok)
                .unwrap_or_else(|| self.read_blob(&receipt))?;
            blobs.insert(receipt.digest, bytes);
        }
        Ok(blobs)
    }

    fn read_blob(
        &self,
        receipt: &DeliveryBlobReceiptV1,
    ) -> Result<Vec<u8>, DeliveryStoreError> {
        let path = self.blob_path(&receipt.digest);
        validate_secure_file(&path).map_err(DeliveryStoreError::Storage)?;
        let mut bytes = Vec::with_capacity(receipt.byte_len as usize);
        File::open(path)
            .map_err(DeliveryStoreError::Storage)?
            .take(receipt.byte_len.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(DeliveryStoreError::Storage)?;
        if bytes.len() as u64 != receipt.byte_len
            || digest_delivery_blob(&bytes) != receipt.digest
        {
            return Err(DeliveryStoreError::Corrupt);
        }
        Ok(bytes)
    }

    fn validate_existing_blob(
        &self,
        receipt: &DeliveryBlobReceiptV1,
    ) -> Result<bool, DeliveryStoreError> {
        let path = self.blob_path(&receipt.digest);
        if !path.exists() {
            return Ok(false);
        }
        self.read_blob(receipt).map(|_| true)
    }

    fn publish_blob(
        &self,
        digest: &DeliveryBlobDigestV1,
        bytes: &[u8],
    ) -> Result<(), DeliveryStoreError> {
        if digest_delivery_blob(bytes) != *digest {
            return Err(DeliveryStoreError::BlobDigestMismatch);
        }
        let target = self.blob_path(digest);
        if target.exists() {
            let receipt = DeliveryBlobReceiptV1::new(digest.clone(), bytes.len() as u64)
                .map_err(|_| DeliveryStoreError::InvalidManifest)?;
            self.read_blob(&receipt)?;
            return self.sync_blob_parent(&target);
        }
        let temporary = self
            .root
            .join(BLOBS_DIRECTORY)
            .join(format!(".tmp-{}", random_hex_16()?));
        secure_create_file(&temporary, bytes).map_err(DeliveryStoreError::Storage)?;
        match publish_temporary_create_new(&temporary, &target) {
            Ok(()) => {
                let receipt = DeliveryBlobReceiptV1::new(digest.clone(), bytes.len() as u64)
                    .map_err(|_| DeliveryStoreError::InvalidManifest)?;
                self.read_blob(&receipt)?;
                self.sync_blob_parent(&target)
            }
            Err(_error) if target.exists() => {
                let receipt = DeliveryBlobReceiptV1::new(digest.clone(), bytes.len() as u64)
                    .map_err(|_| DeliveryStoreError::InvalidManifest)?;
                self.read_blob(&receipt)?;
                self.sync_blob_parent(&target)
            }
            Err(error) => {
                Err(DeliveryStoreError::Storage(error))
            }
        }
    }

    fn sync_blob_parent(&self, target: &Path) -> Result<(), DeliveryStoreError> {
        #[cfg(test)]
        if self.fail_blob_parent_sync_once.replace(false) {
            return Err(DeliveryStoreError::Storage(io::Error::new(
                io::ErrorKind::Other,
                "injected delivery blob parent sync failure",
            )));
        }
        sync_parent_directory(target).map_err(DeliveryStoreError::Storage)
    }

    #[cfg(test)]
    fn fail_blob_parent_sync_once(&self) {
        self.fail_blob_parent_sync_once.set(true);
    }

    fn persist_commit(
        &self,
        prepared: &PreparedDeliveryCommit,
        completed_at_unix_ms: u64,
    ) -> Result<(), DeliveryStoreError> {
        let path = self.commit_path(&prepared.manifest.manifest_digest);
        let record = PersistedCommit {
            schema_version: COMMIT_SCHEMA,
            stage_id: prepared.stage_id.clone(),
            manifest: prepared.manifest.clone(),
            receipt: prepared.receipt.clone(),
            completed_at_unix_ms,
        };
        if path.exists() {
            validate_commit_authority(&path, &record)?;
            return self.sync_commit_parent(&path);
        }
        let bytes = serde_json::to_vec(&record)
            .map_err(|_| DeliveryStoreError::InvalidManifest)?;
        let parent = path.parent().ok_or(DeliveryStoreError::Unavailable)?;
        validate_secure_directory(parent).map_err(DeliveryStoreError::Storage)?;
        let temporary = parent.join(format!(".tmp-{}", random_hex_16()?));
        secure_create_file(&temporary, &bytes).map_err(DeliveryStoreError::Storage)?;
        #[cfg(test)]
        if self.fail_commit_before_rename_once.replace(false) {
            return Err(DeliveryStoreError::Storage(io::Error::new(
                io::ErrorKind::Other,
                "injected delivery commit failure before rename",
            )));
        }
        match publish_temporary_create_new(&temporary, &path) {
            Ok(()) => {
                validate_commit_authority(&path, &record)?;
                self.sync_commit_parent(&path)
            }
            Err(_error) if path.exists() => {
                validate_commit_authority(&path, &record)?;
                self.sync_commit_parent(&path)
            }
            Err(error) => {
                Err(DeliveryStoreError::Storage(error))
            }
        }
    }

    fn sync_commit_parent(&self, path: &Path) -> Result<(), DeliveryStoreError> {
        #[cfg(test)]
        if self.fail_commit_parent_sync_once.replace(false) {
            return Err(DeliveryStoreError::Storage(io::Error::new(
                io::ErrorKind::Other,
                "injected delivery commit parent sync failure",
            )));
        }
        sync_parent_directory(path).map_err(DeliveryStoreError::Storage)
    }

    #[cfg(test)]
    fn fail_commit_parent_sync_once(&self) {
        self.fail_commit_parent_sync_once.set(true);
    }

    #[cfg(test)]
    fn fail_commit_before_rename_once(&self) {
        self.fail_commit_before_rename_once.set(true);
    }

    fn reload_commits(&mut self) -> Result<(), DeliveryStoreError> {
        let directory = self.root.join(COMMITS_DIRECTORY);
        validate_secure_directory(&directory).map_err(DeliveryStoreError::Storage)?;
        let mut entries = Vec::new();
        for entry in fs::read_dir(&directory).map_err(DeliveryStoreError::Storage)? {
            if entries.len() == MAX_BUNDLE_CATALOG_ENTRIES {
                return Err(DeliveryStoreError::Corrupt);
            }
            entries.push(entry.map_err(DeliveryStoreError::Storage)?);
        }
        entries.sort_by_key(|entry| entry.file_name());
        let mut canonical_entries = Vec::with_capacity(entries.len());
        let mut removed_temporary = false;
        for entry in entries {
            let path = entry.path();
            let file_name = entry.file_name();
            if is_owned_commit_temporary_name(&file_name) {
                validate_secure_file(&path).map_err(DeliveryStoreError::Storage)?;
                fs::remove_file(&path).map_err(DeliveryStoreError::Storage)?;
                removed_temporary = true;
                continue;
            }
            if !is_canonical_commit_file_name(&file_name) {
                return Err(DeliveryStoreError::Corrupt);
            }
            canonical_entries.push(entry);
        }
        if removed_temporary {
            sync_directory(&directory).map_err(DeliveryStoreError::Storage)?;
        }
        for entry in canonical_entries {
            let path = entry.path();
            validate_secure_file(&path).map_err(DeliveryStoreError::Storage)?;
            let record: PersistedCommit = read_bounded_json(&path)?;
            if record.schema_version != COMMIT_SCHEMA
                || commit_file_name(&record.manifest.manifest_digest)
                    != entry.file_name().to_string_lossy()
                || digest_delivery_manifest(&record.manifest)
                    != record.manifest.manifest_digest
            {
                return Err(DeliveryStoreError::Corrupt);
            }
            let blobs = self.collect_manifest_blobs(&record.manifest, None)?;
            let bundle = NodeBundle::from_delivery(record.manifest.clone(), &blobs)
                .map_err(map_bundle_error)?;
            let receipt = DeliveryCommitReceiptV1 {
                bundle_id: record.manifest.bundle_id.clone(),
                revision: record.manifest.revision.clone(),
                bundle_digest: record.manifest.bundle_digest.clone(),
                manifest_digest: record.manifest.manifest_digest.clone(),
                blobs: unique_blob_receipts(&record.manifest)?.into_values().collect(),
            };
            if receipt != record.receipt {
                return Err(DeliveryStoreError::Corrupt);
            }
            if self
                .commits
                .insert(
                    record.manifest.manifest_digest.clone(),
                    CommittedDelivery {
                        stage_id: record.stage_id.clone(),
                        manifest: record.manifest,
                        receipt: receipt.clone(),
                        bundle,
                    },
                )
                .is_some()
            {
                return Err(DeliveryStoreError::Corrupt);
            }
            if self.completed.insert(
                record.stage_id,
                CompletedStage {
                    receipt,
                },
            ).is_some() {
                return Err(DeliveryStoreError::Corrupt);
            }
        }
        Ok(())
    }

    fn discard_interrupted_stages(&self) -> Result<(), DeliveryStoreError> {
        let directory = self.root.join(STAGES_DIRECTORY);
        validate_secure_directory(&directory).map_err(DeliveryStoreError::Storage)?;
        let mut count = 0_usize;
        for entry in fs::read_dir(&directory).map_err(DeliveryStoreError::Storage)? {
            count += 1;
            if count > MAX_ACTIVE_DELIVERY_STAGES {
                return Err(DeliveryStoreError::Corrupt);
            }
            let entry = entry.map_err(DeliveryStoreError::Storage)?;
            let path = entry.path();
            DeliveryStageId::new(entry.file_name().to_string_lossy().into_owned())
                .map_err(|_| DeliveryStoreError::Corrupt)?;
            validate_secure_directory(&path).map_err(DeliveryStoreError::Storage)?;
            remove_tree_no_links(&path).map_err(DeliveryStoreError::Storage)?;
            fs::remove_dir(path).map_err(DeliveryStoreError::Storage)?;
        }
        Ok(())
    }

    fn garbage_collect_blobs(&self) -> Result<(), DeliveryStoreError> {
        let referenced = self
            .commits
            .values()
            .flat_map(|commit| {
                commit
                    .manifest
                    .components
                    .iter()
                    .map(|component| digest_hex(component.blob.digest.as_str()).to_owned())
            })
            .collect::<BTreeSet<_>>();
        let directory = self.root.join(BLOBS_DIRECTORY);
        validate_secure_directory(&directory).map_err(DeliveryStoreError::Storage)?;
        let mut count = 0_usize;
        for entry in fs::read_dir(&directory).map_err(DeliveryStoreError::Storage)? {
            count += 1;
            if count > MAX_DELIVERY_CAS_BLOBS + 1 {
                return Err(DeliveryStoreError::Corrupt);
            }
            let entry = entry.map_err(DeliveryStoreError::Storage)?;
            let path = entry.path();
            validate_secure_file(&path).map_err(DeliveryStoreError::Storage)?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let canonical = name.len() == 64
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'));
            if !canonical || !referenced.contains(&name) {
                fs::remove_file(path).map_err(DeliveryStoreError::Storage)?;
            }
        }
        Ok(())
    }

    fn expire(&mut self, now: u64) -> Result<(), DeliveryStoreError> {
        let ttl_ms = DELIVERY_STAGE_TTL.as_millis() as u64;
        let expired = self
            .stages
            .iter()
            .filter(|(_, stage)| now.saturating_sub(stage.created_at_unix_ms) >= ttl_ms)
            .map(|(stage_id, _)| stage_id.clone())
            .collect::<Vec<_>>();
        for stage_id in expired {
            self.stages.remove(&stage_id);
            self.remove_stage_directory(&stage_id)?;
        }
        Ok(())
    }

    fn remove_stage_directory(
        &self,
        stage_id: &DeliveryStageId,
    ) -> Result<(), DeliveryStoreError> {
        #[cfg(test)]
        if self.fail_stage_cleanup_once.replace(false) {
            return Err(DeliveryStoreError::Storage(io::Error::new(
                io::ErrorKind::Other,
                "injected delivery stage cleanup failure",
            )));
        }
        let path = self.stage_root(stage_id);
        if !path.exists() {
            return Ok(());
        }
        validate_secure_directory(&path).map_err(DeliveryStoreError::Storage)?;
        remove_tree_no_links(&path).map_err(DeliveryStoreError::Storage)?;
        fs::remove_dir(path).map_err(DeliveryStoreError::Storage)
    }

    #[cfg(test)]
    fn fail_stage_cleanup_once(&self) {
        self.fail_stage_cleanup_once.set(true);
    }

    fn allocate_stage_id(&self) -> Result<DeliveryStageId, DeliveryStoreError> {
        for _ in 0..8 {
            let mut nonce = [0_u8; crate::protocol::DELIVERY_STAGE_NONCE_BYTES];
            SystemRandom::new()
                .fill(&mut nonce)
                .map_err(|_| DeliveryStoreError::Unavailable)?;
            let stage_id = DeliveryStageId::from_nonce(nonce);
            if !self.stages.contains_key(&stage_id)
                && !self.completed.contains_key(&stage_id)
                && !self.stage_root(&stage_id).exists()
            {
                return Ok(stage_id);
            }
        }
        Err(DeliveryStoreError::Capacity)
    }

    fn stage_root(&self, stage_id: &DeliveryStageId) -> PathBuf {
        self.root.join(STAGES_DIRECTORY).join(stage_id.as_str())
    }

    fn blob_path(&self, digest: &DeliveryBlobDigestV1) -> PathBuf {
        self.root.join(BLOBS_DIRECTORY).join(digest_hex(digest.as_str()))
    }

    fn commit_path(&self, digest: &DeliveryManifestDigestV2) -> PathBuf {
        self.root.join(COMMITS_DIRECTORY).join(commit_file_name(digest))
    }

}

fn unique_blob_receipts(
    manifest: &DeliveryBundleManifestV2,
) -> Result<BTreeMap<DeliveryBlobDigestV1, DeliveryBlobReceiptV1>, DeliveryStoreError> {
    let mut receipts = BTreeMap::new();
    for component in &manifest.components {
        if let Some(existing) = receipts.insert(
            component.blob.digest.clone(),
            component.blob.clone(),
        ) {
            if existing.byte_len != component.blob.byte_len {
                return Err(DeliveryStoreError::InvalidManifest);
            }
        }
    }
    Ok(receipts)
}

fn stage_blob_path(
    root: &Path,
    stage_id: &DeliveryStageId,
    digest: &DeliveryBlobDigestV1,
) -> PathBuf {
    root.join(STAGES_DIRECTORY)
        .join(stage_id.as_str())
        .join(format!("{}.partial", digest_hex(digest.as_str())))
}

fn reconcile_partial_write(path: &Path, previous: &[u8]) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "delivery partial path is not a regular file",
                ));
            }
            validate_secure_file(path)?;
            if previous.is_empty() {
                fs::remove_file(path)
            } else {
                secure_replace_file(path, previous)
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound && previous.is_empty() => Ok(()),
        Err(error) => Err(error),
    }
}

fn digest_hex(value: &str) -> &str {
    value
        .strip_prefix("sha256:")
        .expect("protocol digest is validated")
}

fn commit_file_name(digest: &DeliveryManifestDigestV2) -> String {
    format!("{}.json", digest_hex(digest.as_str()))
}

fn is_owned_commit_temporary_name(name: &OsStr) -> bool {
    name.to_str()
        .and_then(|name| name.strip_prefix(".tmp-"))
        .map_or(false, |nonce| is_lowercase_hex(nonce, 32))
}

fn is_canonical_commit_file_name(name: &OsStr) -> bool {
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

fn ensure_child_directory(path: &Path) -> Result<(), DeliveryStoreError> {
    match secure_create_directory(path) {
        Ok(()) => sync_parent_directory(path).map_err(DeliveryStoreError::Storage),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            validate_secure_directory(path).map_err(DeliveryStoreError::Storage)
        }
        Err(error) => Err(DeliveryStoreError::Storage(error)),
    }
}

fn validate_commit_authority(
    path: &Path,
    expected: &PersistedCommit,
) -> Result<(), DeliveryStoreError> {
    validate_secure_file(path).map_err(DeliveryStoreError::Storage)?;
    let actual: PersistedCommit = read_bounded_json(path)?;
    if actual.schema_version == expected.schema_version
        && actual.stage_id == expected.stage_id
        && actual.manifest == expected.manifest
        && actual.receipt == expected.receipt
    {
        Ok(())
    } else {
        Err(DeliveryStoreError::StageConflict)
    }
}

#[cfg(windows)]
fn sync_parent_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "delivery path has no parent")
    })?;
    File::open(parent)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

// pub(crate): also reused verbatim by `context_pack_store::ContextPackStore`
// for its own create-new atomic publish (same Windows MoveFileExW +
// MOVEFILE_WRITE_THROUGH semantics, same create-new-only Unix rename path).
pub(crate) fn publish_temporary_create_new(source: &Path, target: &Path) -> io::Result<()> {
    if source.parent() != target.parent() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "delivery publish paths must share a parent",
        ));
    }
    validate_secure_file(source)?;
    let published = publish_temporary_create_new_platform(source, target);
    if let Err(publish_error) = published {
        match validate_secure_file(source) {
            Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                Err(publish_error)
            }
            Err(cleanup_error) => Err(io::Error::new(
                cleanup_error.kind(),
                format!(
                    "delivery publish failed ({publish_error}); temporary validation also failed: {cleanup_error}"
                ),
            )),
            Ok(()) => match fs::remove_file(source) {
                Ok(()) => sync_parent_directory(source).map_err(|cleanup_error| {
                    io::Error::new(
                        cleanup_error.kind(),
                        format!(
                            "delivery publish failed ({publish_error}); temporary cleanup sync also failed: {cleanup_error}"
                        ),
                    )
                }).and(Err(publish_error)),
                Err(cleanup_error) if cleanup_error.kind() == io::ErrorKind::NotFound => {
                    Err(publish_error)
                }
                Err(cleanup_error) => Err(io::Error::new(
                    cleanup_error.kind(),
                    format!(
                        "delivery publish failed ({publish_error}); temporary cleanup also failed: {cleanup_error}"
                    ),
                )),
            },
        }
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn publish_temporary_create_new_platform(source: &Path, target: &Path) -> io::Result<()> {
    fs::rename(source, target)
}

#[cfg(windows)]
fn publish_temporary_create_new_platform(source: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    // SAFETY: both paths are NUL-terminated UTF-16 buffers that remain alive
    // for the call. Omitting MOVEFILE_REPLACE_EXISTING preserves create-new
    // CAS and commit-authority semantics; WRITE_THROUGH makes the move durable.
    let moved = unsafe {
        MoveFileExW(
            source.as_ptr(),
            target.as_ptr(),
            MOVEFILE_WRITE_THROUGH,
        )
    };
    if moved == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn random_hex_16() -> Result<String, DeliveryStoreError> {
    let mut nonce = [0_u8; 16];
    SystemRandom::new()
        .fill(&mut nonce)
        .map_err(|_| DeliveryStoreError::Unavailable)?;
    let mut value = String::with_capacity(32);
    for byte in nonce {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    Ok(value)
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T, DeliveryStoreError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(DeliveryStoreError::Storage)?
        .take(MAX_PERSISTED_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(DeliveryStoreError::Storage)?;
    if bytes.len() as u64 > MAX_PERSISTED_MANIFEST_BYTES {
        return Err(DeliveryStoreError::Corrupt);
    }
    serde_json::from_slice(&bytes).map_err(|_| DeliveryStoreError::Corrupt)
}

fn map_bundle_error(error: NodeBundleError) -> DeliveryStoreError {
    match error {
        NodeBundleError::DigestMismatch { .. } => DeliveryStoreError::BundleDigestMismatch,
        NodeBundleError::DeliveryBlobDigestMismatch
        | NodeBundleError::DeliveryBlobLengthMismatch => DeliveryStoreError::BlobDigestMismatch,
        NodeBundleError::InvalidDeliveryManifest
        | NodeBundleError::DeliveryManifestDigestMismatch
        | NodeBundleError::DeliveryBlobMissing
        | NodeBundleError::ExecutableFile { .. } => DeliveryStoreError::InvalidManifest,
        _ => DeliveryStoreError::Corrupt,
    }
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
    use crate::bundle_catalog::{BundleCatalog, BundleCatalogError};
    use crate::bundle_provider::{
        bundle_launch_arguments, validate_bundle_binding, BundleProviderError,
        BundleProviderLayout,
    };
    use crate::protocol::{
        DeliveryBlobReceiptV1, DeliveryComponentKindV2, DeliveryComponentV2,
        DeliveryRelativePathV2, DeliveryScopeV2, SpawnBundleDigest, SpawnBundleId,
        NodeIncarnationId, SessionMode, SpawnBundleRevision,
    };
    use crate::session_environment::{
        MaterializationId, MaterializationOwner, NodeSecretResolveError,
        NodeSecretResolver, NodeSecretValue, NodeSessionMaterializationProfile,
        NodeSessionPathBinding, NodeSessionPathClass, SessionEnvironmentMaterializer,
    };
    use gate4agent_types::{AgentId, AgentInstanceId, SessionGeneration, TransportKind};
    use std::ffi::OsString;
    use std::sync::Arc;
    use ring::digest::{Context, SHA256};
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new(label: &str) -> Self {
            Self(std::env::temp_dir().join(format!(
                "gate4agent-delivery-{label}-{}-{}",
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

    #[cfg(windows)]
    #[test]
    fn delivery_windows_write_through_publish_fixture_preserves_conflict_and_cleans_temp() {
        let root = TestRoot::new("windows-write-through");
        secure_create_directory(&root.0).unwrap();
        let target = root.0.join("published");
        let first_temporary = root.0.join(".tmp-first");
        secure_create_file(&first_temporary, b"first").unwrap();
        publish_temporary_create_new(&first_temporary, &target).unwrap();
        assert!(!first_temporary.exists());
        validate_secure_file(&target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first");

        let conflicting_temporary = root.0.join(".tmp-conflict");
        secure_create_file(&conflicting_temporary, b"second").unwrap();
        assert!(publish_temporary_create_new(&conflicting_temporary, &target).is_err());
        assert!(!conflicting_temporary.exists());
        validate_secure_file(&target).unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"first");
    }

    fn digest_text(bytes: &[u8]) -> String {
        let digest = ring::digest::digest(&SHA256, bytes);
        let mut value = String::from("sha256:");
        for byte in digest.as_ref() {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").unwrap();
        }
        value
    }

    fn bundle_digest(files: &[(String, Vec<u8>)]) -> SpawnBundleDigest {
        let mut context = Context::new(&SHA256);
        context.update(b"g4a-bundle-v1\0");
        for (path, bytes) in files {
            context.update(&(path.len() as u32).to_be_bytes());
            context.update(path.as_bytes());
            context.update(&(bytes.len() as u64).to_be_bytes());
            context.update(bytes);
        }
        let digest = context.finish();
        let mut value = String::from("sha256:");
        for byte in digest.as_ref() {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").unwrap();
        }
        SpawnBundleDigest::new(value).unwrap()
    }

    fn manifest_with(
        bundle_id: &str,
        revision: &str,
        files: Vec<(DeliveryComponentKindV2, DeliveryScopeV2, &str, Vec<u8>)>,
    ) -> (DeliveryBundleManifestV2, BTreeMap<DeliveryBlobDigestV1, Vec<u8>>) {
        let mut files = files
            .into_iter()
            .map(|(kind, scope, path, bytes)| (kind, scope, path.to_owned(), bytes))
            .collect::<Vec<_>>();
        files.sort_by(|left, right| left.2.cmp(&right.2));
        let digest_files = files
            .iter()
            .map(|(_, _, path, bytes)| (path.clone(), bytes.clone()))
            .collect::<Vec<_>>();
        let mut blobs = BTreeMap::new();
        let components = files
            .into_iter()
            .map(|(kind, scope, path, bytes)| {
                let digest = DeliveryBlobDigestV1::new(digest_text(&bytes)).unwrap();
                blobs.entry(digest.clone()).or_insert(bytes.clone());
                DeliveryComponentV2 {
                    kind,
                    scope,
                    relative_path: DeliveryRelativePathV2::new(path).unwrap(),
                    blob: DeliveryBlobReceiptV1::new(digest, bytes.len() as u64).unwrap(),
                }
            })
            .collect();
        let mut manifest = DeliveryBundleManifestV2 {
            bundle_id: SpawnBundleId::new(bundle_id).unwrap(),
            revision: SpawnBundleRevision::new(revision).unwrap(),
            bundle_digest: bundle_digest(&digest_files),
            manifest_digest: DeliveryManifestDigestV2::new(format!(
                "sha256:{}",
                "0".repeat(64),
            ))
            .unwrap(),
            components,
        };
        manifest.manifest_digest = digest_delivery_manifest(&manifest);
        (manifest, blobs)
    }

    fn skill_manifest(
        bundle_id: &str,
        revision: &str,
    ) -> (DeliveryBundleManifestV2, BTreeMap<DeliveryBlobDigestV1, Vec<u8>>) {
        manifest_with(
            bundle_id,
            revision,
            vec![
                (
                    DeliveryComponentKindV2::PluginManifest,
                    DeliveryScopeV2::Session,
                    ".claude-plugin/plugin.json",
                    br#"{"name":"review-tools","description":"review tools","version":"1.0.0"}"#.to_vec(),
                ),
                (
                    DeliveryComponentKindV2::PluginManifest,
                    DeliveryScopeV2::Session,
                    "plugin.json",
                    br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools"}"#.to_vec(),
                ),
                (
                    DeliveryComponentKindV2::Skill,
                    DeliveryScopeV2::Session,
                    "skills/review/SKILL.md",
                    b"---\nname: review\ndescription: review changes\n---\nReview.\n".to_vec(),
                ),
            ],
        )
    }

    fn upload_missing(
        store: &mut DeliveryStore,
        begun: &DeliveryStageBegun,
        blobs: &BTreeMap<DeliveryBlobDigestV1, Vec<u8>>,
    ) {
        for digest in &begun.missing_blobs {
            store
                .put_chunk(&begun.stage_id, digest, 0, &blobs[digest])
                .unwrap();
        }
    }

    fn encode_chunk(bytes: &[u8]) -> crate::protocol::DeliveryBlobChunkHexV1 {
        let mut value = String::with_capacity(bytes.len() * 2);
        for byte in bytes {
            use std::fmt::Write as _;
            write!(&mut value, "{byte:02x}").unwrap();
        }
        crate::protocol::DeliveryBlobChunkHexV1::new(value).unwrap()
    }

    #[test]
    fn delivery_chunks_reject_out_of_order_and_accept_exact_reply_replay() {
        let root = TestRoot::new("chunks");
        let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let begun = store.begin(manifest.clone()).unwrap();
        let digest = begun.missing_blobs[0].clone();
        let bytes = &blobs[&digest];
        let split = bytes.len().min(5);
        assert!(matches!(
            store.put_chunk(&begun.stage_id, &digest, 1, &bytes[..split]),
            Err(DeliveryStoreError::ChunkOutOfOrder),
        ));
        let accepted = store
            .put_chunk(&begun.stage_id, &digest, 0, &bytes[..split])
            .unwrap();
        assert_eq!(accepted.next_offset, split as u64);
        let replay = store
            .put_chunk(&begun.stage_id, &digest, 0, &bytes[..split])
            .unwrap();
        assert_eq!(replay.next_offset, split as u64);
        let resumed = store.begin(manifest).unwrap();
        assert_eq!(resumed.stage_id, begun.stage_id);
        assert!(resumed.missing_blobs.contains(&digest));
        assert!(matches!(
            store.put_chunk(&begun.stage_id, &digest, 0, b"wrong"),
            Err(DeliveryStoreError::ChunkOutOfOrder),
        ));
    }

    #[test]
    fn delivery_chunk_replay_reports_exact_end_after_multiple_chunks() {
        let root = TestRoot::new("multi-chunk-replay");
        let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let begun = store.begin(manifest).unwrap();
        let digest = begun
            .missing_blobs
            .iter()
            .find(|digest| blobs[*digest].len() >= 6)
            .unwrap()
            .clone();
        let bytes = &blobs[&digest];
        assert_eq!(
            store.put_chunk(&begun.stage_id, &digest, 0, &bytes[..2]).unwrap().next_offset,
            2,
        );
        assert_eq!(
            store.put_chunk(&begun.stage_id, &digest, 2, &bytes[2..4]).unwrap().next_offset,
            4,
        );
        assert_eq!(
            store.put_chunk(&begun.stage_id, &digest, 0, &bytes[..2]).unwrap().next_offset,
            2,
        );
        assert_eq!(
            store.put_chunk(&begun.stage_id, &digest, 2, &bytes[2..4]).unwrap().next_offset,
            4,
        );
        assert_eq!(
            store.put_chunk(&begun.stage_id, &digest, 4, &bytes[4..]).unwrap().next_offset,
            bytes.len() as u64,
        );
        assert_eq!(store.stages[&begun.stage_id].received[&digest], *bytes);
        for remaining in begun.missing_blobs.iter().filter(|candidate| **candidate != digest) {
            store
                .put_chunk(&begun.stage_id, remaining, 0, &blobs[remaining])
                .unwrap();
        }
        let prepared = store.prepare_commit(&begun.stage_id).unwrap();
        store.publish_commit(prepared).unwrap();
    }

    #[test]
    fn delivery_chunk_unsafe_partial_path_discards_stage() {
        let root = TestRoot::new("chunk-storage-rollback");
        let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let begun = store.begin(manifest).unwrap();
        let digest = begun.missing_blobs[0].clone();
        let bytes = &blobs[&digest];
        let partial = stage_blob_path(&root.0, &begun.stage_id, &digest);
        secure_create_directory(&partial).unwrap();
        assert!(matches!(
            store.put_chunk(&begun.stage_id, &digest, 0, bytes),
            Err(DeliveryStoreError::Storage(_)),
        ));
        assert!(matches!(
            store.put_chunk(&begun.stage_id, &digest, 0, bytes),
            Err(DeliveryStoreError::UnknownStage),
        ));
    }

    #[test]
    fn delivery_chunk_create_failure_removes_partial_before_exact_retry() {
        let root = TestRoot::new("chunk-create-rollback");
        let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let begun = store.begin(manifest).unwrap();
        let digest = begun.missing_blobs[0].clone();
        let bytes = &blobs[&digest];
        let partial = stage_blob_path(&root.0, &begun.stage_id, &digest);
        store.fail_after_partial_create_once();
        assert!(matches!(
            store.put_chunk(&begun.stage_id, &digest, 0, bytes),
            Err(DeliveryStoreError::Storage(_)),
        ));
        assert!(!partial.exists());
        let accepted = store
            .put_chunk(&begun.stage_id, &digest, 0, bytes)
            .unwrap();
        assert_eq!(accepted.next_offset, bytes.len() as u64);
        for remaining in begun.missing_blobs.iter().filter(|candidate| **candidate != digest) {
            store
                .put_chunk(&begun.stage_id, remaining, 0, &blobs[remaining])
                .unwrap();
        }
        let prepared = store.prepare_commit(&begun.stage_id).unwrap();
        store.publish_commit(prepared).unwrap();
    }

    #[test]
    fn delivery_store_identity_isolated_by_exact_state_file() {
        let parent = TestRoot::new("state-identity");
        secure_create_directory(&parent.0).unwrap();
        let state_a = parent.0.join("node-a.json");
        let state_b = parent.0.join("node-b.json");
        let store_root_a = super::super::delivery_store_root_for_state_path(&state_a).unwrap();
        let store_root_b = super::super::delivery_store_root_for_state_path(&state_b).unwrap();
        assert_ne!(store_root_a, store_root_b);
        assert_eq!(
            store_root_a.file_name().unwrap(),
            std::ffi::OsStr::new("node-a.json.delivery-store"),
        );
        let (store_a, _) = DeliveryStore::open(store_root_a.clone()).unwrap();
        let (store_b, _) = DeliveryStore::open(store_root_b).unwrap();
        assert!(DeliveryStore::open(store_root_a).is_err());
        drop(store_b);
        drop(store_a);
    }

    #[test]
    fn delivery_commit_rejects_blob_tamper_and_bundle_digest_tamper() {
        let root = TestRoot::new("tamper");
        let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let begun = store.begin(manifest).unwrap();
        let digest = begun.missing_blobs[0].clone();
        let mut tampered = blobs[&digest].clone();
        tampered[0] ^= 1;
        assert!(matches!(
            store.put_chunk(&begun.stage_id, &digest, 0, &tampered),
            Err(DeliveryStoreError::BlobDigestMismatch),
        ));

        let (mut wrong_bundle, blobs) = skill_manifest("wrong-bundle", "r1");
        wrong_bundle.bundle_digest = SpawnBundleDigest::new(format!(
            "sha256:{}",
            "f".repeat(64),
        ))
        .unwrap();
        wrong_bundle.manifest_digest = digest_delivery_manifest(&wrong_bundle);
        let begun = store.begin(wrong_bundle).unwrap();
        upload_missing(&mut store, &begun, &blobs);
        assert!(matches!(
            store.prepare_commit(&begun.stage_id),
            Err(DeliveryStoreError::BundleDigestMismatch),
        ));
    }

    #[test]
    fn delivery_stage_bounds_reject_manifest_overflow() {
        let root = TestRoot::new("overflow");
        let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        let digest = DeliveryBlobDigestV1::new(format!("sha256:{}", "1".repeat(64))).unwrap();
        let components = (0..33)
            .map(|index| DeliveryComponentV2 {
                kind: DeliveryComponentKindV2::File,
                scope: DeliveryScopeV2::Session,
                relative_path: DeliveryRelativePathV2::new(format!("files/{index:03}"))
                    .unwrap(),
                blob: DeliveryBlobReceiptV1::new(
                    digest.clone(),
                    crate::protocol::MAX_DELIVERY_FILE_BYTES as u64,
                )
                .unwrap(),
            })
            .collect();
        let manifest = DeliveryBundleManifestV2 {
            bundle_id: SpawnBundleId::new("overflow").unwrap(),
            revision: SpawnBundleRevision::new("r1").unwrap(),
            bundle_digest: SpawnBundleDigest::new(format!("sha256:{}", "2".repeat(64))).unwrap(),
            manifest_digest: DeliveryManifestDigestV2::new(format!("sha256:{}", "3".repeat(64))).unwrap(),
            components,
        };
        assert!(matches!(
            store.begin(manifest),
            Err(DeliveryStoreError::InvalidManifest),
        ));
    }

    #[test]
    fn delivery_cas_deduplicates_identical_blobs() {
        let root = TestRoot::new("dedup");
        let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        let (first, blobs) = skill_manifest("review-tools-a", "r1");
        let begun = store.begin(first).unwrap();
        upload_missing(&mut store, &begun, &blobs);
        let prepared = store.prepare_commit(&begun.stage_id).unwrap();
        store.publish_commit(prepared).unwrap();

        let (second, _) = skill_manifest("review-tools-b", "r1");
        let begun = store.begin(second).unwrap();
        assert!(begun.missing_blobs.is_empty());
    }

    #[test]
    fn delivery_blob_parent_sync_failure_prevents_commit_authority() {
        let root = TestRoot::new("blob-parent-sync");
        let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let manifest_digest = manifest.manifest_digest.clone();
        let begun = store.begin(manifest).unwrap();
        upload_missing(&mut store, &begun, &blobs);
        let prepared = store.prepare_commit(&begun.stage_id).unwrap();
        store.fail_blob_parent_sync_once();
        assert!(matches!(
            store.publish_commit(prepared),
            Err(DeliveryStoreError::Storage(_)),
        ));
        assert!(!store.commit_path(&manifest_digest).exists());
        assert!(!store.commits.contains_key(&manifest_digest));
        assert!(!store.completed.contains_key(&begun.stage_id));

        let prepared = store.prepare_commit(&begun.stage_id).unwrap();
        let receipt = store.publish_commit(prepared).unwrap();
        assert_eq!(receipt.manifest_digest, manifest_digest);
        assert!(store.commit_path(&manifest_digest).exists());
    }

    #[test]
    fn delivery_commit_parent_sync_failure_retries_exact_authority_and_reopens() {
        let root = TestRoot::new("commit-parent-sync");
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let manifest_digest = manifest.manifest_digest.clone();
        let (stage_id, expected_receipt) = {
            let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
            let begun = store.begin(manifest).unwrap();
            upload_missing(&mut store, &begun, &blobs);
            let prepared = store.prepare_commit(&begun.stage_id).unwrap();
            let expected_receipt = prepared.receipt.clone();
            store.fail_commit_parent_sync_once();
            assert!(matches!(
                store.publish_commit(prepared),
                Err(DeliveryStoreError::Storage(_)),
            ));
            assert!(store.commit_path(&manifest_digest).exists());
            assert!(!store.commits.contains_key(&manifest_digest));
            assert!(!store.completed.contains_key(&begun.stage_id));

            let prepared = store.prepare_commit(&begun.stage_id).unwrap();
            assert_eq!(store.publish_commit(prepared).unwrap(), expected_receipt);
            (begun.stage_id, expected_receipt)
        };

        let (mut reopened, bundles) = DeliveryStore::open(root.0.clone()).unwrap();
        assert_eq!(bundles.len(), 1);
        let prepared = reopened.prepare_commit(&stage_id).unwrap();
        assert_eq!(reopened.publish_commit(prepared).unwrap(), expected_receipt);
    }

    #[test]
    fn delivery_commit_temp_before_rename_is_cleaned_on_reopen() {
        let root = TestRoot::new("commit-temp-reopen");
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        {
            let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
            let begun = store.begin(manifest).unwrap();
            upload_missing(&mut store, &begun, &blobs);
            let prepared = store.prepare_commit(&begun.stage_id).unwrap();
            store.fail_commit_before_rename_once();
            assert!(matches!(
                store.publish_commit(prepared),
                Err(DeliveryStoreError::Storage(_)),
            ));
            let entries = fs::read_dir(root.0.join(COMMITS_DIRECTORY))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert_eq!(entries.len(), 1);
            assert!(is_owned_commit_temporary_name(&entries[0].file_name()));
        }

        let (reopened, bundles) = DeliveryStore::open(root.0.clone()).unwrap();
        assert!(bundles.is_empty());
        assert!(reopened.commits.is_empty());
        assert_eq!(fs::read_dir(root.0.join(COMMITS_DIRECTORY)).unwrap().count(), 0);
    }

    #[test]
    fn delivery_commit_malformed_temp_name_fails_closed_without_cleanup() {
        let root = TestRoot::new("commit-malformed-temp");
        let malformed = root
            .0
            .join(COMMITS_DIRECTORY)
            .join(format!(".tmp-{}", "0".repeat(31)));
        let (store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        drop(store);
        secure_create_file(&malformed, b"not a commit").unwrap();
        assert!(matches!(
            DeliveryStore::open(root.0.clone()),
            Err(DeliveryStoreError::Corrupt),
        ));
        assert!(malformed.exists());
    }

    #[cfg(unix)]
    #[test]
    fn delivery_commit_temp_symlink_fails_closed_without_cleanup() {
        let root = TestRoot::new("commit-temp-symlink");
        let target = root.0.join("symlink-target");
        let temporary = root
            .0
            .join(COMMITS_DIRECTORY)
            .join(format!(".tmp-{}", "0".repeat(32)));
        let (store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        drop(store);
        secure_create_file(&target, b"not a commit").unwrap();
        std::os::unix::fs::symlink(&target, &temporary).unwrap();
        assert!(matches!(
            DeliveryStore::open(root.0.clone()),
            Err(DeliveryStoreError::Storage(_)),
        ));
        assert!(fs::symlink_metadata(&temporary)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn delivery_commit_reloads_and_replays_exact_receipt_after_restart() {
        let root = TestRoot::new("restart");
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let (stage_id, receipt) = {
            let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
            let begun = store.begin(manifest.clone()).unwrap();
            upload_missing(&mut store, &begun, &blobs);
            let prepared = store.prepare_commit(&begun.stage_id).unwrap();
            let receipt = prepared.receipt.clone();
            store
                .persist_commit_authority_before_reply(&prepared)
                .unwrap();
            (begun.stage_id, receipt)
        };
        let (mut reloaded, bundles) = DeliveryStore::open(root.0.clone()).unwrap();
        assert_eq!(bundles.len(), 1);
        let prepared = reloaded.prepare_commit(&stage_id).unwrap();
        assert_eq!(reloaded.publish_commit(prepared).unwrap(), receipt);
        let replayed_begin = reloaded.begin(manifest).unwrap();
        assert_eq!(replayed_begin.stage_id, stage_id);
        assert!(replayed_begin.missing_blobs.is_empty());
    }

    #[test]
    fn delivery_catalog_rejects_same_content_with_different_manifest_metadata() {
        let (session_manifest, blobs) = skill_manifest("review-tools", "r1");
        let mut workspace_manifest = session_manifest.clone();
        for component in &mut workspace_manifest.components {
            component.scope = DeliveryScopeV2::Workspace;
        }
        workspace_manifest.manifest_digest = digest_delivery_manifest(&workspace_manifest);
        let first = NodeBundle::from_delivery(session_manifest, &blobs).unwrap();
        let second = NodeBundle::from_delivery(workspace_manifest, &blobs).unwrap();
        let mut catalog = BundleCatalog::new([first]).unwrap();
        assert!(matches!(
            catalog.insert_idempotent(second),
            Err(BundleCatalogError::Conflict { .. }),
        ));
    }

    struct NoSecrets;

    impl NodeSecretResolver for NoSecrets {
        fn resolve(
            &self,
            _: &crate::session_environment::NodeSecretReference,
        ) -> Result<NodeSecretValue, NodeSecretResolveError> {
            Err(NodeSecretResolveError::Unavailable)
        }
    }

    fn materialization_owner(instance: u64) -> MaterializationOwner {
        MaterializationOwner::Session {
            incarnation_id: NodeIncarnationId::from_bytes([9; crate::protocol::NODE_INCARNATION_ID_BYTES]),
            instance_id: AgentInstanceId(instance),
            generation: SessionGeneration(1),
        }
    }

    #[test]
    fn delivery_provider_layout_materializes_exact_claude_codex_and_kimi_trees() {
        let root = TestRoot::new("provider-layouts");
        let (manifest, blobs) = skill_manifest("review-tools", "r1");
        let bundle = NodeBundle::from_delivery(manifest, &blobs).unwrap();
        let materializer = SessionEnvironmentMaterializer::new(
            root.0.clone(),
            Arc::new(NoSecrets),
        )
        .unwrap();
        for (index, provider, expected_layout) in [
            (1, "claude", BundleProviderLayout::Claude),
            (2, "kimi", BundleProviderLayout::Kimi),
        ] {
            let layout = validate_bundle_binding(
                &AgentId::new(provider).unwrap(),
                SessionMode::Pty,
                &bundle,
            )
            .unwrap();
            assert_eq!(layout, expected_layout);
            let profile = NodeSessionMaterializationProfile::from_bundle(&bundle, layout).unwrap();
            let mut ownership = materializer
                .begin(
                    MaterializationId::new(format!("delivery-{provider}")).unwrap(),
                    None,
                    Some(bundle.receipt()),
                    None,
                    materialization_owner(index),
                    None,
                    &profile,
                    10,
                )
                .unwrap();
            materializer.materialize(&mut ownership, &profile, 11).unwrap();
            assert!(ownership.bundle_root().join("skills/review/SKILL.md").is_file());
            materializer.revalidate_bundle(&ownership, &bundle, layout).unwrap();
            ownership.mark_cleanup_required(12).unwrap();
            materializer.cleanup(&ownership).unwrap();
        }

        let codex_layout = validate_bundle_binding(
            &AgentId::new("codex").unwrap(),
            SessionMode::Pty,
            &bundle,
        )
        .unwrap();
        assert_eq!(codex_layout, BundleProviderLayout::Codex);
        let codex_base = NodeSessionMaterializationProfile::new(
            Vec::new(),
            vec![NodeSessionPathBinding::new(
                "CODEX_HOME",
                NodeSessionPathClass::ProviderHome,
            )
            .unwrap()],
            Vec::new(),
        )
        .unwrap();
        let codex_profile = codex_base.with_bundle(&bundle, codex_layout).unwrap();
        let mut codex = materializer
            .begin(
                MaterializationId::new("delivery-codex").unwrap(),
                None,
                Some(bundle.receipt()),
                None,
                materialization_owner(3),
                None,
                &codex_profile,
                20,
            )
            .unwrap();
        materializer.materialize(&mut codex, &codex_profile, 21).unwrap();
        assert!(codex.provider_home().join("skills/review/SKILL.md").is_file());
        assert!(!codex.bundle_root().join("plugin.json").exists());
        materializer
            .revalidate_bundle(&codex, &bundle, codex_layout)
            .unwrap();
        codex.mark_cleanup_required(22).unwrap();
        materializer.cleanup(&codex).unwrap();
    }

    #[test]
    fn delivery_mcp_declaration_materializes_at_claude_plugin_root_and_fails_closed_elsewhere() {
        let root = TestRoot::new("mcp-declaration");
        let mcp_bytes =
            br#"{"mcpServers":{"review":{"command":"review-mcp-server"}}}"#.to_vec();
        let (manifest, blobs) = manifest_with(
            "review-tools-mcp",
            "r1",
            vec![
                (
                    DeliveryComponentKindV2::PluginManifest,
                    DeliveryScopeV2::Session,
                    ".claude-plugin/plugin.json",
                    br#"{"name":"review-tools","description":"review tools","version":"1.0.0"}"#
                        .to_vec(),
                ),
                (
                    DeliveryComponentKindV2::PluginManifest,
                    DeliveryScopeV2::Session,
                    "plugin.json",
                    br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools"}"#
                        .to_vec(),
                ),
                (
                    DeliveryComponentKindV2::Skill,
                    DeliveryScopeV2::Session,
                    "skills/review/SKILL.md",
                    b"---\nname: review\ndescription: review changes\n---\nReview.\n".to_vec(),
                ),
                (
                    DeliveryComponentKindV2::McpDeclaration,
                    DeliveryScopeV2::Session,
                    ".mcp.json",
                    mcp_bytes.clone(),
                ),
            ],
        );
        let bundle = NodeBundle::from_delivery(manifest, &blobs).unwrap();

        // Kimi's --skills-dir and Codex's isolated CODEX_HOME profile expose
        // no grounded MCP-consumption contract reachable through Gate4Agent's
        // existing bundle wiring: fail closed at binding time rather than
        // silently staging the declaration as an inert file.
        assert_eq!(
            validate_bundle_binding(&AgentId::new("kimi").unwrap(), SessionMode::Pty, &bundle),
            Err(BundleProviderError::McpDeclarationUnsupported),
        );
        assert_eq!(
            validate_bundle_binding(&AgentId::new("codex").unwrap(), SessionMode::Pty, &bundle),
            Err(BundleProviderError::McpDeclarationUnsupported),
        );

        // Claude is grounded: the plugin root's .mcp.json materializes at the
        // exact directory the already-wired --plugin-dir argument points at.
        let layout = validate_bundle_binding(
            &AgentId::new("claude").unwrap(),
            SessionMode::Pty,
            &bundle,
        )
        .unwrap();
        assert_eq!(layout, BundleProviderLayout::Claude);

        let materializer = SessionEnvironmentMaterializer::new(
            root.0.clone(),
            Arc::new(NoSecrets),
        )
        .unwrap();
        let profile = NodeSessionMaterializationProfile::from_bundle(&bundle, layout).unwrap();
        let mut ownership = materializer
            .begin(
                MaterializationId::new("delivery-claude-mcp").unwrap(),
                None,
                Some(bundle.receipt()),
                None,
                materialization_owner(81),
                None,
                &profile,
                10,
            )
            .unwrap();
        materializer.materialize(&mut ownership, &profile, 11).unwrap();

        let arguments =
            bundle_launch_arguments(layout, ownership.bundle_root(), ownership.provider_home())
                .unwrap();
        assert_eq!(
            arguments,
            vec![
                OsString::from("--plugin-dir"),
                ownership.bundle_root().as_os_str().to_owned(),
            ],
        );

        let materialized_mcp_path = ownership.bundle_root().join(".mcp.json");
        assert!(materialized_mcp_path.is_file());
        assert_eq!(fs::read(&materialized_mcp_path).unwrap(), mcp_bytes);
        assert!(ownership.bundle_root().join("skills/review/SKILL.md").is_file());

        materializer.revalidate_bundle(&ownership, &bundle, layout).unwrap();
        ownership.mark_cleanup_required(12).unwrap();
        materializer.cleanup(&ownership).unwrap();
    }

    #[test]
    fn delivery_restarted_large_bundle_validates_claude_and_kimi_spawn_binding() {
        let root = TestRoot::new("restarted-large-binding");
        let mut skill = b"---\nname: review-code\ndescription: Controlled H3A review fixture.\n---\n\n"
            .to_vec();
        skill.resize(300 * 1024 + 17, b'x');
        let (manifest, blobs) = manifest_with(
            "bundle.h3a-review",
            "revision-1",
            vec![
                (
                    DeliveryComponentKindV2::PluginManifest,
                    DeliveryScopeV2::Session,
                    "plugin.json",
                    br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"h3a-review"}"#.to_vec(),
                ),
                (
                    DeliveryComponentKindV2::PluginManifest,
                    DeliveryScopeV2::Session,
                    ".claude-plugin/plugin.json",
                    br#"{"name":"h3a-review","description":"controlled fixture","version":"1.0.0"}"#.to_vec(),
                ),
                (
                    DeliveryComponentKindV2::Skill,
                    DeliveryScopeV2::Session,
                    "skills/review-code/SKILL.md",
                    skill,
                ),
            ],
        );
        {
            let (mut store, _) = DeliveryStore::open(root.0.clone()).unwrap();
            let begun = store.begin(manifest).unwrap();
            upload_missing(&mut store, &begun, &blobs);
            let prepared = store.prepare_commit(&begun.stage_id).unwrap();
            store.publish_commit(prepared).unwrap();
        }
        let (_reopened, bundles) = DeliveryStore::open(root.0.clone()).unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(
            validate_bundle_binding(
                &AgentId::new("claude").unwrap(),
                SessionMode::Pty,
                &bundles[0],
            )
            .unwrap(),
            BundleProviderLayout::Claude,
        );
        assert_eq!(
            validate_bundle_binding(
                &AgentId::new("kimi").unwrap(),
                SessionMode::Pty,
                &bundles[0],
            )
            .unwrap(),
            BundleProviderLayout::Kimi,
        );
        let bundle = &bundles[0];
        let materializer = SessionEnvironmentMaterializer::new(
            root.0.join("materialized"),
            Arc::new(NoSecrets),
        )
        .unwrap();
        let profile = NodeSessionMaterializationProfile::from_bundle(
            bundle,
            BundleProviderLayout::Kimi,
        )
        .unwrap();
        let mut ownership = materializer
            .begin(
                MaterializationId::new("restarted-kimi-spawn").unwrap(),
                None,
                Some(bundle.receipt()),
                None,
                materialization_owner(71),
                None,
                &profile,
                10,
            )
            .unwrap();
        materializer.materialize(&mut ownership, &profile, 11).unwrap();
        materializer
            .revalidate_bundle(&ownership, bundle, BundleProviderLayout::Kimi)
            .unwrap();
        let arguments = bundle_launch_arguments(
            BundleProviderLayout::Kimi,
            ownership.bundle_root(),
            ownership.provider_home(),
        )
        .unwrap();
        let overlay = gate4agent_runtime_native::NativeInstanceLaunchOverlay::new(
            AgentId::new("kimi").unwrap(),
            TransportKind::Pty,
            Vec::new(),
            arguments,
        )
        .unwrap();
        let catalog = super::super::active_registry().unwrap();
        let (_handle, runtime) = gate4agent_runtime_native::NativeRuntime::new(
            catalog,
            gate4agent_runtime_native::NativeRuntimeConfig::default(),
        );
        runtime
            .native_launch_profile_control()
            .install_native_instance_launch_overlay(AgentInstanceId(71), overlay)
            .unwrap();

        let catalog = super::super::active_registry().unwrap();
        let (handle, spawn_runtime) = gate4agent_runtime_native::NativeRuntime::new(
            catalog,
            gate4agent_runtime_native::NativeRuntimeConfig::default(),
        );
        let workspace_id = crate::protocol::WorkspaceId::new("delivery-spawn").unwrap();
        let node_id = crate::protocol::NodeId::new("delivery-spawn-node").unwrap();
        let workspace = super::super::WorkspaceConfig::new(
            workspace_id.clone(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let mut shared = super::super::NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            node_id.clone(),
            vec![workspace],
            vec![AgentId::new("kimi").unwrap()],
        );
        let profile_id = crate::protocol::SpawnProfileId::new("kimi-delivery").unwrap();
        shared.spawn_profiles = super::super::SpawnProfileRegistry::new([
            crate::protocol::SpawnProfileDefaults {
                profile_id: profile_id.clone(),
                revision: crate::protocol::SpawnProfileRevision::new("r1").unwrap(),
                provider: AgentId::new("kimi").unwrap(),
                mode: SessionMode::Pty,
                terminal_size: gate4agent_types::TerminalSize {
                    rows: 24,
                    columns: 80,
                },
                prompt: None,
                bundle_id: None,
                context_id: None,
                environment_profile_id: None,
            },
        ])
        .unwrap();
        shared.bundle_catalog = std::sync::RwLock::new(
            BundleCatalog::new([bundle.clone()]).unwrap(),
        );
        shared.session_environment_materializer = Some(
            SessionEnvironmentMaterializer::new(
                root.0.join("spawn-materialized"),
                Arc::new(NoSecrets),
            )
            .unwrap(),
        );
        shared.native_launch_profile_control = Some(spawn_runtime.native_launch_profile_control());
        let spec = crate::protocol::SpawnSpec {
            target: crate::protocol::SpawnTarget {
                node_id,
                workspace_id,
                worktree_id: None,
            },
            profile_id,
            expected_profile_revision: crate::protocol::SpawnProfileRevision::new("r1").unwrap(),
            overrides: crate::protocol::SpawnOverrides {
                provider: crate::protocol::SpawnOverride::Set {
                    value: AgentId::new("kimi").unwrap(),
                },
                mode: crate::protocol::SpawnOverride::Set {
                    value: SessionMode::Pty,
                },
                terminal_size: crate::protocol::SpawnOverride::Inherit,
                prompt: crate::protocol::SpawnOverride::Clear,
                bundle_id: crate::protocol::SpawnOverride::Set {
                    value: bundle.id().clone(),
                },
                context_id: crate::protocol::SpawnOverride::Clear,
                environment_profile_id: crate::protocol::SpawnOverride::Clear,
            },
            deadline_ms: crate::protocol::SpawnDeadlineMs::new(30_000).unwrap(),
            idempotency_key: crate::protocol::SpawnIdempotencyKey::new(
                "restarted-delivery-spawn",
            )
            .unwrap(),
            required_capabilities: crate::protocol::SpawnRequiredCapabilities::default(),
        };
        let resolved = shared.resolve_spawn_spec(&spec).unwrap();
        let receipt = shared.resolve_bundle(&resolved, None).unwrap().unwrap();
        assert_eq!(receipt, bundle.receipt());
        let address = crate::protocol::SessionAddress {
            workspace_id: resolved.target.workspace_id.clone(),
            session: crate::protocol::SessionKey {
                instance_id: AgentInstanceId(72),
                generation: SessionGeneration(1),
            },
        };
        let (prepared_overlay, _materialization) = shared
            .prepare_session_materialization(
                &address,
                &resolved.provider,
                resolved.mode,
                None,
                Some(&receipt),
                None,
                None,
            )
            .unwrap()
            .unwrap();
        let overlay = prepared_overlay.unwrap();
        let _overlay_guard = shared
            .install_prepared_launch_overlay(address.session.instance_id, overlay)
            .unwrap();
    }

    #[test]
    fn delivery_provider_layout_rejects_unsupported_component_before_materialization() {
        let root = TestRoot::new("unsupported-layout");
        let (mut manifest, blobs) = skill_manifest("review-tools", "r1");
        manifest.components[2].kind = DeliveryComponentKindV2::Prompt;
        manifest.manifest_digest = digest_delivery_manifest(&manifest);
        let bundle = NodeBundle::from_delivery(manifest, &blobs).unwrap();
        assert!(validate_bundle_binding(
            &AgentId::new("claude").unwrap(),
            SessionMode::Pty,
            &bundle,
        )
        .is_err());
        assert!(validate_bundle_binding(
            &AgentId::new("claude").unwrap(),
            SessionMode::Inline,
            &bundle,
        )
        .is_err());
        let (mut workspace_manifest, workspace_blobs) =
            skill_manifest("workspace-review-tools", "r1");
        for component in &mut workspace_manifest.components {
            component.scope = DeliveryScopeV2::Workspace;
        }
        workspace_manifest.manifest_digest = digest_delivery_manifest(&workspace_manifest);
        let workspace_bundle =
            NodeBundle::from_delivery(workspace_manifest, &workspace_blobs).unwrap();
        assert!(validate_bundle_binding(
            &AgentId::new("kimi").unwrap(),
            SessionMode::Pty,
            &workspace_bundle,
        )
        .is_err());
        assert!(!root.0.exists());
    }

    #[test]
    fn delivery_store_reload_removes_unreferenced_atomic_publish_blobs() {
        let root = TestRoot::new("orphan");
        {
            let (store, _) = DeliveryStore::open(root.0.clone()).unwrap();
            let bytes = b"orphaned-before-commit-authority";
            let digest = digest_delivery_blob(bytes);
            store.publish_blob(&digest, bytes).unwrap();
            assert!(store.blob_path(&digest).is_file());
        }
        let (_reloaded, bundles) = DeliveryStore::open(root.0.clone()).unwrap();
        assert!(bundles.is_empty());
        assert!(fs::read_dir(root.0.join(BLOBS_DIRECTORY))
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn delivery_store_reload_rejects_oversized_commit_directory_before_reading_records() {
        let root = TestRoot::new("oversized-directory");
        {
            let (_store, _) = DeliveryStore::open(root.0.clone()).unwrap();
            for index in 0..=MAX_BUNDLE_CATALOG_ENTRIES {
                secure_create_file(
                    &root
                        .0
                        .join(COMMITS_DIRECTORY)
                        .join(format!("{index:064x}.json")),
                    b"not-read-before-bound",
                )
                .unwrap();
            }
        }
        assert!(matches!(
            DeliveryStore::open(root.0.clone()),
            Err(DeliveryStoreError::Corrupt),
        ));
    }

    #[tokio::test]
    async fn delivery_request_path_cleanup_failure_returns_receipt_and_publishes_catalog() {
        let root = TestRoot::new("request-path");
        let catalog = super::super::active_registry().unwrap();
        let (handle, runtime) = gate4agent_runtime_native::NativeRuntime::new(
            catalog,
            gate4agent_runtime_native::NativeRuntimeConfig::default(),
        );
        let workspace = super::super::WorkspaceConfig::new(
            crate::protocol::WorkspaceId::new("delivery-request").unwrap(),
            std::env::current_dir().unwrap(),
        )
        .unwrap();
        let shared = super::super::NodeShared::new(
            handle,
            "fixture-token".to_owned(),
            crate::protocol::NodeId::new("delivery-node").unwrap(),
            vec![workspace],
            vec![AgentId::new("claude").unwrap()],
        );
        assert!(!super::super::node_compatibility_support(&shared)
            .unwrap()
            .capabilities
            .iter()
            .any(|capability| {
                capability.as_str()
                    == crate::protocol::NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY
            }));
        let (store, _) = DeliveryStore::open(root.0.clone()).unwrap();
        *shared
            .delivery_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(store);
        assert!(super::super::node_compatibility_support(&shared)
            .unwrap()
            .capabilities
            .iter()
            .any(|capability| {
                capability.as_str()
                    == crate::protocol::NODE_DELIVERY_BUNDLE_V2_STAGE_COMMIT_CAPABILITY
            }));

        let (manifest, blobs) = skill_manifest("request-tools", "r1");
        let uncontrolled = super::super::process_request_inner(
            &shared,
            71,
            crate::protocol::ClientRole::Operator,
            crate::protocol::NodeRequest::BeginDeliveryStage {
                manifest: manifest.clone(),
            },
        )
        .await
        .unwrap_err();
        assert_eq!(uncontrolled.code, crate::protocol::NodeFailureCode::ControllerRequired);
        shared
            .acquire_controller(
                71,
                crate::protocol::ClientRole::Operator,
                crate::protocol::DEFAULT_CONTROLLER_LEASE_MS,
            )
            .unwrap();
        let begun = super::super::process_request_inner(
            &shared,
            71,
            crate::protocol::ClientRole::Operator,
            crate::protocol::NodeRequest::BeginDeliveryStage { manifest },
        )
        .await
        .unwrap();
        let crate::protocol::NodeResponse::DeliveryStageBegun {
            stage_id,
            missing_blobs,
            ..
        } = begun
        else {
            panic!("delivery begin returned another response");
        };
        for digest in missing_blobs {
            let bytes = &blobs[&digest];
            super::super::process_request_inner(
                &shared,
                71,
                crate::protocol::ClientRole::Operator,
                crate::protocol::NodeRequest::PutDeliveryBlobChunk {
                    stage_id: stage_id.clone(),
                    blob_digest: digest,
                    offset: 0,
                    chunk_hex: encode_chunk(bytes),
                },
            )
            .await
                .unwrap();
        }
        shared
            .delivery_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .unwrap()
            .fail_stage_cleanup_once();
        let committed = super::super::process_request_inner(
            &shared,
            71,
            crate::protocol::ClientRole::Operator,
            crate::protocol::NodeRequest::CommitDeliveryStage {
                stage_id: stage_id.clone(),
            },
        )
        .await
        .unwrap();
        let crate::protocol::NodeResponse::DeliveryCommitted { receipt } = committed else {
            panic!("delivery commit returned another response");
        };
        assert_eq!(receipt.bundle_id.as_str(), "request-tools");
        assert!(shared
            .bundle_catalog
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&receipt.bundle_id)
            .is_some());
        assert!(root
            .0
            .join(STAGES_DIRECTORY)
            .join(stage_id.as_str())
            .is_dir());
        let replayed = super::super::process_request_inner(
            &shared,
            71,
            crate::protocol::ClientRole::Operator,
            crate::protocol::NodeRequest::CommitDeliveryStage {
                stage_id: stage_id.clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            replayed,
            crate::protocol::NodeResponse::DeliveryCommitted {
                receipt: receipt.clone(),
            },
        );
        let store = shared
            .delivery_store
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        drop(store);
        let (mut reopened, bundles) = DeliveryStore::open(root.0.clone()).unwrap();
        assert_eq!(bundles.len(), 1);
        assert!(!root
            .0
            .join(STAGES_DIRECTORY)
            .join(stage_id.as_str())
            .exists());
        let prepared = reopened.prepare_commit(&stage_id).unwrap();
        assert_eq!(reopened.publish_commit(prepared).unwrap(), receipt);
        drop(shared);
        drop(runtime);
    }
}

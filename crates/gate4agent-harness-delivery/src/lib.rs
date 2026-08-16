//! Pure, reviewed-source compiler for staged Gate4Agent delivery bundles.

use gate4agent_node_protocol::{
    DeliveryBlobDigestV1, DeliveryBlobReceiptV1, DeliveryBundleManifestV2,
    DeliveryComponentKindV2, DeliveryComponentV2, DeliveryManifestDigestV2,
    DeliveryRelativePathV2, DeliveryScopeV2, SpawnBundleDigest, SpawnBundleId,
    SpawnBundleRevision, MAX_DELIVERY_FILES, MAX_DELIVERY_FILE_BYTES,
    MAX_DELIVERY_TOTAL_BYTES, MAX_LAUNCH_BUNDLES,
};
use ring::digest::{Context, SHA256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

const BUNDLE_DIGEST_DOMAIN: &[u8] = b"g4a-bundle-v1\0";

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewedDeliverySourceV2 {
    root: PathBuf,
    mount_path: Option<DeliveryRelativePathV2>,
    kind: DeliveryComponentKindV2,
    scope: DeliveryScopeV2,
}

impl ReviewedDeliverySourceV2 {
    pub fn new(
        root: impl Into<PathBuf>,
        mount_path: Option<DeliveryRelativePathV2>,
        kind: DeliveryComponentKindV2,
        scope: DeliveryScopeV2,
    ) -> Result<Self, DeliveryCompileError> {
        let root = root.into();
        if !root.is_absolute() {
            return Err(DeliveryCompileError::SourceRootNotAbsolute(root));
        }
        if kind == DeliveryComponentKindV2::McpDeclaration {
            return Err(DeliveryCompileError::McpDeclarationReserved);
        }
        Ok(Self {
            root,
            mount_path,
            kind,
            scope,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn mount_path(&self) -> Option<&DeliveryRelativePathV2> {
        self.mount_path.as_ref()
    }

    pub fn kind(&self) -> DeliveryComponentKindV2 {
        self.kind
    }

    pub fn scope(&self) -> DeliveryScopeV2 {
        self.scope
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct CompiledDeliveryBlobV1 {
    receipt: DeliveryBlobReceiptV1,
    bytes: Vec<u8>,
}

impl std::fmt::Debug for CompiledDeliveryBlobV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CompiledDeliveryBlobV1")
            .field("receipt", &self.receipt)
            .field("byte_len", &self.bytes.len())
            .finish()
    }
}

impl CompiledDeliveryBlobV1 {
    pub fn receipt(&self) -> &DeliveryBlobReceiptV1 {
        &self.receipt
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompiledDeliveryBundleV2 {
    manifest: DeliveryBundleManifestV2,
    blobs: Vec<CompiledDeliveryBlobV1>,
}

impl CompiledDeliveryBundleV2 {
    pub fn manifest(&self) -> &DeliveryBundleManifestV2 {
        &self.manifest
    }

    pub fn blobs(&self) -> &[CompiledDeliveryBlobV1] {
        &self.blobs
    }

    pub fn verify(&self) -> Result<(), DeliveryCompileError> {
        self.manifest
            .validate()
            .map_err(|error| DeliveryCompileError::InvalidContract(error.to_string()))?;
        if manifest_digest(&self.manifest) != self.manifest.manifest_digest {
            return Err(DeliveryCompileError::ManifestDigestMismatch);
        }

        let mut by_digest = BTreeMap::new();
        for blob in &self.blobs {
            if blob.receipt.byte_len != blob.bytes.len() as u64
                || blob.receipt.digest != blob_digest(&blob.bytes)
            {
                return Err(DeliveryCompileError::BlobDigestMismatch(
                    blob.receipt.digest.clone(),
                ));
            }
            if by_digest.insert(blob.receipt.digest.clone(), blob.bytes.as_slice()).is_some() {
                return Err(DeliveryCompileError::DuplicateBlob(
                    blob.receipt.digest.clone(),
                ));
            }
        }

        let mut files = Vec::with_capacity(self.manifest.components.len());
        for component in &self.manifest.components {
            let bytes = by_digest
                .get(&component.blob.digest)
                .ok_or_else(|| DeliveryCompileError::MissingBlob(component.blob.digest.clone()))?;
            if component.blob.byte_len != bytes.len() as u64 {
                return Err(DeliveryCompileError::BlobLengthMismatch(
                    component.blob.digest.clone(),
                ));
            }
            files.push((component.relative_path.as_str(), *bytes));
        }
        if bundle_digest(&files) != self.manifest.bundle_digest {
            return Err(DeliveryCompileError::BundleDigestMismatch);
        }
        Ok(())
    }
}

pub fn compile_reviewed_delivery_bundle_v2(
    bundle_id: SpawnBundleId,
    revision: SpawnBundleRevision,
    sources: &[ReviewedDeliverySourceV2],
) -> Result<CompiledDeliveryBundleV2, DeliveryCompileError> {
    if sources.is_empty() {
        return Err(DeliveryCompileError::NoReviewedSources);
    }

    let mut files = Vec::new();
    let mut captured_total_bytes = 0_usize;
    for source in sources {
        scan_source(source, &mut files, &mut captured_total_bytes)?;
    }
    files.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    if files.is_empty() {
        return Err(DeliveryCompileError::NoFiles);
    }
    if files.len() > MAX_DELIVERY_FILES {
        return Err(DeliveryCompileError::TooManyFiles);
    }

    let mut folded_paths = BTreeSet::new();
    let mut total_bytes = 0_usize;
    let mut components = Vec::with_capacity(files.len());
    let mut blobs_by_digest = BTreeMap::new();
    for file in &files {
        let folded = file.relative_path.as_str().to_lowercase();
        if !folded_paths.insert(folded) {
            return Err(DeliveryCompileError::CaseFoldPathCollision(
                file.relative_path.as_str().to_owned(),
            ));
        }
        total_bytes = total_bytes
            .checked_add(file.bytes.len())
            .ok_or(DeliveryCompileError::TotalTooLarge)?;
        if total_bytes > MAX_DELIVERY_TOTAL_BYTES {
            return Err(DeliveryCompileError::TotalTooLarge);
        }
        let receipt = DeliveryBlobReceiptV1::new(
            blob_digest(&file.bytes),
            file.bytes.len() as u64,
        )
        .map_err(|error| DeliveryCompileError::InvalidContract(error.to_string()))?;
        blobs_by_digest
            .entry(receipt.digest.clone())
            .or_insert_with(|| CompiledDeliveryBlobV1 {
                receipt: receipt.clone(),
                bytes: file.bytes.clone(),
            });
        components.push(DeliveryComponentV2 {
            kind: file.kind,
            scope: file.scope,
            relative_path: file.relative_path.clone(),
            blob: receipt,
        });
    }

    let digest_files = files
        .iter()
        .map(|file| (file.relative_path.as_str(), file.bytes.as_slice()))
        .collect::<Vec<_>>();
    let mut manifest = DeliveryBundleManifestV2 {
        bundle_id,
        revision,
        bundle_digest: bundle_digest(&digest_files),
        manifest_digest: DeliveryManifestDigestV2::new(format!("sha256:{}", "0".repeat(64)))
            .expect("fixed placeholder digest is valid"),
        components,
    };
    manifest.manifest_digest = manifest_digest(&manifest);
    manifest
        .validate()
        .map_err(|error| DeliveryCompileError::InvalidContract(error.to_string()))?;

    let compiled = CompiledDeliveryBundleV2 {
        manifest,
        blobs: blobs_by_digest.into_values().collect(),
    };
    compiled.verify()?;
    Ok(compiled)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeliveryCatalogV2 {
    bundles: BTreeMap<SpawnBundleId, CompiledDeliveryBundleV2>,
}

impl DeliveryCatalogV2 {
    pub fn new(
        bundles: impl IntoIterator<Item = CompiledDeliveryBundleV2>,
    ) -> Result<Self, DeliveryCatalogError> {
        let mut catalog = Self::default();
        for bundle in bundles {
            catalog.insert(bundle)?;
        }
        Ok(catalog)
    }

    pub fn insert(
        &mut self,
        bundle: CompiledDeliveryBundleV2,
    ) -> Result<(), DeliveryCatalogError> {
        bundle
            .verify()
            .map_err(DeliveryCatalogError::InvalidBundle)?;
        let id = bundle.manifest.bundle_id.clone();
        if self.bundles.contains_key(&id) {
            return Err(DeliveryCatalogError::DuplicateBundle(id));
        }
        if self.bundles.len() == MAX_LAUNCH_BUNDLES {
            return Err(DeliveryCatalogError::CatalogFull);
        }
        self.bundles.insert(id, bundle);
        Ok(())
    }

    pub fn get(&self, id: &SpawnBundleId) -> Option<&CompiledDeliveryBundleV2> {
        self.bundles.get(id)
    }

    pub fn iter(
        &self,
    ) -> impl ExactSizeIterator<Item = (&SpawnBundleId, &CompiledDeliveryBundleV2)> {
        self.bundles.iter()
    }

    pub fn len(&self) -> usize {
        self.bundles.len()
    }

    pub fn is_empty(&self) -> bool {
        self.bundles.is_empty()
    }
}

#[derive(Debug, Error)]
pub enum DeliveryCatalogError {
    #[error("delivery catalog contains duplicate bundle {0}")]
    DuplicateBundle(SpawnBundleId),
    #[error("delivery catalog exceeds the bundle count limit")]
    CatalogFull,
    #[error("delivery catalog rejected an invalid bundle: {0}")]
    InvalidBundle(DeliveryCompileError),
}

#[derive(Debug, Error)]
pub enum DeliveryCompileError {
    #[error("reviewed delivery source root is not absolute: {0}")]
    SourceRootNotAbsolute(PathBuf),
    #[error("at least one explicit reviewed delivery source is required")]
    NoReviewedSources,
    #[error("reviewed delivery sources contain no files")]
    NoFiles,
    #[error("MCP declarations are reserved for fixed H3B adapters and cannot be source input")]
    McpDeclarationReserved,
    #[error("delivery source is a symlink or reparse point: {0}")]
    LinkOrReparse(PathBuf),
    #[error("delivery source is not a regular file or directory: {0}")]
    SpecialFile(PathBuf),
    #[error("delivery source includes a forbidden root or path component: {0}")]
    ForbiddenRoot(PathBuf),
    #[error("delivery source resolves outside its protected root: {0}")]
    EscapedRoot(PathBuf),
    #[error("delivery source changed while it was being captured: {0}")]
    SourceChanged(PathBuf),
    #[error("delivery source is not protected from untrusted writes: {0}")]
    InsecurePermissions(PathBuf),
    #[error("delivery source contains an executable payload: {0}")]
    Executable(PathBuf),
    #[error("delivery source path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    #[error("delivery source file exceeds the byte limit: {0}")]
    FileTooLarge(PathBuf),
    #[error("delivery bundle exceeds the file count limit")]
    TooManyFiles,
    #[error("delivery bundle exceeds the total byte limit")]
    TotalTooLarge,
    #[error("delivery paths collide under case folding: {0}")]
    CaseFoldPathCollision(String),
    #[error("delivery source I/O failed at {path}: {source}")]
    Io { path: PathBuf, source: io::Error },
    #[error("delivery contract validation failed: {0}")]
    InvalidContract(String),
    #[error("delivery blob digest mismatch for {0}")]
    BlobDigestMismatch(DeliveryBlobDigestV1),
    #[error("delivery bundle has duplicate blob {0}")]
    DuplicateBlob(DeliveryBlobDigestV1),
    #[error("delivery bundle is missing blob {0}")]
    MissingBlob(DeliveryBlobDigestV1),
    #[error("delivery blob length mismatch for {0}")]
    BlobLengthMismatch(DeliveryBlobDigestV1),
    #[error("delivery bundle digest mismatch")]
    BundleDigestMismatch,
    #[error("delivery manifest digest mismatch")]
    ManifestDigestMismatch,
}

#[derive(Clone)]
struct CapturedFile {
    relative_path: DeliveryRelativePathV2,
    kind: DeliveryComponentKindV2,
    scope: DeliveryScopeV2,
    bytes: Vec<u8>,
}

fn scan_source(
    source: &ReviewedDeliverySourceV2,
    files: &mut Vec<CapturedFile>,
    total_bytes: &mut usize,
) -> Result<(), DeliveryCompileError> {
    validate_path_components(&source.root)?;
    let protected = ProtectedSourceRoot::open(&source.root)?;
    if protected.opened.metadata.is_file() {
        let file_name = source
            .root()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| DeliveryCompileError::NonUtf8Path(source.root.clone()))?;
        let relative_path = join_delivery_path(source.mount_path.as_ref(), file_name)?;
        capture_file(
            source,
            protected.containment_root(),
            protected.root(),
            relative_path,
            files,
            total_bytes,
        )?;
    } else {
        scan_directory(
            source,
            protected.containment_root(),
            protected.root(),
            Path::new(""),
            files,
            total_bytes,
        )?;
    }
    protected.verify_stable()
}

fn scan_directory(
    source: &ReviewedDeliverySourceV2,
    canonical_root: &Path,
    directory: &Path,
    relative: &Path,
    files: &mut Vec<CapturedFile>,
    total_bytes: &mut usize,
) -> Result<(), DeliveryCompileError> {
    let opened_directory = open_verified_path(canonical_root, directory, true)?;
    let mut entries = fs::read_dir(directory)
        .map_err(|source_error| DeliveryCompileError::Io {
            path: directory.to_owned(),
            source: source_error,
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source_error| DeliveryCompileError::Io {
            path: directory.to_owned(),
            source: source_error,
        })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        validate_path_components(&path)?;
        let file_name = entry
            .file_name()
            .into_string()
            .map_err(|_| DeliveryCompileError::NonUtf8Path(path.clone()))?;
        let child_relative = relative.join(file_name);
        let metadata = fs::symlink_metadata(&path).map_err(|source_error| {
            DeliveryCompileError::Io {
                path: path.clone(),
                source: source_error,
            }
        })?;
        validate_entry_type(&path, &metadata)?;
        if metadata.is_dir() {
            scan_directory(
                source,
                canonical_root,
                &path,
                &child_relative,
                files,
                total_bytes,
            )?;
        } else {
            let relative_text = path_to_forward_slashes(&child_relative)?;
            let relative_path = join_delivery_path(source.mount_path.as_ref(), &relative_text)?;
            capture_file(
                source,
                canonical_root,
                &path,
                relative_path,
                files,
                total_bytes,
            )?;
        }
    }
    verify_opened_path(canonical_root, directory, &opened_directory)
}

fn capture_file(
    source: &ReviewedDeliverySourceV2,
    canonical_root: &Path,
    path: &Path,
    relative_path: DeliveryRelativePathV2,
    files: &mut Vec<CapturedFile>,
    total_bytes: &mut usize,
) -> Result<(), DeliveryCompileError> {
    if files.len() == MAX_DELIVERY_FILES {
        return Err(DeliveryCompileError::TooManyFiles);
    }
    let captured = read_regular_no_follow(canonical_root, path, *total_bytes)?;
    let bytes = captured.bytes;
    *total_bytes = total_bytes
        .checked_add(bytes.len())
        .ok_or(DeliveryCompileError::TotalTooLarge)?;
    if *total_bytes > MAX_DELIVERY_TOTAL_BYTES {
        return Err(DeliveryCompileError::TotalTooLarge);
    }
    if is_executable(&captured.metadata, relative_path.as_str(), &bytes) {
        return Err(DeliveryCompileError::Executable(path.to_owned()));
    }
    if matches!(
        source.kind,
        DeliveryComponentKindV2::Skill
            | DeliveryComponentKindV2::PluginManifest
            | DeliveryComponentKindV2::Prompt
            | DeliveryComponentKindV2::Instructions
            | DeliveryComponentKindV2::AgentDefinition
            | DeliveryComponentKindV2::Command
    ) && std::str::from_utf8(&bytes).is_err()
    {
        return Err(DeliveryCompileError::InvalidContract(format!(
            "text component {} is not UTF-8",
            relative_path.as_str(),
        )));
    }
    files.push(CapturedFile {
        relative_path,
        kind: source.kind,
        scope: source.scope,
        bytes,
    });
    Ok(())
}

struct ProtectedSourceRoot {
    canonical: PathBuf,
    containment: PathBuf,
    opened: OpenedPath,
}

impl ProtectedSourceRoot {
    fn open(root: &Path) -> Result<Self, DeliveryCompileError> {
        if !root.is_absolute()
            || root.components().any(|component| {
                matches!(component, Component::CurDir | Component::ParentDir)
            })
        {
            return Err(DeliveryCompileError::SourceRootNotAbsolute(root.to_owned()));
        }
        reject_reparse_ancestors(root)?;
        let canonical = fs::canonicalize(root).map_err(|source| DeliveryCompileError::Io {
            path: root.to_owned(),
            source,
        })?;
        reject_reparse_ancestors(&canonical)?;
        let root_metadata = fs::symlink_metadata(&canonical).map_err(|source| {
            DeliveryCompileError::Io {
                path: canonical.clone(),
                source,
            }
        })?;
        validate_entry_type(&canonical, &root_metadata)?;
        let containment = if root_metadata.is_dir() {
            canonical.clone()
        } else {
            canonical
                .parent()
                .ok_or_else(|| DeliveryCompileError::EscapedRoot(canonical.clone()))?
                .to_owned()
        };
        let opened = open_verified_path(&containment, &canonical, root_metadata.is_dir())?;
        let protected = Self {
            canonical,
            containment,
            opened,
        };
        protected.verify_stable()?;
        Ok(protected)
    }

    fn root(&self) -> &Path {
        &self.canonical
    }

    fn containment_root(&self) -> &Path {
        &self.containment
    }

    fn verify_stable(&self) -> Result<(), DeliveryCompileError> {
        verify_opened_path(
            &self.containment,
            &self.canonical,
            &self.opened,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PathFingerprint {
    identity: PathIdentity,
    byte_len: u64,
    modified: Option<std::time::SystemTime>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathIdentity {
    first: u64,
    second: u64,
}

struct OpenedPath {
    file: File,
    metadata: fs::Metadata,
    fingerprint: PathFingerprint,
}

fn reject_reparse_ancestors(path: &Path) -> Result<(), DeliveryCompileError> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|source| {
            DeliveryCompileError::Io {
                path: ancestor.to_owned(),
                source,
            }
        })?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(DeliveryCompileError::LinkOrReparse(ancestor.to_owned()));
        }
    }
    Ok(())
}

fn open_verified_path(
    canonical_root: &Path,
    path: &Path,
    directory: bool,
) -> Result<OpenedPath, DeliveryCompileError> {
    let path_metadata = fs::symlink_metadata(path).map_err(|source| {
        DeliveryCompileError::Io {
            path: path.to_owned(),
            source,
        }
    })?;
    if path_metadata.file_type().is_symlink() || is_reparse_point(&path_metadata) {
        return Err(DeliveryCompileError::LinkOrReparse(path.to_owned()));
    }
    let file = open_path_no_follow(path, directory).map_err(|source| {
        DeliveryCompileError::Io {
            path: path.to_owned(),
            source,
        }
    })?;
    let metadata = file.metadata().map_err(|source| DeliveryCompileError::Io {
        path: path.to_owned(),
        source,
    })?;
    if metadata.file_type().is_symlink()
        || is_reparse_point(&metadata)
        || metadata.is_dir() != directory
        || metadata.is_file() == directory
    {
        return Err(DeliveryCompileError::SpecialFile(path.to_owned()));
    }
    validate_source_permissions(&file, &metadata, path)?;
    let final_path = fs::canonicalize(path).map_err(|source| DeliveryCompileError::Io {
        path: path.to_owned(),
        source,
    })?;
    if final_path != canonical_root && !final_path.starts_with(canonical_root) {
        return Err(DeliveryCompileError::EscapedRoot(path.to_owned()));
    }
    let identity = path_identity(&file, &metadata).map_err(|source| {
        DeliveryCompileError::Io {
            path: path.to_owned(),
            source,
        }
    })?;
    let fingerprint = PathFingerprint {
        identity,
        byte_len: metadata.len(),
        modified: metadata.modified().ok(),
    };
    Ok(OpenedPath {
        file,
        metadata,
        fingerprint,
    })
}

fn verify_opened_path(
    canonical_root: &Path,
    path: &Path,
    original: &OpenedPath,
) -> Result<(), DeliveryCompileError> {
    let current = open_verified_path(canonical_root, path, original.metadata.is_dir())?;
    if current.fingerprint != original.fingerprint {
        return Err(DeliveryCompileError::SourceChanged(path.to_owned()));
    }
    Ok(())
}

struct CapturedBytes {
    bytes: Vec<u8>,
    metadata: fs::Metadata,
}

fn read_regular_no_follow(
    canonical_root: &Path,
    path: &Path,
    current_total: usize,
) -> Result<CapturedBytes, DeliveryCompileError> {
    let mut opened = open_verified_path(canonical_root, path, false)?;
    if opened.metadata.len() > MAX_DELIVERY_FILE_BYTES as u64 {
        return Err(DeliveryCompileError::FileTooLarge(path.to_owned()));
    }
    let declared_len = usize::try_from(opened.metadata.len())
        .map_err(|_| DeliveryCompileError::FileTooLarge(path.to_owned()))?;
    if current_total
        .checked_add(declared_len)
        .is_none_or(|total| total > MAX_DELIVERY_TOTAL_BYTES)
    {
        return Err(DeliveryCompileError::TotalTooLarge);
    }
    let mut bytes = Vec::with_capacity(declared_len);
    (&mut opened.file)
        .take(MAX_DELIVERY_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| DeliveryCompileError::Io {
            path: path.to_owned(),
            source,
        })?;
    if bytes.len() > MAX_DELIVERY_FILE_BYTES {
        return Err(DeliveryCompileError::FileTooLarge(path.to_owned()));
    }
    let handle_metadata = opened.file.metadata().map_err(|source| {
        DeliveryCompileError::Io {
            path: path.to_owned(),
            source,
        }
    })?;
    let handle_fingerprint = PathFingerprint {
        identity: path_identity(&opened.file, &handle_metadata).map_err(|source| {
            DeliveryCompileError::Io {
                path: path.to_owned(),
                source,
            }
        })?,
        byte_len: handle_metadata.len(),
        modified: handle_metadata.modified().ok(),
    };
    if bytes.len() as u64 != opened.metadata.len()
        || handle_fingerprint != opened.fingerprint
    {
        return Err(DeliveryCompileError::SourceChanged(path.to_owned()));
    }
    verify_opened_path(canonical_root, path, &opened)?;
    Ok(CapturedBytes {
        bytes,
        metadata: opened.metadata,
    })
}

fn open_path_no_follow(path: &Path, directory: bool) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_no_follow(&mut options, directory);
    options.open(path)
}

#[cfg(unix)]
fn set_no_follow(options: &mut OpenOptions, directory: bool) {
    use std::os::unix::fs::OpenOptionsExt;
    let directory_flag = if directory { libc::O_DIRECTORY } else { 0 };
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | directory_flag);
}

#[cfg(windows)]
fn set_no_follow(options: &mut OpenOptions, directory: bool) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
    };
    let directory_flag = if directory { FILE_FLAG_BACKUP_SEMANTICS } else { 0 };
    options
        .share_mode(FILE_SHARE_READ)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | directory_flag);
}

#[cfg(not(any(unix, windows)))]
fn set_no_follow(_: &mut OpenOptions, _: bool) {}

#[cfg(unix)]
fn path_identity(_: &File, metadata: &fs::Metadata) -> io::Result<PathIdentity> {
    use std::os::unix::fs::MetadataExt;
    Ok(PathIdentity {
        first: metadata.dev(),
        second: metadata.ino(),
    })
}

#[cfg(windows)]
fn path_identity(file: &File, _: &fs::Metadata) -> io::Result<PathIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as _, &mut information)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(PathIdentity {
        first: information.dwVolumeSerialNumber as u64,
        second: ((information.nFileIndexHigh as u64) << 32)
            | information.nFileIndexLow as u64,
    })
}

#[cfg(not(any(unix, windows)))]
fn path_identity(_: &File, metadata: &fs::Metadata) -> io::Result<PathIdentity> {
    Ok(PathIdentity {
        first: metadata.len(),
        second: 0,
    })
}

#[cfg(unix)]
fn validate_source_permissions(
    _: &File,
    metadata: &fs::Metadata,
    path: &Path,
) -> Result<(), DeliveryCompileError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(DeliveryCompileError::InsecurePermissions(path.to_owned()));
    }
    Ok(())
}

#[cfg(windows)]
fn validate_source_permissions(
    file: &File,
    _: &fs::Metadata,
    path: &Path,
) -> Result<(), DeliveryCompileError> {
    windows_delivery_security::validate_owner_only(file)
        .map_err(|_| DeliveryCompileError::InsecurePermissions(path.to_owned()))
}

#[cfg(not(any(unix, windows)))]
fn validate_source_permissions(
    _: &File,
    _: &fs::Metadata,
    path: &Path,
) -> Result<(), DeliveryCompileError> {
    Err(DeliveryCompileError::InsecurePermissions(path.to_owned()))
}

#[cfg(windows)]
mod windows_delivery_security {
    use super::*;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetSecurityInfo, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        CreateWellKnownSid, EqualSid, GetAce, GetAclInformation,
        GetSecurityDescriptorControl, ACCESS_ALLOWED_ACE, ACL_SIZE_INFORMATION,
        AclSizeInformation, DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION,
        SECURITY_MAX_SID_SIZE, SE_DACL_PROTECTED, WinCreatorOwnerRightsSid,
    };
    use windows_sys::Win32::Storage::FileSystem::FILE_ALL_ACCESS;

    pub(super) fn validate_owner_only(file: &File) -> io::Result<()> {
        let mut owner = std::ptr::null_mut();
        let mut dacl = std::ptr::null_mut();
        let mut descriptor = std::ptr::null_mut();
        let status = unsafe {
            GetSecurityInfo(
                file.as_raw_handle() as _,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                &mut owner,
                std::ptr::null_mut(),
                &mut dacl,
                std::ptr::null_mut(),
                &mut descriptor,
            )
        };
        if status != 0 {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let result = (|| {
            if owner.is_null() || dacl.is_null() {
                return Err(insecure("delivery DACL is missing"));
            }
            let mut control = 0_u16;
            let mut revision = 0_u32;
            if unsafe {
                GetSecurityDescriptorControl(descriptor, &mut control, &mut revision)
            } == 0
                || control & SE_DACL_PROTECTED == 0
            {
                return Err(insecure("delivery DACL is not protected"));
            }
            let mut information = ACL_SIZE_INFORMATION::default();
            if unsafe {
                GetAclInformation(
                    dacl,
                    &mut information as *mut _ as *mut _,
                    std::mem::size_of::<ACL_SIZE_INFORMATION>() as u32,
                    AclSizeInformation,
                )
            } == 0
                || information.AceCount != 1
            {
                return Err(insecure("delivery DACL is not owner-only"));
            }
            let mut ace = std::ptr::null_mut();
            if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
                return Err(io::Error::last_os_error());
            }
            let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let sid = &allowed.SidStart as *const u32 as *mut _;
            let mut owner_rights = [0_u8; SECURITY_MAX_SID_SIZE as usize];
            let mut owner_rights_len = owner_rights.len() as u32;
            if unsafe {
                CreateWellKnownSid(
                    WinCreatorOwnerRightsSid,
                    std::ptr::null_mut(),
                    owner_rights.as_mut_ptr() as *mut _,
                    &mut owner_rights_len,
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if allowed.Header.AceType != 0
                || allowed.Header.AceFlags != 0
                || allowed.Mask != FILE_ALL_ACCESS
                || (unsafe { EqualSid(owner, sid) } == 0
                    && unsafe { EqualSid(owner_rights.as_mut_ptr() as *mut _, sid) } == 0)
            {
                return Err(insecure("delivery DACL is not owner-only"));
            }
            Ok(())
        })();
        unsafe {
            LocalFree(descriptor);
        }
        result
    }

    #[cfg(test)]
    pub(super) fn protect_owner_only(path: &Path) -> io::Result<()> {
        set_dacl(path, "D:P(A;;FA;;;OW)")
    }

    #[cfg(test)]
    pub(super) fn make_world_writable(path: &Path) -> io::Result<()> {
        set_dacl(path, "D:P(A;;FA;;;WD)")
    }

    #[cfg(test)]
    fn set_dacl(path: &Path, descriptor_text: &str) -> io::Result<()> {
        use std::ffi::OsStr;
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Security::Authorization::{
            ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SetNamedSecurityInfoW, SDDL_REVISION_1,
        };
        use windows_sys::Win32::Security::{
            GetSecurityDescriptorDacl, PROTECTED_DACL_SECURITY_INFORMATION,
        };

        let sddl = OsStr::new(descriptor_text)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let mut descriptor = std::ptr::null_mut();
        if unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let result = (|| {
            let mut present = 0_i32;
            let mut defaulted = 0_i32;
            let mut dacl = std::ptr::null_mut();
            if unsafe {
                GetSecurityDescriptorDacl(
                    descriptor,
                    &mut present,
                    &mut dacl,
                    &mut defaulted,
                )
            } == 0
                || present == 0
                || dacl.is_null()
            {
                return Err(io::Error::last_os_error());
            }
            let mut wide = path
                .as_os_str()
                .encode_wide()
                .chain(std::iter::once(0))
                .collect::<Vec<_>>();
            let status = unsafe {
                SetNamedSecurityInfoW(
                    wide.as_mut_ptr(),
                    SE_FILE_OBJECT,
                    DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    dacl,
                    std::ptr::null_mut(),
                )
            };
            if status != 0 {
                return Err(io::Error::from_raw_os_error(status as i32));
            }
            Ok(())
        })();
        unsafe {
            LocalFree(descriptor);
        }
        result
    }

    fn insecure(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }
}

fn validate_entry_type(path: &Path, metadata: &fs::Metadata) -> Result<(), DeliveryCompileError> {
    if metadata.file_type().is_symlink() || is_reparse_point(metadata) {
        return Err(DeliveryCompileError::LinkOrReparse(path.to_owned()));
    }
    if !metadata.is_file() && !metadata.is_dir() {
        return Err(DeliveryCompileError::SpecialFile(path.to_owned()));
    }
    Ok(())
}

fn validate_path_components(path: &Path) -> Result<(), DeliveryCompileError> {
    for component in path.components() {
        if let Component::Normal(value) = component {
            let value = value
                .to_str()
                .ok_or_else(|| DeliveryCompileError::NonUtf8Path(path.to_owned()))?;
            if is_forbidden_component(value) {
                return Err(DeliveryCompileError::ForbiddenRoot(path.to_owned()));
            }
        }
    }
    Ok(())
}

fn is_forbidden_component(value: &str) -> bool {
    matches!(
        value.to_ascii_lowercase().as_str(),
        ".git"
            | ".env"
            | "vendor"
            | "auth"
            | "credentials"
            | "tokens"
            | ".config"
            | ".codex"
            | ".claude"
            | ".gemini"
            | ".kimi"
            | ".qwen"
            | ".ssh"
            | ".aws"
    )
}

fn join_delivery_path(
    mount: Option<&DeliveryRelativePathV2>,
    child: &str,
) -> Result<DeliveryRelativePathV2, DeliveryCompileError> {
    let value = match mount {
        Some(mount) => format!("{}/{}", mount.as_str(), child),
        None => child.to_owned(),
    };
    DeliveryRelativePathV2::new(value)
        .map_err(|error| DeliveryCompileError::InvalidContract(error.to_string()))
}

fn path_to_forward_slashes(path: &Path) -> Result<String, DeliveryCompileError> {
    let mut parts = Vec::new();
    for component in path.components() {
        let Component::Normal(value) = component else {
            return Err(DeliveryCompileError::InvalidContract(
                "source-relative path is not normalized".to_owned(),
            ));
        };
        parts.push(
            value
                .to_str()
                .ok_or_else(|| DeliveryCompileError::NonUtf8Path(path.to_owned()))?,
        );
    }
    Ok(parts.join("/"))
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x400 != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

fn is_executable(metadata: &fs::Metadata, path: &str, bytes: &[u8]) -> bool {
    if unix_executable(metadata) {
        return true;
    }
    let extension = path
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase());
    if extension.as_deref().is_some_and(|extension| {
        matches!(
            extension,
            "exe" | "com" | "bat" | "cmd" | "ps1" | "sh" | "bash" | "zsh"
                | "fish" | "py" | "rb" | "pl" | "js" | "mjs" | "cjs"
        )
    }) {
        return true;
    }
    bytes.starts_with(b"#!")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(b"\x7fELF")
        || matches!(
            bytes.get(..4),
            Some([0xfe, 0xed, 0xfa, 0xce])
                | Some([0xfe, 0xed, 0xfa, 0xcf])
                | Some([0xce, 0xfa, 0xed, 0xfe])
                | Some([0xcf, 0xfa, 0xed, 0xfe])
                | Some([0xca, 0xfe, 0xba, 0xbe])
        )
}

#[cfg(unix)]
fn unix_executable(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn unix_executable(_: &fs::Metadata) -> bool {
    false
}

fn blob_digest(bytes: &[u8]) -> DeliveryBlobDigestV1 {
    let digest = ring::digest::digest(&SHA256, bytes);
    DeliveryBlobDigestV1::new(format!("sha256:{}", lower_hex(digest.as_ref())))
        .expect("SHA-256 formatting is valid")
}

fn bundle_digest(files: &[(&str, &[u8])]) -> SpawnBundleDigest {
    let mut context = Context::new(&SHA256);
    context.update(BUNDLE_DIGEST_DOMAIN);
    for (path, bytes) in files {
        context.update(&(path.len() as u32).to_be_bytes());
        context.update(path.as_bytes());
        context.update(&(bytes.len() as u64).to_be_bytes());
        context.update(bytes);
    }
    SpawnBundleDigest::new(format!("sha256:{}", lower_hex(context.finish().as_ref())))
        .expect("SHA-256 formatting is valid")
}

fn manifest_digest(manifest: &DeliveryBundleManifestV2) -> DeliveryManifestDigestV2 {
    let digest = ring::digest::digest(
        &SHA256,
        &manifest.canonical_manifest_digest_material(),
    );
    DeliveryManifestDigestV2::new(format!("sha256:{}", lower_hex(digest.as_ref())))
        .expect("SHA-256 formatting is valid")
}

fn lower_hex(bytes: &[u8]) -> String {
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut value, "{byte:02x}").expect("writing to a String cannot fail");
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let target = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"));
            let base = target.join("harness-delivery-tests");
            fs::create_dir_all(&base).unwrap();
            let path = base.join(format!(
                "reviewed-source-{}-{}",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path).unwrap();
            protect_test_path(&path);
            Self(path)
        }

        fn write(&self, relative: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(&path, bytes).unwrap();
            protect_test_path(&path);
            path
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn source(
        path: PathBuf,
        mount: Option<&str>,
        kind: DeliveryComponentKindV2,
        scope: DeliveryScopeV2,
    ) -> ReviewedDeliverySourceV2 {
        ReviewedDeliverySourceV2::new(
            path,
            mount.map(|path| DeliveryRelativePathV2::new(path).unwrap()),
            kind,
            scope,
        )
        .unwrap()
    }

    fn compile_one(source: ReviewedDeliverySourceV2) -> CompiledDeliveryBundleV2 {
        compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("review-bundle").unwrap(),
            SpawnBundleRevision::new("review-bundle.r2").unwrap(),
            &[source],
        )
        .unwrap()
    }

    #[cfg(unix)]
    fn protect_test_path(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        let directory = fs::metadata(path).unwrap().is_dir();
        fs::set_permissions(
            path,
            fs::Permissions::from_mode(if directory { 0o700 } else { 0o600 }),
        )
        .unwrap();
    }

    #[cfg(windows)]
    fn protect_test_path(path: &Path) {
        windows_delivery_security::protect_owner_only(path).unwrap();
    }

    #[cfg(unix)]
    fn make_test_path_insecure(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o666)).unwrap();
    }

    #[cfg(windows)]
    fn make_test_path_insecure(path: &Path) {
        windows_delivery_security::make_world_writable(path).unwrap();
    }

    #[test]
    fn delivery_compiler_digest_is_exact_stable_and_metadata_sensitive() {
        let root = TestRoot::new();
        let file = root.write("definition.md", b"review\n");
        let workspace = compile_one(source(
            file.clone(),
            None,
            DeliveryComponentKindV2::AgentDefinition,
            DeliveryScopeV2::Workspace,
        ));
        let session = compile_one(source(
            file,
            None,
            DeliveryComponentKindV2::Instructions,
            DeliveryScopeV2::Session,
        ));

        assert_eq!(
            workspace.manifest.bundle_digest.as_str(),
            "sha256:ba71773aa731bdc2e460cbde965db223f9fa8d499a1b97acc8ceeb376dcc432a",
        );
        assert_eq!(
            workspace.blobs[0].receipt.digest.as_str(),
            "sha256:452b7b642e325cb2b5b20ac28536ada8b0ad312217a98ab7ff23330603b63126",
        );
        assert_eq!(workspace.manifest.bundle_digest, session.manifest.bundle_digest);
        assert_ne!(
            workspace.manifest.manifest_digest,
            session.manifest.manifest_digest,
        );
        workspace.verify().unwrap();
        session.verify().unwrap();
    }

    #[test]
    fn delivery_compiler_rejects_reserved_executable_mcp_and_case_collisions() {
        let forbidden = TestRoot::new();
        let forbidden_file = forbidden.write(".env/secret.txt", b"secret");
        let error = compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("review-bundle").unwrap(),
            SpawnBundleRevision::new("r1").unwrap(),
            &[source(
                forbidden_file,
                None,
                DeliveryComponentKindV2::File,
                DeliveryScopeV2::Workspace,
            )],
        )
        .unwrap_err();
        assert!(matches!(error, DeliveryCompileError::ForbiddenRoot(_)));

        let executable = TestRoot::new();
        let executable_file = executable.write("run.cmd", b"echo unsafe\n");
        let error = compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("review-bundle").unwrap(),
            SpawnBundleRevision::new("r1").unwrap(),
            &[source(
                executable_file,
                None,
                DeliveryComponentKindV2::Command,
                DeliveryScopeV2::Workspace,
            )],
        )
        .unwrap_err();
        assert!(matches!(error, DeliveryCompileError::Executable(_)));

        let mcp = TestRoot::new();
        let mcp_file = mcp.write("mcp.json", b"{}");
        assert!(matches!(
            ReviewedDeliverySourceV2::new(
                mcp_file,
                None,
                DeliveryComponentKindV2::McpDeclaration,
                DeliveryScopeV2::Workspace,
            ),
            Err(DeliveryCompileError::McpDeclarationReserved),
        ));

        let first = TestRoot::new();
        let second = TestRoot::new();
        let first_file = first.write("same.txt", b"first");
        let second_file = second.write("same.txt", b"second");
        let error = compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("review-bundle").unwrap(),
            SpawnBundleRevision::new("r1").unwrap(),
            &[
                source(
                    first_file,
                    Some("Agents"),
                    DeliveryComponentKindV2::File,
                    DeliveryScopeV2::Workspace,
                ),
                source(
                    second_file,
                    Some("agents"),
                    DeliveryComponentKindV2::File,
                    DeliveryScopeV2::Workspace,
                ),
            ],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            DeliveryCompileError::CaseFoldPathCollision(_)
        ));
    }

    #[test]
    fn delivery_compiler_rejects_unprotected_source_permissions() {
        let root = TestRoot::new();
        let file = root.write("review.txt", b"review\n");
        make_test_path_insecure(&file);
        let error = compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("review-bundle").unwrap(),
            SpawnBundleRevision::new("r1").unwrap(),
            &[source(
                file,
                None,
                DeliveryComponentKindV2::File,
                DeliveryScopeV2::Workspace,
            )],
        )
        .unwrap_err();
        assert!(matches!(
            error,
            DeliveryCompileError::InsecurePermissions(_)
        ));
    }

    #[test]
    fn delivery_compiler_rejects_links_and_tampering() {
        let root = TestRoot::new();
        let target = root.write("target.txt", b"review\n");
        let link = root.0.join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &link).unwrap();
        #[cfg(windows)]
        if std::os::windows::fs::symlink_file(&target, &link).is_err() {
            return;
        }
        let error = compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("review-bundle").unwrap(),
            SpawnBundleRevision::new("r1").unwrap(),
            &[source(
                link,
                None,
                DeliveryComponentKindV2::File,
                DeliveryScopeV2::Workspace,
            )],
        )
        .unwrap_err();
        assert!(matches!(error, DeliveryCompileError::LinkOrReparse(_)));

        let mut compiled = compile_one(source(
            target,
            None,
            DeliveryComponentKindV2::File,
            DeliveryScopeV2::Workspace,
        ));
        compiled.blobs[0].bytes[0] ^= 1;
        assert!(matches!(
            compiled.verify(),
            Err(DeliveryCompileError::BlobDigestMismatch(_))
        ));
    }

    #[test]
    fn delivery_catalog_is_bounded_and_rejects_duplicate_identity() {
        let root = TestRoot::new();
        let bundle = compile_one(source(
            root.write("definition.md", b"review\n"),
            None,
            DeliveryComponentKindV2::AgentDefinition,
            DeliveryScopeV2::Workspace,
        ));
        let error = DeliveryCatalogV2::new([bundle.clone(), bundle]).unwrap_err();
        assert!(matches!(error, DeliveryCatalogError::DuplicateBundle(_)));
    }

    #[test]
    fn delivery_catalog_iterator_is_exact_and_canonical_by_bundle_id() {
        let first_root = TestRoot::new();
        let second_root = TestRoot::new();
        let second = compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("bundle-b").unwrap(),
            SpawnBundleRevision::new("r1").unwrap(),
            &[source(
                second_root.write("bundle-b.txt", b"bundle b\n"),
                None,
                DeliveryComponentKindV2::File,
                DeliveryScopeV2::Workspace,
            )],
        )
        .unwrap();
        let first = compile_reviewed_delivery_bundle_v2(
            SpawnBundleId::new("bundle-a").unwrap(),
            SpawnBundleRevision::new("r1").unwrap(),
            &[source(
                first_root.write("bundle-a.txt", b"bundle a\n"),
                None,
                DeliveryComponentKindV2::File,
                DeliveryScopeV2::Workspace,
            )],
        )
        .unwrap();
        let catalog = DeliveryCatalogV2::new([second, first]).unwrap();

        let identities = catalog
            .iter()
            .map(|(id, bundle)| {
                (
                    id.as_str().to_owned(),
                    bundle.manifest().revision.as_str().to_owned(),
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(
            identities,
            vec![
                ("bundle-a".to_owned(), "r1".to_owned()),
                ("bundle-b".to_owned(), "r1".to_owned()),
            ],
        );
    }
}

use crate::protocol::{
    ResolvedBundleReceipt, SpawnBundleDigest, SpawnBundleId, SpawnBundleRevision,
};
use ring::digest::{Context, SHA256};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};
use thiserror::Error;

pub const MAX_BUNDLE_CATALOG_ENTRIES: usize = 128;
pub const MAX_BUNDLE_FILES: usize = 128;
pub const MAX_BUNDLE_FILE_BYTES: usize = 1024 * 1024;
pub const MAX_BUNDLE_TOTAL_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_BUNDLE_PATH_BYTES: usize = 512;

const AGENT_PLUGINS_SCHEMA: &str =
    "https://agent-plugins.org/schemas/1.0.0/plugin.schema.json";
const MAX_SKILL_FRONTMATTER_BYTES: usize = 16 * 1024;
const MAX_SKILL_NAME_CHARS: usize = 64;
const MAX_SKILL_DESCRIPTION_CHARS: usize = 1024;
const DIGEST_DOMAIN: &[u8] = b"g4a-bundle-v1\0";

/// One immutable, normalized file captured from a validated bundle root.
#[derive(Clone, Eq, PartialEq)]
pub struct NodeBundleFile {
    path: String,
    bytes: Vec<u8>,
}

impl fmt::Debug for NodeBundleFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeBundleFile")
            .field("path", &self.path)
            .field("byte_length", &self.bytes.len())
            .finish()
    }
}

impl NodeBundleFile {
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }
}

/// Immutable host-local bundle bytes and their public content receipt.
#[derive(Clone, Eq, PartialEq)]
pub struct NodeBundle {
    id: SpawnBundleId,
    revision: SpawnBundleRevision,
    digest: SpawnBundleDigest,
    files: Vec<NodeBundleFile>,
}

impl fmt::Debug for NodeBundle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NodeBundle")
            .field("id", &self.id)
            .field("revision", &self.revision)
            .field("digest", &self.digest)
            .field("files", &self.files)
            .finish()
    }
}

impl NodeBundle {
    pub fn new(
        id: SpawnBundleId,
        revision: SpawnBundleRevision,
        expected_digest: SpawnBundleDigest,
        root: impl AsRef<Path>,
    ) -> Result<Self, NodeBundleError> {
        let root = ProtectedBundleRoot::open(root.as_ref())?;

        let mut scanner = BundleScanner::default();
        scanner.scan_directory(&root, root.canonical(), &[])?;
        root.verify_stable()?;
        scanner.files.sort_by(|left, right| left.path.cmp(&right.path));
        validate_bundle_contract(&scanner.files, &scanner.directories)?;

        let actual_digest = digest_files(&scanner.files);
        if actual_digest != expected_digest {
            return Err(NodeBundleError::DigestMismatch {
                expected: expected_digest,
                actual: actual_digest,
            });
        }

        Ok(Self {
            id,
            revision,
            digest: actual_digest,
            files: scanner.files,
        })
    }

    pub fn id(&self) -> &SpawnBundleId {
        &self.id
    }

    pub fn revision(&self) -> &SpawnBundleRevision {
        &self.revision
    }

    pub fn digest(&self) -> &SpawnBundleDigest {
        &self.digest
    }

    pub fn files(&self) -> &[NodeBundleFile] {
        &self.files
    }

    pub fn receipt(&self) -> ResolvedBundleReceipt {
        ResolvedBundleReceipt {
            id: self.id.clone(),
            revision: self.revision.clone(),
            digest: self.digest.clone(),
        }
    }
}

/// Bounded immutable lookup table for bundles installed before node startup.
#[derive(Clone, Default, Eq, PartialEq)]
pub struct BundleCatalog {
    bundles: BTreeMap<SpawnBundleId, NodeBundle>,
}

impl fmt::Debug for BundleCatalog {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BundleCatalog")
            .field("bundles", &self.bundles.values().collect::<Vec<_>>())
            .finish()
    }
}

impl BundleCatalog {
    pub fn new(
        bundles: impl IntoIterator<Item = NodeBundle>,
    ) -> Result<Self, BundleCatalogError> {
        let mut catalog = BTreeMap::new();
        for bundle in bundles {
            if catalog.len() == MAX_BUNDLE_CATALOG_ENTRIES {
                return Err(BundleCatalogError::TooMany {
                    max: MAX_BUNDLE_CATALOG_ENTRIES,
                });
            }
            let id = bundle.id.clone();
            if catalog.insert(id.clone(), bundle).is_some() {
                return Err(BundleCatalogError::Duplicate { id });
            }
        }
        Ok(Self { bundles: catalog })
    }

    pub fn get(&self, id: &SpawnBundleId) -> Option<&NodeBundle> {
        self.bundles.get(id)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &NodeBundle> {
        self.bundles.values()
    }
}

/// Test-only preparation for local fixture bundle trees. Production callers
/// must provision the same protection through their operator-owned startup
/// path rather than mutating source permissions during bundle loading.
#[cfg(feature = "fixture")]
pub fn protect_bundle_source_tree_fixture(root: &Path) -> io::Result<()> {
    protect_fixture_path(root)
}

#[cfg(feature = "fixture")]
fn protect_fixture_path(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture bundle source cannot contain links or reparse points",
        ));
    }
    if metadata.is_dir() {
        protect_fixture_permissions(path, true)?;
        for entry in fs::read_dir(path)? {
            protect_fixture_path(&entry?.path())?;
        }
        return Ok(());
    }
    if !metadata.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture bundle source must contain only regular files and directories",
        ));
    }
    protect_fixture_permissions(path, false)
}

#[cfg(all(feature = "fixture", unix))]
fn protect_fixture_permissions(path: &Path, directory: bool) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = fs::symlink_metadata(path)?;
    if metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "fixture bundle source is not owned by the current user",
        ));
    }
    let mode = if directory { 0o700 } else { 0o600 };
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(all(feature = "fixture", windows))]
fn protect_fixture_permissions(path: &Path, _: bool) -> io::Result<()> {
    windows_bundle_security::protect_owner_only(path)
}

#[cfg(all(feature = "fixture", not(any(unix, windows))))]
fn protect_fixture_permissions(_: &Path, _: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "fixture bundle source protection is unsupported on this platform",
    ))
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BundleCatalogError {
    #[error("bundle catalog exceeds the {max}-bundle limit")]
    TooMany { max: usize },
    #[error("bundle catalog contains duplicate bundle {id}")]
    Duplicate { id: SpawnBundleId },
}

#[derive(Debug, Error)]
pub enum NodeBundleError {
    #[error("bundle root must be an absolute path without dot or parent components")]
    RootNotAbsolute,
    #[error("bundle root is not a regular directory or is a symlink/reparse point")]
    UnsafeRoot,
    #[error("bundle root or entry is not protected from untrusted writes: {path:?}")]
    InsecurePermissions { path: PathBuf },
    #[error("bundle source changed identity while it was being captured: {path:?}")]
    SourceChanged { path: PathBuf },
    #[error("bundle source resolves outside its protected canonical root: {path:?}")]
    EscapedRoot { path: PathBuf },
    #[error("bundle path is not valid UTF-8: {path:?}")]
    PathNotUtf8 { path: PathBuf },
    #[error("bundle path exceeds the {max}-byte limit: {path}")]
    PathTooLong { path: String, max: usize },
    #[error("bundle path is not portable: {path}")]
    UnsafePath { path: String },
    #[error("bundle contains a case-folded path collision: {first} and {second}")]
    CaseFoldCollision { first: String, second: String },
    #[error("bundle contains a symlink, reparse point, or special file: {path}")]
    UnsafeFileType { path: String },
    #[error("bundle contains an executable file unsupported by the skills-only floor: {path}")]
    ExecutableFile { path: String },
    #[error("bundle exceeds the {max}-file limit")]
    TooManyFiles { max: usize },
    #[error("bundle file {path} exceeds the {max}-byte limit")]
    FileTooLarge { path: String, max: usize },
    #[error("bundle exceeds the {max}-byte total limit")]
    TotalTooLarge { max: usize },
    #[error("bundle root mcp.json is unsupported in the F6.1 skills-only floor")]
    McpUnsupported,
    #[error("bundle contains an unsupported root entry: {path}")]
    UnsupportedRoot { path: String },
    #[error("bundle requires a regular UTF-8 root plugin.json")]
    MissingManifest,
    #[error("bundle plugin.json is invalid: {reason}")]
    InvalidManifest { reason: &'static str },
    #[error("bundle .claude-plugin/plugin.json is invalid: {reason}")]
    InvalidClaudeManifest { reason: &'static str },
    #[error("bundle requires at least one skills/<name>/SKILL.md component")]
    MissingSkills,
    #[error("bundle skill {path} is invalid: {reason}")]
    InvalidSkill { path: String, reason: &'static str },
    #[error("bundle digest mismatch: expected {expected}, captured {actual}")]
    DigestMismatch {
        expected: SpawnBundleDigest,
        actual: SpawnBundleDigest,
    },
    #[error("bundle filesystem operation failed at {path:?}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

#[derive(Default)]
struct BundleScanner {
    files: Vec<NodeBundleFile>,
    directories: BTreeSet<String>,
    folded_paths: BTreeMap<String, String>,
    total_bytes: usize,
}

impl BundleScanner {
    fn scan_directory(
        &mut self,
        root: &ProtectedBundleRoot,
        absolute: &Path,
        relative_components: &[String],
    ) -> Result<(), NodeBundleError> {
        let directory = open_verified_path(root.canonical(), absolute, true)?;
        let entries = fs::read_dir(absolute).map_err(|source| NodeBundleError::Io {
            path: absolute.to_path_buf(),
            source,
        })?;
        for entry in entries {
            let entry = entry.map_err(|source| NodeBundleError::Io {
                path: absolute.to_path_buf(),
                source,
            })?;
            let component = entry.file_name().into_string().map_err(|name| {
                NodeBundleError::PathNotUtf8 {
                    path: PathBuf::from(name),
                }
            })?;
            validate_component(&component)?;

            let mut components = relative_components.to_vec();
            components.push(component);
            let relative = components.join("/");
            if relative.as_bytes().len() > MAX_BUNDLE_PATH_BYTES {
                return Err(NodeBundleError::PathTooLong {
                    path: relative,
                    max: MAX_BUNDLE_PATH_BYTES,
                });
            }
            self.record_folded_path(&relative)?;

            let absolute_entry = entry.path();
            let metadata = fs::symlink_metadata(&absolute_entry).map_err(|source| {
                NodeBundleError::Io {
                    path: absolute_entry.clone(),
                    source,
                }
            })?;
            if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
                return Err(NodeBundleError::UnsafeFileType { path: relative });
            }
            if metadata.is_dir() {
                validate_directory_location(&relative, &components)?;
                self.directories.insert(relative);
                self.scan_directory(root, &absolute_entry, &components)?;
            } else if metadata.is_file() {
                validate_file_location(&relative, &components)?;
                if self.files.len() == MAX_BUNDLE_FILES {
                    return Err(NodeBundleError::TooManyFiles {
                        max: MAX_BUNDLE_FILES,
                    });
                }
                if metadata.len() > MAX_BUNDLE_FILE_BYTES as u64 {
                    return Err(NodeBundleError::FileTooLarge {
                        path: relative,
                        max: MAX_BUNDLE_FILE_BYTES,
                    });
                }
                let captured = read_regular_no_follow(
                    root.canonical(),
                    &absolute_entry,
                    &relative,
                )?;
                if is_executable(&captured.metadata, &relative, &captured.bytes) {
                    return Err(NodeBundleError::ExecutableFile { path: relative });
                }
                self.total_bytes = self.total_bytes.checked_add(captured.bytes.len()).ok_or(
                    NodeBundleError::TotalTooLarge {
                        max: MAX_BUNDLE_TOTAL_BYTES,
                    },
                )?;
                if self.total_bytes > MAX_BUNDLE_TOTAL_BYTES {
                    return Err(NodeBundleError::TotalTooLarge {
                        max: MAX_BUNDLE_TOTAL_BYTES,
                    });
                }
                self.files.push(NodeBundleFile {
                    path: relative,
                    bytes: captured.bytes,
                });
            } else {
                return Err(NodeBundleError::UnsafeFileType { path: relative });
            }
        }
        verify_opened_path(root.canonical(), absolute, &directory)?;
        Ok(())
    }

    fn record_folded_path(&mut self, path: &str) -> Result<(), NodeBundleError> {
        let folded = path.chars().flat_map(char::to_lowercase).collect::<String>();
        if let Some(existing) = self.folded_paths.insert(folded, path.to_owned()) {
            if existing != path {
                return Err(NodeBundleError::CaseFoldCollision {
                    first: existing,
                    second: path.to_owned(),
                });
            }
        }
        Ok(())
    }
}

struct ProtectedBundleRoot {
    canonical: PathBuf,
    opened: OpenedPath,
}

impl ProtectedBundleRoot {
    fn open(root: &Path) -> Result<Self, NodeBundleError> {
        validate_absolute_root(root)?;
        reject_reparse_ancestors(root)?;
        let canonical = fs::canonicalize(root).map_err(|source| NodeBundleError::Io {
            path: root.to_path_buf(),
            source,
        })?;
        reject_reparse_ancestors(&canonical)?;
        let opened = open_verified_path(&canonical, &canonical, true)?;
        let protected = Self { canonical, opened };
        protected.verify_stable()?;
        Ok(protected)
    }

    fn canonical(&self) -> &Path {
        &self.canonical
    }

    fn verify_stable(&self) -> Result<(), NodeBundleError> {
        verify_opened_path(&self.canonical, &self.canonical, &self.opened)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PathIdentity {
    first: u64,
    second: u64,
}

struct OpenedPath {
    file: File,
    metadata: fs::Metadata,
    identity: PathIdentity,
}

fn validate_absolute_root(root: &Path) -> Result<(), NodeBundleError> {
    if !root.is_absolute()
        || root.components().any(|component| {
            matches!(component, Component::CurDir | Component::ParentDir)
        })
    {
        return Err(NodeBundleError::RootNotAbsolute);
    }
    Ok(())
}

fn reject_reparse_ancestors(path: &Path) -> Result<(), NodeBundleError> {
    for ancestor in path.ancestors() {
        let metadata = fs::symlink_metadata(ancestor).map_err(|source| NodeBundleError::Io {
            path: ancestor.to_path_buf(),
            source,
        })?;
        if metadata.file_type().is_symlink() || is_reparse_point(&metadata) {
            return Err(NodeBundleError::UnsafeRoot);
        }
    }
    Ok(())
}

fn open_verified_path(
    canonical_root: &Path,
    path: &Path,
    directory: bool,
) -> Result<OpenedPath, NodeBundleError> {
    let path_metadata = fs::symlink_metadata(path).map_err(|source| NodeBundleError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if path_metadata.file_type().is_symlink() || is_reparse_point(&path_metadata) {
        return Err(NodeBundleError::UnsafeFileType {
            path: path.to_string_lossy().into_owned(),
        });
    }

    let file = open_path_no_follow(path, directory).map_err(|source| NodeBundleError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let metadata = file.metadata().map_err(|source| NodeBundleError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink()
        || is_reparse_point(&metadata)
        || metadata.is_dir() != directory
        || metadata.is_file() == directory
    {
        return Err(NodeBundleError::UnsafeFileType {
            path: path.to_string_lossy().into_owned(),
        });
    }
    validate_source_permissions(&file, &metadata, path)?;
    let final_path = fs::canonicalize(path).map_err(|source| NodeBundleError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    if final_path != canonical_root && !final_path.starts_with(canonical_root) {
        return Err(NodeBundleError::EscapedRoot {
            path: path.to_path_buf(),
        });
    }
    let identity = path_identity(&file, &metadata).map_err(|source| NodeBundleError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(OpenedPath {
        file,
        metadata,
        identity,
    })
}

fn verify_opened_path(
    canonical_root: &Path,
    path: &Path,
    original: &OpenedPath,
) -> Result<(), NodeBundleError> {
    let current = open_verified_path(canonical_root, path, original.metadata.is_dir())?;
    if current.identity != original.identity {
        return Err(NodeBundleError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn validate_component(component: &str) -> Result<(), NodeBundleError> {
    let invalid_character = component.chars().any(|character| {
        character <= '\u{1f}'
            || matches!(character, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
    });
    let stem = component.split('.').next().unwrap_or(component);
    let uppercase_stem = stem.to_ascii_uppercase();
    let reserved = matches!(uppercase_stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || reserved_numbered_name(&uppercase_stem, "COM")
        || reserved_numbered_name(&uppercase_stem, "LPT");
    if component.is_empty()
        || component == "."
        || component == ".."
        || component.ends_with('.')
        || component.ends_with(' ')
        || invalid_character
        || reserved
    {
        return Err(NodeBundleError::UnsafePath {
            path: component.to_owned(),
        });
    }
    Ok(())
}

fn reserved_numbered_name(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|suffix| {
        suffix.len() == 1 && matches!(suffix.as_bytes()[0], b'1'..=b'9')
    })
}

fn validate_directory_location(
    relative: &str,
    components: &[String],
) -> Result<(), NodeBundleError> {
    match components {
        [root] if root == "skills" || root == ".claude-plugin" => Ok(()),
        [root, skill] if root == "skills" && valid_skill_name(skill) => Ok(()),
        [root, _, _, ..] if root == "skills" => Ok(()),
        [root, ..] if root == ".claude-plugin" => {
            Err(NodeBundleError::UnsupportedRoot {
                path: relative.to_owned(),
            })
        }
        _ => Err(NodeBundleError::UnsupportedRoot {
            path: relative.to_owned(),
        }),
    }
}

fn validate_file_location(
    relative: &str,
    components: &[String],
) -> Result<(), NodeBundleError> {
    match components {
        [file] if file == "plugin.json" => Ok(()),
        [file] if file == "mcp.json" => Err(NodeBundleError::McpUnsupported),
        [root, file] if root == ".claude-plugin" && file == "plugin.json" => Ok(()),
        [root, _, _, ..] if root == "skills" => Ok(()),
        _ => Err(NodeBundleError::UnsupportedRoot {
            path: relative.to_owned(),
        }),
    }
}

fn validate_bundle_contract(
    files: &[NodeBundleFile],
    directories: &BTreeSet<String>,
) -> Result<(), NodeBundleError> {
    let by_path = files
        .iter()
        .map(|file| (file.path.as_str(), file))
        .collect::<BTreeMap<_, _>>();
    let manifest = by_path
        .get("plugin.json")
        .ok_or(NodeBundleError::MissingManifest)?;
    validate_manifest(&manifest.bytes)?;

    if let Some(manifest) = by_path.get(".claude-plugin/plugin.json") {
        validate_claude_manifest(&manifest.bytes)?;
    } else if directories.contains(".claude-plugin") {
        return Err(NodeBundleError::InvalidClaudeManifest {
            reason: "manifest file is missing",
        });
    }

    let skill_directories = directories
        .iter()
        .filter_map(|path| path.strip_prefix("skills/"))
        .filter(|path| !path.contains('/'))
        .collect::<Vec<_>>();
    if skill_directories.is_empty() {
        return Err(NodeBundleError::MissingSkills);
    }
    for skill_name in skill_directories {
        let path = format!("skills/{skill_name}/SKILL.md");
        let skill = by_path.get(path.as_str()).ok_or_else(|| {
            NodeBundleError::InvalidSkill {
                path: path.clone(),
                reason: "required SKILL.md file is missing",
            }
        })?;
        validate_skill(&path, skill_name, &skill.bytes)?;
    }
    Ok(())
}

fn validate_manifest(bytes: &[u8]) -> Result<(), NodeBundleError> {
    let text = std::str::from_utf8(bytes).map_err(|_| NodeBundleError::InvalidManifest {
        reason: "manifest is not UTF-8",
    })?;
    let value: Value = serde_json::from_str(text).map_err(|_| {
        NodeBundleError::InvalidManifest {
            reason: "manifest is not valid JSON",
        }
    })?;
    let object = value.as_object().ok_or(NodeBundleError::InvalidManifest {
        reason: "manifest must be an object",
    })?;
    if object.len() != 2 || !object.contains_key("$schema") || !object.contains_key("name") {
        return Err(NodeBundleError::InvalidManifest {
            reason: "skills-only manifest permits exactly $schema and name",
        });
    }
    if object.get("$schema").and_then(Value::as_str) != Some(AGENT_PLUGINS_SCHEMA) {
        return Err(NodeBundleError::InvalidManifest {
            reason: "unsupported Agent Plugins schema",
        });
    }
    let name = object
        .get("name")
        .and_then(Value::as_str)
        .ok_or(NodeBundleError::InvalidManifest {
            reason: "name must be a string",
        })?;
    if !valid_plugin_name(name) {
        return Err(NodeBundleError::InvalidManifest {
            reason: "name violates Agent Plugins 1.0.0 constraints",
        });
    }
    Ok(())
}

fn validate_claude_manifest(bytes: &[u8]) -> Result<(), NodeBundleError> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        NodeBundleError::InvalidClaudeManifest {
            reason: "manifest is not UTF-8",
        }
    })?;
    let value: Value = serde_json::from_str(text).map_err(|_| {
        NodeBundleError::InvalidClaudeManifest {
            reason: "manifest is not valid JSON",
        }
    })?;
    let object = value.as_object().ok_or(NodeBundleError::InvalidClaudeManifest {
        reason: "manifest must be an object",
    })?;
    if !object.contains_key("name")
        || object.keys().any(|key| {
            !matches!(key.as_str(), "name" | "version" | "description")
        })
    {
        return Err(NodeBundleError::InvalidClaudeManifest {
            reason: "manifest permits only name, version, and description",
        });
    }
    for value in object.values() {
        let value = value.as_str().ok_or(NodeBundleError::InvalidClaudeManifest {
            reason: "manifest fields must be strings",
        })?;
        if value.is_empty() {
            return Err(NodeBundleError::InvalidClaudeManifest {
                reason: "manifest fields must not be empty",
            });
        }
    }
    Ok(())
}

fn valid_plugin_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && name.chars().count() <= 64
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'.'))
        && !name.contains("--")
        && !name.contains("..")
}

fn valid_skill_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    !bytes.is_empty()
        && name.chars().count() <= MAX_SKILL_NAME_CHARS
        && bytes[0] != b'-'
        && bytes[bytes.len() - 1] != b'-'
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
        && !name.contains("--")
}

fn validate_skill(path: &str, directory_name: &str, bytes: &[u8]) -> Result<(), NodeBundleError> {
    let text = std::str::from_utf8(bytes).map_err(|_| NodeBundleError::InvalidSkill {
        path: path.to_owned(),
        reason: "SKILL.md is not UTF-8",
    })?;
    let frontmatter = extract_frontmatter(text).ok_or_else(|| NodeBundleError::InvalidSkill {
        path: path.to_owned(),
        reason: "bounded YAML frontmatter is missing",
    })?;
    let fields = parse_frontmatter_fields(frontmatter).map_err(|reason| {
        NodeBundleError::InvalidSkill {
            path: path.to_owned(),
            reason,
        }
    })?;
    let name = fields.name.ok_or_else(|| NodeBundleError::InvalidSkill {
        path: path.to_owned(),
        reason: "frontmatter name is missing",
    })?;
    let description = fields.description.ok_or_else(|| NodeBundleError::InvalidSkill {
        path: path.to_owned(),
        reason: "frontmatter description is missing",
    })?;
    if !valid_skill_name(&name) || name != directory_name {
        return Err(NodeBundleError::InvalidSkill {
            path: path.to_owned(),
            reason: "frontmatter name must be valid and match its directory",
        });
    }
    if description.trim().is_empty()
        || description.chars().count() > MAX_SKILL_DESCRIPTION_CHARS
    {
        return Err(NodeBundleError::InvalidSkill {
            path: path.to_owned(),
            reason: "description must contain 1-1024 characters",
        });
    }
    Ok(())
}

fn extract_frontmatter(text: &str) -> Option<&str> {
    let (opening, remainder) = next_line(text)?;
    if opening != "---" {
        return None;
    }
    let mut consumed = 0usize;
    let mut cursor = remainder;
    loop {
        if cursor.is_empty() {
            return None;
        }
        let (line, rest) = next_line(cursor)?;
        if line == "---" {
            return (consumed <= MAX_SKILL_FRONTMATTER_BYTES).then_some(
                &remainder[..remainder.len() - cursor.len()],
            );
        }
        consumed = consumed.checked_add(cursor.len() - rest.len())?;
        if consumed > MAX_SKILL_FRONTMATTER_BYTES {
            return None;
        }
        cursor = rest;
    }
}

fn next_line(text: &str) -> Option<(&str, &str)> {
    if let Some(index) = text.find('\n') {
        let line = text[..index].strip_suffix('\r').unwrap_or(&text[..index]);
        Some((line, &text[index + 1..]))
    } else {
        Some((text.strip_suffix('\r').unwrap_or(text), ""))
    }
}

#[derive(Default)]
struct SkillFields {
    name: Option<String>,
    description: Option<String>,
}

fn parse_frontmatter_fields(frontmatter: &str) -> Result<SkillFields, &'static str> {
    let lines = frontmatter.lines().collect::<Vec<_>>();
    let mut fields = SkillFields::default();
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index].strip_suffix('\r').unwrap_or(lines[index]);
        if line.contains('\t') {
            return Err("frontmatter tabs are unsupported");
        }
        if line.trim().is_empty() || line.trim_start().starts_with('#') || line.starts_with(' ') {
            index += 1;
            continue;
        }
        let (key, raw_value) = line
            .split_once(':')
            .ok_or("frontmatter top-level entries must be mappings")?;
        let key = key.trim();
        let raw_value = raw_value.trim();
        if key == "name" || key == "description" {
            let (value, consumed_lines) = if matches!(raw_value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
                parse_block_scalar(&lines[index + 1..], raw_value.starts_with('>'))
            } else {
                (parse_yaml_scalar(raw_value)?, 0)
            };
            let destination = if key == "name" {
                &mut fields.name
            } else {
                &mut fields.description
            };
            if destination.replace(value).is_some() {
                return Err("frontmatter contains a duplicate required field");
            }
            index += consumed_lines;
        }
        index += 1;
    }
    Ok(fields)
}

fn parse_block_scalar(lines: &[&str], folded: bool) -> (String, usize) {
    let mut values = Vec::new();
    for line in lines {
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break;
        }
        values.push(line.trim().to_owned());
    }
    let value = if folded {
        values.join(" ")
    } else {
        values.join("\n")
    };
    (value, values.len())
}

fn parse_yaml_scalar(value: &str) -> Result<String, &'static str> {
    if value.is_empty() {
        return Ok(String::new());
    }
    if value.starts_with('"') {
        return serde_json::from_str::<String>(value)
            .map_err(|_| "frontmatter contains an invalid quoted scalar");
    }
    if value.starts_with('\'') {
        if value.len() < 2 || !value.ends_with('\'') {
            return Err("frontmatter contains an invalid quoted scalar");
        }
        return Ok(value[1..value.len() - 1].replace("''", "'"));
    }
    let without_comment = value.split_once(" #").map_or(value, |(value, _)| value);
    Ok(without_comment.trim().to_owned())
}

struct CapturedFile {
    bytes: Vec<u8>,
    metadata: fs::Metadata,
}

fn read_regular_no_follow(
    canonical_root: &Path,
    path: &Path,
    relative: &str,
) -> Result<CapturedFile, NodeBundleError> {
    let mut opened = open_verified_path(canonical_root, path, false)?;
    if opened.metadata.len() > MAX_BUNDLE_FILE_BYTES as u64 {
        return Err(NodeBundleError::FileTooLarge {
            path: relative.to_owned(),
            max: MAX_BUNDLE_FILE_BYTES,
        });
    }
    let mut bytes = Vec::with_capacity(opened.metadata.len() as usize);
    (&mut opened.file)
        .take(MAX_BUNDLE_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| NodeBundleError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    if bytes.len() > MAX_BUNDLE_FILE_BYTES {
        return Err(NodeBundleError::FileTooLarge {
            path: relative.to_owned(),
            max: MAX_BUNDLE_FILE_BYTES,
        });
    }
    if bytes.len() as u64 != opened.metadata.len() {
        return Err(NodeBundleError::SourceChanged {
            path: path.to_path_buf(),
        });
    }
    verify_opened_path(canonical_root, path, &opened)?;
    Ok(CapturedFile {
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
) -> Result<(), NodeBundleError> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    if metadata.uid() != unsafe { libc::geteuid() }
        || metadata.permissions().mode() & 0o022 != 0
    {
        return Err(NodeBundleError::InsecurePermissions {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

#[cfg(windows)]
fn validate_source_permissions(
    file: &File,
    _: &fs::Metadata,
    path: &Path,
) -> Result<(), NodeBundleError> {
    windows_bundle_security::validate_owner_only(file).map_err(|_| {
        NodeBundleError::InsecurePermissions {
            path: path.to_path_buf(),
        }
    })
}

#[cfg(not(any(unix, windows)))]
fn validate_source_permissions(
    _: &File,
    _: &fs::Metadata,
    path: &Path,
) -> Result<(), NodeBundleError> {
    Err(NodeBundleError::InsecurePermissions {
        path: path.to_path_buf(),
    })
}

#[cfg(windows)]
mod windows_bundle_security {
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
                return Err(insecure("bundle DACL is missing"));
            }
            let mut control = 0u16;
            let mut revision = 0u32;
            if unsafe {
                GetSecurityDescriptorControl(descriptor, &mut control, &mut revision)
            } == 0
                || control & SE_DACL_PROTECTED == 0
            {
                return Err(insecure("bundle DACL is not protected"));
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
                return Err(insecure("bundle DACL is not owner-only"));
            }
            let mut ace = std::ptr::null_mut();
            if unsafe { GetAce(dacl, 0, &mut ace) } == 0 || ace.is_null() {
                return Err(io::Error::last_os_error());
            }
            let allowed = unsafe { &*(ace as *const ACCESS_ALLOWED_ACE) };
            let sid = &allowed.SidStart as *const u32 as *mut _;
            let mut owner_rights = [0u8; SECURITY_MAX_SID_SIZE as usize];
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
                    && unsafe {
                        EqualSid(owner_rights.as_mut_ptr() as *mut _, sid)
                    } == 0)
            {
                return Err(insecure("bundle DACL is not owner-only"));
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

    #[cfg(any(test, feature = "fixture"))]
    pub(super) fn protect_owner_only(path: &Path) -> io::Result<()> {
        set_dacl(path, "D:P(A;;FA;;;OW)")
    }

    #[cfg(test)]
    pub(super) fn make_world_writable_for_test(path: &Path) -> io::Result<()> {
        set_dacl(path, "D:P(A;;FA;;;WD)")
    }

    #[cfg(any(test, feature = "fixture"))]
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
            let mut present = 0i32;
            let mut defaulted = 0i32;
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
}

#[cfg(windows)]
fn is_reparse_point(metadata: &fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes()
        & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
        != 0
}

#[cfg(not(windows))]
fn is_reparse_point(_: &fs::Metadata) -> bool {
    false
}

fn is_executable(metadata: &fs::Metadata, path: &str, bytes: &[u8]) -> bool {
    if unix_executable(metadata) {
        return true;
    }
    let extension = path.rsplit_once('.').map(|(_, extension)| extension.to_ascii_lowercase());
    if extension.as_deref().is_some_and(|extension| {
        matches!(
            extension,
            "exe" | "com" | "bat" | "cmd" | "ps1" | "sh" | "bash" | "zsh" | "fish"
                | "py" | "rb" | "pl" | "js" | "mjs" | "cjs"
        )
    }) {
        return true;
    }
    bytes.starts_with(b"#!")
        || bytes.starts_with(b"MZ")
        || bytes.starts_with(b"\x7fELF")
        || matches!(bytes.get(..4), Some([0xfe, 0xed, 0xfa, 0xce])
            | Some([0xfe, 0xed, 0xfa, 0xcf])
            | Some([0xce, 0xfa, 0xed, 0xfe])
            | Some([0xcf, 0xfa, 0xed, 0xfe])
            | Some([0xca, 0xfe, 0xba, 0xbe]))
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

fn digest_files(files: &[NodeBundleFile]) -> SpawnBundleDigest {
    let mut context = Context::new(&SHA256);
    context.update(DIGEST_DOMAIN);
    for file in files {
        let path = file.path.as_bytes();
        context.update(&(path.len() as u32).to_be_bytes());
        context.update(path);
        context.update(&(file.bytes.len() as u64).to_be_bytes());
        context.update(&file.bytes);
    }
    let digest = context.finish();
    let mut value = String::with_capacity(71);
    value.push_str("sha256:");
    for byte in digest.as_ref() {
        use std::fmt::Write;
        write!(&mut value, "{byte:02x}").expect("writing to String cannot fail");
    }
    SpawnBundleDigest::new(value).expect("SHA-256 formatting is valid")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP: AtomicU64 = AtomicU64::new(1);

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            let target = std::env::var_os("CARGO_TARGET_DIR")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .unwrap_or_else(|| std::env::current_dir().unwrap().join("target"));
            let base = target.join("bundle-catalog-tests");
            fs::create_dir_all(&base).unwrap();
            let path = base.join(format!(
                "gate4agent-bundle-catalog-{}-{}",
                std::process::id(),
                NEXT_TEMP.fetch_add(1, Ordering::Relaxed),
            ));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn write(&self, relative: &str, bytes: &[u8]) {
            let path = self.0.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, bytes).unwrap();
        }

        fn valid() -> Self {
            let root = Self::new();
            root.write(
                "plugin.json",
                br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools"}"#,
            );
            root.write(
                "skills/review-code/SKILL.md",
                b"---\nname: review-code\ndescription: Review code for correctness and safety.\n---\n\nReview the selected change.\n",
            );
            root
        }

        fn expected_digest(&self) -> SpawnBundleDigest {
            self.protect();
            let root = ProtectedBundleRoot::open(&self.0).unwrap();
            let mut scanner = BundleScanner::default();
            scanner.scan_directory(&root, root.canonical(), &[]).unwrap();
            scanner.files.sort_by(|left, right| left.path.cmp(&right.path));
            digest_files(&scanner.files)
        }

        fn protect(&self) {
            #[cfg(windows)]
            protect_tree(&self.0).unwrap();
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn bundle(root: &TestRoot, digest: SpawnBundleDigest) -> Result<NodeBundle, NodeBundleError> {
        root.protect();
        NodeBundle::new(
            SpawnBundleId::new("review-tools").unwrap(),
            SpawnBundleRevision::new("review-tools-r1").unwrap(),
            digest,
            root.path(),
        )
    }

    #[test]
    fn node_bundle_rejects_changed_digest() {
        let root = TestRoot::valid();
        let expected = root.expected_digest();
        root.write(
            "skills/review-code/SKILL.md",
            b"---\nname: review-code\ndescription: Changed after the digest was pinned.\n---\n",
        );

        let error = bundle(&root, expected).unwrap_err();
        assert!(matches!(error, NodeBundleError::DigestMismatch { .. }));
    }

    #[test]
    fn node_bundle_rejects_unsafe_paths_and_symlinks() {
        let traversal = TestRoot::valid();
        let error = NodeBundle::new(
            SpawnBundleId::new("review-tools").unwrap(),
            SpawnBundleRevision::new("review-tools-r1").unwrap(),
            zero_digest(),
            traversal.path().join("skills/.."),
        )
        .unwrap_err();
        assert!(matches!(error, NodeBundleError::RootNotAbsolute));

        let reserved = TestRoot::valid();
        reserved.write("skills/review-code/CON.txt", b"unsafe");
        let error = bundle(&reserved, zero_digest()).unwrap_err();
        assert!(matches!(error, NodeBundleError::UnsafePath { .. }));

        let executable = TestRoot::valid();
        executable.write("skills/review-code/scripts/run.sh", b"exit 0\n");
        let error = bundle(&executable, zero_digest()).unwrap_err();
        assert!(matches!(error, NodeBundleError::ExecutableFile { .. }));

        let mcp = TestRoot::valid();
        mcp.write("mcp.json", b"{}");
        let error = bundle(&mcp, zero_digest()).unwrap_err();
        assert!(matches!(error, NodeBundleError::McpUnsupported));

        let linked = TestRoot::valid();
        let linked_target = TestRoot::new();
        let target = linked_target.path().join("target.txt");
        fs::write(&target, b"target").unwrap();
        let link = linked.path().join("skills/review-code/link.txt");
        if create_file_symlink(&target, &link).is_ok() {
            let error = bundle(&linked, zero_digest()).unwrap_err();
            assert!(
                matches!(&error, NodeBundleError::UnsafeFileType { .. }),
                "symlink must fail closed as UnsafeFileType, got {error:?}",
            );
        }

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;

            let writable = TestRoot::valid();
            fs::set_permissions(writable.path(), fs::Permissions::from_mode(0o777)).unwrap();
            let error = NodeBundle::new(
                SpawnBundleId::new("review-tools").unwrap(),
                SpawnBundleRevision::new("review-tools-r1").unwrap(),
                zero_digest(),
                writable.path(),
            )
            .unwrap_err();
            assert!(matches!(error, NodeBundleError::InsecurePermissions { .. }));

            let swapped = TestRoot::valid();
            let expected = swapped.expected_digest();
            let swapped_target = TestRoot::new();
            let target = swapped_target.path().join("replacement.md");
            fs::write(&target, b"replacement").unwrap();
            let skill = swapped.path().join("skills/review-code/SKILL.md");
            fs::remove_file(&skill).unwrap();
            std::os::unix::fs::symlink(&target, &skill).unwrap();
            let error = bundle(&swapped, expected).unwrap_err();
            assert!(
                matches!(&error, NodeBundleError::UnsafeFileType { .. }),
                "swapped skill must fail closed as UnsafeFileType, got {error:?}",
            );
        }

        #[cfg(windows)]
        {
            let insecure = TestRoot::valid();
            insecure.protect();
            windows_bundle_security::make_world_writable_for_test(insecure.path()).unwrap();
            let error = NodeBundle::new(
                SpawnBundleId::new("review-tools").unwrap(),
                SpawnBundleRevision::new("review-tools-r1").unwrap(),
                zero_digest(),
                insecure.path(),
            )
            .unwrap_err();
            assert!(matches!(error, NodeBundleError::InsecurePermissions { .. }));
        }
    }

    #[test]
    fn node_bundle_rejects_case_fold_collision_when_source_can_represent_it() {
        let collision = TestRoot::valid();
        let references = collision.path().join("skills/review-code/references");
        collision.write("skills/review-code/references/A.txt", b"upper");
        collision.write("skills/review-code/references/a.txt", b"lower");

        let names = fs::read_dir(references)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<BTreeSet<_>>();
        if !names.contains(std::ffi::OsStr::new("A.txt"))
            || !names.contains(std::ffi::OsStr::new("a.txt"))
        {
            return;
        }

        let error = bundle(&collision, zero_digest()).unwrap_err();
        assert!(matches!(error, NodeBundleError::CaseFoldCollision { .. }));
    }

    #[test]
    fn node_bundle_validates_schema_manifest_and_skill_contract() {
        let root = TestRoot::valid();
        root.write(
            ".claude-plugin/plugin.json",
            br#"{"name":"review-tools","version":"1.0.0","description":"Review helpers"}"#,
        );
        let digest = root.expected_digest();
        let valid = bundle(&root, digest).unwrap();
        assert_eq!(valid.files().len(), 3);
        assert_eq!(valid.receipt().id.as_str(), "review-tools");
        let error = BundleCatalog::new([valid.clone(), valid]).unwrap_err();
        assert!(matches!(error, BundleCatalogError::Duplicate { .. }));

        root.write(
            "plugin.json",
            br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools","version":"1.0.0"}"#,
        );
        let error = bundle(&root, root.expected_digest()).unwrap_err();
        assert!(matches!(error, NodeBundleError::InvalidManifest { .. }));

        root.write(
            "plugin.json",
            br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools"}"#,
        );
        root.write(
            "skills/review-code/SKILL.md",
            b"---\nname: another-name\ndescription: Wrong directory binding.\n---\n",
        );
        let error = bundle(&root, root.expected_digest()).unwrap_err();
        assert!(matches!(error, NodeBundleError::InvalidSkill { .. }));
    }

    #[test]
    fn node_bundle_digest_is_stable() {
        let first = TestRoot::valid();
        first.write("skills/review-code/references/z.txt", b"last");
        first.write("skills/review-code/references/a.txt", b"first");
        let expected = first.expected_digest();

        let second = TestRoot::new();
        second.write("skills/review-code/references/a.txt", b"first");
        second.write("skills/review-code/references/z.txt", b"last");
        second.write(
            "skills/review-code/SKILL.md",
            b"---\nname: review-code\ndescription: Review code for correctness and safety.\n---\n\nReview the selected change.\n",
        );
        second.write(
            "plugin.json",
            br#"{"$schema":"https://agent-plugins.org/schemas/1.0.0/plugin.schema.json","name":"review-tools"}"#,
        );

        assert_eq!(expected, second.expected_digest());
        let captured = bundle(&first, expected.clone()).unwrap();
        first.write("skills/review-code/references/a.txt", b"mutated later");
        assert_eq!(captured.digest(), &expected);
        assert_eq!(
            captured
                .files()
                .iter()
                .find(|file| file.path() == "skills/review-code/references/a.txt")
                .unwrap()
                .bytes(),
            b"first",
        );

        let marker = "G4A_DEBUG_MUST_NOT_LEAK_MARKER";
        let debug_root = TestRoot::valid();
        debug_root.write(
            "skills/review-code/references/private.txt",
            marker.as_bytes(),
        );
        let debug_bundle = bundle(&debug_root, debug_root.expected_digest()).unwrap();
        let debug_file = debug_bundle
            .files()
            .iter()
            .find(|file| file.path().ends_with("private.txt"))
            .unwrap();
        let debug_catalog = BundleCatalog::new([debug_bundle.clone()]).unwrap();
        assert!(!format!("{debug_file:?}").contains(marker));
        assert!(!format!("{debug_bundle:?}").contains(marker));
        assert!(!format!("{debug_catalog:?}").contains(marker));
    }

    #[cfg(feature = "fixture")]
    #[test]
    fn protect_bundle_source_tree_fixture_establishes_exact_loader_boundary() {
        let root = TestRoot::valid();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777)).unwrap();
            fs::set_permissions(
                root.path().join("plugin.json"),
                fs::Permissions::from_mode(0o666),
            )
            .unwrap();
        }
        #[cfg(windows)]
        windows_bundle_security::make_world_writable_for_test(root.path()).unwrap();

        protect_bundle_source_tree_fixture(root.path()).unwrap();
        let protected = ProtectedBundleRoot::open(root.path()).unwrap();
        let mut scanner = BundleScanner::default();
        scanner
            .scan_directory(&protected, protected.canonical(), &[])
            .unwrap();
        scanner.files.sort_by(|left, right| left.path.cmp(&right.path));
        let digest = digest_files(&scanner.files);
        let loaded = NodeBundle::new(
            SpawnBundleId::new("review-tools").unwrap(),
            SpawnBundleRevision::new("review-tools-r1").unwrap(),
            digest,
            root.path(),
        )
        .unwrap();
        assert_eq!(loaded.files().len(), 2);
    }

    #[cfg(unix)]
    fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn create_file_symlink(target: &Path, link: &Path) -> io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    fn zero_digest() -> SpawnBundleDigest {
        SpawnBundleDigest::new(format!("sha256:{}", "0".repeat(64))).unwrap()
    }

    #[cfg(windows)]
    fn protect_tree(path: &Path) -> io::Result<()> {
        let metadata = fs::symlink_metadata(path)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() && !is_reparse_point(&metadata) {
            for entry in fs::read_dir(path)? {
                protect_tree(&entry?.path())?;
            }
        }
        if !metadata.file_type().is_symlink() && !is_reparse_point(&metadata) {
            windows_bundle_security::protect_owner_only(path)?;
        }
        Ok(())
    }
}

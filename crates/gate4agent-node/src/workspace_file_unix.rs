use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

use crate::protocol::{RepositoryPath, MAX_WORKSPACE_FILE_BYTES};
use ring::digest::{digest, SHA256};

pub(crate) const WORKSPACE_ENTRY_CREATE_PENDING: u8 = 0;
pub(crate) const WORKSPACE_ENTRY_CREATE_COMMITTING: u8 = 1;
pub(crate) const WORKSPACE_ENTRY_CREATE_CANCELED: u8 = 2;

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceFileBytes {
    Utf8(String),
    NonUtf8 { byte_length: u64 },
    TooLarge,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WorkspaceFileReadErrorKind {
    InvalidPath,
    UnsafePath,
    NotFound,
    NotRegularFile,
    ReparsePoint,
    AccessDenied,
    RevisionConflict,
    AlreadyExists,
    ParentNotFound,
    ParentNotDirectory,
    Canceled,
    Io,
}

pub(crate) fn create_workspace_file(
    canonical_root: &Path,
    repository_path: &RepositoryPath,
    commit_state: &AtomicU8,
) -> Result<String, WorkspaceFileReadError> {
    let components = repository_components(repository_path)?;
    let (parent, name) = open_create_parent(canonical_root, &components)?;
    begin_create_commit(commit_state)?;
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC,
            0o600,
        )
    };
    if raw < 0 {
        return Err(create_target_error(&io::Error::last_os_error()));
    }
    let file = unsafe { File::from_raw_fd(raw) };
    file.sync_all()
        .map_err(|error| WorkspaceFileReadError::from_io(&error))?;
    Ok(sha256_hex(&[]))
}

pub(crate) fn create_workspace_directory(
    canonical_root: &Path,
    repository_path: &RepositoryPath,
    commit_state: &AtomicU8,
) -> Result<(), WorkspaceFileReadError> {
    let components = repository_components(repository_path)?;
    let (parent, name) = open_create_parent(canonical_root, &components)?;
    begin_create_commit(commit_state)?;
    let result = unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) };
    if result != 0 {
        return Err(create_target_error(&io::Error::last_os_error()));
    }
    Ok(())
}

fn begin_create_commit(commit_state: &AtomicU8) -> Result<(), WorkspaceFileReadError> {
    commit_state
        .compare_exchange(
            WORKSPACE_ENTRY_CREATE_PENDING,
            WORKSPACE_ENTRY_CREATE_COMMITTING,
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .map(|_| ())
        .map_err(|_| WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::Canceled))
}

fn open_create_parent<'a>(
    canonical_root: &Path,
    components: &'a [CString],
) -> Result<(OwnedFd, &'a CString), WorkspaceFileReadError> {
    let mut parent = open_root(canonical_root)?;
    for component in &components[..components.len() - 1] {
        parent = open_relative(&parent, component, true).map_err(parent_create_error)?;
    }
    Ok((parent, &components[components.len() - 1]))
}

fn parent_create_error(error: WorkspaceFileReadError) -> WorkspaceFileReadError {
    match error.kind() {
        WorkspaceFileReadErrorKind::NotFound => {
            WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::ParentNotFound)
        }
        WorkspaceFileReadErrorKind::NotRegularFile => {
            WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::ParentNotDirectory)
        }
        _ => error,
    }
}

fn create_target_error(error: &io::Error) -> WorkspaceFileReadError {
    match error.raw_os_error() {
        Some(libc::EEXIST) => {
            WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::AlreadyExists)
        }
        Some(libc::ENOENT) => {
            WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::ParentNotFound)
        }
        Some(libc::ENOTDIR) => {
            WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::ParentNotDirectory)
        }
        _ => WorkspaceFileReadError::from_io(error),
    }
}

pub(crate) fn write_workspace_file(
    canonical_root: &Path,
    repository_path: &RepositoryPath,
    expected_revision: &str,
    text: &str,
) -> Result<String, WorkspaceFileReadError> {
    if text.len() > MAX_WORKSPACE_FILE_BYTES {
        return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath));
    }
    let components = repository_components(repository_path)?;
    let mut parent = open_root(canonical_root)?;
    for component in &components[..components.len() - 1] {
        parent = open_relative(&parent, component, true)?;
    }
    let name = &components[components.len() - 1];
    let current = open_relative(&parent, name, false)?;
    let current_stat = descriptor_stat(&current)?;
    let WorkspaceFileBytes::Utf8(current) = read_bounded(current)? else {
        return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::NotRegularFile));
    };
    if sha256_hex(current.as_bytes()) != expected_revision {
        return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::RevisionConflict));
    }

    let temp_name = CString::new(format!(".gate4agent-save-{}-{}", std::process::id(), next_temp_id()))
        .expect("internal temporary file name is valid");
    let raw = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            temp_name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_CLOEXEC,
            0o600,
        )
    };
    if raw < 0 {
        return Err(WorkspaceFileReadError::from_io(&io::Error::last_os_error()));
    }
    let mut temp = unsafe { File::from_raw_fd(raw) };
    let result = (|| {
        temp.write_all(text.as_bytes())
            .and_then(|_| temp.sync_all())
            .map_err(|error| WorkspaceFileReadError::from_io(&error))?;
        let current = open_relative(&parent, name, false)?;
        let observed_stat = descriptor_stat(&current)?;
        if current_stat.st_dev != observed_stat.st_dev || current_stat.st_ino != observed_stat.st_ino {
            return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::RevisionConflict));
        }
        let WorkspaceFileBytes::Utf8(current) = read_bounded(current)? else {
            return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::RevisionConflict));
        };
        if sha256_hex(current.as_bytes()) != expected_revision {
            return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::RevisionConflict));
        }
        let renamed = unsafe {
            libc::renameat(parent.as_raw_fd(), temp_name.as_ptr(), parent.as_raw_fd(), name.as_ptr())
        };
        if renamed != 0 {
            return Err(WorkspaceFileReadError::from_io(&io::Error::last_os_error()));
        }
        Ok(sha256_hex(text.as_bytes()))
    })();
    if result.is_err() {
        unsafe { libc::unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0); }
    }
    result
}

fn descriptor_stat(descriptor: &OwnedFd) -> Result<libc::stat, WorkspaceFileReadError> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(descriptor.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(WorkspaceFileReadError::from_io(&io::Error::last_os_error()));
    }
    Ok(unsafe { stat.assume_init() })
}

fn sha256_hex(bytes: &[u8]) -> String {
    digest(&SHA256, bytes).as_ref().iter().map(|byte| format!("{byte:02x}")).collect()
}

fn next_temp_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Debug)]
pub(crate) struct WorkspaceFileReadError {
    kind: WorkspaceFileReadErrorKind,
}

impl WorkspaceFileReadError {
    pub(crate) fn kind(&self) -> WorkspaceFileReadErrorKind {
        self.kind
    }

    fn new(kind: WorkspaceFileReadErrorKind) -> Self {
        Self { kind }
    }

    fn from_io(error: &io::Error) -> Self {
        Self::new(errno_kind(error.raw_os_error()))
    }
}

/// Reads a regular file by walking from a retained workspace root descriptor.
/// No caller-controlled component is resolved through an absolute path.
pub(crate) fn read_workspace_file(
    canonical_root: &Path,
    repository_path: &RepositoryPath,
) -> Result<WorkspaceFileBytes, WorkspaceFileReadError> {
    let components = repository_components(repository_path)?;
    let mut parent = open_root(canonical_root)?;

    for component in &components[..components.len() - 1] {
        parent = open_relative(&parent, component, true)?;
    }
    let file = open_relative(&parent, &components[components.len() - 1], false)?;
    read_bounded(file)
}

fn repository_components(
    repository_path: &RepositoryPath,
) -> Result<Vec<CString>, WorkspaceFileReadError> {
    let value = repository_path.as_bytes();
    if value.is_empty() || value.starts_with(b"/") || value.contains(&0) {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::InvalidPath,
        ));
    }
    value
        .split(|byte| *byte == b'/')
        .map(|component| {
            if component.is_empty() || component == b"." || component == b".." {
                return Err(WorkspaceFileReadError::new(
                    WorkspaceFileReadErrorKind::InvalidPath,
                ));
            }
            CString::new(component).map_err(|_| {
                WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath)
            })
        })
        .collect()
}

fn open_root(path: &Path) -> Result<OwnedFd, WorkspaceFileReadError> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(|_| {
        WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath)
    })?;
    let raw = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(WorkspaceFileReadError::from_io(
            &io::Error::last_os_error(),
        ));
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
    ensure_descriptor_type(&descriptor, true)?;
    Ok(descriptor)
}

fn open_relative(
    parent: &OwnedFd,
    name: &CString,
    expect_directory: bool,
) -> Result<OwnedFd, WorkspaceFileReadError> {
    let mut flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    if expect_directory {
        flags |= libc::O_DIRECTORY;
    }
    let raw = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if raw < 0 {
        let error = io::Error::last_os_error();
        return Err(open_relative_error(parent, name, expect_directory, error));
    }
    let descriptor = unsafe { OwnedFd::from_raw_fd(raw) };
    ensure_descriptor_type(&descriptor, expect_directory)?;
    Ok(descriptor)
}

fn open_relative_error(
    parent: &OwnedFd,
    name: &CString,
    expect_directory: bool,
    error: io::Error,
) -> WorkspaceFileReadError {
    if let Some(kind) = entry_kind(parent, name) {
        if kind == libc::S_IFLNK {
            return WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::ReparsePoint);
        }
        let expected = if expect_directory {
            libc::S_IFDIR
        } else {
            libc::S_IFREG
        };
        if kind != expected {
            return WorkspaceFileReadError::new(
                WorkspaceFileReadErrorKind::NotRegularFile,
            );
        }
    }
    WorkspaceFileReadError::from_io(&error)
}

fn entry_kind(parent: &OwnedFd, name: &CString) -> Option<libc::mode_t> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result != 0 {
        return None;
    }
    let stat = unsafe { stat.assume_init() };
    Some(stat.st_mode & libc::S_IFMT)
}

fn ensure_descriptor_type(
    descriptor: &OwnedFd,
    expect_directory: bool,
) -> Result<(), WorkspaceFileReadError> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(descriptor.as_raw_fd(), stat.as_mut_ptr()) } != 0 {
        return Err(WorkspaceFileReadError::from_io(
            &io::Error::last_os_error(),
        ));
    }
    let stat = unsafe { stat.assume_init() };
    let kind = stat.st_mode & libc::S_IFMT;
    let expected = if expect_directory {
        libc::S_IFDIR
    } else {
        libc::S_IFREG
    };
    if kind != expected {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::NotRegularFile,
        ));
    }
    Ok(())
}

fn read_bounded(descriptor: OwnedFd) -> Result<WorkspaceFileBytes, WorkspaceFileReadError> {
    let mut file = File::from(descriptor);
    let mut bytes = Vec::with_capacity(MAX_WORKSPACE_FILE_BYTES.min(16 * 1_024));
    Read::by_ref(&mut file)
        .take((MAX_WORKSPACE_FILE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| WorkspaceFileReadError::from_io(&error))?;
    if bytes.len() > MAX_WORKSPACE_FILE_BYTES {
        return Ok(WorkspaceFileBytes::TooLarge);
    }
    let byte_length = bytes.len() as u64;
    match String::from_utf8(bytes) {
        Ok(text) => Ok(WorkspaceFileBytes::Utf8(text)),
        Err(_) => Ok(WorkspaceFileBytes::NonUtf8 { byte_length }),
    }
}

fn errno_kind(errno: Option<i32>) -> WorkspaceFileReadErrorKind {
    match errno {
        Some(libc::ENOENT) => WorkspaceFileReadErrorKind::NotFound,
        Some(libc::EACCES) | Some(libc::EPERM) => WorkspaceFileReadErrorKind::AccessDenied,
        Some(libc::ELOOP) => WorkspaceFileReadErrorKind::ReparsePoint,
        Some(libc::ENOTDIR) | Some(libc::EISDIR) => {
            WorkspaceFileReadErrorKind::NotRegularFile
        }
        Some(libc::EINVAL) | Some(libc::ENAMETOOLONG) => {
            WorkspaceFileReadErrorKind::InvalidPath
        }
        _ => WorkspaceFileReadErrorKind::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TestRoot(PathBuf);

    impl TestRoot {
        fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(1);
            let root = std::env::temp_dir().join(format!(
                "gate4agent-workspace-entry-create-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed),
            ));
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for TestRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn path(value: &str) -> RepositoryPath {
        RepositoryPath::utf8(value.to_owned()).unwrap()
    }

    #[test]
    fn workspace_entry_create_is_handle_relative_and_no_overwrite() {
        let root = TestRoot::new();
        std::fs::create_dir(root.0.join("src")).unwrap();

        let revision = create_workspace_file(
            &root.0,
            &path("src/new.rs"),
            &AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING),
        )
        .unwrap();
        assert_eq!(revision, sha256_hex(&[]));
        assert_eq!(std::fs::read(root.0.join("src/new.rs")).unwrap(), Vec::<u8>::new());
        assert_eq!(
            create_workspace_file(
                &root.0,
                &path("src/new.rs"),
                &AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING),
            )
            .unwrap_err()
            .kind(),
            WorkspaceFileReadErrorKind::AlreadyExists,
        );

        create_workspace_directory(
            &root.0,
            &path("src/new-dir"),
            &AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING),
        )
        .unwrap();
        assert!(root.0.join("src/new-dir").is_dir());
        assert_eq!(
            create_workspace_directory(
                &root.0,
                &path("src/new-dir"),
                &AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING),
            )
            .unwrap_err()
            .kind(),
            WorkspaceFileReadErrorKind::AlreadyExists,
        );
        assert_eq!(
            create_workspace_file(
                &root.0,
                &path("missing/new.rs"),
                &AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING),
            )
            .unwrap_err()
            .kind(),
            WorkspaceFileReadErrorKind::ParentNotFound,
        );
        assert_eq!(
            create_workspace_file(
                &root.0,
                &path("src/new.rs/child"),
                &AtomicU8::new(WORKSPACE_ENTRY_CREATE_PENDING),
            )
            .unwrap_err()
            .kind(),
            WorkspaceFileReadErrorKind::ParentNotDirectory,
        );
    }
}

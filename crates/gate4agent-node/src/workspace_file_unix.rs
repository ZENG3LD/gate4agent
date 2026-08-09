use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::protocol::{RepositoryPath, MAX_WORKSPACE_FILE_BYTES};

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
    Io,
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
    file.by_ref()
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

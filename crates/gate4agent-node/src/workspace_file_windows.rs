use std::ffi::{c_void, OsStr, OsString};
use std::io::{self, Read, Write};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU8, Ordering};

use crate::protocol::{RepositoryPath, MAX_WORKSPACE_FILE_BYTES};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, GetFinalPathNameByHandleW, ReplaceFileW,
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_LIST_DIRECTORY,
    FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING, REPLACEFILE_WRITE_THROUGH, VOLUME_NAME_DOS,
};
use ring::digest::{digest, SHA256};

pub(crate) const WORKSPACE_ENTRY_CREATE_PENDING: u8 = 0;
pub(crate) const WORKSPACE_ENTRY_CREATE_COMMITTING: u8 = 1;
pub(crate) const WORKSPACE_ENTRY_CREATE_CANCELED: u8 = 2;

const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
const FILE_OPEN: u32 = 1;
const FILE_CREATE: u32 = 2;
const FILE_WRITE_DATA_ACCESS: u32 = 0x0000_0002;
const FILE_DIRECTORY_FILE: u32 = 0x0000_0001;
const FILE_SYNCHRONOUS_IO_NONALERT: u32 = 0x0000_0020;
const FILE_NON_DIRECTORY_FILE: u32 = 0x0000_0040;
const FILE_OPEN_REPARSE_POINT_OPTION: u32 = 0x0020_0000;
const OBJ_CASE_INSENSITIVE: u32 = 0x0000_0040;

const STATUS_NO_SUCH_FILE: i32 = 0xC000_000Fu32 as i32;
const STATUS_ACCESS_DENIED: i32 = 0xC000_0022u32 as i32;
const STATUS_OBJECT_NAME_INVALID: i32 = 0xC000_0033u32 as i32;
const STATUS_OBJECT_NAME_NOT_FOUND: i32 = 0xC000_0034u32 as i32;
const STATUS_OBJECT_NAME_COLLISION: i32 = 0xC000_0035u32 as i32;
const STATUS_OBJECT_PATH_NOT_FOUND: i32 = 0xC000_003Au32 as i32;
const STATUS_SHARING_VIOLATION: i32 = 0xC000_0043u32 as i32;
const STATUS_FILE_IS_A_DIRECTORY: i32 = 0xC000_00BAu32 as i32;
const STATUS_NOT_A_DIRECTORY: i32 = 0xC000_0103u32 as i32;

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
    let repository_path = repository_path.as_utf8().ok_or_else(|| {
        WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath)
    })?;
    let components = validate_repository_path(repository_path)?;
    let (parent, name) = open_create_parent(canonical_root, &components)?;
    begin_create_commit(commit_state)?;
    let handle = create_relative(&parent, name, false)?;
    let raw = handle.into_raw_handle();
    let file = unsafe { std::fs::File::from_raw_handle(raw) };
    file.sync_all()
        .map_err(|error| WorkspaceFileReadError::from_io(&error))?;
    Ok(sha256_hex(&[]))
}

pub(crate) fn create_workspace_directory(
    canonical_root: &Path,
    repository_path: &RepositoryPath,
    commit_state: &AtomicU8,
) -> Result<(), WorkspaceFileReadError> {
    let repository_path = repository_path.as_utf8().ok_or_else(|| {
        WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath)
    })?;
    let components = validate_repository_path(repository_path)?;
    let (parent, name) = open_create_parent(canonical_root, &components)?;
    begin_create_commit(commit_state)?;
    let _directory = create_relative(&parent, name, true)?;
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
    components: &'a [&OsStr],
) -> Result<(OwnedHandle, &'a OsStr), WorkspaceFileReadError> {
    let mut parent = open_verified_root(canonical_root)?;
    for component in &components[..components.len() - 1] {
        parent = open_relative(&parent, component, true).map_err(parent_create_error)?;
    }
    Ok((parent, components[components.len() - 1]))
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

fn create_relative(
    parent: &OwnedHandle,
    name: &OsStr,
    directory: bool,
) -> Result<OwnedHandle, WorkspaceFileReadError> {
    let mut name = simple_name_wide(name)?;
    let byte_length = name
        .len()
        .checked_mul(2)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath))?;
    let mut unicode = UnicodeString {
        length: byte_length,
        maximum_length: byte_length,
        buffer: name.as_mut_ptr(),
    };
    let mut attributes = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: parent.0,
        object_name: &mut unicode,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut io_status = IoStatusBlock { status: 0, information: 0 };
    let mut handle = std::ptr::null_mut();
    let desired_access = if directory {
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES
    } else {
        FILE_WRITE_DATA_ACCESS | FILE_READ_ATTRIBUTES
    };
    let type_option = if directory { FILE_DIRECTORY_FILE } else { FILE_NON_DIRECTORY_FILE };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access | SYNCHRONIZE_ACCESS,
            &mut attributes,
            &mut io_status,
            std::ptr::null_mut(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_CREATE,
            type_option | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT_OPTION,
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return Err(WorkspaceFileReadError::new(match nt_status_kind(status) {
            WorkspaceFileReadErrorKind::NotFound => WorkspaceFileReadErrorKind::ParentNotFound,
            kind => kind,
        }));
    }
    let handle = OwnedHandle(handle);
    ensure_handle_type(&handle, directory)?;
    Ok(handle)
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
    let repository_path = repository_path.as_utf8().ok_or_else(|| {
        WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath)
    })?;
    let components = validate_repository_path(repository_path)?;
    let root = open_verified_root(canonical_root)?;
    let mut parent = root;
    for component in &components[..components.len() - 1] {
        parent = open_relative(&parent, component, true)?;
    }
    let name = components[components.len() - 1];
    let original = open_relative(&parent, name, false)?;
    let original_path = final_path_for_handle(&original)?;
    let current = read_bounded(original)?;
    let WorkspaceFileBytes::Utf8(current) = current else {
        return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::NotRegularFile));
    };
    if sha256_hex(current.as_bytes()) != expected_revision {
        return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::RevisionConflict));
    }

    let parent_path = final_path_for_handle(&parent)?;
    let temp_name = format!(".gate4agent-save-{}-{}", std::process::id(), next_temp_id());
    let temp_path = parent_path.join(temp_name);
    let result = (|| {
        let mut temp = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|error| WorkspaceFileReadError::from_io(&error))?;
        temp.write_all(text.as_bytes())
            .and_then(|_| temp.sync_all())
            .map_err(|error| WorkspaceFileReadError::from_io(&error))?;
        drop(temp);

        let current = open_relative(&parent, name, false)?;
        if normalized_windows_path(&final_path_for_handle(&current)?)
            != normalized_windows_path(&original_path)
        {
            return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::RevisionConflict));
        }
        let WorkspaceFileBytes::Utf8(current) = read_bounded(current)? else {
            return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::RevisionConflict));
        };
        if sha256_hex(current.as_bytes()) != expected_revision {
            return Err(WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::RevisionConflict));
        }

        let target_wide = wide_nul(&original_path)?;
        let temp_wide = wide_nul(&temp_path)?;
        let replaced = unsafe {
            ReplaceFileW(
                target_wide.as_ptr(),
                temp_wide.as_ptr(),
                std::ptr::null(),
                REPLACEFILE_WRITE_THROUGH,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if replaced == 0 {
            return Err(WorkspaceFileReadError::from_io(&io::Error::last_os_error()));
        }
        Ok(sha256_hex(text.as_bytes()))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
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
        let kind = match error.kind() {
            io::ErrorKind::NotFound => WorkspaceFileReadErrorKind::NotFound,
            io::ErrorKind::PermissionDenied => WorkspaceFileReadErrorKind::AccessDenied,
            _ => WorkspaceFileReadErrorKind::Io,
        };
        Self::new(kind)
    }
}

/// Reads one regular workspace file without resolving any caller-controlled
/// component through Win32 path parsing.
///
/// The registered root is opened once and verified. Every descendant is then
/// opened relative to its retained parent handle with reparse traversal
/// disabled. The final bytes are read from that same verified handle.
pub(crate) fn read_workspace_file(
    canonical_root: &Path,
    repository_path: &RepositoryPath,
) -> Result<WorkspaceFileBytes, WorkspaceFileReadError> {
    let repository_path = repository_path.as_utf8().ok_or_else(|| {
        WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath)
    })?;
    let components = validate_repository_path(repository_path)?;
    let root = open_verified_root(canonical_root)?;
    let mut parent = root;

    for component in &components[..components.len() - 1] {
        parent = open_relative(&parent, component, true)?;
    }
    let file = open_relative(&parent, components[components.len() - 1], false)?;
    read_bounded(file)
}

fn validate_repository_path(value: &str) -> Result<Vec<&OsStr>, WorkspaceFileReadError> {
    if value.contains(['\\', ':']) {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::UnsafePath,
        ));
    }
    if value.is_empty() || value.contains('\0') {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::InvalidPath,
        ));
    }
    let components = value
        .split('/')
        .map(OsStr::new)
        .collect::<Vec<_>>();
    if components.is_empty()
        || components.iter().any(|component| {
            component.is_empty() || *component == OsStr::new(".") || *component == OsStr::new("..")
        })
    {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::InvalidPath,
        ));
    }
    Ok(components)
}

fn open_verified_root(path: &Path) -> Result<OwnedHandle, WorkspaceFileReadError> {
    let wide = wide_nul(path)?;
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(WorkspaceFileReadError::from_io(&io::Error::last_os_error()));
    }
    let handle = OwnedHandle(handle);
    ensure_handle_type(&handle, true)?;
    let final_path = final_path_for_handle(&handle)?;
    if normalized_windows_path(path) != normalized_windows_path(&final_path) {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::ReparsePoint,
        ));
    }
    Ok(handle)
}

fn open_relative(
    parent: &OwnedHandle,
    name: &OsStr,
    expect_directory: bool,
) -> Result<OwnedHandle, WorkspaceFileReadError> {
    let mut name = simple_name_wide(name)?;
    let byte_length = name
        .len()
        .checked_mul(2)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| WorkspaceFileReadError::new(WorkspaceFileReadErrorKind::InvalidPath))?;
    let mut unicode = UnicodeString {
        length: byte_length,
        maximum_length: byte_length,
        buffer: name.as_mut_ptr(),
    };
    let mut attributes = ObjectAttributes {
        length: std::mem::size_of::<ObjectAttributes>() as u32,
        root_directory: parent.0,
        object_name: &mut unicode,
        attributes: OBJ_CASE_INSENSITIVE,
        security_descriptor: std::ptr::null_mut(),
        security_quality_of_service: std::ptr::null_mut(),
    };
    let mut io_status = IoStatusBlock {
        status: 0,
        information: 0,
    };
    let mut handle = std::ptr::null_mut();
    let desired_access = if expect_directory {
        FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES
    } else {
        FILE_READ_DATA | FILE_READ_ATTRIBUTES
    };
    let type_option = if expect_directory {
        FILE_DIRECTORY_FILE
    } else {
        FILE_NON_DIRECTORY_FILE
    };
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            desired_access | SYNCHRONIZE_ACCESS,
            &mut attributes,
            &mut io_status,
            std::ptr::null_mut(),
            0,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            type_option | FILE_SYNCHRONOUS_IO_NONALERT | FILE_OPEN_REPARSE_POINT_OPTION,
            std::ptr::null_mut(),
            0,
        )
    };
    if status < 0 {
        return Err(WorkspaceFileReadError::new(nt_status_kind(status)));
    }
    let handle = OwnedHandle(handle);
    ensure_handle_type(&handle, expect_directory)?;
    Ok(handle)
}

fn read_bounded(handle: OwnedHandle) -> Result<WorkspaceFileBytes, WorkspaceFileReadError> {
    let raw = handle.into_raw_handle();
    let mut file = unsafe { std::fs::File::from_raw_handle(raw) };
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

fn simple_name_wide(name: &OsStr) -> Result<Vec<u16>, WorkspaceFileReadError> {
    let wide = name.encode_wide().collect::<Vec<_>>();
    if wide.is_empty()
        || wide.contains(&0)
        || wide.contains(&('/' as u16))
        || wide.contains(&('\\' as u16))
        || wide.contains(&(':' as u16))
        || wide == ['.' as u16]
        || wide == ['.' as u16, '.' as u16]
    {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::InvalidPath,
        ));
    }
    Ok(wide)
}

fn ensure_handle_type(
    handle: &OwnedHandle,
    expect_directory: bool,
) -> Result<(), WorkspaceFileReadError> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    let success = unsafe { GetFileInformationByHandle(handle.0, &mut information) };
    if success == 0 {
        return Err(WorkspaceFileReadError::from_io(&io::Error::last_os_error()));
    }
    if information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::ReparsePoint,
        ));
    }
    let is_directory = information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0;
    if is_directory != expect_directory {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::NotRegularFile,
        ));
    }
    Ok(())
}

fn final_path_for_handle(handle: &OwnedHandle) -> Result<PathBuf, WorkspaceFileReadError> {
    let mut buffer = vec![0u16; 512];
    loop {
        let length = unsafe {
            GetFinalPathNameByHandleW(
                handle.0,
                buffer.as_mut_ptr(),
                buffer.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_DOS,
            )
        } as usize;
        if length == 0 {
            return Err(WorkspaceFileReadError::from_io(&io::Error::last_os_error()));
        }
        if length < buffer.len() {
            return Ok(PathBuf::from(OsString::from_wide(&buffer[..length])));
        }
        buffer.resize(length + 1, 0);
    }
}

fn wide_nul(path: &Path) -> Result<Vec<u16>, WorkspaceFileReadError> {
    let mut wide = path.as_os_str().encode_wide().collect::<Vec<_>>();
    if wide.contains(&0) {
        return Err(WorkspaceFileReadError::new(
            WorkspaceFileReadErrorKind::InvalidPath,
        ));
    }
    wide.push(0);
    Ok(wide)
}

fn normalized_windows_path(path: &Path) -> Vec<u16> {
    let mut units = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let verbatim = "\\\\?\\".encode_utf16().collect::<Vec<_>>();
    let verbatim_unc = "\\\\?\\UNC\\".encode_utf16().collect::<Vec<_>>();
    if starts_with_ascii_case_insensitive(&units, &verbatim_unc) {
        let mut unc = vec!['\\' as u16, '\\' as u16];
        unc.extend_from_slice(&units[verbatim_unc.len()..]);
        units = unc;
    } else if starts_with_ascii_case_insensitive(&units, &verbatim) {
        units.drain(..verbatim.len());
    }
    for unit in &mut units {
        if *unit == '/' as u16 {
            *unit = '\\' as u16;
        } else if *unit <= u8::MAX as u16 && (*unit as u8).is_ascii_uppercase() {
            *unit = (*unit as u8).to_ascii_lowercase() as u16;
        }
    }
    while units.len() > 3 && units.last() == Some(&('\\' as u16)) {
        units.pop();
    }
    units
}

fn starts_with_ascii_case_insensitive(value: &[u16], prefix: &[u16]) -> bool {
    value.len() >= prefix.len()
        && value.iter().zip(prefix).all(|(left, right)| {
            *left <= u8::MAX as u16
                && *right <= u8::MAX as u16
                && (*left as u8).eq_ignore_ascii_case(&(*right as u8))
        })
}

fn nt_status_kind(status: i32) -> WorkspaceFileReadErrorKind {
    match status {
        STATUS_NO_SUCH_FILE | STATUS_OBJECT_NAME_NOT_FOUND | STATUS_OBJECT_PATH_NOT_FOUND => {
            WorkspaceFileReadErrorKind::NotFound
        }
        STATUS_ACCESS_DENIED | STATUS_SHARING_VIOLATION => {
            WorkspaceFileReadErrorKind::AccessDenied
        }
        STATUS_OBJECT_NAME_INVALID => WorkspaceFileReadErrorKind::InvalidPath,
        STATUS_OBJECT_NAME_COLLISION => WorkspaceFileReadErrorKind::AlreadyExists,
        STATUS_FILE_IS_A_DIRECTORY | STATUS_NOT_A_DIRECTORY => {
            WorkspaceFileReadErrorKind::NotRegularFile
        }
        _ => WorkspaceFileReadErrorKind::Io,
    }
}

#[repr(C)]
struct UnicodeString {
    length: u16,
    maximum_length: u16,
    buffer: *mut u16,
}

#[repr(C)]
struct ObjectAttributes {
    length: u32,
    root_directory: HANDLE,
    object_name: *mut UnicodeString,
    attributes: u32,
    security_descriptor: *mut c_void,
    security_quality_of_service: *mut c_void,
}

#[repr(C)]
struct IoStatusBlock {
    status: isize,
    information: usize,
}

#[link(name = "ntdll")]
extern "system" {
    fn NtCreateFile(
        file_handle: *mut HANDLE,
        desired_access: u32,
        object_attributes: *mut ObjectAttributes,
        io_status_block: *mut IoStatusBlock,
        allocation_size: *mut i64,
        file_attributes: u32,
        share_access: u32,
        create_disposition: u32,
        create_options: u32,
        ea_buffer: *mut c_void,
        ea_length: u32,
    ) -> i32;
}

#[derive(Debug)]
struct OwnedHandle(HANDLE);

impl OwnedHandle {
    fn into_raw_handle(mut self) -> RawHandle {
        let handle = self.0;
        self.0 = INVALID_HANDLE_VALUE;
        handle as RawHandle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE && !self.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
            Self(std::fs::canonicalize(root).unwrap())
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

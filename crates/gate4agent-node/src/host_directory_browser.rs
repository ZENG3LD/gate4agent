use crate::platform;
use crate::protocol::{
    HostDirectoryEntry, HostDirectoryListing, OpaqueHostPath, MAX_HOST_DIRECTORY_ENTRIES,
    MAX_WORKSPACE_ROOT_BYTES,
};
use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HostDirectoryBrowseErrorKind {
    Invalid,
    ReadFailed,
}

#[derive(Debug)]
pub(crate) struct HostDirectoryBrowseError {
    kind: HostDirectoryBrowseErrorKind,
}

impl HostDirectoryBrowseError {
    pub(crate) fn kind(&self) -> HostDirectoryBrowseErrorKind {
        self.kind
    }

    fn invalid() -> Self {
        Self {
            kind: HostDirectoryBrowseErrorKind::Invalid,
        }
    }

    fn read_failed() -> Self {
        Self {
            kind: HostDirectoryBrowseErrorKind::ReadFailed,
        }
    }
}

pub(crate) fn browse_host_directories(
    directory: Option<OpaqueHostPath>,
    after: Option<OpaqueHostPath>,
) -> Result<HostDirectoryListing, HostDirectoryBrowseError> {
    let after = after
        .as_ref()
        .map(validate_cursor)
        .transpose()?;
    let (directory, parent, mut entries) = match directory {
        Some(directory) => {
            let directory = canonical_directory(&directory)?;
            let parent = canonical_parent(&directory);
            let entries = read_child_directories(&directory, after.as_ref())?;
            (
                Some(opaque_path(directory)?),
                parent.map(opaque_path).transpose()?,
                entries,
            )
        }
        None => {
            let entries = virtual_root_entries()?;
            (None, None, entries)
        }
    };

    entries.sort_by(host_directory_entry_cmp);
    if let Some(after) = after.as_ref() {
        entries.retain(|entry| native_path_cmp(&entry.path, after).is_gt());
    }
    let has_more = entries.len() > MAX_HOST_DIRECTORY_ENTRIES;
    if has_more {
        entries.truncate(MAX_HOST_DIRECTORY_ENTRIES);
    }
    let next_after = has_more
        .then(|| entries.last().map(|entry| entry.path.clone()))
        .flatten();

    Ok(HostDirectoryListing {
        directory,
        parent,
        entries,
        next_after,
        incomplete: has_more,
    })
}

fn validate_cursor(path: &OpaqueHostPath) -> Result<OpaqueHostPath, HostDirectoryBrowseError> {
    let text = path.as_utf8().ok_or_else(HostDirectoryBrowseError::invalid)?;
    validate_supported_absolute_path(text)?;
    Ok(path.clone())
}

fn canonical_directory(path: &OpaqueHostPath) -> Result<String, HostDirectoryBrowseError> {
    let text = path.as_utf8().ok_or_else(HostDirectoryBrowseError::invalid)?;
    validate_supported_absolute_path(text)?;
    let canonical = fs::canonicalize(text).map_err(|_| HostDirectoryBrowseError::invalid())?;
    if !canonical.is_dir() {
        return Err(HostDirectoryBrowseError::invalid());
    }
    canonical_path_text(canonical)
}

fn canonical_parent(directory: &str) -> Option<String> {
    let parent = Path::new(directory).parent()?;
    if parent == Path::new(directory) || parent.as_os_str().is_empty() {
        return None;
    }
    canonical_path_text(parent.to_path_buf()).ok()
}

fn read_child_directories(
    directory: &str,
    after: Option<&OpaqueHostPath>,
) -> Result<Vec<HostDirectoryEntry>, HostDirectoryBrowseError> {
    let reader = fs::read_dir(directory).map_err(|_| HostDirectoryBrowseError::read_failed())?;
    let mut entries = Vec::new();
    for entry in reader {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        let is_link = file_type.is_symlink();
        let is_directory = if file_type.is_dir() {
            true
        } else if is_link {
            match entry.metadata() {
                Ok(metadata) => metadata.is_dir(),
                Err(_) => false,
            }
        } else {
            false
        };
        if !is_directory {
            continue;
        }
        let display_name = match entry.file_name().into_string() {
            Ok(name) if !name.is_empty() && !name.chars().any(char::is_control) => name,
            _ => continue,
        };
        let canonical = match fs::canonicalize(entry.path())
            .map_err(|_| HostDirectoryBrowseError::read_failed())
            .and_then(canonical_path_text)
        {
            Ok(canonical) => canonical,
            Err(_) => continue,
        };
        let entry = match HostDirectoryEntry::new(
            opaque_path(canonical)?,
            display_name,
            is_link,
        ) {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        if after.is_some_and(|after| !native_path_cmp(&entry.path, after).is_gt()) {
            continue;
        }
        push_bounded_entry(&mut entries, entry);
    }
    Ok(entries)
}

fn push_bounded_entry(entries: &mut Vec<HostDirectoryEntry>, entry: HostDirectoryEntry) {
    if let Ok(index) = entries.binary_search_by(|current| native_path_cmp(&current.path, &entry.path)) {
        if host_directory_entry_cmp(&entry, &entries[index]).is_lt() {
            entries[index] = entry;
        }
        return;
    }
    let index = entries
        .binary_search_by(|current| host_directory_entry_cmp(current, &entry))
        .unwrap_or_else(|index| index);
    entries.insert(index, entry);
    if entries.len() > MAX_HOST_DIRECTORY_ENTRIES + 1 {
        entries.pop();
    }
}

fn canonical_path_text(path: PathBuf) -> Result<String, HostDirectoryBrowseError> {
    let text = path
        .into_os_string()
        .into_string()
        .map_err(|_| HostDirectoryBrowseError::invalid())?;
    let text = platform::normalize_canonical_root(text);
    validate_supported_absolute_path(&text)?;
    Ok(text)
}

fn opaque_path(path: String) -> Result<OpaqueHostPath, HostDirectoryBrowseError> {
    OpaqueHostPath::utf8(path).map_err(|_| HostDirectoryBrowseError::invalid())
}

fn validate_supported_absolute_path(path: &str) -> Result<(), HostDirectoryBrowseError> {
    #[cfg(windows)]
    if path.starts_with(r"\\") {
        return Err(HostDirectoryBrowseError::invalid());
    }
    if path.len() > MAX_WORKSPACE_ROOT_BYTES
        || !platform::workspace_root_supported(path)
    {
        return Err(HostDirectoryBrowseError::invalid());
    }
    Ok(())
}

fn native_path_cmp(left: &OpaqueHostPath, right: &OpaqueHostPath) -> Ordering {
    let left = left
        .as_utf8()
        .expect("host directory browser only emits UTF-8 paths");
    let right = right
        .as_utf8()
        .expect("host directory browser only emits UTF-8 paths");
    native_text_cmp(left, right)
}

fn host_directory_entry_cmp(
    left: &HostDirectoryEntry,
    right: &HostDirectoryEntry,
) -> Ordering {
    native_path_cmp(&left.path, &right.path)
        .then_with(|| left.is_link.cmp(&right.is_link))
        .then_with(|| native_text_cmp(&left.display_name, &right.display_name))
}

fn native_text_cmp(left: &str, right: &str) -> Ordering {
    #[cfg(windows)]
    {
        return left
            .bytes()
            .map(|byte| byte.to_ascii_lowercase())
            .cmp(right.bytes().map(|byte| byte.to_ascii_lowercase()));
    }
    #[cfg(not(windows))]
    {
        left.as_bytes().cmp(right.as_bytes())
    }
}

#[cfg(windows)]
fn virtual_root_entries() -> Result<Vec<HostDirectoryEntry>, HostDirectoryBrowseError> {
    let drives = unsafe { windows_sys::Win32::Storage::FileSystem::GetLogicalDrives() };
    if drives == 0 {
        return Err(HostDirectoryBrowseError::read_failed());
    }
    let mut entries = Vec::new();
    for index in 0..26_u32 {
        if drives & (1 << index) == 0 {
            continue;
        }
        let letter = char::from_u32(u32::from(b'A') + index)
            .expect("logical drive index must map to ASCII");
        let display_name = format!(r"{letter}:\");
        let canonical = match fs::canonicalize(&display_name)
            .map_err(|_| HostDirectoryBrowseError::read_failed())
            .and_then(canonical_path_text)
        {
            Ok(canonical) => canonical,
            Err(_) => continue,
        };
        entries.push(HostDirectoryEntry::new(
            opaque_path(canonical)?,
            display_name,
            false,
        ).map_err(|_| HostDirectoryBrowseError::invalid())?);
    }
    Ok(entries)
}

#[cfg(unix)]
fn virtual_root_entries() -> Result<Vec<HostDirectoryEntry>, HostDirectoryBrowseError> {
    let canonical = canonical_path_text(
        fs::canonicalize("/").map_err(|_| HostDirectoryBrowseError::read_failed())?,
    )?;
    Ok(vec![HostDirectoryEntry::new(
        opaque_path(canonical)?,
        "/".to_owned(),
        false,
    ).map_err(|_| HostDirectoryBrowseError::invalid())?])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

    fn temporary_directory(label: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, AtomicOrdering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "gate4agent-host-directory-{label}-{}-{id}",
            std::process::id(),
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn host_directory_browse_is_sorted_bounded_and_cursor_native() {
        let root = temporary_directory("page");
        for index in (0..=MAX_HOST_DIRECTORY_ENTRIES).rev() {
            fs::create_dir(root.join(format!("directory-{index:03}"))).unwrap();
        }
        fs::write(root.join("ordinary-file"), b"not a directory").unwrap();

        let root_path = OpaqueHostPath::utf8(root.to_string_lossy().into_owned()).unwrap();
        let first = browse_host_directories(Some(root_path.clone()), None).unwrap();
        assert_eq!(first.entries.len(), MAX_HOST_DIRECTORY_ENTRIES);
        assert!(first.incomplete);
        assert_eq!(first.next_after, first.entries.last().map(|entry| entry.path.clone()));
        assert!(first.entries.windows(2).all(|pair| {
            native_path_cmp(&pair[0].path, &pair[1].path).is_lt()
        }));

        let second = browse_host_directories(Some(root_path), first.next_after).unwrap();
        assert_eq!(second.entries.len(), 1);
        assert!(!second.incomplete);
        assert_eq!(second.next_after, None);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn host_directory_page_boundary_deduplicates_canonical_aliases() {
        let mut entries = Vec::new();
        for index in 0..=MAX_HOST_DIRECTORY_ENTRIES {
            let path = opaque_path(format!("/directory-{index:03}")).unwrap();
            push_bounded_entry(
                &mut entries,
                HostDirectoryEntry::new(path, format!("directory-{index:03}"), false).unwrap(),
            );
        }
        let aliased_path = entries[MAX_HOST_DIRECTORY_ENTRIES - 1].path.clone();
        push_bounded_entry(
            &mut entries,
            HostDirectoryEntry::new(aliased_path.clone(), "zz-alias".to_owned(), true).unwrap(),
        );

        assert_eq!(entries.len(), MAX_HOST_DIRECTORY_ENTRIES + 1);
        assert_eq!(
            entries.iter().filter(|entry| entry.path == aliased_path).count(),
            1,
        );
    }

    #[test]
    fn host_directory_browse_rejects_non_absolute_input() {
        let relative = OpaqueHostPath::utf8("relative-directory".to_owned()).unwrap();
        let error = browse_host_directories(Some(relative), None).unwrap_err();
        assert_eq!(error.kind(), HostDirectoryBrowseErrorKind::Invalid);
    }

    #[test]
    fn host_directory_browse_virtual_roots_are_canonical() {
        let listing = browse_host_directories(None, None).unwrap();
        assert_eq!(listing.directory, None);
        assert_eq!(listing.parent, None);
        assert!(!listing.entries.is_empty());
        assert!(listing.entries.iter().all(|entry| {
            let path = entry.path.as_utf8().unwrap();
            Path::new(path).is_absolute()
                && !path.chars().any(char::is_control)
                && !entry.is_link
        }));
        #[cfg(windows)]
        assert!(listing
            .entries
            .iter()
            .all(|entry| !entry.path.as_utf8().unwrap().starts_with(r"\\")));
        #[cfg(unix)]
        assert_eq!(listing.entries[0].path.as_utf8(), Some("/"));
    }

    #[cfg(unix)]
    #[test]
    fn host_directory_browse_marks_directory_links() {
        use std::os::unix::fs::symlink;

        let root = temporary_directory("link");
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        symlink(&target, root.join("alias")).unwrap();
        let root_path = OpaqueHostPath::utf8(root.to_string_lossy().into_owned()).unwrap();

        let listing = browse_host_directories(Some(root_path), None).unwrap();
        assert!(listing.entries.iter().any(|entry| {
            entry.display_name == "alias" && entry.is_link
        }));
        fs::remove_dir_all(root).unwrap();
    }
}

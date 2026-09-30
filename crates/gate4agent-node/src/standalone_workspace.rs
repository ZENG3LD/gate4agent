use std::fs::{File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::time::timeout;

const GIT_INIT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StandaloneWorkspaceErrorKind {
    InvalidRoot,
    InvalidInitialBranch,
    GitInitializationFailed,
    RecoveryRequired,
}

#[derive(Debug)]
pub(crate) struct StandaloneWorkspaceError {
    pub kind: StandaloneWorkspaceErrorKind,
    pub message: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileSystemIdentity {
    volume: u64,
    file: u64,
}

struct GuardedDirectory {
    handle: File,
    identity: FileSystemIdentity,
}

struct PreparedDirectory {
    directory_created: bool,
    root_guard: GuardedDirectory,
}

pub(crate) struct PreparedStandaloneWorkspace {
    root: String,
    root_guard: GuardedDirectory,
    git_guard: GuardedDirectory,
}

impl PreparedStandaloneWorkspace {
    pub(crate) fn root(&self) -> &str {
        &self.root
    }

    pub(crate) async fn verify_for_registration(&self) -> Result<(), StandaloneWorkspaceError> {
        let root = PathBuf::from(&self.root);
        let root_identity = self.root_guard.identity;
        let git_identity = self.git_guard.identity;
        tokio::task::spawn_blocking(move || {
            verify_exact_independent_repository(&root, root_identity, Some(git_identity))
        })
        .await
        .map_err(|_| recovery_error())?
        .map(|_| ())
        .map_err(|_| recovery_error())
    }

    pub(crate) async fn compensate_registration_failure(self) -> bool {
        let root = PathBuf::from(self.root);
        let root_guard = self.root_guard;
        let git_guard = self.git_guard;
        tokio::task::spawn_blocking(move || {
            let root_identity = root_guard.identity;
            let git_identity = git_guard.identity;
            let _guards = (root_guard.handle, git_guard.handle);
            verify_exact_independent_repository(
                &root,
                root_identity,
                Some(git_identity),
            )
        })
        .await
        .ok();
        false
    }
}

pub(crate) async fn prepare_standalone_workspace(
    root: String,
    initial_branch: Option<&str>,
) -> Result<PreparedStandaloneWorkspace, StandaloneWorkspaceError> {
    if let Some(initial_branch) = initial_branch {
        validate_initial_branch(initial_branch).await?;
    }
    let path = PathBuf::from(&root);
    let inspection_path = path.clone();
    let prepared_directory = tokio::task::spawn_blocking(move || {
        prepare_empty_directory(&inspection_path)
    })
    .await
    .map_err(|_| error(
        StandaloneWorkspaceErrorKind::GitInitializationFailed,
        "standalone workspace directory inspection task failed",
    ))??;

    if let Err(initialization_failure) = run_git_init(&path, initial_branch).await {
        let cleanup_path = path.clone();
        let unchanged = tokio::task::spawn_blocking(move || {
            failed_initialization_left_no_owned_artifact(
                &cleanup_path,
                prepared_directory.directory_created,
                prepared_directory.root_guard,
            )
        })
        .await
        .is_ok_and(|result| result.is_ok());
        if unchanged {
            return Err(initialization_failure);
        }
        return Err(error(
            StandaloneWorkspaceErrorKind::RecoveryRequired,
            "standalone workspace initialization is partial and requires recovery",
        ));
    }

    let verification_path = path.clone();
    let root_identity = prepared_directory.root_guard.identity;
    let git_guard = tokio::task::spawn_blocking(move || {
        let git_guard = directory_guard(&verification_path.join(".git"))?;
        verify_exact_independent_repository(
            &verification_path,
            root_identity,
            Some(git_guard.identity),
        )?;
        Ok::<_, io::Error>(git_guard)
    })
    .await
    .map_err(|_| recovery_error())?
    .map_err(|_| recovery_error())?;

    Ok(PreparedStandaloneWorkspace {
        root,
        root_guard: prepared_directory.root_guard,
        git_guard,
    })
}

async fn validate_initial_branch(branch: &str) -> Result<(), StandaloneWorkspaceError> {
    if branch.is_empty()
        || branch.len() > gate4agent_node_protocol::MAX_REPOSITORY_PATH_BYTES
        || branch.starts_with('-')
        || branch.chars().any(char::is_control)
    {
        return Err(error(
            StandaloneWorkspaceErrorKind::InvalidInitialBranch,
            "initial branch is not a bounded Git branch name",
        ));
    }
    let status = bounded_git_command(None, ["check-ref-format", "--branch", branch]).await
        .map_err(|_| error(
            StandaloneWorkspaceErrorKind::GitInitializationFailed,
            "Git branch validation could not be started",
        ))?;
    if status {
        Ok(())
    } else {
        Err(error(
            StandaloneWorkspaceErrorKind::InvalidInitialBranch,
            "initial branch is not a valid Git branch name",
        ))
    }
}

async fn run_git_init(
    root: &Path,
    initial_branch: Option<&str>,
) -> Result<(), StandaloneWorkspaceError> {
    let initial_branch_argument = initial_branch.map(|branch| format!("--initial-branch={branch}"));
    let mut arguments = vec!["init", "--quiet"];
    if let Some(argument) = initial_branch_argument.as_deref() {
        arguments.push(argument);
    }
    let success = bounded_git_command(Some(root), arguments).await.map_err(|_| error(
        StandaloneWorkspaceErrorKind::GitInitializationFailed,
        "git init could not be started",
    ))?;
    if success {
        Ok(())
    } else {
        Err(error(
            StandaloneWorkspaceErrorKind::GitInitializationFailed,
            "git init failed or exceeded its bounded deadline",
        ))
    }
}

async fn bounded_git_command<'a>(
    root: Option<&Path>,
    arguments: impl IntoIterator<Item = &'a str>,
) -> std::io::Result<bool> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("--no-pager")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    if let Some(root) = root {
        command.current_dir(root);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        command.as_std_mut().creation_flags(0x0800_0000);
    }
    match timeout(GIT_INIT_TIMEOUT, command.status()).await {
        Ok(status) => status.map(|status| status.success()),
        Err(_) => Ok(false),
    }
}

fn prepare_empty_directory(path: &Path) -> Result<PreparedDirectory, StandaloneWorkspaceError> {
    let directory_created = match std::fs::symlink_metadata(path) {
        Ok(_) => false,
        Err(io_error) if io_error.kind() == io::ErrorKind::NotFound => {
            std::fs::create_dir(path).map_err(|_| error(
                StandaloneWorkspaceErrorKind::InvalidRoot,
                "standalone workspace root could not be created",
            ))?;
            true
        }
        Err(_) => return Err(error(
            StandaloneWorkspaceErrorKind::InvalidRoot,
            "standalone workspace root cannot be inspected",
        )),
    };
    let root_guard = directory_guard(path).map_err(|_| {
        if directory_created {
            recovery_error()
        } else {
            error(
                StandaloneWorkspaceErrorKind::InvalidRoot,
                "standalone workspace root must be a real non-reparse directory",
            )
        }
    })?;
    let mut entries = std::fs::read_dir(path).map_err(|_| error(
        StandaloneWorkspaceErrorKind::InvalidRoot,
        "standalone workspace root cannot be read",
    ))?;
    if entries.next().transpose().map_err(|_| error(
        StandaloneWorkspaceErrorKind::InvalidRoot,
        "standalone workspace root cannot be read",
    ))?.is_some() {
        return Err(error(
            StandaloneWorkspaceErrorKind::InvalidRoot,
            "standalone workspace root must be empty",
        ));
    }
    if directory_identity(path).ok() != Some(root_guard.identity) {
        return Err(recovery_error());
    }
    Ok(PreparedDirectory { directory_created, root_guard })
}

fn verify_exact_independent_repository(
    root: &Path,
    expected_root: FileSystemIdentity,
    expected_git: Option<FileSystemIdentity>,
) -> io::Result<FileSystemIdentity> {
    if directory_identity(root)? != expected_root {
        return Err(identity_changed_error());
    }
    let entries = std::fs::read_dir(root)?.collect::<Result<Vec<_>, _>>()?;
    if entries.len() != 1 || entries[0].file_name() != ".git" {
        return Err(identity_changed_error());
    }
    let git_identity = directory_identity(&root.join(".git"))?;
    if expected_git.is_some_and(|expected| expected != git_identity) {
        return Err(identity_changed_error());
    }
    if directory_identity(root)? != expected_root
        || directory_identity(&root.join(".git"))? != git_identity
    {
        return Err(identity_changed_error());
    }
    Ok(git_identity)
}

fn failed_initialization_left_no_owned_artifact(
    root: &Path,
    directory_created: bool,
    root_guard: GuardedDirectory,
) -> io::Result<()> {
    if directory_created
        || directory_identity_from_handle(&root_guard.handle)? != root_guard.identity
        || directory_identity(root)? != root_guard.identity
        || std::fs::read_dir(root)?.next().transpose()?.is_some()
        || directory_identity(root)? != root_guard.identity
    {
        return Err(identity_changed_error());
    }
    Ok(())
}

fn directory_identity(path: &Path) -> io::Result<FileSystemIdentity> {
    directory_guard(path).map(|guard| guard.identity)
}

fn directory_guard(path: &Path) -> io::Result<GuardedDirectory> {
    let handle = open_directory_no_follow(path)?;
    let identity = directory_identity_from_handle(&handle)?;
    Ok(GuardedDirectory { handle, identity })
}

fn open_directory_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    set_directory_no_follow(&mut options);
    options.open(path)
}

#[cfg(windows)]
fn set_directory_no_follow(options: &mut OpenOptions) {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ,
        FILE_SHARE_WRITE,
    };
    options
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT);
}

#[cfg(unix)]
fn set_directory_no_follow(options: &mut OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_CLOEXEC | libc::O_DIRECTORY | libc::O_NOFOLLOW);
}

#[cfg(not(any(unix, windows)))]
fn set_directory_no_follow(_: &mut OpenOptions) {}

#[cfg(windows)]
fn directory_identity_from_handle(file: &File) -> io::Result<FileSystemIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_DIRECTORY,
    };
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe {
        GetFileInformationByHandle(file.as_raw_handle() as _, &mut information)
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    validate_windows_directory_attributes(information.dwFileAttributes)?;
    if information.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0 {
        return Err(identity_changed_error());
    }
    Ok(FileSystemIdentity {
        volume: information.dwVolumeSerialNumber as u64,
        file: ((information.nFileIndexHigh as u64) << 32)
            | information.nFileIndexLow as u64,
    })
}

#[cfg(windows)]
fn validate_windows_directory_attributes(attributes: u32) -> io::Result<()> {
    use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
    if attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Err(identity_changed_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn directory_identity_from_handle(file: &File) -> io::Result<FileSystemIdentity> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata()?;
    Ok(FileSystemIdentity { volume: metadata.dev(), file: metadata.ino() })
}

#[cfg(not(any(unix, windows)))]
fn directory_identity_from_handle(file: &File) -> io::Result<FileSystemIdentity> {
    let metadata = file.metadata()?;
    let created = metadata
        .created()?
        .elapsed()
        .unwrap_or_default()
        .as_nanos() as u64;
    Ok(FileSystemIdentity { volume: metadata.len(), file: created })
}

fn identity_changed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Other,
        "standalone workspace filesystem identity changed",
    )
}

fn recovery_error() -> StandaloneWorkspaceError {
    error(
        StandaloneWorkspaceErrorKind::RecoveryRequired,
        "standalone workspace filesystem identity changed; recovery is required",
    )
}

fn error(
    kind: StandaloneWorkspaceErrorKind,
    message: &'static str,
) -> StandaloneWorkspaceError {
    StandaloneWorkspaceError { kind, message }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compensation_rejects_replaced_root_git_and_reparse_identity() {
        let fixture = temporary_root("identity-substitution");
        std::fs::create_dir_all(&fixture).unwrap();

        let git_target = fixture.join("git-target");
        let displaced_git = fixture.join("displaced-git");
        let prepared_git = prepare_standalone_workspace(
            git_target.to_string_lossy().into_owned(),
            Some("main"),
        )
        .await
        .unwrap();
        assert!(std::fs::rename(git_target.join(".git"), &displaced_git).is_err());
        assert!(!prepared_git.compensate_registration_failure().await);
        assert!(git_target.join(".git").is_dir());
        assert!(!displaced_git.exists());

        let root_target = fixture.join("root-target");
        let displaced_root = fixture.join("displaced-root");
        let prepared_root = prepare_standalone_workspace(
            root_target.to_string_lossy().into_owned(),
            None,
        )
        .await
        .unwrap();
        assert!(std::fs::rename(&root_target, &displaced_root).is_err());
        assert!(!prepared_root.compensate_registration_failure().await);
        assert!(root_target.join(".git").is_dir());
        assert!(!displaced_root.exists());

        use windows_sys::Win32::Storage::FileSystem::{
            FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        assert!(validate_windows_directory_attributes(
            FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT,
        )
        .is_err());

        std::fs::remove_dir_all(fixture).unwrap();
    }

    fn temporary_root(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "gate4agent-node-standalone-{label}-{}-{unique}",
            std::process::id(),
        ))
    }
}

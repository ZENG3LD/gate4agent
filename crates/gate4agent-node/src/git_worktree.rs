use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::timeout;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
#[cfg(windows)]
const CREATE_SUSPENDED: u32 = 0x0000_0004;
const GIT_STDERR_MAX_BYTES: usize = 4 * 1_024;
const GIT_LIST_MAX_BYTES: usize = 256 * 1_024;
const GIT_WORKTREE_MAX_ENTRIES: usize = 512;
const GIT_READ_TIMEOUT_MS: u64 = 5_000;
const GIT_REMOVE_PREFLIGHT_TIMEOUT_MS: u64 = 30_000;
const GIT_MUTATION_TIMEOUT_MS: u64 = 180_000;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NativeGitWorktreeSnapshot {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub is_bare: bool,
    pub is_main: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    pub prunable_reason: Option<String>,
    pub workspace_id: Option<crate::protocol::WorkspaceId>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitWorktreeErrorKind {
    Invalid,
    NotRepository,
    Conflict,
    Protected,
    Dirty,
    Locked,
    Failed,
}

#[derive(Debug)]
pub(crate) struct GitWorktreeError {
    pub kind: GitWorktreeErrorKind,
    pub message: String,
}

impl GitWorktreeError {
    fn new(kind: GitWorktreeErrorKind, message: impl Into<String>) -> Self {
        Self { kind, message: message.into() }
    }
}

pub(crate) struct GitCommandOutput {
    pub success: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    pub timed_out: bool,
}

pub(crate) async fn run_git_read_bounded(
    root: &str,
    arguments: &[&str],
    output_limit: usize,
    timeout_ms: u64,
) -> io::Result<GitCommandOutput> {
    run_git_bounded(root, arguments, output_limit, timeout_ms, true).await
}

async fn run_git_mutation_bounded(
    root: &str,
    arguments: &[OsString],
    output_limit: usize,
    timeout_ms: u64,
) -> io::Result<GitCommandOutput> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("--no-pager")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
    collect_child_output(command, output_limit, timeout_ms).await
}

async fn run_git_bounded(
    root: &str,
    arguments: &[&str],
    output_limit: usize,
    timeout_ms: u64,
    read_only: bool,
) -> io::Result<GitCommandOutput> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("--no-pager")
        .arg("-c")
        .arg("core.fsmonitor=false")
        .arg("-c")
        .arg("core.quotepath=false")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if read_only {
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;

        command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    }
    collect_child_output(command, output_limit, timeout_ms).await
}

async fn collect_child_output(
    mut command: tokio::process::Command,
    output_limit: usize,
    timeout_ms: u64,
) -> io::Result<GitCommandOutput> {
    command.kill_on_drop(true);
    #[cfg(windows)]
    let (mut child, job) = windows_job::spawn_contained(&mut command)?;
    #[cfg(not(windows))]
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "git stdout pipe is unavailable")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "git stderr pipe is unavailable")
    })?;
    let stdout_task = tokio::spawn(read_process_output(stdout, output_limit));
    let stderr_task = tokio::spawn(read_process_output(stderr, GIT_STDERR_MAX_BYTES));
    #[cfg(windows)]
    let mut child = ChildGuard::new(child, job);
    #[cfg(not(windows))]
    let mut child = ChildGuard::new(child);
    let (status, timed_out) = match timeout(
        Duration::from_millis(timeout_ms),
        child.child_mut().wait(),
    ).await {
        Ok(status) => (status?, false),
        Err(_) => {
            child.terminate_tree();
            (child.child_mut().wait().await?, true)
        }
    };
    child.disarm();
    let (stdout, stdout_truncated) = stdout_task
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))??;
    let (stderr, stderr_truncated) = stderr_task
        .await
        .map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))??;
    Ok(GitCommandOutput {
        success: status.success(),
        stdout,
        stderr,
        truncated: stdout_truncated || stderr_truncated,
        timed_out,
    })
}

struct ChildGuard {
    child: Option<tokio::process::Child>,
    #[cfg(windows)]
    job: Option<windows_job::WindowsJob>,
}

impl ChildGuard {
    #[cfg(not(windows))]
    fn new(child: tokio::process::Child) -> Self {
        Self { child: Some(child) }
    }

    #[cfg(windows)]
    fn new(child: tokio::process::Child, job: windows_job::WindowsJob) -> Self {
        Self { child: Some(child), job: Some(job) }
    }

    fn child_mut(&mut self) -> &mut tokio::process::Child {
        self.child.as_mut().expect("Git child guard is armed")
    }

    fn terminate_tree(&mut self) {
        #[cfg(windows)]
        self.job.take();
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }

    fn disarm(&mut self) {
        #[cfg(windows)]
        self.job.take();
        self.child.take();
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate_tree();
        let Some(mut child) = self.child.take() else { return };
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                let _ = timeout(Duration::from_secs(5), child.wait()).await;
            });
        }
    }
}

#[cfg(windows)]
mod windows_job {
    use std::ffi::c_void;
    use std::io;
    use std::mem::{size_of, zeroed};
    use std::os::windows::process::CommandExt;
    use std::ptr::null;
    use std::time::{Duration, Instant};

    type Bool = i32;
    type Dword = u32;
    type Handle = *mut c_void;

    const JOB_OBJECT_EXTENDED_LIMIT_INFORMATION: Dword = 9;
    const JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE: Dword = 0x0000_2000;
    const TH32CS_SNAPTHREAD: Dword = 0x0000_0004;
    const THREAD_SUSPEND_RESUME: Dword = 0x0002;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;
    const THREAD_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(2);
    const THREAD_DISCOVERY_POLL: Duration = Duration::from_millis(10);

    #[repr(C)]
    struct JobObjectBasicLimitInformation {
        per_process_user_time_limit: i64,
        per_job_user_time_limit: i64,
        limit_flags: Dword,
        minimum_working_set_size: usize,
        maximum_working_set_size: usize,
        active_process_limit: Dword,
        affinity: usize,
        priority_class: Dword,
        scheduling_class: Dword,
    }

    #[repr(C)]
    struct IoCounters {
        read_operation_count: u64,
        write_operation_count: u64,
        other_operation_count: u64,
        read_transfer_count: u64,
        write_transfer_count: u64,
        other_transfer_count: u64,
    }

    #[repr(C)]
    struct JobObjectExtendedLimitInformation {
        basic_limit_information: JobObjectBasicLimitInformation,
        io_info: IoCounters,
        process_memory_limit: usize,
        job_memory_limit: usize,
        peak_process_memory_used: usize,
        peak_job_memory_used: usize,
    }

    #[repr(C)]
    struct ThreadEntry32 {
        size: Dword,
        usage_count: Dword,
        thread_id: Dword,
        owner_process_id: Dword,
        base_priority: i32,
        delta_priority: i32,
        flags: Dword,
    }

    #[link(name = "kernel32")]
    extern "system" {
        fn AssignProcessToJobObject(job: Handle, process: Handle) -> Bool;
        fn CloseHandle(handle: Handle) -> Bool;
        fn CreateJobObjectW(attributes: *const c_void, name: *const u16) -> Handle;
        fn CreateToolhelp32Snapshot(flags: Dword, process_id: Dword) -> Handle;
        fn OpenThread(desired_access: Dword, inherit_handle: Bool, thread_id: Dword) -> Handle;
        fn ResumeThread(thread: Handle) -> Dword;
        fn SetInformationJobObject(
            job: Handle,
            information_class: Dword,
            information: *const c_void,
            information_length: Dword,
        ) -> Bool;
        fn Thread32First(snapshot: Handle, entry: *mut ThreadEntry32) -> Bool;
        fn Thread32Next(snapshot: Handle, entry: *mut ThreadEntry32) -> Bool;
    }

    struct OwnedHandle(Handle);

    unsafe impl Send for OwnedHandle {}

    impl OwnedHandle {
        fn new(handle: Handle) -> io::Result<Self> {
            if handle.is_null() || handle == INVALID_HANDLE_VALUE {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(handle))
            }
        }

        fn raw(&self) -> Handle {
            self.0
        }
    }

    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }

    pub(super) struct WindowsJob {
        handle: OwnedHandle,
    }

    impl WindowsJob {
        fn new() -> io::Result<Self> {
            let handle = OwnedHandle::new(unsafe { CreateJobObjectW(null(), null()) })?;
            let mut limits: JobObjectExtendedLimitInformation = unsafe { zeroed() };
            limits.basic_limit_information.limit_flags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    handle.raw(),
                    JOB_OBJECT_EXTENDED_LIMIT_INFORMATION,
                    &limits as *const _ as *const c_void,
                    size_of::<JobObjectExtendedLimitInformation>() as Dword,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { handle })
        }

        fn assign(&self, child: &tokio::process::Child) -> io::Result<()> {
            let process = child.raw_handle().ok_or_else(|| {
                io::Error::new(io::ErrorKind::Other, "suspended Git process has no process handle")
            })?;
            let assigned = unsafe {
                AssignProcessToJobObject(self.handle.raw(), process as Handle)
            };
            if assigned == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        }
    }

    fn open_primary_thread(process_id: Dword) -> io::Result<OwnedHandle> {
        let deadline = Instant::now() + THREAD_DISCOVERY_TIMEOUT;
        loop {
            let snapshot = OwnedHandle::new(unsafe {
                CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)
            })?;
            let mut entry: ThreadEntry32 = unsafe { zeroed() };
            entry.size = size_of::<ThreadEntry32>() as Dword;
            let mut has_entry = unsafe { Thread32First(snapshot.raw(), &mut entry) } != 0;
            while has_entry {
                if entry.owner_process_id == process_id {
                    return OwnedHandle::new(unsafe {
                        OpenThread(THREAD_SUSPEND_RESUME, 0, entry.thread_id)
                    });
                }
                has_entry = unsafe { Thread32Next(snapshot.raw(), &mut entry) } != 0;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "suspended Git process primary thread was not found",
                ));
            }
            std::thread::sleep(THREAD_DISCOVERY_POLL);
        }
    }

    fn resume_primary_thread(child: &tokio::process::Child) -> io::Result<()> {
        let thread = open_primary_thread(child.id().ok_or_else(|| {
            io::Error::new(io::ErrorKind::Other, "suspended Git process has no process ID")
        })?)?;
        let previous_suspend_count = unsafe { ResumeThread(thread.raw()) };
        if previous_suspend_count == Dword::MAX {
            Err(io::Error::last_os_error())
        } else if previous_suspend_count != 1 {
            Err(io::Error::new(
                io::ErrorKind::Other,
                format!("unexpected Git primary thread suspend count: {previous_suspend_count}"),
            ))
        } else {
            Ok(())
        }
    }

    pub(super) fn spawn_contained(
        command: &mut tokio::process::Command,
    ) -> io::Result<(tokio::process::Child, WindowsJob)> {
        let job = WindowsJob::new()?;
        command
            .as_std_mut()
            .creation_flags(super::CREATE_SUSPENDED | super::CREATE_NO_WINDOW);
        let mut child = command.spawn()?;
        if let Err(error) = job.assign(&child) {
            let _ = child.start_kill();
            return Err(error);
        }
        if let Err(error) = resume_primary_thread(&child) {
            drop(job);
            let _ = child.start_kill();
            return Err(error);
        }
        Ok((child, job))
    }
}

async fn read_process_output<R>(reader: R, max_bytes: usize) -> io::Result<(Vec<u8>, bool)>
where
    R: AsyncRead + Unpin,
{
    let mut reader = reader.take((max_bytes + 1) as u64);
    let mut output = Vec::with_capacity(max_bytes.min(8 * 1_024));
    reader.read_to_end(&mut output).await?;
    let truncated = output.len() > max_bytes;
    output.truncate(max_bytes);
    Ok((output, truncated))
}

pub(crate) async fn list_worktrees(
    root: &str,
) -> Result<Vec<NativeGitWorktreeSnapshot>, GitWorktreeError> {
    list_worktrees_with_deadline(root, Instant::now() + Duration::from_millis(GIT_READ_TIMEOUT_MS * 2)).await
}

pub(crate) async fn list_worktrees_with_deadline(
    root: &str,
    deadline: Instant,
) -> Result<Vec<NativeGitWorktreeSnapshot>, GitWorktreeError> {
    let nul = run_git_read_bounded(
        root,
        &["worktree", "list", "--porcelain", "-z"],
        GIT_LIST_MAX_BYTES,
        remaining_timeout_ms(deadline)?,
    )
    .await
    .map_err(io_error)?;
    if nul.timed_out {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "git worktree list timed out",
        ));
    }
    if nul.success {
        if nul.truncated {
            return Err(GitWorktreeError::new(
                GitWorktreeErrorKind::Failed,
                "git worktree list exceeded its bounded output",
            ));
        }
        return bounded_worktree_list(parse_worktree_list_nul(&nul.stdout));
    }

    let fallback = run_git_read_bounded(
        root,
        &["worktree", "list", "--porcelain"],
        GIT_LIST_MAX_BYTES,
        remaining_timeout_ms(deadline)?,
    )
    .await
    .map_err(io_error)?;
    let output = command_result("git worktree list", fallback)?;
    bounded_worktree_list(parse_worktree_list_lines(&output))
}

pub(crate) async fn create_worktree(
    source_root: &str,
    target_root: &str,
    branch: &str,
    base: Option<&str>,
) -> Result<NativeGitWorktreeSnapshot, GitWorktreeError> {
    create_worktree_with_timeout(
        source_root,
        target_root,
        branch,
        base,
        GIT_MUTATION_TIMEOUT_MS,
    ).await
}

pub(crate) async fn create_worktree_with_timeout(
    source_root: &str,
    target_root: &str,
    branch: &str,
    base: Option<&str>,
    mutation_timeout_ms: u64,
) -> Result<NativeGitWorktreeSnapshot, GitWorktreeError> {
    let deadline = Instant::now()
        + Duration::from_millis(mutation_timeout_ms.max(1).min(GIT_MUTATION_TIMEOUT_MS));
    let worktrees = list_worktrees_with_deadline(source_root, deadline).await?;
    let repository_root = repository_root(&worktrees)?;
    let target = normalized_absent_target(target_root)?;
    let target_text = path_to_string(&target)?;
    if worktrees.iter().any(|worktree| paths_equal(&worktree.path, &target_text)) {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Conflict,
            "target is already registered as a Git worktree",
        ));
    }
    validate_branch_with_timeout(
        &repository_root,
        branch,
        remaining_timeout_ms(deadline)?,
    ).await?;
    let base_commit = match base {
        Some(base) => Some(resolve_base_commit_with_timeout(
            &repository_root,
            base,
            remaining_timeout_ms(deadline)?,
        ).await?),
        None => None,
    };

    let mut arguments = vec![
        OsString::from("worktree"),
        OsString::from("add"),
        OsString::from("--no-track"),
        OsString::from("-b"),
        OsString::from(branch),
        OsString::from("--"),
        target.as_os_str().to_owned(),
    ];
    if let Some(base_commit) = base_commit {
        arguments.push(OsString::from(base_commit));
    }
    let output = run_git_mutation_bounded(
        &repository_root,
        &arguments,
        GIT_STDERR_MAX_BYTES,
        remaining_timeout_ms(deadline)?,
    )
    .await
    .map_err(io_error)?;
    command_result("git worktree add", output)?;

    let created = list_worktrees_with_deadline(&repository_root, deadline)
        .await?
        .into_iter()
        .find(|worktree| paths_equal(&worktree.path, &target_text))
        .ok_or_else(|| GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "Git created the worktree but it was not present in the authoritative listing",
        ))?;
    Ok(created)
}

pub(crate) async fn remove_worktree(
    source_root: &str,
    requested_target: &str,
) -> Result<NativeGitWorktreeSnapshot, GitWorktreeError> {
    let requested_target = removal_lookup_path(requested_target)?;
    let worktrees = list_worktrees(source_root).await?;
    let repository_root = repository_root(&worktrees)?;
    let target = worktrees
        .iter()
        .find(|worktree| paths_equal(&worktree.path, &requested_target))
        .cloned()
        .ok_or_else(|| GitWorktreeError::new(
            GitWorktreeErrorKind::Protected,
            "refusing to remove a path that is not in Git's worktree listing",
        ))?;
    validate_removal_target(&repository_root, &target, &worktrees)?;
    ensure_clean(&target.path).await?;

    let arguments = vec![
        OsString::from("worktree"),
        OsString::from("remove"),
        OsString::from("--"),
        OsString::from(&target.path),
    ];
    let output = run_git_mutation_bounded(
        &repository_root,
        &arguments,
        GIT_STDERR_MAX_BYTES,
        GIT_MUTATION_TIMEOUT_MS,
    )
    .await
    .map_err(io_error)?;
    command_result("git worktree remove", output)?;
    if list_worktrees(&repository_root)
        .await?
        .iter()
        .any(|worktree| paths_equal(&worktree.path, &target.path))
    {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "Git reported success but the worktree remains registered",
        ));
    }
    Ok(target)
}

pub(crate) fn removal_lookup_path(value: &str) -> Result<String, GitWorktreeError> {
    if value.is_empty() || value.len() > crate::protocol::MAX_WORKSPACE_ROOT_BYTES {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            "worktree target is empty or exceeds the path limit",
        ));
    }
    let target = Path::new(value);
    if !target.is_absolute() {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            "worktree target must be an absolute path",
        ));
    }
    match std::fs::canonicalize(target) {
        Ok(canonical) => path_to_string(&normalize_windows_verbatim_path(canonical)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(value.to_owned()),
        Err(error) => Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Protected,
            format!("worktree target cannot be resolved safely: {error}"),
        )),
    }
}

fn repository_root(worktrees: &[NativeGitWorktreeSnapshot]) -> Result<String, GitWorktreeError> {
    worktrees
        .iter()
        .find(|worktree| worktree.is_main && !worktree.is_bare)
        .map(|worktree| worktree.path.clone())
        .ok_or_else(|| GitWorktreeError::new(
            GitWorktreeErrorKind::NotRepository,
            "Git did not report a non-bare main worktree",
        ))
}

async fn validate_branch_with_timeout(
    root: &str,
    branch: &str,
    timeout_ms: u64,
) -> Result<(), GitWorktreeError> {
    if branch.is_empty() || branch.len() > 1_024 || branch.chars().any(char::is_control) {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            "worktree branch is empty, too long, or contains control characters",
        ));
    }
    let output = run_git_read_bounded(
        root,
        &["check-ref-format", "--branch", branch],
        GIT_STDERR_MAX_BYTES,
        timeout_ms.max(1),
    )
    .await
    .map_err(io_error)?;
    if output.timed_out {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "git check-ref-format timed out",
        ));
    }
    if !output.success {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            "worktree branch is not a valid Git branch name",
        ));
    }
    Ok(())
}

pub(crate) async fn resolve_base_commit_with_timeout(
    root: &str,
    base: &str,
    timeout_ms: u64,
) -> Result<String, GitWorktreeError> {
    if base.is_empty() || base.len() > 1_024 || base.chars().any(char::is_control) {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            "worktree base is empty, too long, or contains control characters",
        ));
    }
    let peeled = format!("{base}^{{commit}}");
    let output = run_git_read_bounded(
        root,
        &["rev-parse", "--verify", "--quiet", "--end-of-options", &peeled],
        GIT_STDERR_MAX_BYTES,
        timeout_ms.max(1),
    )
    .await
    .map_err(io_error)?;
    if output.timed_out {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "git worktree base resolution timed out",
        ));
    }
    if output.truncated {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "git worktree base resolution exceeded its bounded output",
        ));
    }
    if !output.success {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Conflict,
            "worktree base does not resolve to a commit",
        ));
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if commit.is_empty() || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "git worktree base resolution returned an invalid object ID",
        ));
    }
    Ok(commit)
}

fn remaining_timeout_ms(deadline: Instant) -> Result<u64, GitWorktreeError> {
    let remaining = deadline.checked_duration_since(Instant::now()).ok_or_else(|| {
        GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "managed Git operation exceeded its bounded deadline",
        )
    })?;
    let millis = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
    if millis == 0 {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "managed Git operation exceeded its bounded deadline",
        ));
    }
    Ok(millis)
}

async fn ensure_clean(target: &str) -> Result<(), GitWorktreeError> {
    let output = run_git_read_bounded(
        target,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
        GIT_LIST_MAX_BYTES,
        GIT_REMOVE_PREFLIGHT_TIMEOUT_MS,
    )
    .await
    .map_err(io_error)?;
    let output = command_result("git status", output)?;
    if !output.is_empty() {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Dirty,
            "worktree has modified or untracked files",
        ));
    }
    Ok(())
}

fn validate_removal_target(
    repository_root: &str,
    target: &NativeGitWorktreeSnapshot,
    worktrees: &[NativeGitWorktreeSnapshot],
) -> Result<(), GitWorktreeError> {
    if target.is_main || target.is_bare || dangerous_path(&target.path, repository_root) {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Protected,
            "refusing to remove the main, bare, or protected worktree path",
        ));
    }
    if target.locked {
        let reason = target.lock_reason.as_deref().unwrap_or("no reason reported");
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Locked,
            format!("worktree is locked by Git: {reason}"),
        ));
    }
    if target.prunable {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Protected,
            "refusing to remove a prunable worktree through the clean removal path",
        ));
    }
    if let Some(nested) = worktrees.iter().find(|worktree| {
        !paths_equal(&worktree.path, &target.path)
            && path_contains(&target.path, &worktree.path)
    }) {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Protected,
            format!("worktree contains another registered worktree: {}", nested.path),
        ));
    }
    Ok(())
}

fn normalized_absent_target(value: &str) -> Result<PathBuf, GitWorktreeError> {
    if value.is_empty() || value.len() > crate::protocol::MAX_WORKSPACE_ROOT_BYTES {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            "worktree target is empty or exceeds the path limit",
        ));
    }
    let target = Path::new(value);
    if !target.is_absolute() {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            "worktree target must be an absolute path",
        ));
    }
    match std::fs::symlink_metadata(target) {
        Ok(_) => return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Conflict,
            "worktree target already exists",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }
    let file_name = target.file_name().ok_or_else(|| GitWorktreeError::new(
        GitWorktreeErrorKind::Invalid,
        "worktree target must name a directory below an existing parent",
    ))?;
    let parent = target.parent().ok_or_else(|| GitWorktreeError::new(
        GitWorktreeErrorKind::Invalid,
        "worktree target has no parent directory",
    ))?;
    let canonical_parent = std::fs::canonicalize(parent).map_err(|error| {
        GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            format!("worktree target parent is unavailable: {error}"),
        )
    })?;
    if !canonical_parent.is_dir() {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Invalid,
            "worktree target parent is not a directory",
        ));
    }
    Ok(normalize_windows_verbatim_path(canonical_parent).join(file_name))
}

#[cfg(windows)]
fn normalize_windows_verbatim_path(path: PathBuf) -> PathBuf {
    let Some(value) = path.to_str() else {
        return path;
    };
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

#[cfg(not(windows))]
fn normalize_windows_verbatim_path(path: PathBuf) -> PathBuf {
    path
}

fn path_to_string(path: &Path) -> Result<String, GitWorktreeError> {
    path.as_os_str().to_str().map(str::to_owned).ok_or_else(|| GitWorktreeError::new(
        GitWorktreeErrorKind::Invalid,
        "worktree target is not valid Unicode",
    ))
}

fn dangerous_path(target: &str, repository_root: &str) -> bool {
    let target_path = Path::new(target);
    let repository_root = Path::new(repository_root);
    let home = configured_home_directory();
    dangerous_path_with_home(target_path, repository_root, home.as_deref())
}

pub(crate) fn validate_managed_allocation_root(
    repository_root: &str,
    allocation_root: &str,
) -> Result<(), GitWorktreeError> {
    let allocation = Path::new(allocation_root);
    let repository = Path::new(repository_root);
    let home = configured_home_directory();
    if dangerous_path_with_home(allocation, repository, home.as_deref())
        || native_path_contains(repository, allocation)
    {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Protected,
            "managed worktree allocation root overlaps a protected path",
        ));
    }
    Ok(())
}

fn dangerous_path_with_home(
    target: &Path,
    repository_root: &Path,
    home: Option<&Path>,
) -> bool {
    if native_paths_equal(target, repository_root) {
        return true;
    }
    if target.parent().is_none() || native_path_contains(target, repository_root) {
        return true;
    }
    home.is_some_and(|home| {
        native_paths_equal(target, home) || native_path_contains(target, home)
    })
}

#[cfg(windows)]
fn configured_home_directory() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(PathBuf::from)
}

#[cfg(not(windows))]
fn configured_home_directory() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn path_contains(parent: &str, child: &str) -> bool {
    native_path_contains(Path::new(parent), Path::new(child))
}

pub(crate) fn paths_equal(left: &str, right: &str) -> bool {
    native_paths_equal(Path::new(left), Path::new(right))
}

#[cfg(windows)]
fn native_path_contains(parent: &Path, child: &Path) -> bool {
    let (Some(normalized_parent), Some(normalized_child)) = (
        normalized_path_for_compare(parent),
        normalized_path_for_compare(child),
    ) else {
        return parent != child && child.starts_with(parent);
    };
    normalized_child.len() > normalized_parent.len()
        && normalized_child.starts_with(&normalized_parent)
        && normalized_child.as_bytes().get(normalized_parent.len()) == Some(&b'/')
}

#[cfg(not(windows))]
fn native_path_contains(parent: &Path, child: &Path) -> bool {
    parent != child && child.starts_with(parent)
}

#[cfg(windows)]
fn native_paths_equal(left: &Path, right: &Path) -> bool {
    match (
        normalized_path_for_compare(left),
        normalized_path_for_compare(right),
    ) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

#[cfg(not(windows))]
fn native_paths_equal(left: &Path, right: &Path) -> bool {
    left == right
}

#[cfg(windows)]
fn normalized_path_for_compare(value: &Path) -> Option<String> {
    Some(
        value
            .to_str()?
            .trim_end_matches(['/', '\\'])
            .replace('\\', "/")
            .to_ascii_lowercase(),
    )
}

fn command_result(
    operation: &'static str,
    output: GitCommandOutput,
) -> Result<Vec<u8>, GitWorktreeError> {
    if output.timed_out {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            format!("{operation} timed out"),
        ));
    }
    if output.truncated {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            format!("{operation} exceeded its bounded output"),
        ));
    }
    if !output.success {
        let stderr = sanitize_diagnostic(&output.stderr);
        let lower = stderr.to_ascii_lowercase();
        let kind = if lower.contains("not a git repository") {
            GitWorktreeErrorKind::NotRepository
        } else if lower.contains("already exists")
            || lower.contains("already checked out")
            || lower.contains("is already used by worktree")
        {
            GitWorktreeErrorKind::Conflict
        } else if lower.contains("locked working tree") {
            GitWorktreeErrorKind::Locked
        } else {
            GitWorktreeErrorKind::Failed
        };
        let message = if stderr.is_empty() {
            format!("{operation} failed")
        } else {
            format!("{operation} failed: {stderr}")
        };
        return Err(GitWorktreeError::new(kind, message));
    }
    Ok(output.stdout)
}

fn bounded_worktree_list(
    worktrees: Vec<NativeGitWorktreeSnapshot>,
) -> Result<Vec<NativeGitWorktreeSnapshot>, GitWorktreeError> {
    if worktrees.len() > GIT_WORKTREE_MAX_ENTRIES {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            format!(
                "git worktree list exceeds the {GIT_WORKTREE_MAX_ENTRIES}-entry safety limit",
            ),
        ));
    }
    if worktrees.iter().any(|worktree| {
        worktree.path.is_empty()
            || worktree.path.len() > crate::protocol::MAX_WORKSPACE_ROOT_BYTES
            || worktree.path.as_bytes().contains(&0)
    }) {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            "git worktree list contains an invalid or oversized host path",
        ));
    }
    Ok(worktrees)
}

fn sanitize_diagnostic(output: &[u8]) -> String {
    String::from_utf8_lossy(output)
        .chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn io_error(error: io::Error) -> GitWorktreeError {
    GitWorktreeError::new(
        GitWorktreeErrorKind::Failed,
        format!("Git process failed: {error}"),
    )
}

fn parse_worktree_list_nul(output: &[u8]) -> Vec<NativeGitWorktreeSnapshot> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    for field in output.split(|byte| *byte == 0) {
        if field.is_empty() {
            if !fields.is_empty() {
                records.push(parse_worktree_record(&fields, records.is_empty(), false));
                fields.clear();
                if records.len() > GIT_WORKTREE_MAX_ENTRIES {
                    break;
                }
            }
        } else {
            fields.push(String::from_utf8_lossy(field).into_owned());
        }
    }
    if !fields.is_empty() {
        records.push(parse_worktree_record(&fields, records.is_empty(), false));
    }
    records.into_iter().flatten().collect()
}

fn parse_worktree_list_lines(output: &[u8]) -> Vec<NativeGitWorktreeSnapshot> {
    let text = String::from_utf8_lossy(output).replace("\r\n", "\n");
    text.split("\n\n")
        .take(GIT_WORKTREE_MAX_ENTRIES + 1)
        .filter_map(|block| {
            let fields = block
                .lines()
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>();
            parse_worktree_record(&fields, false, true)
        })
        .enumerate()
        .map(|(index, mut worktree)| {
            worktree.is_main = index == 0;
            worktree
        })
        .collect()
}

fn parse_worktree_record(
    fields: &[String],
    is_main: bool,
    decode_paths: bool,
) -> Option<NativeGitWorktreeSnapshot> {
    let mut path = None;
    let mut head = String::new();
    let mut branch = None;
    let mut is_bare = false;
    let mut locked = false;
    let mut lock_reason = None;
    let mut prunable = false;
    let mut prunable_reason = None;
    for field in fields {
        if let Some(value) = field.strip_prefix("worktree ") {
            path = Some(if decode_paths { decode_git_path(value) } else { value.to_owned() });
        } else if let Some(value) = field.strip_prefix("HEAD ") {
            head = value.to_owned();
        } else if let Some(value) = field.strip_prefix("branch ") {
            branch = Some(value.strip_prefix("refs/heads/").unwrap_or(value).to_owned());
        } else if field == "bare" {
            is_bare = true;
        } else if field == "locked" || field.starts_with("locked ") {
            locked = true;
            lock_reason = field.strip_prefix("locked ").map(|value| {
                if decode_paths { decode_git_path(value) } else { value.to_owned() }
            });
        } else if field == "prunable" || field.starts_with("prunable ") {
            prunable = true;
            prunable_reason = field.strip_prefix("prunable ").map(|value| {
                if decode_paths { decode_git_path(value) } else { value.to_owned() }
            });
        }
    }
    Some(NativeGitWorktreeSnapshot {
        path: path?,
        head,
        branch,
        is_bare,
        is_main,
        locked,
        lock_reason,
        prunable,
        prunable_reason,
        workspace_id: None,
    })
}

fn decode_git_path(value: &str) -> String {
    if !(value.starts_with('"') && value.ends_with('"') && value.len() >= 2) {
        return value.to_owned();
    }
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(value.len() - 2);
    let mut index = 1;
    while index + 1 < bytes.len() {
        let byte = bytes[index];
        index += 1;
        if byte != b'\\' || index + 1 > bytes.len() {
            decoded.push(byte);
            continue;
        }
        let escaped = bytes[index];
        index += 1;
        match escaped {
            b'\\' | b'"' => decoded.push(escaped),
            b'n' => decoded.push(b'\n'),
            b'r' => decoded.push(b'\r'),
            b't' => decoded.push(b'\t'),
            b'b' => decoded.push(8),
            b'f' => decoded.push(12),
            b'v' => decoded.push(11),
            b'0'..=b'7' => {
                let mut value = (escaped - b'0') as u16;
                for _ in 0..2 {
                    let Some(next @ b'0'..=b'7') = bytes.get(index).copied() else { break };
                    value = value * 8 + (next - b'0') as u16;
                    index += 1;
                }
                decoded.push(value.min(255) as u8);
            }
            other => decoded.push(other),
        }
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_GIT_TEMP: AtomicU64 = AtomicU64::new(1);

    #[test]
    fn nul_worktree_parser_preserves_paths_and_git_safety_flags() {
        let output = b"worktree C:/repo\0HEAD abc\0branch refs/heads/main\0\0worktree C:/trees/feature one\0HEAD def\0branch refs/heads/feature/one\0locked owner request\0\0worktree C:/gone\0HEAD 000\0detached\0prunable gitdir file points to non-existent location\0\0";
        let parsed = parse_worktree_list_nul(output);

        assert_eq!(parsed.len(), 3);
        assert!(parsed[0].is_main);
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(parsed[1].path, "C:/trees/feature one");
        assert_eq!(parsed[1].branch.as_deref(), Some("feature/one"));
        assert!(parsed[1].locked);
        assert_eq!(parsed[1].lock_reason.as_deref(), Some("owner request"));
        assert!(parsed[2].prunable);
        assert!(parsed[2].branch.is_none());
    }

    #[test]
    fn fallback_parser_decodes_c_quoted_paths_and_marks_only_first_main() {
        let output = b"worktree \"C:/repo/line\\nfeed\"\nHEAD abc\nbranch refs/heads/main\n\nworktree C:/tree\nHEAD def\nbranch refs/heads/topic\n\n";
        let parsed = parse_worktree_list_lines(output);

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].path, "C:/repo/line\nfeed");
        assert!(parsed[0].is_main);
        assert!(!parsed[1].is_main);
    }

    #[cfg(windows)]
    #[test]
    fn windows_path_comparison_is_case_insensitive_and_separator_agnostic() {
        assert!(paths_equal(r"C:\Repo\Tree", "c:/repo/tree/"));
        assert!(path_contains(r"C:\Repo", "c:/repo/tree"));
    }

    #[cfg(unix)]
    #[test]
    fn unix_path_comparison_is_case_sensitive_and_treats_backslash_as_a_name_byte() {
        assert!(!paths_equal("/srv/Repo", "/srv/repo"));
        assert!(path_contains("/srv/Repo", "/srv/Repo/tree"));
        assert!(!path_contains("/srv/repo", "/srv/repo\\tree"));
    }

    #[test]
    fn removal_safety_protects_home_and_ancestors_but_not_home_children() {
        let repository_root = Path::new("D:/repository");
        let home = Path::new("C:/Users/operator");

        assert!(dangerous_path_with_home(home, repository_root, Some(home)));
        assert!(dangerous_path_with_home(
            Path::new("C:/Users"),
            repository_root,
            Some(home),
        ));
        assert!(!dangerous_path_with_home(
            Path::new("C:/Users/operator/worktree"),
            repository_root,
            Some(home),
        ));
    }

    #[test]
    fn oversized_worktree_path_is_rejected_before_wire_conversion() {
        let oversized = "x".repeat(crate::protocol::MAX_WORKSPACE_ROOT_BYTES + 1);
        let error = bounded_worktree_list(vec![worktree(&oversized, true)]).unwrap_err();
        assert_eq!(error.kind, GitWorktreeErrorKind::Failed);
        assert_eq!(
            error.message,
            "git worktree list contains an invalid or oversized host path",
        );
    }

    #[test]
    fn removal_safety_rejects_main_locked_prunable_and_nested_worktrees() {
        let main = worktree("C:/repo", true);
        let mut locked = worktree("C:/trees/locked", false);
        locked.locked = true;
        let mut prunable = worktree("C:/trees/gone", false);
        prunable.prunable = true;
        let parent = worktree("C:/trees/parent", false);
        let nested = worktree("C:/trees/parent/nested", false);

        assert_eq!(
            validate_removal_target("C:/repo", &main, &[main.clone()]).unwrap_err().kind,
            GitWorktreeErrorKind::Protected,
        );
        assert_eq!(
            validate_removal_target("C:/repo", &locked, &[main.clone(), locked.clone()]).unwrap_err().kind,
            GitWorktreeErrorKind::Locked,
        );
        assert_eq!(
            validate_removal_target("C:/repo", &prunable, &[main.clone(), prunable.clone()]).unwrap_err().kind,
            GitWorktreeErrorKind::Protected,
        );
        assert_eq!(
            validate_removal_target("C:/repo", &parent, &[main, parent.clone(), nested]).unwrap_err().kind,
            GitWorktreeErrorKind::Protected,
        );
    }

    fn worktree(path: &str, is_main: bool) -> NativeGitWorktreeSnapshot {
        NativeGitWorktreeSnapshot {
            path: path.to_owned(),
            head: "abc".to_owned(),
            branch: Some("topic".to_owned()),
            is_bare: false,
            is_main,
            locked: false,
            lock_reason: None,
            prunable: false,
            prunable_reason: None,
            workspace_id: None,
        }
    }

    #[tokio::test]
    async fn managed_git_roundtrip_removes_clean_advanced_worktree_without_deleting_branch() {
        let sequence = NEXT_GIT_TEMP.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "gate4agent-managed-git-{}-{sequence}",
            std::process::id(),
        ));
        if parent.exists() {
            std::fs::remove_dir_all(&parent).unwrap();
        }
        let source = parent.join("source");
        let allocation = parent.join("allocation");
        let target = allocation.join("mw-test");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&allocation).unwrap();
        run_git(&source, &["init"]);
        run_git(&source, &["config", "user.email", "gate4agent@example.invalid"]);
        run_git(&source, &["config", "user.name", "Gate4Agent Test"]);
        std::fs::write(source.join("seed.txt"), "seed\n").unwrap();
        run_git(&source, &["add", "seed.txt"]);
        run_git(&source, &["commit", "-m", "seed"]);
        let source_text = source.to_string_lossy().into_owned();
        let target_text = target.to_string_lossy().into_owned();
        let created = create_worktree(
            &source_text,
            &target_text,
            "gate4agent/mw-test",
            Some("HEAD"),
        ).await.unwrap();
        let created_target = PathBuf::from(&created.path);
        assert!(created_target.is_dir());
        std::fs::write(created_target.join("advanced.txt"), "advanced\n").unwrap();
        run_git(&created_target, &["add", "advanced.txt"]);
        run_git(&created_target, &["commit", "-m", "advanced"]);
        remove_worktree(&source_text, &created.path).await.unwrap();
        assert!(!created_target.exists());
        let status = std::process::Command::new("git")
            .arg("-C").arg(&source)
            .args(["show-ref", "--verify", "--quiet", "refs/heads/gate4agent/mw-test"])
            .status().unwrap();
        assert!(status.success(), "cleanup must not delete the managed branch");
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[tokio::test]
    async fn aborting_bounded_child_prevents_late_mutation() {
        let sequence = NEXT_GIT_TEMP.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "gate4agent-child-cancel-{}-{sequence}",
            std::process::id(),
        ));
        if parent.exists() {
            std::fs::remove_dir_all(&parent).unwrap();
        }
        std::fs::create_dir_all(&parent).unwrap();
        let marker = parent.join("late-mutation");

        #[cfg(windows)]
        let mut command = {
            let escaped = marker.to_string_lossy().replace('\'', "''");
            let mut command = tokio::process::Command::new("powershell.exe");
            command.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("Start-Sleep -Milliseconds 750; Set-Content -LiteralPath '{escaped}' -Value late"),
            ]);
            command
        };
        #[cfg(unix)]
        let mut command = {
            let escaped = marker.to_string_lossy().replace('\'', "'\\''");
            let mut command = tokio::process::Command::new("sh");
            command.args(["-c", &format!("sleep 0.75; printf late > '{escaped}'")]);
            command
        };
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let task = tokio::spawn(collect_child_output(command, 1_024, 5_000));
        tokio::time::sleep(Duration::from_millis(100)).await;
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(1_000)).await;

        assert!(!marker.exists(), "cancelled child performed a late mutation");
        std::fs::remove_dir_all(parent).unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn aborting_bounded_child_terminates_descendants_before_late_mutation() {
        let sequence = NEXT_GIT_TEMP.fetch_add(1, Ordering::Relaxed);
        let parent = std::env::temp_dir().join(format!(
            "gate4agent-job-cancel-{}-{sequence}",
            std::process::id(),
        ));
        if parent.exists() {
            std::fs::remove_dir_all(&parent).unwrap();
        }
        std::fs::create_dir_all(&parent).unwrap();
        let ready = parent.join("descendant-ready");
        let marker = parent.join("descendant-late-mutation");
        let child_script = parent.join("descendant.ps1");
        std::fs::write(
            &child_script,
            format!(
                "Start-Sleep -Milliseconds 1500\r\nSet-Content -LiteralPath '{}' -Value late\r\n",
                marker.to_string_lossy().replace('\'', "''"),
            ),
        ).unwrap();
        let parent = parent.canonicalize().unwrap();
        let escaped_parent = parent.to_string_lossy().replace('\'', "''");
        let escaped_ready = ready.to_string_lossy().replace('\'', "''");
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!(
                "Start-Process -FilePath 'powershell.exe' -WindowStyle Hidden -WorkingDirectory '{escaped_parent}' -ArgumentList @('-NoLogo','-NoProfile','-NonInteractive','-File','descendant.ps1'); Set-Content -LiteralPath '{escaped_ready}' -Value ready; Start-Sleep -Seconds 10"
            ),
        ]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        let task = tokio::spawn(collect_child_output(command, 1_024, 15_000));
        let ready_deadline = Instant::now() + Duration::from_secs(3);
        while !ready.exists() && Instant::now() < ready_deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready.exists(), "descendant process was not started before cancellation");
        task.abort();
        let _ = task.await;
        tokio::time::sleep(Duration::from_millis(2_500)).await;

        assert!(!marker.exists(), "cancelled job descendant performed a late mutation");
        std::fs::remove_dir_all(parent).unwrap();
    }

    fn run_git(root: &Path, arguments: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C").arg(root).args(arguments).output().unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            arguments,
            String::from_utf8_lossy(&output.stderr),
        );
    }
}

use crate::protocol::GitWorktreeSnapshot;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::timeout;

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const GIT_STDERR_MAX_BYTES: usize = 4 * 1_024;
const GIT_LIST_MAX_BYTES: usize = 256 * 1_024;
const GIT_WORKTREE_MAX_ENTRIES: usize = 512;
const GIT_READ_TIMEOUT_MS: u64 = 5_000;
const GIT_REMOVE_PREFLIGHT_TIMEOUT_MS: u64 = 30_000;
const GIT_MUTATION_TIMEOUT_MS: u64 = 180_000;

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
    use std::os::windows::process::CommandExt;

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
    command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    collect_child_output(command, output_limit, timeout_ms).await
}

async fn run_git_bounded(
    root: &str,
    arguments: &[&str],
    output_limit: usize,
    timeout_ms: u64,
    read_only: bool,
) -> io::Result<GitCommandOutput> {
    use std::os::windows::process::CommandExt;

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
    command.as_std_mut().creation_flags(CREATE_NO_WINDOW);
    collect_child_output(command, output_limit, timeout_ms).await
}

async fn collect_child_output(
    mut command: tokio::process::Command,
    output_limit: usize,
    timeout_ms: u64,
) -> io::Result<GitCommandOutput> {
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "git stdout pipe is unavailable")
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        io::Error::new(io::ErrorKind::Other, "git stderr pipe is unavailable")
    })?;
    let stdout_task = tokio::spawn(read_process_output(stdout, output_limit));
    let stderr_task = tokio::spawn(read_process_output(stderr, GIT_STDERR_MAX_BYTES));
    let (status, timed_out) = match timeout(Duration::from_millis(timeout_ms), child.wait()).await {
        Ok(status) => (status?, false),
        Err(_) => {
            let _ = child.kill().await;
            (child.wait().await?, true)
        }
    };
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
) -> Result<Vec<GitWorktreeSnapshot>, GitWorktreeError> {
    let nul = run_git_read_bounded(
        root,
        &["worktree", "list", "--porcelain", "-z"],
        GIT_LIST_MAX_BYTES,
        GIT_READ_TIMEOUT_MS,
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
        GIT_READ_TIMEOUT_MS,
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
) -> Result<GitWorktreeSnapshot, GitWorktreeError> {
    let worktrees = list_worktrees(source_root).await?;
    let repository_root = repository_root(&worktrees)?;
    let target = normalized_absent_target(target_root)?;
    let target_text = path_to_string(&target)?;
    if worktrees.iter().any(|worktree| paths_equal(&worktree.path, &target_text)) {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Conflict,
            "target is already registered as a Git worktree",
        ));
    }
    validate_branch(&repository_root, branch).await?;
    let base_commit = match base {
        Some(base) => Some(resolve_base_commit(&repository_root, base).await?),
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
        GIT_MUTATION_TIMEOUT_MS,
    )
    .await
    .map_err(io_error)?;
    command_result("git worktree add", output)?;

    let created = list_worktrees(&repository_root)
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
) -> Result<GitWorktreeSnapshot, GitWorktreeError> {
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

fn repository_root(worktrees: &[GitWorktreeSnapshot]) -> Result<String, GitWorktreeError> {
    worktrees
        .iter()
        .find(|worktree| worktree.is_main && !worktree.is_bare)
        .map(|worktree| worktree.path.clone())
        .ok_or_else(|| GitWorktreeError::new(
            GitWorktreeErrorKind::NotRepository,
            "Git did not report a non-bare main worktree",
        ))
}

async fn validate_branch(root: &str, branch: &str) -> Result<(), GitWorktreeError> {
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
        GIT_READ_TIMEOUT_MS,
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

async fn resolve_base_commit(root: &str, base: &str) -> Result<String, GitWorktreeError> {
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
        GIT_READ_TIMEOUT_MS,
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
    target: &GitWorktreeSnapshot,
    worktrees: &[GitWorktreeSnapshot],
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

fn normalize_windows_verbatim_path(path: PathBuf) -> PathBuf {
    let value = path.to_string_lossy();
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{rest}"))
    } else if let Some(rest) = value.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path
    }
}

fn path_to_string(path: &Path) -> Result<String, GitWorktreeError> {
    path.as_os_str().to_str().map(str::to_owned).ok_or_else(|| GitWorktreeError::new(
        GitWorktreeErrorKind::Invalid,
        "worktree target is not valid Unicode",
    ))
}

fn dangerous_path(target: &str, repository_root: &str) -> bool {
    if paths_equal(target, repository_root) {
        return true;
    }
    let target_path = Path::new(target);
    if target_path.parent().is_none() || path_contains(target, repository_root) {
        return true;
    }
    std::env::var_os("USERPROFILE")
        .and_then(|home| home.to_str().map(str::to_owned))
        .is_some_and(|home| paths_equal(target, &home) || path_contains(target, &home))
}

fn path_contains(parent: &str, child: &str) -> bool {
    let parent = normalized_path_for_compare(parent);
    let child = normalized_path_for_compare(child);
    child.len() > parent.len()
        && child.starts_with(&parent)
        && child.as_bytes().get(parent.len()) == Some(&b'/')
}

pub(crate) fn paths_equal(left: &str, right: &str) -> bool {
    normalized_path_for_compare(left) == normalized_path_for_compare(right)
}

fn normalized_path_for_compare(value: &str) -> String {
    value
        .trim_end_matches(['/', '\\'])
        .replace('\\', "/")
        .to_ascii_lowercase()
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
    worktrees: Vec<GitWorktreeSnapshot>,
) -> Result<Vec<GitWorktreeSnapshot>, GitWorktreeError> {
    if worktrees.len() > GIT_WORKTREE_MAX_ENTRIES {
        return Err(GitWorktreeError::new(
            GitWorktreeErrorKind::Failed,
            format!(
                "git worktree list exceeds the {GIT_WORKTREE_MAX_ENTRIES}-entry safety limit",
            ),
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

fn parse_worktree_list_nul(output: &[u8]) -> Vec<GitWorktreeSnapshot> {
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

fn parse_worktree_list_lines(output: &[u8]) -> Vec<GitWorktreeSnapshot> {
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
) -> Option<GitWorktreeSnapshot> {
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
    Some(GitWorktreeSnapshot {
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

    fn worktree(path: &str, is_main: bool) -> GitWorktreeSnapshot {
        GitWorktreeSnapshot {
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
}

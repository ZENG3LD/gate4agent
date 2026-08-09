//! Bounded, shell-free operating-system process observation for PTY sessions.

use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
#[cfg(windows)]
use serde_json::Value;
use thiserror::Error;

use crate::agent::{
    is_expected_agent_command_line, AgentSpec, ForegroundObservation, RuntimePlatform,
};

const PROCESS_SCAN_TIMEOUT: Duration = Duration::from_secs(3);
const PROCESS_SCAN_MAX_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PtyForegroundSource {
    PosixForegroundGroup,
    ProcessTree,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtyForegroundObservation {
    pub root_pid: u32,
    pub observed_pid: u32,
    pub observed_process: String,
    pub readiness: ForegroundObservation,
    pub source: PtyForegroundSource,
}

#[derive(Debug, Error)]
pub enum PtyProcessProbeError {
    #[error("PTY child process ID is unavailable")]
    MissingRootPid,
    #[error("failed to start process-table probe: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("process-table probe failed: {0}")]
    Wait(#[source] std::io::Error),
    #[error("process-table probe timed out")]
    TimedOut,
    #[error("process-table probe output exceeded {max_bytes} bytes")]
    OutputTooLarge { max_bytes: usize },
    #[error("process-table probe output was not valid UTF-8")]
    InvalidUtf8,
    #[error("process-table probe returned no row for root PID {0}")]
    MissingRoot(u32),
    #[error("process-table probe returned invalid structured output")]
    InvalidOutput,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ProcessRow {
    pub(crate) pid: u32,
    pub(crate) ppid: u32,
    pub(crate) pgid: Option<u32>,
    pub(crate) state: String,
    pub(crate) started_at: String,
    pub(crate) name: String,
    pub(crate) command: String,
}

pub(crate) fn observe_pty_foreground(
    root_pid: Option<u32>,
    foreground_pgid: Option<u32>,
    spec: &AgentSpec,
) -> Result<PtyForegroundObservation, PtyProcessProbeError> {
    let root_pid = root_pid.ok_or(PtyProcessProbeError::MissingRootPid)?;
    let platform = RuntimePlatform::current();
    let rows = query_process_rows()?;
    select_foreground(&rows, root_pid, foreground_pgid, spec, platform)
        .ok_or(PtyProcessProbeError::MissingRoot(root_pid))
}

fn select_foreground(
    rows: &[ProcessRow],
    root_pid: u32,
    foreground_pgid: Option<u32>,
    spec: &AgentSpec,
    platform: RuntimePlatform,
) -> Option<PtyForegroundObservation> {
    let root = rows.iter().find(|row| row.pid == root_pid)?;
    let descendants = collect_descendants(rows, root_pid);
    let descendant_pids: HashSet<_> = descendants.iter().map(|(row, _)| row.pid).collect();

    let mut candidates: Vec<(&ProcessRow, usize)> = std::iter::once((root, 0))
        .chain(descendants.iter().copied())
        .filter(|(row, _)| {
            foreground_pgid.map_or(true, |pgid| row.pgid == Some(pgid))
                && (row.pid == root_pid || descendant_pids.contains(&row.pid))
        })
        .collect();

    if candidates.is_empty() {
        candidates = std::iter::once((root, 0))
            .chain(descendants.iter().copied())
            .filter(|(row, _)| row.state.contains('+'))
            .collect();
    }
    if candidates.is_empty() {
        candidates = std::iter::once((root, 0))
            .chain(descendants.iter().copied())
            .collect();
    }

    candidates.sort_by_key(|(_, depth)| *depth);
    let selected = candidates
        .iter()
        .rev()
        .find(|(row, _)| is_expected_agent_command_line(spec, &row.command, platform))
        .copied()
        .or_else(|| candidates.last().copied())?;
    let matched_agent = is_expected_agent_command_line(spec, &selected.0.command, platform);
    let process_name = if matched_agent {
        spec.id.as_str().to_owned()
    } else {
        selected.0.name.clone()
    };

    Some(PtyForegroundObservation {
        root_pid,
        observed_pid: selected.0.pid,
        observed_process: selected.0.name.clone(),
        readiness: ForegroundObservation {
            process_name: Some(process_name.clone()),
            has_child_processes: !descendants.is_empty(),
            is_shell: is_shell_process(&process_name),
        },
        source: if foreground_pgid.is_some() {
            PtyForegroundSource::PosixForegroundGroup
        } else {
            PtyForegroundSource::ProcessTree
        },
    })
}

pub(crate) fn collect_descendants<'a>(
    rows: &'a [ProcessRow],
    root_pid: u32,
) -> Vec<(&'a ProcessRow, usize)> {
    let mut children: HashMap<u32, Vec<&ProcessRow>> = HashMap::new();
    for row in rows {
        children.entry(row.ppid).or_default().push(row);
    }
    let mut descendants = Vec::new();
    let mut queue = vec![(root_pid, 0usize)];
    let mut visited = HashSet::from([root_pid]);
    let mut index = 0;
    while let Some((pid, depth)) = queue.get(index).copied() {
        index += 1;
        for child in children.get(&pid).into_iter().flatten() {
            if !visited.insert(child.pid) {
                continue;
            }
            let child_depth = depth.saturating_add(1);
            descendants.push((*child, child_depth));
            queue.push((child.pid, child_depth));
        }
    }
    descendants
}

fn is_shell_process(value: &str) -> bool {
    let value = value
        .trim()
        .trim_matches(['"', '\''])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let value = value.strip_suffix(".exe").unwrap_or(&value);
    matches!(
        value,
        "sh" | "bash" | "zsh" | "fish" | "dash" | "cmd" | "powershell" | "pwsh"
    )
}

#[cfg(unix)]
pub(crate) fn query_process_rows() -> Result<Vec<ProcessRow>, PtyProcessProbeError> {
    let mut command = Command::new("ps");
    command.args(["-axo", "pid=,ppid=,pgid=,stat=,lstart=,args="]);
    let output = run_bounded(&mut command)?;
    let text = String::from_utf8(output).map_err(|_| PtyProcessProbeError::InvalidUtf8)?;
    let rows: Vec<_> = text.lines().filter_map(parse_posix_process_row).collect();
    if rows.is_empty() {
        Err(PtyProcessProbeError::InvalidOutput)
    } else {
        Ok(rows)
    }
}

#[cfg(unix)]
fn parse_posix_process_row(line: &str) -> Option<ProcessRow> {
    let fields: Vec<_> = line.split_whitespace().collect();
    if fields.len() < 10 {
        return None;
    }
    let command = fields[9..].join(" ");
    let name = command
        .split_whitespace()
        .next()
        .unwrap_or_default()
        .trim_matches(['"', '\''])
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned();
    Some(ProcessRow {
        pid: fields[0].parse().ok()?,
        ppid: fields[1].parse().ok()?,
        pgid: fields[2].parse().ok(),
        state: fields[3].to_owned(),
        started_at: fields[4..9].join(" "),
        name,
        command,
    })
}

#[cfg(windows)]
pub(crate) fn query_process_rows() -> Result<Vec<ProcessRow>, PtyProcessProbeError> {
    query_process_rows_cim().or_else(|_| query_process_rows_native())
}

/// Return the process table used for ownership and termination decisions.
///
/// Windows CIM preserves command lines for foreground classification, but a
/// newly spawned process can be absent from its first successful snapshot.
/// Toolhelp is an immediate kernel snapshot and carries the PID, parent PID,
/// executable name, and creation identity required by the tree reaper.
#[cfg(windows)]
pub(crate) fn query_process_tree_rows() -> Result<Vec<ProcessRow>, PtyProcessProbeError> {
    query_process_rows_native()
}

#[cfg(unix)]
pub(crate) fn query_process_tree_rows() -> Result<Vec<ProcessRow>, PtyProcessProbeError> {
    query_process_rows()
}

#[cfg(windows)]
fn query_process_rows_cim() -> Result<Vec<ProcessRow>, PtyProcessProbeError> {
    const SCRIPT: &str = "[Console]::OutputEncoding=[System.Text.Encoding]::UTF8; Get-CimInstance -ClassName Win32_Process -Property CommandLine,CreationDate,Name,ParentProcessId,ProcessId | Select-Object CommandLine,Name,ParentProcessId,ProcessId,@{Name='StartedAt';Expression={$_.CreationDate.ToUniversalTime().Ticks}} | ConvertTo-Json -Compress";
    let mut command = Command::new("powershell.exe");
    command.args(["-NoProfile", "-NonInteractive", "-Command", SCRIPT]);
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
    let output = run_bounded(&mut command)?;
    let value: Value =
        serde_json::from_slice(&output).map_err(|_| PtyProcessProbeError::InvalidOutput)?;
    let values: Vec<&Value> = match &value {
        Value::Array(values) => values.iter().collect(),
        Value::Object(_) => vec![&value],
        _ => return Err(PtyProcessProbeError::InvalidOutput),
    };
    let rows: Vec<_> = values
        .into_iter()
        .filter_map(parse_windows_process_row)
        .collect();
    if rows.is_empty() {
        Err(PtyProcessProbeError::InvalidOutput)
    } else {
        Ok(rows)
    }
}

#[cfg(windows)]
fn query_process_rows_native() -> Result<Vec<ProcessRow>, PtyProcessProbeError> {
    use std::ffi::c_void;
    use std::mem::{size_of, zeroed};

    type Handle = *mut c_void;
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x0000_1000;
    const INVALID_HANDLE_VALUE: Handle = -1isize as Handle;

    #[repr(C)]
    struct ProcessEntry32W {
        size: u32,
        usage: u32,
        process_id: u32,
        default_heap_id: usize,
        module_id: u32,
        threads: u32,
        parent_process_id: u32,
        base_priority: i32,
        flags: u32,
        executable: [u16; 260],
    }

    #[repr(C)]
    struct FileTime {
        low: u32,
        high: u32,
    }

    #[link(name = "kernel32")]
    extern "system" {
        #[link_name = "CreateToolhelp32Snapshot"]
        fn create_toolhelp32_snapshot(flags: u32, process_id: u32) -> Handle;
        #[link_name = "Process32FirstW"]
        fn process32_first(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        #[link_name = "Process32NextW"]
        fn process32_next(snapshot: Handle, entry: *mut ProcessEntry32W) -> i32;
        #[link_name = "OpenProcess"]
        fn open_process(access: u32, inherit_handle: i32, process_id: u32) -> Handle;
        #[link_name = "GetProcessTimes"]
        fn get_process_times(
            process: Handle,
            creation: *mut FileTime,
            exit: *mut FileTime,
            kernel: *mut FileTime,
            user: *mut FileTime,
        ) -> i32;
        #[link_name = "CloseHandle"]
        fn close_handle(handle: Handle) -> i32;
    }

    fn creation_identity(
        process_id: u32,
        open_process: unsafe extern "system" fn(u32, i32, u32) -> Handle,
        get_process_times: unsafe extern "system" fn(
            Handle,
            *mut FileTime,
            *mut FileTime,
            *mut FileTime,
            *mut FileTime,
        ) -> i32,
        close_handle: unsafe extern "system" fn(Handle) -> i32,
    ) -> String {
        // SAFETY: the handle is opened read-only for the enumerated PID and is
        // closed on every successful open path. FILETIME pointers are valid.
        unsafe {
            let process = open_process(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id);
            if process.is_null() {
                return String::new();
            }
            let mut creation: FileTime = zeroed();
            let mut exit: FileTime = zeroed();
            let mut kernel: FileTime = zeroed();
            let mut user: FileTime = zeroed();
            let ok = get_process_times(process, &mut creation, &mut exit, &mut kernel, &mut user);
            let _ = close_handle(process);
            if ok == 0 {
                return String::new();
            }
            ((u64::from(creation.high) << 32) | u64::from(creation.low)).to_string()
        }
    }

    // SAFETY: the snapshot handle and entry layout follow Toolhelp32's API;
    // `size` is initialized before the first and every subsequent call.
    unsafe {
        let snapshot = create_toolhelp32_snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE || snapshot.is_null() {
            return Err(PtyProcessProbeError::InvalidOutput);
        }
        let mut entry: ProcessEntry32W = zeroed();
        entry.size = size_of::<ProcessEntry32W>() as u32;
        if process32_first(snapshot, &mut entry) == 0 {
            let _ = close_handle(snapshot);
            return Err(PtyProcessProbeError::InvalidOutput);
        }

        let mut rows = Vec::new();
        loop {
            let name_end = entry
                .executable
                .iter()
                .position(|unit| *unit == 0)
                .unwrap_or(entry.executable.len());
            let name = String::from_utf16_lossy(&entry.executable[..name_end]);
            rows.push(ProcessRow {
                pid: entry.process_id,
                ppid: entry.parent_process_id,
                pgid: None,
                state: String::new(),
                started_at: creation_identity(
                    entry.process_id,
                    open_process,
                    get_process_times,
                    close_handle,
                ),
                command: name.clone(),
                name,
            });

            entry = zeroed();
            entry.size = size_of::<ProcessEntry32W>() as u32;
            if process32_next(snapshot, &mut entry) == 0 {
                break;
            }
        }
        let _ = close_handle(snapshot);
        if rows.is_empty() {
            Err(PtyProcessProbeError::InvalidOutput)
        } else {
            Ok(rows)
        }
    }
}

#[cfg(windows)]
fn parse_windows_process_row(value: &Value) -> Option<ProcessRow> {
    let pid = json_u32(value.get("ProcessId")?)?;
    let ppid = json_u32(value.get("ParentProcessId")?)?;
    let name = value
        .get("Name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let command = value
        .get("CommandLine")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(name);
    Some(ProcessRow {
        pid,
        ppid,
        pgid: None,
        state: String::new(),
        started_at: json_identity(value.get("StartedAt")),
        name: name.to_owned(),
        command: command.to_owned(),
    })
}

#[cfg(windows)]
fn json_identity(value: Option<&Value>) -> String {
    value
        .and_then(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| value.as_i64().map(|value| value.to_string()))
                .or_else(|| value.as_u64().map(|value| value.to_string()))
        })
        .unwrap_or_default()
}

#[cfg(windows)]
fn json_u32(value: &Value) -> Option<u32> {
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .or_else(|| value.as_str()?.parse().ok())
}

pub(crate) fn run_bounded(command: &mut Command) -> Result<Vec<u8>, PtyProcessProbeError> {
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    let mut child = command.spawn().map_err(PtyProcessProbeError::Spawn)?;
    let stdout = child
        .stdout
        .take()
        .ok_or(PtyProcessProbeError::InvalidOutput)?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        stdout
            .take((PROCESS_SCAN_MAX_BYTES + 1) as u64)
            .read_to_end(&mut output)
            .map(|_| output)
    });
    let deadline = Instant::now() + PROCESS_SCAN_TIMEOUT;
    loop {
        if child
            .try_wait()
            .map_err(PtyProcessProbeError::Wait)?
            .is_some()
        {
            break;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return Err(PtyProcessProbeError::TimedOut);
        }
        thread::sleep(Duration::from_millis(10));
    }
    let output = reader
        .join()
        .map_err(|_| PtyProcessProbeError::InvalidOutput)?
        .map_err(PtyProcessProbeError::Wait)?;
    if output.len() > PROCESS_SCAN_MAX_BYTES {
        return Err(PtyProcessProbeError::OutputTooLarge {
            max_bytes: PROCESS_SCAN_MAX_BYTES,
        });
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::builtin_registry;

    fn row(pid: u32, ppid: u32, pgid: Option<u32>, name: &str, command: &str) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            pgid,
            state: String::new(),
            started_at: "Mon Jul 13 12:54:47 2026".to_owned(),
            name: name.to_owned(),
            command: command.to_owned(),
        }
    }

    #[test]
    fn expected_agent_beats_deeper_tool_child() {
        let rows = vec![
            row(10, 1, Some(10), "bash", "bash"),
            row(
                11,
                10,
                Some(11),
                "node",
                "node /opt/node_modules/@openai/codex/bin/codex.js",
            ),
            row(12, 11, Some(11), "git", "git status"),
        ];
        let observation = select_foreground(
            &rows,
            10,
            Some(11),
            builtin_registry().get_by_id("codex").unwrap(),
            RuntimePlatform::Linux,
        )
        .unwrap();
        assert_eq!(observation.observed_pid, 11);
        assert_eq!(observation.readiness.process_name.as_deref(), Some("codex"));
        assert!(observation.readiness.has_child_processes);
    }

    #[test]
    fn cycles_and_duplicate_edges_do_not_loop() {
        let rows = vec![
            row(10, 12, None, "bash", "bash"),
            row(11, 10, None, "node", "node codex"),
            row(12, 11, None, "helper", "helper"),
        ];
        assert_eq!(collect_descendants(&rows, 10).len(), 2);
    }
}

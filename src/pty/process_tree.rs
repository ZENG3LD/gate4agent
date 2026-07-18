//! Identity-aware descendant teardown for agent-owned PTY processes.

use std::collections::HashMap;
#[cfg(windows)]
use std::collections::HashSet;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use thiserror::Error;

use super::os_process::{
    collect_descendants, query_process_rows, run_bounded, ProcessRow, PtyProcessProbeError,
};

pub const PTY_DESCENDANT_GRACE: Duration = Duration::from_secs(2);
const PTY_ROOT_EXIT_CONFIRMATION_GRACE: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PtyTreeTerminationReport {
    pub root_pid: Option<u32>,
    pub captured_descendants: usize,
    pub graceful_attempted: usize,
    pub force_attempted: usize,
    pub degraded_reason: Option<String>,
}

impl PtyTreeTerminationReport {
    pub(crate) fn root_only(root_pid: Option<u32>, reason: impl Into<String>) -> Self {
        Self {
            root_pid,
            captured_descendants: 0,
            graceful_attempted: 0,
            force_attempted: 0,
            degraded_reason: Some(reason.into()),
        }
    }
}

#[derive(Debug, Error)]
pub enum PtyTreeTerminationError {
    #[error("failed to terminate PTY root process: {0}")]
    Root(String),
}

#[derive(Clone, Debug)]
struct DescendantSnapshot {
    root: ProcessRow,
    #[cfg(windows)]
    root_pid: u32,
    #[cfg(unix)]
    captured_at: chrono::NaiveDateTime,
    descendants: Vec<ProcessRow>,
}

pub(crate) fn terminate_process_tree(
    root_pid: Option<u32>,
    kill_root: impl FnOnce() -> Result<(), String>,
) -> Result<PtyTreeTerminationReport, PtyTreeTerminationError> {
    let snapshot = match root_pid {
        Some(pid) => capture_snapshot(pid),
        None => Ok(None),
    };
    let (snapshot, mut report) = match snapshot {
        Ok(Some(snapshot)) => {
            let report = PtyTreeTerminationReport {
                root_pid,
                captured_descendants: snapshot.descendants.len(),
                graceful_attempted: 0,
                force_attempted: 0,
                degraded_reason: None,
            };
            (Some(snapshot), report)
        }
        Ok(None) => (
            None,
            PtyTreeTerminationReport::root_only(root_pid, "PTY root PID unavailable"),
        ),
        Err(error) => (
            None,
            PtyTreeTerminationReport::root_only(root_pid, error.to_string()),
        ),
    };

    if let Some(snapshot) = snapshot.as_ref() {
        report.graceful_attempted = signal_initial(snapshot);
    }

    let mut root_force_attempted = 0;
    if let Err(error) = kill_root() {
        let root_ended = wait_for_root_ownership_end(root_pid, snapshot.as_ref());
        if !root_ended {
            let forced = snapshot.as_ref().is_some_and(force_identity_matched_root);
            root_force_attempted = usize::from(forced);
            if !forced || !wait_for_root_ownership_end(root_pid, snapshot.as_ref()) {
                return Err(PtyTreeTerminationError::Root(error));
            }
        }
    }

    if let Some(snapshot) = snapshot.as_ref() {
        report.force_attempted = root_force_attempted + force_identity_matched_survivors(snapshot);
    }
    Ok(report)
}

fn capture_snapshot(root_pid: u32) -> Result<Option<DescendantSnapshot>, PtyProcessProbeError> {
    #[cfg(unix)]
    let captured_at = chrono::Local::now().naive_local();
    let rows = query_process_rows()?;
    let Some(root) = rows.iter().find(|row| row.pid == root_pid).cloned() else {
        return Ok(None);
    };
    let descendants = collect_descendants(&rows, root_pid)
        .into_iter()
        .map(|(row, _)| row.clone())
        .collect();
    Ok(Some(DescendantSnapshot {
        root,
        #[cfg(windows)]
        root_pid,
        #[cfg(unix)]
        captured_at,
        descendants,
    }))
}

fn wait_for_root_ownership_end(
    root_pid: Option<u32>,
    snapshot: Option<&DescendantSnapshot>,
) -> bool {
    let Some(root_pid) = root_pid else {
        return true;
    };
    let deadline = Instant::now() + PTY_ROOT_EXIT_CONFIRMATION_GRACE;
    loop {
        if root_ownership_ended(root_pid, snapshot) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn root_ownership_ended(root_pid: u32, snapshot: Option<&DescendantSnapshot>) -> bool {
    let Ok(rows) = query_process_rows() else {
        return false;
    };
    let mut matches = rows.iter().filter(|row| row.pid == root_pid);
    let Some(current) = matches.next() else {
        return true;
    };
    if matches.next().is_some() {
        return false;
    }
    snapshot.is_some_and(|snapshot| !same_process_identity(&snapshot.root, current))
}

fn same_process_identity(expected: &ProcessRow, current: &ProcessRow) -> bool {
    expected.pid == current.pid
        && !expected.started_at.is_empty()
        && expected.started_at == current.started_at
        && expected.pgid == current.pgid
}

#[cfg(windows)]
fn force_identity_matched_root(snapshot: &DescendantSnapshot) -> bool {
    let Ok(rows) = query_process_rows() else {
        return false;
    };
    let mut matches = rows.iter().filter(|row| row.pid == snapshot.root.pid);
    let Some(current) = matches.next() else {
        return true;
    };
    if matches.next().is_some() || !same_process_identity(&snapshot.root, current) {
        return false;
    }
    let mut command = Command::new("taskkill.exe");
    command.args(["/PID", &snapshot.root.pid.to_string(), "/F"]);
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0800_0000);
    run_bounded(&mut command).is_ok()
}

#[cfg(unix)]
fn force_identity_matched_root(snapshot: &DescendantSnapshot) -> bool {
    let Ok(rows) = query_process_rows() else {
        return false;
    };
    let matches: Vec<_> = rows
        .iter()
        .filter(|row| row.pid == snapshot.root.pid)
        .collect();
    if matches.len() != 1
        || !same_process_identity(&snapshot.root, matches[0])
        || !has_unambiguous_posix_identity(&snapshot.root, snapshot)
    {
        return false;
    }
    signal_posix("-KILL", &[snapshot.root.pid]) == 1
}

#[cfg(unix)]
fn signal_initial(snapshot: &DescendantSnapshot) -> usize {
    let pids: Vec<_> = snapshot.descendants.iter().map(|row| row.pid).collect();
    signal_posix("-TERM", &pids)
}

#[cfg(windows)]
fn signal_initial(snapshot: &DescendantSnapshot) -> usize {
    // Windows has no portable graceful descendant signal. Revalidate exact CIM
    // creation identities immediately and force only matching captured owners.
    force_windows_identity_matched(snapshot)
}

#[cfg(unix)]
fn force_identity_matched_survivors(snapshot: &DescendantSnapshot) -> usize {
    if snapshot.descendants.is_empty() {
        return 0;
    }
    thread::sleep(PTY_DESCENDANT_GRACE);
    let Ok(rows) = query_process_rows() else {
        return 0;
    };
    let targets = identity_matched_survivors(snapshot, &rows, true);
    signal_posix("-KILL", &targets)
}

#[cfg(windows)]
fn force_identity_matched_survivors(_snapshot: &DescendantSnapshot) -> usize {
    // Initial Windows termination is already forced after a fresh identity
    // check, so there is no delayed PID-based escalation window.
    0
}

fn identity_matched_survivors(
    snapshot: &DescendantSnapshot,
    live_rows: &[ProcessRow],
    require_unambiguous_second: bool,
) -> Vec<u32> {
    let expected: HashMap<_, _> = snapshot
        .descendants
        .iter()
        .map(|row| (row.pid, row))
        .collect();
    let mut live = HashMap::<u32, Option<&ProcessRow>>::new();
    for row in live_rows {
        if !expected.contains_key(&row.pid) {
            continue;
        }
        live.entry(row.pid)
            .and_modify(|entry| *entry = None)
            .or_insert(Some(row));
    }

    snapshot
        .descendants
        .iter()
        .filter(|expected| {
            let Some(Some(current)) = live.get(&expected.pid) else {
                return false;
            };
            expected.started_at == current.started_at
                && expected.pgid == current.pgid
                && (!require_unambiguous_second
                    || has_unambiguous_posix_identity(expected, snapshot))
        })
        .map(|row| row.pid)
        .collect()
}

#[cfg(unix)]
fn has_unambiguous_posix_identity(row: &ProcessRow, snapshot: &DescendantSnapshot) -> bool {
    use chrono::Timelike;

    let Ok(started_at) =
        chrono::NaiveDateTime::parse_from_str(&row.started_at, "%a %b %e %H:%M:%S %Y")
    else {
        return false;
    };
    let captured_second = snapshot
        .captured_at
        .with_nanosecond(0)
        .unwrap_or(snapshot.captured_at);
    started_at < captured_second
}

#[cfg(windows)]
fn has_unambiguous_posix_identity(_row: &ProcessRow, _snapshot: &DescendantSnapshot) -> bool {
    true
}

#[cfg(unix)]
fn signal_posix(signal: &str, pids: &[u32]) -> usize {
    if pids.is_empty() {
        return 0;
    }
    let mut command = Command::new("kill");
    command.arg(signal).arg("--");
    for pid in pids {
        command.arg(pid.to_string());
    }
    if run_bounded(&mut command).is_ok() {
        pids.len()
    } else {
        0
    }
}

#[cfg(windows)]
fn force_windows_identity_matched(snapshot: &DescendantSnapshot) -> usize {
    let Ok(rows) = query_process_rows() else {
        return 0;
    };
    let targets = identity_matched_survivors(snapshot, &rows, false);
    let target_set: HashSet<_> = targets.iter().copied().collect();
    let mut ordered: Vec<_> = collect_descendants(&rows, snapshot.root_pid)
        .into_iter()
        .filter(|(row, _)| target_set.contains(&row.pid))
        .map(|(row, depth)| (row.pid, depth))
        .collect();
    ordered.sort_by_key(|(_, depth)| std::cmp::Reverse(*depth));

    let mut attempted = 0;
    for (pid, _) in ordered {
        let mut command = Command::new("taskkill.exe");
        command.args(["/PID", &pid.to_string(), "/F"]);
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
        if run_bounded(&mut command).is_ok() {
            attempted += 1;
        }
    }
    attempted
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(pid: u32, ppid: u32, pgid: Option<u32>, started_at: &str) -> ProcessRow {
        ProcessRow {
            pid,
            ppid,
            pgid,
            state: String::new(),
            started_at: started_at.to_owned(),
            name: "agent".to_owned(),
            command: "agent".to_owned(),
        }
    }

    #[test]
    fn survivor_identity_requires_unique_pid_row_and_matching_birth() {
        let snapshot = DescendantSnapshot {
            root: row(10, 1, Some(10), "Mon Jul 18 11:59:59 2026"),
            #[cfg(windows)]
            root_pid: 10,
            #[cfg(unix)]
            captured_at: chrono::NaiveDateTime::parse_from_str(
                "2026-07-18 12:00:02",
                "%Y-%m-%d %H:%M:%S",
            )
            .unwrap(),
            descendants: vec![row(11, 10, Some(11), "Mon Jul 18 12:00:00 2026")],
        };
        assert_eq!(
            identity_matched_survivors(&snapshot, &snapshot.descendants, false),
            vec![11]
        );
        let duplicate = vec![
            snapshot.descendants[0].clone(),
            snapshot.descendants[0].clone(),
        ];
        assert!(identity_matched_survivors(&snapshot, &duplicate, false).is_empty());
        let reused = vec![row(11, 1, Some(12), "Mon Jul 18 12:00:01 2026")];
        assert!(identity_matched_survivors(&snapshot, &reused, false).is_empty());
    }
}

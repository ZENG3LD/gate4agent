use crate::{
    CandidateLocator, DiscoveredCandidate, NativeHistoryDiscoveryIssue,
    NativeHistoryDiscoveryIssueKind, NativeHistoryLimits, NativeHistoryRoot,
};
use gate4agent_adapters::HistorySourceLayout;
use std::cmp::Reverse;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

pub(crate) struct DiscoveryResult {
    pub(crate) candidates: Vec<DiscoveredCandidate>,
    pub(crate) issues: Vec<NativeHistoryDiscoveryIssue>,
}

pub(crate) fn discover_root(
    root: &NativeHistoryRoot,
    root_slot: usize,
    limits: NativeHistoryLimits,
    requested_limit: usize,
    report_candidate_overflow: bool,
) -> DiscoveryResult {
    let mut result = DiscoveryResult {
        candidates: Vec::new(),
        issues: Vec::new(),
    };
    let Ok(metadata) = fs::symlink_metadata(&root.path) else {
        return result;
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::Inaccessible,
        ));
        return result;
    }
    let Ok(canonical_root) = fs::canonicalize(&root.path) else {
        result.issues.push(issue(
            root,
            root_slot,
            NativeHistoryDiscoveryIssueKind::Inaccessible,
        ));
        return result;
    };
    let scan_root = if root.layout == HistorySourceLayout::SessionJsonWithSiblingMessageJson {
        canonical_root.join("session")
    } else {
        canonical_root.clone()
    };
    if !scan_root.is_dir() {
        return result;
    }

    let mut stack = vec![(scan_root, 0usize)];
    let candidate_limit = requested_limit.min(limits.max_candidates).max(1);
    let mut entries_seen = 0usize;
    let mut limit_reported = false;
    let mut candidate_limit_reported = false;
    while let Some((directory, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries.flatten() {
            entries_seen = entries_seen.saturating_add(1);
            if entries_seen > limits.max_walk_entries {
                if !limit_reported {
                    result.issues.push(issue(
                        root,
                        root_slot,
                        NativeHistoryDiscoveryIssueKind::EntryLimitReached,
                    ));
                    limit_reported = true;
                }
                stack.clear();
                break;
            }
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if depth < limits.max_walk_depth
                    && should_descend(root.adapter_id.as_str(), &path)
                {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !file_type.is_file() || !matches_file(root.adapter_id.as_str(), &path) {
                continue;
            }
            let Ok(primary) = fs::canonicalize(&path) else {
                continue;
            };
            if !primary.starts_with(&canonical_root) {
                continue;
            }
            let Some(session_id_hint) = session_id_hint(root.adapter_id.as_str(), &primary) else {
                continue;
            };
            let modified_at_unix_ms = entry
                .metadata()
                .ok()
                .and_then(|metadata| metadata.modified().ok())
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .and_then(|duration| u64::try_from(duration.as_millis()).ok());
            result.candidates.push(DiscoveredCandidate {
                locator: CandidateLocator::File {
                    root: canonical_root.clone(),
                    primary,
                    layout: root.layout,
                },
                session_id_hint,
                modified_at_unix_ms,
            });
            if report_candidate_overflow
                && result.candidates.len() > candidate_limit
                && !candidate_limit_reported
            {
                result.issues.push(issue(
                    root,
                    root_slot,
                    NativeHistoryDiscoveryIssueKind::CandidateLimitReached,
                ));
                candidate_limit_reported = true;
            }
            if result.candidates.len() > candidate_limit.saturating_mul(2) {
                retain_newest(&mut result.candidates, candidate_limit);
            }
        }
    }
    retain_newest(&mut result.candidates, candidate_limit);
    result
}

pub(crate) fn dedupe_and_sort(candidates: &mut Vec<DiscoveredCandidate>) {
    candidates.sort_by(|left, right| {
        Reverse(left.modified_at_unix_ms)
            .cmp(&Reverse(right.modified_at_unix_ms))
            .then_with(|| locator_sort_key(&left.locator).cmp(&locator_sort_key(&right.locator)))
    });
    let mut seen = HashSet::new();
    candidates.retain(|candidate| seen.insert(locator_sort_key(&candidate.locator)));
}

fn locator_sort_key(locator: &CandidateLocator) -> String {
    match locator {
        CandidateLocator::File { primary, .. } => primary.to_string_lossy().into_owned(),
    }
}

fn retain_newest(candidates: &mut Vec<DiscoveredCandidate>, limit: usize) {
    candidates.sort_by(|left, right| {
        Reverse(left.modified_at_unix_ms)
            .cmp(&Reverse(right.modified_at_unix_ms))
            .then_with(|| locator_sort_key(&left.locator).cmp(&locator_sort_key(&right.locator)))
    });
    candidates.truncate(limit);
}

fn matches_file(adapter_id: &str, path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match adapter_id {
        "grok" => name == "summary.json",
        "rovo" => name == "metadata.json",
        "kimi" => {
            name == "state.json"
                && path
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("session_"))
        }
        "cursor" => extension == "jsonl" && has_component(path, "agent-transcripts"),
        "openclaw" => extension == "jsonl" && has_component(path, "sessions"),
        "claude-code" => extension == "jsonl" && !has_component(path, "subagents"),
        "codex" | "pi" | "omp" => extension == "jsonl",
        _ => false,
    }
}

fn should_descend(adapter_id: &str, path: &Path) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    if adapter_id == "claude-code" && name == "subagents" {
        return false;
    }
    true
}

fn session_id_hint(adapter_id: &str, path: &Path) -> Option<String> {
    let hint = match adapter_id {
        "grok" | "rovo" | "kimi" => path.parent()?.file_name()?.to_str()?.to_owned(),
        _ => path.file_stem()?.to_str()?.to_owned(),
    };
    let hint = hint.trim();
    (!hint.is_empty()).then(|| hint.to_owned())
}

fn has_component(path: &Path, expected: &str) -> bool {
    path.components()
        .any(|component| component.as_os_str() == expected)
}

fn issue(
    root: &NativeHistoryRoot,
    root_slot: usize,
    kind: NativeHistoryDiscoveryIssueKind,
) -> NativeHistoryDiscoveryIssue {
    NativeHistoryDiscoveryIssue {
        root_slot,
        adapter_id: root.adapter_id.clone(),
        kind,
    }
}

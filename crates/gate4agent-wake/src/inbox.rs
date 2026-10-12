//! Per-session inbox: one JSON file per letter, written by the local client
//! and drained by a process the provider runs (the Claude channel MCP
//! server, or a hook such as Kimi `Stop` / Claude `Stop`).
//!
//! Files hold the plaintext prompt until drained, so the directory must sit
//! under the store root with owner-only permissions. Writes are
//! tmp-then-rename so a reader never sees half a letter.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{wake_prompt, ProviderSession, WakeError, WakeLetter};

/// One queued letter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxLetter {
    /// Sender nick.
    pub from_nick: String,
    /// Recipient nick (this session).
    pub to: String,
    /// Matrix event id.
    pub event_id: String,
    /// Room id, if known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    /// Full wake prompt ([`wake_prompt`]).
    pub prompt: String,
}

/// A drained letter and the file it came from (already removed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxEntry {
    /// File name the letter had.
    pub name: String,
    /// The letter.
    pub letter: InboxLetter,
}

/// File-name stem for an event id: `[A-Za-z0-9_-]` only.
pub fn entry_stem(event_id: &str) -> String {
    let stem: String = event_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if stem.trim_matches('_').is_empty() {
        "letter".to_string()
    } else {
        stem
    }
}

/// Writes one letter into `dir` (created if missing, owner-only on unix).
/// The same event id overwrites its own entry, so a retried wake does not
/// queue twice.
pub fn write_letter(
    dir: &Path,
    session: &ProviderSession,
    letter: &WakeLetter<'_>,
) -> Result<PathBuf, WakeError> {
    create_private_dir(dir)?;
    let entry = InboxLetter {
        from_nick: letter.from_nick.to_string(),
        to: session.nick.clone(),
        event_id: letter.event_id.to_string(),
        room: letter.room.map(str::to_string),
        prompt: wake_prompt(session, letter),
    };
    let bytes = serde_json::to_vec(&entry).map_err(|err| WakeError::Transport(err.to_string()))?;
    let stem = entry_stem(letter.event_id);
    let tmp = dir.join(format!(".{stem}.tmp"));
    let path = dir.join(format!("{stem}.json"));
    let mut file = private_file(&tmp)?;
    file.write_all(&bytes)
        .and_then(|()| file.sync_all())
        .map_err(|err| WakeError::Transport(format!("inbox write: {}", err.kind())))?;
    drop(file);
    fs::rename(&tmp, &path)
        .map_err(|err| WakeError::Transport(format!("inbox rename: {}", err.kind())))?;
    Ok(path)
}

/// Reads and removes every letter in `dir`, oldest file name first. A file
/// that does not parse is removed and skipped. A missing dir is empty.
pub fn drain(dir: &Path) -> Vec<InboxEntry> {
    let Ok(read) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<(std::time::SystemTime, String, PathBuf)> = read
        .filter_map(Result::ok)
        .filter_map(|item| {
            let name = item.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') || !name.ends_with(".json") {
                return None;
            }
            let modified = item
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            Some((modified, name, item.path()))
        })
        .collect();
    paths.sort();
    let mut out = Vec::new();
    for (_, name, path) in paths {
        // Claim by rename first: two consumers (channel server and a hook)
        // may drain the same dir, and only the one whose rename wins reads.
        let claimed = dir.join(format!(".claim-{}-{name}", std::process::id()));
        if fs::rename(&path, &claimed).is_err() {
            continue;
        }
        let parsed = fs::read(&claimed)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<InboxLetter>(&bytes).ok());
        let _ = fs::remove_file(&claimed);
        if let Some(letter) = parsed {
            out.push(InboxEntry { name, letter });
        }
    }
    out
}

/// Number of letters waiting in `dir` (no claim, no read).
pub fn pending(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|read| {
            read.filter_map(Result::ok)
                .filter(|item| {
                    let name = item.file_name().to_string_lossy().into_owned();
                    !name.starts_with('.') && name.ends_with(".json")
                })
                .count()
        })
        .unwrap_or(0)
}

/// Sub-directory of an inbox where consumers announce themselves.
pub const LIVE_DIR: &str = ".live";

/// A consumer that holds the session open right now (channel server,
/// rewake waiter) refreshes its file at least this often.
pub const HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(2);

/// A heartbeat older than this means the consumer is gone.
pub const HEARTBEAT_STALE: std::time::Duration = std::time::Duration::from_secs(15);

/// Consumer names used in `.live/`.
pub mod consumer {
    /// `m4a-claude-channel` MCP server inside a running Claude Code.
    pub const CLAUDE_CHANNEL: &str = "claude-channel";
    /// `m4a-inbox wait` under a Claude `asyncRewake` hook.
    pub const CLAUDE_REWAKE: &str = "claude-rewake";
    /// Stop hooks (`m4a-inbox drain --format <fmt>`): armed, not live.
    pub fn stop(format: &str) -> String {
        format!("stop-{format}")
    }
}

fn live_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(LIVE_DIR).join(entry_stem(name))
}

/// A live consumer's heartbeat file. Removed on drop.
pub struct Presence {
    path: PathBuf,
}

impl Presence {
    /// Writes the heartbeat (pid inside) under `dir/.live/name`.
    pub fn announce(dir: &Path, name: &str) -> Result<Self, WakeError> {
        let path = live_path(dir, name);
        if let Some(parent) = path.parent() {
            create_private_dir(dir)?;
            create_private_dir(parent)?;
        }
        let presence = Self { path };
        presence.beat();
        Ok(presence)
    }

    /// Refreshes the heartbeat.
    pub fn beat(&self) {
        if let Ok(mut file) = private_file(&self.path) {
            let _ = write!(file, "{}", std::process::id());
        }
    }
}

impl Drop for Presence {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// Whether consumer `name` beat within [`HEARTBEAT_STALE`].
pub fn is_live(dir: &Path, name: &str) -> bool {
    fs::metadata(live_path(dir, name))
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| modified.elapsed().ok())
        .is_some_and(|age| age <= HEARTBEAT_STALE)
}

/// Marks a one-shot consumer (a Stop hook) as installed for this inbox.
/// Unlike [`Presence`] it stays until [`disarm`].
pub fn arm(dir: &Path, name: &str) -> Result<(), WakeError> {
    create_private_dir(dir)?;
    create_private_dir(&dir.join(LIVE_DIR))?;
    private_file(&live_path(dir, name)).map(|_| ())
}

/// Removes an [`arm`] marker.
pub fn disarm(dir: &Path, name: &str) {
    let _ = fs::remove_file(live_path(dir, name));
}

/// Whether [`arm`] ran for `name` and was not disarmed.
pub fn is_armed(dir: &Path, name: &str) -> bool {
    live_path(dir, name).exists()
}

fn create_private_dir(dir: &Path) -> Result<(), WakeError> {
    fs::create_dir_all(dir)
        .map_err(|err| WakeError::Unavailable(format!("inbox dir: {}", err.kind())))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(dir, fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn private_file(path: &Path) -> Result<fs::File, WakeError> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options
        .open(path)
        .map_err(|err| WakeError::Transport(format!("inbox open: {}", err.kind())))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tests::{letter, session};
    use crate::{ProviderKind, SessionKind};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "m4a-inbox-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn write_then_drain_round_trips_and_empties() {
        let dir = temp_dir("rt");
        let s = session(SessionKind::local(ProviderKind::ClaudeCode));
        let path = write_letter(&dir, &s, &letter("hello")).unwrap();
        assert!(path.ends_with("_ev1.json"));
        // Same event id again overwrites, never duplicates.
        write_letter(&dir, &s, &letter("hello")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let drained = drain(&dir);
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].letter.from_nick, "carol");
        assert!(drained[0].letter.prompt.ends_with("hello"));
        assert!(drain(&dir).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn presence_beats_and_clears_and_arm_sticks() {
        let dir = temp_dir("live");
        assert!(!is_live(&dir, consumer::CLAUDE_CHANNEL));
        {
            let _p = Presence::announce(&dir, consumer::CLAUDE_CHANNEL).unwrap();
            assert!(is_live(&dir, consumer::CLAUDE_CHANNEL));
            assert!(!is_live(&dir, consumer::CLAUDE_REWAKE));
        }
        assert!(!is_live(&dir, consumer::CLAUDE_CHANNEL));
        let stop = consumer::stop("codex-stop");
        assert!(!is_armed(&dir, &stop));
        arm(&dir, &stop).unwrap();
        assert!(is_armed(&dir, &stop));
        // Markers are not letters.
        assert_eq!(pending(&dir), 0);
        assert!(drain(&dir).is_empty());
        disarm(&dir, &stop);
        assert!(!is_armed(&dir, &stop));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stems_are_file_safe() {
        assert_eq!(entry_stem("$abc:def/../x"), "_abc_def____x");
        assert_eq!(entry_stem("$"), "letter");
    }
}

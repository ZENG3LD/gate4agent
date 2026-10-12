//! Sessions that announced themselves to the local client.
//!
//! A provider session registers from inside itself (a `SessionStart` hook
//! or the agent running `m4a-inbox register`): one JSON file per session
//! under `<store root>/provider-sessions/`. The local client adopts every
//! live record, opens its mail store, and wakes it through
//! [`super::chain::plan_chain`]. Grok sessions keep their own index
//! (`active_sessions.json`) and the leader path; they are not listed here.
//!
//! Records hold ids and paths only. No bearer, no URL with a key.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

use super::inbox::entry_stem;
use super::{ProviderKind, ProviderSession, SessionKind, Surface};

/// Directory under the store root.
pub const REGISTRY_DIR: &str = "provider-sessions";
/// Directory under the store root holding one inbox per session.
pub const INBOX_ROOT: &str = "inbox";
/// A record not refreshed for this long is treated as gone.
pub const RECORD_STALE: Duration = Duration::from_secs(7 * 24 * 3600);

/// On-disk record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionRecord {
    /// Provider id ([`ProviderKind::id`]).
    pub provider: String,
    /// `local` or `web`.
    #[serde(default = "local")]
    pub surface: String,
    /// Vendor session / thread / chat id.
    pub session_id: String,
    /// mail4agent nick.
    pub nick: String,
    /// Working directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// No client holds the session open (spawn fallback allowed).
    #[serde(default)]
    pub headless: bool,
    /// Pid of the client process, when known (liveness on Linux).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
}

fn local() -> String {
    "local".to_string()
}

impl SessionRecord {
    /// The session, or `None` for an unknown provider.
    pub fn session(&self) -> Option<ProviderSession> {
        let provider = ProviderKind::parse(&self.provider)?;
        let surface = if self.surface.eq_ignore_ascii_case("web") {
            Surface::Web
        } else {
            Surface::Local
        };
        Some(ProviderSession {
            kind: SessionKind { provider, surface },
            session_id: self.session_id.clone(),
            nick: self.nick.clone(),
            cwd: self.cwd.clone(),
            headless: self.headless,
        })
    }
}

/// Inbox of one session under `store_root`.
pub fn inbox_dir(store_root: &Path, session_id: &str) -> PathBuf {
    store_root.join(INBOX_ROOT).join(entry_stem(session_id))
}

/// Writes (or refreshes) `record`. Owner-only on unix.
pub fn register(store_root: &Path, record: &SessionRecord) -> std::io::Result<PathBuf> {
    let dir = store_root.join(REGISTRY_DIR);
    fs::create_dir_all(&dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
    }
    let stem = entry_stem(&record.session_id);
    let path = dir.join(format!("{stem}.json"));
    let tmp = dir.join(format!(".{stem}.tmp"));
    fs::write(
        &tmp,
        serde_json::to_vec_pretty(record).map_err(std::io::Error::other)?,
    )?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Removes a record.
pub fn unregister(store_root: &Path, session_id: &str) {
    let path = store_root
        .join(REGISTRY_DIR)
        .join(format!("{}.json", entry_stem(session_id)));
    let _ = fs::remove_file(path);
}

fn pid_alive(pid: u32) -> bool {
    if cfg!(target_os = "linux") {
        Path::new(&format!("/proc/{pid}")).exists()
    } else {
        true
    }
}

/// Live records: parsed, known provider, fresh, and (Linux) pid alive.
pub fn live_sessions(store_root: &Path) -> Vec<ProviderSession> {
    let Ok(read) = fs::read_dir(store_root.join(REGISTRY_DIR)) else {
        return Vec::new();
    };
    let mut out: Vec<ProviderSession> = read
        .filter_map(Result::ok)
        .filter(|item| {
            let name = item.file_name().to_string_lossy().into_owned();
            !name.starts_with('.') && name.ends_with(".json")
        })
        .filter(|item| {
            item.metadata()
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .is_none_or(|age| age <= RECORD_STALE)
        })
        .filter_map(|item| fs::read(item.path()).ok())
        .filter_map(|bytes| serde_json::from_slice::<SessionRecord>(&bytes).ok())
        .filter(|record| record.pid.is_none_or(pid_alive))
        .filter_map(|record| record.session())
        .collect();
    out.sort_by(|a, b| a.session_id.cmp(&b.session_id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_list_unregister() {
        let root = std::env::temp_dir().join(format!("m4a-reg-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let record = SessionRecord {
            provider: "codex".into(),
            surface: "local".into(),
            session_id: "thr/1".into(),
            nick: "builder".into(),
            cwd: Some("/tmp".into()),
            headless: false,
            pid: Some(std::process::id()),
        };
        register(&root, &record).unwrap();
        register(
            &root,
            &SessionRecord {
                provider: "gemini".into(),
                session_id: "x".into(),
                ..record.clone()
            },
        )
        .unwrap();
        let live = live_sessions(&root);
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].kind, SessionKind::local(ProviderKind::Codex));
        assert_eq!(live[0].nick, "builder");
        assert!(inbox_dir(&root, "thr/1").ends_with("inbox/thr_1"));
        unregister(&root, "thr/1");
        unregister(&root, "x");
        assert!(live_sessions(&root).is_empty());
        let _ = fs::remove_dir_all(&root);
    }
}

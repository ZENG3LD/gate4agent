use std::env;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 256 * 1024;
const MAX_MESSAGE_BYTES: usize = 2 * 1024;

pub fn install_panic_hook() {
    std::panic::set_hook(Box::new(|_| {
        let _ = append("tui-crash.log", "panic");
    }));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeDiagnostic {
    PreferencesLoadFailed,
    PreferencesSaveFailed,
    C2ReconnectFailed,
    C2InitialSnapshotFailed,
    C2StartupFailed,
    C2ControlDisconnected,
    NodeDisconnected,
    Fatal,
}

impl RuntimeDiagnostic {
    fn message(self) -> &'static str {
        match self {
            Self::PreferencesLoadFailed => "preferences-load-failed",
            Self::PreferencesSaveFailed => "preferences-save-failed",
            Self::C2ReconnectFailed => "c2-reconnect-failed",
            Self::C2InitialSnapshotFailed => "c2-initial-snapshot-failed",
            Self::C2StartupFailed => "c2-startup-failed",
            Self::C2ControlDisconnected => "c2-control-disconnected",
            Self::NodeDisconnected => "node-disconnected",
            Self::Fatal => "fatal",
        }
    }
}

pub fn record_runtime(diagnostic: RuntimeDiagnostic) {
    let _ = append("tui-runtime.log", diagnostic.message());
}

pub fn log_directory() -> Option<PathBuf> {
    if cfg!(windows) {
        return nonempty_env("LOCALAPPDATA")
            .map(|root| root.join("Gate4Agent").join("logs"));
    }
    if let Some(root) = nonempty_env("XDG_STATE_HOME") {
        return Some(root.join("gate4agent").join("logs"));
    }
    nonempty_env("HOME").map(|root| root.join(".local").join("state").join("gate4agent").join("logs"))
}

fn append(name: &str, message: &str) -> io::Result<()> {
    let Some(directory) = log_directory() else {
        return Ok(());
    };
    fs::create_dir_all(&directory)?;
    let path = directory.join(name);
    rotate_if_needed(&path)?;
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let message = sanitize(message);
    writeln!(file, "{timestamp} {message}")?;
    file.flush()
}

fn rotate_if_needed(path: &Path) -> io::Result<()> {
    if fs::metadata(path).map(|metadata| metadata.len()).unwrap_or(0) < MAX_LOG_BYTES {
        return Ok(());
    }
    let rotated = path.with_extension("log.1");
    let _ = fs::remove_file(&rotated);
    fs::rename(path, rotated)
}

fn sanitize(message: &str) -> String {
    let mut output = String::with_capacity(message.len().min(MAX_MESSAGE_BYTES));
    for ch in message.chars() {
        if output.len().saturating_add(ch.len_utf8()) > MAX_MESSAGE_BYTES {
            break;
        }
        if ch.is_control() {
            output.push(' ');
        } else {
            output.push(ch);
        }
    }
    output
}

fn nonempty_env(name: &str) -> Option<PathBuf> {
    env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostic_sanitizer_is_bounded_and_removes_control_characters() {
        let input = format!("secret-free\r\n{}", "x".repeat(MAX_MESSAGE_BYTES * 2));
        let sanitized = sanitize(&input);
        assert!(!sanitized.contains(['\r', '\n']));
        assert!(sanitized.len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn runtime_diagnostic_message_cannot_include_arbitrary_secret_detail() {
        let secret = "token=private endpoint=\\\\.\\pipe\\private path=C:\\private";
        for diagnostic in [
            RuntimeDiagnostic::PreferencesLoadFailed,
            RuntimeDiagnostic::PreferencesSaveFailed,
            RuntimeDiagnostic::C2ReconnectFailed,
            RuntimeDiagnostic::C2InitialSnapshotFailed,
            RuntimeDiagnostic::C2StartupFailed,
            RuntimeDiagnostic::C2ControlDisconnected,
            RuntimeDiagnostic::NodeDisconnected,
            RuntimeDiagnostic::Fatal,
        ] {
            let message = diagnostic.message();
            assert!(!message.contains(secret));
            assert!(message.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
            }));
        }
    }
}

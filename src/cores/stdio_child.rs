//! Piped stdio child. No PTY, no screen buffer.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use super::CoreError;

const STDERR_TAIL_LINES: usize = 20;

pub(crate) struct StdioChild {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<Result<String, std::io::Error>>,
    stderr_tail: Arc<Mutex<VecDeque<String>>>,
    read_timeout: Duration,
}

impl StdioChild {
    pub(crate) fn spawn(mut command: Command) -> Result<Self, CoreError> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| std::io::Error::other("child stdout was not piped"))?;
        let stderr = child.stderr.take();
        let stderr_tail = Arc::new(Mutex::new(VecDeque::new()));
        if let Some(stderr) = stderr {
            let tail = Arc::clone(&stderr_tail);
            thread::Builder::new()
                .name("core-stderr".into())
                .spawn(move || drain_stderr(stderr, tail))?;
        }
        let (tx, rx) = mpsc::sync_channel(64);
        thread::Builder::new()
            .name("core-stdout".into())
            .spawn(move || read_stdout(stdout, tx))?;
        Ok(Self {
            child,
            stdin,
            lines: rx,
            stderr_tail,
            read_timeout: Duration::from_secs(5),
        })
    }

    pub(crate) fn set_read_timeout(&mut self, timeout: Duration) {
        self.read_timeout = timeout;
    }

    pub(crate) fn write_line(&mut self, line: &str) -> Result<(), CoreError> {
        let stdin = self.stdin.as_mut().ok_or(CoreError::Eof)?;
        stdin.write_all(line.as_bytes())?;
        if !line.ends_with('\n') {
            stdin.write_all(b"\n")?;
        }
        stdin.flush()?;
        Ok(())
    }

    /// One non-empty stdout line, or a timeout / EOF.
    pub(crate) fn read_raw_line(&mut self) -> Result<String, CoreError> {
        match self.lines.recv_timeout(self.read_timeout) {
            Ok(Ok(line)) => Ok(line),
            Ok(Err(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => Err(CoreError::Eof),
            Ok(Err(err)) => Err(CoreError::Io(err)),
            Err(RecvTimeoutError::Timeout) => Err(CoreError::Timeout(self.read_timeout)),
            Err(RecvTimeoutError::Disconnected) => Err(CoreError::Eof),
        }
    }

    pub(crate) fn stderr_tail(&self) -> String {
        let guard = self
            .stderr_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

impl Drop for StdioChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn drain_stderr(stderr: impl std::io::Read, tail: Arc<Mutex<VecDeque<String>>>) {
    let reader = BufReader::new(stderr);
    for line in reader.lines() {
        let Ok(line) = line else { break };
        let mut guard = tail.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.len() == STDERR_TAIL_LINES {
            guard.pop_front();
        }
        guard.push_back(line);
    }
}

fn read_stdout(stdout: impl std::io::Read, tx: SyncSender<Result<String, std::io::Error>>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => {
                let _ = tx.send(Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "stdout closed",
                )));
                break;
            }
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if tx.send(Ok(trimmed.to_owned())).is_err() {
                    break;
                }
            }
            Err(err) => {
                let _ = tx.send(Err(err));
                break;
            }
        }
    }
}

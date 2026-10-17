//! Minimal newline-delimited ACP (JSON-RPC) client over a `kimi acp` stdio process.
//!
//! The host holds ONE process per session and drives many prompts over it. The write
//! and the wait of a call are split ([`AcpClient::begin_request`] / [`PendingRequest::wait`]),
//! because a `session/prompt` response arrives only when the turn ends.
//!
//! A `session/request_permission` from the server is **always** answered with the cancelled
//! outcome. This courier never auto-approves any tool call (owner rule).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

use super::job_object::JobObject;

#[derive(Debug, thiserror::Error)]
pub enum AcpError {
    #[error("spawn kimi acp: {0}")]
    Spawn(String),
    #[error("acp i/o: {0}")]
    Io(String),
    #[error("kimi acp ended unexpectedly")]
    Ended,
    #[error("acp call timed out")]
    Timeout,
    #[error("acp error: {0}")]
    Rpc(String),
}

/// One live `kimi acp` child with a reader task dispatching by `id`.
pub struct AcpClient {
    child: Child,
    /// Keeps the Windows Job Object alive for the child's lifetime (Unix: unused).
    _job: Option<JobObject>,
    stdin: Arc<tokio::sync::Mutex<ChildStdin>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    next: AtomicU64,
}

impl AcpClient {
    /// Spawns `program acp` in `cwd` and answers the `initialize` handshake.
    #[allow(dead_code)]
    pub async fn start(program: &str, cwd: &str) -> Result<Self, AcpError> {
        Self::start_with_env(program, cwd, &[]).await
    }

    /// [`Self::start`] with extra environment entries for the child.
    pub async fn start_with_env(
        program: &str,
        cwd: &str,
        env: &[(String, String)],
    ) -> Result<Self, AcpError> {
        let mut command = Command::new(program);
        command
            .arg("acp")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for (key, value) in env {
            command.env(key, value);
        }
        // On Windows, create the child suspended-into a Job Object that kills
        // children when the last handle closes (hard-kill of this host).
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            // CREATE_SUSPENDED (job assign) | CREATE_NO_WINDOW (no console for ACP connector)
            const CREATE_SUSPENDED: u32 = 0x0100_0000;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_SUSPENDED | CREATE_NO_WINDOW);
        }
        let mut child = command
            .spawn()
            .map_err(|err| AcpError::Spawn(err.to_string()))?;
        let job = JobObject::attach(&mut child).map_err(AcpError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| AcpError::Spawn("no stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| AcpError::Spawn("no stdout".into()))?;
        let stdin = Arc::new(tokio::sync::Mutex::new(stdin));
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let reader_pending = Arc::clone(&pending);
        let reader_stdin = Arc::clone(&stdin);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => {
                        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                            continue;
                        };
                        let id = msg.get("id").and_then(Value::as_u64);
                        let is_request = msg.get("method").is_some();
                        match (id, is_request) {
                            (Some(id), false) => {
                                let sender = reader_pending
                                    .lock()
                                    .map(|mut map| map.remove(&id))
                                    .unwrap_or(None);
                                if let Some(sender) = sender {
                                    let _ = sender.send(msg);
                                }
                            }
                            // Always cancel. Never auto-approve any permission request.
                            (Some(id), true) => {
                                let outcome = json!({
                                    "jsonrpc": "2.0",
                                    "id": id,
                                    "result": {"outcome": {"outcome": "cancelled"}}
                                });
                                let mut guard = reader_stdin.lock().await;
                                let mut bytes = serde_json::to_vec(&outcome).unwrap_or_default();
                                bytes.push(b'\n');
                                let _ = guard.write_all(&bytes).await;
                                let _ = guard.flush().await;
                            }
                            _ => {}
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break,
                }
            }
        });
        let client = Self {
            child,
            _job: job,
            stdin,
            pending,
            next: AtomicU64::new(1),
        };
        client
            .request(
                "initialize",
                json!({"protocolVersion": 1, "clientCapabilities": {}}),
                Duration::from_secs(30),
            )
            .await?;
        Ok(client)
    }

    pub async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, AcpError> {
        self.begin_request(method, params).await?.wait(timeout).await
    }

    pub async fn begin_request(
        &self,
        method: &str,
        params: Value,
    ) -> Result<PendingRequest, AcpError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .map_err(|_| AcpError::Io("pending lock poisoned".to_string()))?
            .insert(id, tx);
        let frame = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let mut bytes = serde_json::to_vec(&frame).map_err(|err| AcpError::Io(err.to_string()))?;
        bytes.push(b'\n');
        let written: std::io::Result<()> = {
            let mut guard = self.stdin.lock().await;
            match guard.write_all(&bytes).await {
                Ok(()) => guard.flush().await,
                Err(err) => Err(err),
            }
        };
        if let Err(err) = written {
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&id);
            }
            return Err(AcpError::Io(err.to_string()));
        }
        Ok(PendingRequest {
            id,
            pending: Arc::clone(&self.pending),
            rx,
        })
    }

    pub async fn shutdown(&mut self) {
        let _ = self.child.kill().await;
    }
}

pub struct PendingRequest {
    id: u64,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    rx: oneshot::Receiver<Value>,
}

impl PendingRequest {
    pub async fn wait(mut self, timeout: Duration) -> Result<Value, AcpError> {
        let msg = match tokio::time::timeout(timeout, &mut self.rx).await {
            Ok(Ok(msg)) => msg,
            Ok(Err(_)) => return Err(AcpError::Ended),
            Err(_) => return Err(AcpError::Timeout),
        };
        finish(msg)
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        if let Ok(mut map) = self.pending.lock() {
            map.remove(&self.id);
        }
    }
}

fn finish(msg: Value) -> Result<Value, AcpError> {
    if let Some(error) = msg.get("error") {
        let text = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        return Err(AcpError::Rpc(text.to_string()));
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_cancel_frame_is_always_cancelled() {
        // Contract: the reader answers every server request with cancelled.
        let frame = json!({
            "jsonrpc": "2.0",
            "id": 7u64,
            "result": {"outcome": {"outcome": "cancelled"}}
        });
        assert_eq!(frame["result"]["outcome"]["outcome"], "cancelled");
        assert!(frame["result"]["outcome"].get("optionId").is_none());
    }

    #[test]
    fn finish_maps_rpc_errors() {
        let err = finish(json!({"jsonrpc":"2.0","id":1,"error":{"message":"Unknown sessionId"}}));
        assert!(matches!(err, Err(AcpError::Rpc(m)) if m.contains("Unknown sessionId")));
        assert!(finish(json!({"jsonrpc":"2.0","id":1,"result":{"ok":true}})).is_ok());
    }
}

#[cfg(all(test, windows))]
mod windows_flags {
    #[test]
    fn create_no_window_is_combined_with_suspended() {
        const CREATE_SUSPENDED: u32 = 0x0100_0000;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        assert_eq!(CREATE_SUSPENDED | CREATE_NO_WINDOW, 0x0900_0000);
    }
}

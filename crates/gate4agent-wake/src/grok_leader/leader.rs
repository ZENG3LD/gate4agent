//! One leader connection: register, initialize, `session/load`, `session/prompt`.
//!
//! Notifications are skipped. Reverse requests are not answered, so a
//! permission modal stays with the TUI (first answer wins). The connection
//! is closed when the prompt response arrives. This never spawns `grok`.

use std::path::Path;
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::{timeout, Instant};

use super::acp::{
    acp_envelope, acp_request, classify_inbound, disconnect_message, initialize_params,
    register_message, registered_is_ready, server_type, session_load_params, session_prompt_params,
    Inbound,
};
use super::frame::{encode_frame, MAX_FRAME_BYTES};
use super::pipe::PushError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
const READY_TIMEOUT: Duration = Duration::from_secs(15);
const INIT_TIMEOUT: Duration = Duration::from_secs(10);
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);
const PROMPT_TIMEOUT: Duration = Duration::from_secs(15 * 60);

const INIT_ID: i64 = 0;
const LOAD_ID: i64 = 1;
const PROMPT_ID: i64 = 2;

pub async fn push_into_session(
    sock: &Path,
    session_id: &str,
    cwd: &str,
    text: &str,
) -> Result<(), PushError> {
    #[cfg(windows)]
    let stream = {
        let pipe = super::pipe::leader_pipe_os_path(sock);
        timeout(
            CONNECT_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                tokio::net::windows::named_pipe::ClientOptions::new().open(pipe)
            }),
        )
        .await
        .map_err(|_| PushError::LeaderAbsent)?
        .map_err(|_| PushError::LeaderAbsent)?
        .map_err(|_| PushError::LeaderAbsent)?
    };
    #[cfg(not(windows))]
    let stream = {
        timeout(CONNECT_TIMEOUT, tokio::net::UnixStream::connect(sock))
            .await
            .map_err(|_| PushError::LeaderAbsent)?
            .map_err(|_| PushError::LeaderAbsent)?
    };
    finish(stream, session_id, cwd, text).await
}

/// Wakes one already-running local session with an already-decrypted room
/// plaintext. This is [`push_into_session`]: `session/load`, then
/// `session/prompt`, on `sock`. It does not open a mailbox, it does not
/// call `POST /mail/send` or `POST /admin/listener`, and it is `Ok` only
/// when the leader answers the prompt.
pub async fn wake_decrypted_room(
    sock: &Path,
    session_id: &str,
    cwd: &str,
    plaintext: &str,
) -> Result<(), PushError> {
    push_into_session(sock, session_id, cwd, plaintext).await
}

/// Synchronous wake for a caller that is not already on a tokio runtime.
///
/// `Ok` means `session/load` succeeded and the `session/prompt` frame was
/// written. The leader's prompt result is the end of the agent turn, so
/// this does not wait for it: the worker thread keeps the pipe open until
/// that result or [`PROMPT_TIMEOUT`]. Waiting here stalled the listen loop
/// for the whole turn and the busy session never saw the next letter.
pub fn wake_decrypted_room_blocking(
    sock: &Path,
    session_id: &str,
    cwd: &str,
    plaintext: &str,
) -> Result<(), PushError> {
    let sock = sock.to_path_buf();
    let session_id = session_id.to_string();
    let cwd = cwd.to_string();
    let plaintext = plaintext.to_string();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .enable_time()
            .build()
        {
            Ok(runtime) => runtime,
            Err(err) => {
                let _ = tx.send(Err(PushError::Protocol(err.to_string())));
                return;
            }
        };
        runtime.block_on(async move {
            let stream = match connect_leader(&sock).await {
                Ok(stream) => stream,
                Err(err) => {
                    let _ = tx.send(Err(err));
                    return;
                }
            };
            let (mut reader, mut writer) = tokio::io::split(stream);
            if let Err(err) =
                prepare_session(&mut reader, &mut writer, &session_id, &cwd).await
            {
                let _ = tx.send(Err(err));
                return;
            }
            let payload = acp_request(PROMPT_ID, "session/prompt", session_prompt_params(&session_id, &plaintext));
            if let Err(err) = write_value(&mut writer, &acp_envelope(&payload)).await {
                let _ = tx.send(Err(err));
                return;
            }
            let _ = tx.send(Ok(()));
            let _ = acp_call_already_written(
                &mut reader,
                PROMPT_ID,
                PROMPT_TIMEOUT,
                PushError::PromptFailed,
            )
            .await;
            let _ = write_value(&mut writer, &disconnect_message()).await;
        });
    });
    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(result) => result,
        Err(_) => Err(PushError::LeaderAbsent),
    }
}

async fn connect_leader(sock: &Path) -> Result<LeaderStream, PushError> {
    #[cfg(windows)]
    {
        let pipe = super::pipe::leader_pipe_os_path(sock);
        timeout(
            CONNECT_TIMEOUT,
            tokio::task::spawn_blocking(move || {
                tokio::net::windows::named_pipe::ClientOptions::new().open(pipe)
            }),
        )
        .await
        .map_err(|_| PushError::LeaderAbsent)?
        .map_err(|_| PushError::LeaderAbsent)?
        .map_err(|_| PushError::LeaderAbsent)
    }
    #[cfg(not(windows))]
    {
        timeout(CONNECT_TIMEOUT, tokio::net::UnixStream::connect(sock))
            .await
            .map_err(|_| PushError::LeaderAbsent)?
            .map_err(|_| PushError::LeaderAbsent)
    }
}

#[cfg(windows)]
type LeaderStream = tokio::net::windows::named_pipe::NamedPipeClient;
#[cfg(not(windows))]
type LeaderStream = tokio::net::UnixStream;

async fn prepare_session<R, W>(
    reader: &mut R,
    writer: &mut W,
    session_id: &str,
    cwd: &str,
) -> Result<(), PushError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_value(writer, &register_message()).await?;
    let registered = read_until(reader, REGISTER_TIMEOUT, |value| {
        registered_is_ready(value).is_some()
    })
    .await?;
    if !registered_is_ready(&registered).unwrap_or(true) {
        read_until(reader, READY_TIMEOUT, |value| {
            server_type(value) == Some("leader_ready")
        })
        .await
        .map_err(|_| PushError::LeaderNotReady)?;
    }
    acp_call(
        reader,
        writer,
        INIT_ID,
        "initialize",
        initialize_params(),
        INIT_TIMEOUT,
        PushError::Protocol,
    )
    .await?;
    acp_call(
        reader,
        writer,
        LOAD_ID,
        "session/load",
        session_load_params(session_id, cwd),
        LOAD_TIMEOUT,
        PushError::SessionLoadFailed,
    )
    .await?;
    Ok(())
}

async fn acp_call_already_written<R, F>(
    reader: &mut R,
    id: i64,
    limit: Duration,
    fail: F,
) -> Result<(), PushError>
where
    R: AsyncRead + Unpin,
    F: Fn(String) -> PushError,
{
    let deadline = Instant::now() + limit;
    loop {
        let value = read_one(reader, deadline).await?;
        if let Some(err) = terminal(&value) {
            return Err(err);
        }
        if server_type(&value) != Some("acp") {
            continue;
        }
        let Some(inner_text) = value.get("payload").and_then(Value::as_str) else {
            continue;
        };
        let inner: Value = match serde_json::from_str(inner_text) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        match classify_inbound(&inner, id) {
            Inbound::Ignore => continue,
            Inbound::Matched { ok: true, .. } => return Ok(()),
            Inbound::Matched { ok: false, error } => {
                let message = truncate(&error.unwrap_or_else(|| "error".to_string()));
                if message.contains("leader_starting") {
                    return Err(PushError::LeaderNotReady);
                }
                return Err(fail(message));
            }
        }
    }
}

async fn finish<S>(stream: S, session_id: &str, cwd: &str, text: &str) -> Result<(), PushError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = tokio::io::split(stream);
    let result = drive(&mut reader, &mut writer, session_id, cwd, text).await;
    let _ = write_value(&mut writer, &disconnect_message()).await;
    result
}

async fn drive<R, W>(
    reader: &mut R,
    writer: &mut W,
    session_id: &str,
    cwd: &str,
    text: &str,
) -> Result<(), PushError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    write_value(writer, &register_message()).await?;
    let registered = read_until(reader, REGISTER_TIMEOUT, |value| {
        registered_is_ready(value).is_some()
    })
    .await?;
    if !registered_is_ready(&registered).unwrap_or(true) {
        read_until(reader, READY_TIMEOUT, |value| {
            server_type(value) == Some("leader_ready")
        })
        .await
        .map_err(|_| PushError::LeaderNotReady)?;
    }

    acp_call(
        reader,
        writer,
        INIT_ID,
        "initialize",
        initialize_params(),
        INIT_TIMEOUT,
        PushError::Protocol,
    )
    .await?;
    acp_call(
        reader,
        writer,
        LOAD_ID,
        "session/load",
        session_load_params(session_id, cwd),
        LOAD_TIMEOUT,
        PushError::SessionLoadFailed,
    )
    .await?;
    acp_call(
        reader,
        writer,
        PROMPT_ID,
        "session/prompt",
        session_prompt_params(session_id, text),
        PROMPT_TIMEOUT,
        PushError::PromptFailed,
    )
    .await
}

async fn acp_call<R, W, F>(
    reader: &mut R,
    writer: &mut W,
    id: i64,
    method: &str,
    params: Value,
    limit: Duration,
    fail: F,
) -> Result<(), PushError>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
    F: Fn(String) -> PushError,
{
    let payload = acp_request(id, method, params);
    write_value(writer, &acp_envelope(&payload)).await?;
    let deadline = Instant::now() + limit;
    loop {
        let value = read_one(reader, deadline).await?;
        if let Some(err) = terminal(&value) {
            return Err(err);
        }
        if server_type(&value) != Some("acp") {
            continue;
        }
        let Some(inner_text) = value.get("payload").and_then(Value::as_str) else {
            continue;
        };
        let inner: Value = match serde_json::from_str(inner_text) {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        match classify_inbound(&inner, id) {
            Inbound::Ignore => continue,
            Inbound::Matched { ok: true, .. } => return Ok(()),
            Inbound::Matched { ok: false, error } => {
                let message = truncate(&error.unwrap_or_else(|| "error".to_string()));
                if message.contains("leader_starting") {
                    return Err(PushError::LeaderNotReady);
                }
                return Err(fail(message));
            }
        }
    }
}

fn terminal(value: &Value) -> Option<PushError> {
    match server_type(value) {
        Some("error") => {
            let message = value
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("error");
            Some(PushError::Protocol(truncate(message)))
        }
        Some("shutting_down") | Some("shutdown") => {
            Some(PushError::Protocol("leader-shutdown".to_string()))
        }
        _ => None,
    }
}

async fn read_until<R, F>(reader: &mut R, limit: Duration, pred: F) -> Result<Value, PushError>
where
    R: AsyncRead + Unpin,
    F: Fn(&Value) -> bool,
{
    let deadline = Instant::now() + limit;
    loop {
        let value = read_one(reader, deadline).await?;
        if let Some(err) = terminal(&value) {
            return Err(err);
        }
        if pred(&value) {
            return Ok(value);
        }
    }
}

async fn read_one<R: AsyncRead + Unpin>(
    reader: &mut R,
    deadline: Instant,
) -> Result<Value, PushError> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err(PushError::Protocol("timeout".to_string()));
    }
    timeout(left, read_value(reader))
        .await
        .map_err(|_| PushError::Protocol("timeout".to_string()))?
}

async fn read_value<R: AsyncRead + Unpin>(reader: &mut R) -> Result<Value, PushError> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(|err| PushError::Protocol(err.to_string()))?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_BYTES {
        return Err(PushError::FrameTooLarge);
    }
    let mut buf = vec![0u8; len as usize];
    reader
        .read_exact(&mut buf)
        .await
        .map_err(|err| PushError::Protocol(err.to_string()))?;
    serde_json::from_slice(&buf).map_err(|err| PushError::Protocol(err.to_string()))
}

async fn write_value<W: AsyncWrite + Unpin>(
    writer: &mut W,
    value: &Value,
) -> Result<(), PushError> {
    let bytes = serde_json::to_vec(value).map_err(|err| PushError::Protocol(err.to_string()))?;
    let frame = encode_frame(&bytes).map_err(|_| PushError::FrameTooLarge)?;
    writer
        .write_all(&frame)
        .await
        .map_err(|err| PushError::Protocol(err.to_string()))?;
    writer
        .flush()
        .await
        .map_err(|err| PushError::Protocol(err.to_string()))?;
    Ok(())
}

fn truncate(message: &str) -> String {
    let mut out = String::new();
    for ch in message.chars().take(180) {
        out.push(ch);
    }
    out
}

#[cfg(all(test, unix))]
mod socket_tests {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    use super::wake_decrypted_room;
    use super::super::frame::encode_frame;

    async fn read_frame(stream: &mut tokio::net::UnixStream) -> Value {
        let mut len_buf = [0u8; 4];
        tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut len_buf))
            .await
            .expect("frame header wait")
            .expect("frame header");
        let len = u32::from_be_bytes(len_buf) as usize;
        assert!(len < 1_000_000, "frame too large");
        let mut buf = vec![0u8; len];
        tokio::time::timeout(Duration::from_secs(3), stream.read_exact(&mut buf))
            .await
            .expect("frame body wait")
            .expect("frame body");
        serde_json::from_slice(&buf).expect("frame json")
    }

    async fn write_frame(stream: &mut tokio::net::UnixStream, value: &Value) {
        let bytes = serde_json::to_vec(value).expect("json");
        let frame = encode_frame(&bytes).expect("frame");
        stream.write_all(&frame).await.expect("write");
        stream.flush().await.expect("flush");
    }

    fn sock_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "mail4agent-grok-leader-{}-{}",
            std::process::id(),
            name
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        dir
    }

    async fn serve(listener: UnixListener, refuse_prompt: bool, prompts: Arc<Mutex<Vec<String>>>) {
        let (mut stream, _) = listener.accept().await.expect("accept");
        let register = read_frame(&mut stream).await;
        assert_eq!(register["type"], "register");
        write_frame(&mut stream, &json!({"type": "registered", "ready": true})).await;
        loop {
            let value = read_frame(&mut stream).await;
            if value.get("type").and_then(Value::as_str) == Some("disconnect") {
                break;
            }
            if value.get("type").and_then(Value::as_str) != Some("acp") {
                continue;
            }
            let payload = value
                .get("payload")
                .and_then(Value::as_str)
                .expect("payload");
            let inner: Value = serde_json::from_str(payload).expect("inner");
            if inner.get("method").and_then(Value::as_str) == Some("session/prompt") {
                let text = inner["params"]["prompt"][0]["text"]
                    .as_str()
                    .expect("prompt text")
                    .to_string();
                prompts.lock().expect("prompts").push(text);
                if refuse_prompt {
                    let id = inner["id"].clone();
                    let body = json!({"jsonrpc":"2.0","id": id, "error": {"message": "refused"}})
                        .to_string();
                    write_frame(&mut stream, &json!({"type":"acp","payload": body})).await;
                    continue;
                }
            }
            let id = inner["id"].clone();
            let body = json!({"jsonrpc":"2.0","id": id, "result": {}}).to_string();
            write_frame(&mut stream, &json!({"type":"acp","payload": body})).await;
        }
    }

    #[tokio::test]
    async fn wake_decrypted_room_prompts_the_leader_socket_once() {
        let dir = sock_dir("once");
        let path = dir.join("leader.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let server = tokio::spawn(serve(listener, false, Arc::clone(&prompts)));
        wake_decrypted_room(&path, "sess-local", "/tmp", "already-decrypted")
            .await
            .expect("prompt answered");
        server.await.expect("server");
        let got = prompts.lock().expect("prompts");
        assert_eq!(got.len(), 1, "session/prompt fired more than once");
        assert_eq!(got[0], "already-decrypted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn wake_decrypted_room_does_not_succeed_when_the_prompt_is_refused() {
        let dir = sock_dir("refused");
        let path = dir.join("leader.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let prompts = Arc::new(Mutex::new(Vec::new()));
        let server = tokio::spawn(serve(listener, true, Arc::clone(&prompts)));
        let err = wake_decrypted_room(&path, "sess-local", "/tmp", "already-decrypted")
            .await
            .expect_err("a refused prompt is not success");
        let _ = tokio::time::timeout(Duration::from_secs(3), server).await;
        assert!(
            !matches!(err, super::super::pipe::PushError::LeaderAbsent),
            "the socket was up; refusal must not look like a missing leader: {err}"
        );
        let text = err.to_string();
        assert!(
            text.contains("prompt-failed") || text.contains("refused"),
            "refusal was swallowed: {err}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

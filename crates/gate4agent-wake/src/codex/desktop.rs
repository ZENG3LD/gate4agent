//! Desktop follower IPC (inspected on Desktop 26.1007.2314.0).
//! Routes to the existing thread owner. No provider spawn, auth access,
//! thread creation or UI input. Internal protocol: fail closed on drift.
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use serde_json::{json, Value};
use super::{WakeError, WakeOutcome};

const MAX_FRAME: usize = 1024 * 1024;
// Owner snapshots include history: the inspected live snapshot exceeded 1 MiB.
const MAX_SNAPSHOT_FRAME: usize = 16 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(20);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

pub fn default_endpoint() -> Option<PathBuf> {
    #[cfg(windows)]
    { Some(PathBuf::from(r"\\.\pipe\codex-ipc")) }
    #[cfg(not(windows))]
    { None } // Unix requires desktop:///absolute/ipc.sock explicitly.
}

pub fn probe(path: &Path, thread: &str) -> Result<(), WakeError> {
    discover(&mut connect(path)?, thread).map(|_| ())
}

pub fn wake(path: &Path, thread: &str, text: &str) -> Result<WakeOutcome, WakeError> {
    wake_on(&mut connect(path)?, thread, text)
}

fn wake_on<S: Read + Write>(stream: &mut S, thread: &str, text: &str) -> Result<WakeOutcome, WakeError> {
    let (client, owner) = discover(stream, thread)?;
    // Observe the real owner's state, not our courier's process-level identity.
    // Busy/unknown states are retryable; never insert another start placeholder.
    // This is not an atomic owner gate: a human can submit after the snapshot.
    // Full concurrency safety requires an owner-side prepared-message operation.
    require_idle(stream, thread, &client, &owner)?;
    let params = json!({"conversationId": thread, "turnStart": {
        "request": {"threadId": thread, "input": [{"type": "text", "text": text, "text_elements": []}]},
        "context": {"inheritThreadSettings": true}
    }});
    let reply = call(stream, "thread-follower-start-turn", 2, params, Some(&client), Some(&owner))?;
    if reply.pointer("/result/result/turn/id").and_then(Value::as_str)
        .filter(|id| !id.is_empty()).is_none() {
        return Err(WakeError::Transport("desktop did not return an accepted turn".into()));
    }
    Ok(WakeOutcome::Delivered)
}

fn require_idle<S: Read + Write>(stream: &mut S, thread: &str, client: &str, owner: &str) -> Result<(), WakeError> {
    write_frame(stream, &json!({"type": "broadcast", "method": "thread-stream-following-changed",
        "version": 1, "sourceClientId": client, "targetClientIds": [owner],
        "params": {"conversationId": thread, "hostId": "local", "following": true}}))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let update = read_frame(stream, deadline)?;
        if update["type"] != "broadcast" || update["method"] != "thread-stream-state-changed"
            || update["version"] != 11 || update["sourceClientId"] != owner
            || update.pointer("/params/conversationId").and_then(Value::as_str) != Some(thread)
            || update.pointer("/params/hostId").and_then(Value::as_str) != Some("local")
            || update.pointer("/params/change/type").and_then(Value::as_str) != Some("snapshot") { continue; }
        let state = &update["params"]["change"]["conversationState"];
        // Fail closed for transient, unloaded and changed protocol states.
        if state["id"] != thread || state["resumeState"] != "resumed"
            || state.pointer("/threadRuntimeStatus/type").and_then(Value::as_str) != Some("idle")
            || state["unconfirmedTurnSubmissions"].as_array().is_some_and(|items| !items.is_empty()) {
            return Err(WakeError::Unavailable("desktop thread busy or idle state unconfirmed; retry later".into()));
        }
        let busy = match state["turnHistory"]["kind"].as_str() {
            Some("canonical") => state["turnHistory"]["history"]["entitiesByKey"].as_object()
                .map(|items| items.values().any(|turn| turn["status"] == "inProgress")),
            _ => state["turns"].as_array().map(|items| items.iter().any(|turn| turn["status"] == "inProgress")),
        };
        if busy != Some(false) {
            return Err(WakeError::Unavailable("desktop turn state busy or unavailable; retry later".into()));
        }
        return Ok(());
    }
}

fn discover<S: Read + Write>(stream: &mut S, thread: &str) -> Result<(String, String), WakeError> {
    let hello = call(stream, "initialize", 0, json!({"clientType": "mail4agent"}), None, None)?;
    let client = hello.pointer("/result/clientId").and_then(Value::as_str)
        .ok_or_else(|| WakeError::Transport("desktop initialization wire changed".into()))?;
    let owner = call(stream, "thread-owner-discovery", 1,
        json!({"hostId": "local", "conversationId": thread}), Some(client), None)?;
    let owner_id = owner["handledByClientId"].as_str()
        .ok_or_else(|| WakeError::Transport("desktop owner discovery wire changed".into()))?;
    if owner.pointer("/result/supportsUntrustedAppInput").and_then(Value::as_bool) != Some(true) {
        return Err(WakeError::Unavailable("desktop owner protocol unsupported".into()));
    }
    Ok((client.into(), owner_id.into()))
}

fn call<S: Read + Write>(stream: &mut S, method: &str, version: u32, params: Value,
    client: Option<&str>, owner: Option<&str>) -> Result<Value, WakeError> {
    let id = format!("m4a-{}-{}", std::process::id(), NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let mut request = json!({"type": "request", "requestId": id, "method": method,
        "version": version, "params": params, "timeoutMs": 15000});
    if let Some(client) = client { request["sourceClientId"] = client.into(); }
    if let Some(owner) = owner { request["targetClientId"] = owner.into(); }
    write_frame(stream, &request)?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let reply = read_frame(stream, deadline)?;
        if reply["type"] != "response" || reply["requestId"] != id { continue; }
        if reply["resultType"] != "success" {
            // Provider errors may contain prompt text; never echo them.
            return Err(WakeError::Unavailable(format!("desktop {method} refused")));
        }
        if let Some(owner) = owner {
            if reply["handledByClientId"] != owner {
                return Err(WakeError::Transport("desktop owner changed during delivery".into()));
            }
        }
        return Ok(reply);
    }
}

fn write_frame<S: Write>(stream: &mut S, value: &Value) -> Result<(), WakeError> {
    let bytes = serde_json::to_vec(value).map_err(|_| transport())?;
    if bytes.is_empty() || bytes.len() > MAX_FRAME { return Err(transport()); }
    stream.write_all(&(bytes.len() as u32).to_le_bytes()).map_err(|_| transport())?;
    stream.write_all(&bytes).map_err(|_| transport())?;
    stream.flush().map_err(|_| transport())
}

fn read_frame<S: Read>(stream: &mut S, deadline: Instant) -> Result<Value, WakeError> {
    let mut header = [0; 4];
    read_exact_until(stream, &mut header, deadline)?;
    let size = u32::from_le_bytes(header) as usize;
    if size == 0 || size > MAX_SNAPSHOT_FRAME { return Err(transport()); }
    let mut bytes = vec![0; size];
    read_exact_until(stream, &mut bytes, deadline)?;
    serde_json::from_slice(&bytes).map_err(|_| transport())
}

fn read_exact_until<S: Read>(stream: &mut S, mut bytes: &mut [u8], deadline: Instant) -> Result<(), WakeError> {
    while !bytes.is_empty() {
        if Instant::now() >= deadline { return Err(WakeError::Transport("desktop IPC timed out".into())); }
        match stream.read(bytes) {
            Ok(0) => return Err(transport()),
            Ok(n) => { bytes = &mut bytes[n..]; }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {},
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock || err.raw_os_error() == Some(232) => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return Err(WakeError::Transport(format!("desktop IPC read failed ({:?}/{})",
                err.kind(), err.raw_os_error().unwrap_or(0)))),
        }
    }
    Ok(())
}

fn transport() -> WakeError { WakeError::Transport("desktop IPC frame failed".into()) }

#[cfg(windows)]
struct Pipe(std::fs::File);

#[cfg(windows)]
impl Read for Pipe {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        use std::os::windows::io::AsRawHandle;
        #[link(name = "kernel32")]
        extern "system" {
            fn PeekNamedPipe(handle: *mut std::ffi::c_void, buffer: *mut std::ffi::c_void,
                size: u32, read: *mut u32, available: *mut u32, left: *mut u32) -> i32;
        }
        let mut available = 0;
        let ok = unsafe { PeekNamedPipe(self.0.as_raw_handle(), std::ptr::null_mut(), 0,
            std::ptr::null_mut(), &mut available, std::ptr::null_mut()) };
        if ok == 0 { return Err(std::io::Error::last_os_error()); }
        if available == 0 { return Err(std::io::ErrorKind::WouldBlock.into()); }
        let size = bytes.len().min(available as usize);
        self.0.read(&mut bytes[..size])
    }
}

#[cfg(windows)]
impl Write for Pipe {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.0.write(bytes) }
    fn flush(&mut self) -> std::io::Result<()> { self.0.flush() }
}

#[cfg(windows)]
fn connect(path: &Path) -> Result<Pipe, WakeError> {
    if !path.to_string_lossy().starts_with(r"\\.\pipe\") {
        return Err(WakeError::Unavailable("desktop requires a local named pipe".into()));
    }
    let stream = std::fs::OpenOptions::new().read(true).write(true).open(path)
        .map_err(|_| WakeError::Unavailable("desktop IPC not available".into()))?;
    Ok(Pipe(stream))
}

#[cfg(unix)]
fn connect(path: &Path) -> Result<std::os::unix::net::UnixStream, WakeError> {
    let stream = std::os::unix::net::UnixStream::connect(path)
        .map_err(|_| WakeError::Unavailable("desktop IPC not available".into()))?;
    stream.set_read_timeout(Some(TIMEOUT)).map_err(|_| transport())?;
    stream.set_write_timeout(Some(TIMEOUT)).map_err(|_| transport())?;
    Ok(stream)
}

#[cfg(not(any(unix, windows)))]
fn connect(_: &Path) -> Result<std::io::Cursor<Vec<u8>>, WakeError> {
    Err(WakeError::Unavailable("desktop IPC unsupported on this platform".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frames_reject_unbounded_and_truncated_input() {
        for bytes in [vec![0,0,0,0], vec![255,255,255,255], vec![4,0,0,0,b'{']] {
            assert!(read_frame(&mut std::io::Cursor::new(bytes), Instant::now()+TIMEOUT).is_err());
        }
    }
    #[test]
    fn frame_roundtrip() {
        let mut stream = std::io::Cursor::new(Vec::new());
        let value = json!({"body": "Тест", "type": "response"});
        write_frame(&mut stream, &value).unwrap();
        stream.set_position(0);
        assert_eq!(read_frame(&mut stream, Instant::now()+TIMEOUT).unwrap(), value);
    }
    #[test]
    fn existing_owner_receives_turn_without_thread_creation() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            for method in ["initialize", "thread-owner-discovery", "thread-follower-start-turn"] {
                if method == "thread-follower-start-turn" {
                    let following = read_frame(&mut stream, Instant::now()+TIMEOUT).unwrap();
                    assert_eq!(following["method"], "thread-stream-following-changed");
                    write_frame(&mut stream, &json!({"type": "broadcast", "method": "thread-stream-state-changed",
                        "version": 11, "sourceClientId": "owner", "params": {"hostId": "local",
                        "conversationId": "existing-thread", "change": {"type": "snapshot", "conversationState": {
                            "id": "existing-thread", "resumeState": "resumed", "threadRuntimeStatus": {"type": "idle"}, "turns": []
                        }}}})).unwrap();
                }
                let req = read_frame(&mut stream, Instant::now()+TIMEOUT).unwrap();
                assert_eq!(req["method"], method);
                let result = match method {
                    "initialize" => json!({"clientId": "courier"}),
                    "thread-owner-discovery" => {
                        assert_eq!(req["params"]["conversationId"], "existing-thread");
                        json!({"supportsUntrustedAppInput": true})
                    }
                    _ => {
                        assert_eq!(req["targetClientId"], "owner");
                        assert_eq!(req["version"], 2);
                        assert_eq!(req["params"]["turnStart"]["request"]["threadId"], "existing-thread");
                        assert_eq!(req["params"]["turnStart"]["request"]["input"][0]["text_elements"], json!([]));
                        json!({"result": {"turn": {"id": "accepted"}}})
                    }
                };
                write_frame(&mut stream, &json!({"type": "response", "requestId": req["requestId"],
                    "resultType": "success", "handledByClientId": "owner", "result": result})).unwrap();
            }
        });
        let mut stream = std::net::TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(TIMEOUT)).unwrap();
        assert_eq!(wake_on(&mut stream, "existing-thread", "letter").unwrap(), WakeOutcome::Delivered);
        server.join().unwrap();
    }

    #[test]
    fn busy_or_unknown_owner_never_receives_start() {
        for status in ["active", "notLoaded", "unknown"] {
            let update = json!({"type": "broadcast", "method": "thread-stream-state-changed", "version": 11,
                "sourceClientId": "owner", "params": {"hostId": "local", "conversationId": "thread",
                "change": {"type": "snapshot", "conversationState": {"id": "thread", "resumeState": "resumed",
                    "threadRuntimeStatus": {"type": status}, "turns": []}}}});
            let mut input = std::io::Cursor::new(Vec::new());
            write_frame(&mut input, &update).unwrap();
            input.set_position(0);
            struct Duplex { input: std::io::Cursor<Vec<u8>>, output: Vec<u8> }
            impl Read for Duplex { fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> { self.input.read(bytes) } }
            impl Write for Duplex {
                fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> { self.output.write(bytes) }
                fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
            }
            let mut stream = Duplex { input, output: Vec::new() };
            assert!(require_idle(&mut stream, "thread", "courier", "owner").is_err());
            let sent = read_frame(&mut std::io::Cursor::new(stream.output), Instant::now()+TIMEOUT).unwrap();
            assert_eq!(sent["method"], "thread-stream-following-changed");
        }
    }
}

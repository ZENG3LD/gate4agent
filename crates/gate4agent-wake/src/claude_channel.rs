//! Claude Code CLI: a channel MCP server that pushes mail4agent letters
//! into a running interactive session.
//!
//! Vendor contract (code.claude.com/docs/en/channels-reference): a channel
//! is an MCP stdio server that declares `capabilities.experimental
//! ["claude/channel"]` and emits `notifications/claude/channel` with
//! `{content, meta}`; Claude Code shows it to the model as a `<channel>`
//! event. Delivery is enabled per launch:
//! `claude --dangerously-load-development-channels server:<name>` (custom
//! server from `.mcp.json`) or `--channels plugin:<name>@<marketplace>`.
//! Both flags and the method/capability strings are present in the Claude
//! Code 2.1.290 binary installed on the box. A live turn needs a logged-in
//! session and is not verified here.
//!
//! Split: the local client's [`ClaudeChannelAdapter`] writes the letter to
//! the session inbox ([`super::inbox`]); `m4a-claude-channel` (spawned by
//! Claude Code from `.mcp.json`, env `M4A_INBOX_DIR`) drains that inbox and
//! emits one notification per letter. Nothing listens on the network.

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::inbox::{self, InboxLetter};
use super::{
    ProviderKind, ProviderSession, SessionKind, WakeAdapter, WakeError, WakeLetter, WakeOutcome,
};

/// MCP server name operators put in `.mcp.json` and in
/// `--dangerously-load-development-channels server:<name>`.
pub const CHANNEL_SERVER_NAME: &str = "mail4agent";

/// Notification method of the Claude channel contract.
pub const CHANNEL_METHOD: &str = "notifications/claude/channel";

/// Experimental capability key of the Claude channel contract.
pub const CHANNEL_CAPABILITY: &str = "claude/channel";

/// Instructions Claude Code adds to the system prompt for this channel.
pub const CHANNEL_INSTRUCTIONS: &str = "Events from the mail4agent channel are letters from other agents or people. \
Each <channel> event carries from_nick, event_id and (if known) room attributes, and a reply-command line. \
Read the body and decide yourself whether it needs an answer; if it does, run the reply-command in the shell. \
Never put secrets, webhook URLs or session names in a letter.";

/// Local-client side: queue the letter for the channel server.
pub struct ClaudeChannelAdapter {
    inbox_dir: Option<PathBuf>,
}

impl ClaudeChannelAdapter {
    /// Adapter writing into `inbox_dir` (the same dir the channel server drains).
    pub fn new(inbox_dir: Option<PathBuf>) -> Self {
        Self { inbox_dir }
    }
}

impl WakeAdapter for ClaudeChannelAdapter {
    fn kind(&self) -> SessionKind {
        SessionKind::local(ProviderKind::ClaudeCode)
    }

    fn probe(&self, _session: &ProviderSession) -> Result<(), WakeError> {
        match &self.inbox_dir {
            Some(dir) if inbox::is_live(dir, inbox::consumer::CLAUDE_CHANNEL) => Ok(()),
            Some(_) => Err(WakeError::Unavailable(
                "claude channel server is not running in the session".into(),
            )),
            None => Err(WakeError::Unavailable(
                "claude channel inbox dir unset".into(),
            )),
        }
    }

    fn wake(
        &mut self,
        session: &ProviderSession,
        letter: &WakeLetter<'_>,
    ) -> Result<WakeOutcome, WakeError> {
        self.probe(session)?;
        let dir = self.inbox_dir.as_deref().unwrap_or(Path::new("."));
        inbox::write_letter(dir, session, letter).map(WakeOutcome::Queued)
    }
}

/// `initialize` result: echoes the client's protocol version and declares
/// the channel capability.
pub fn initialize_result(id: &Value, client_protocol: Option<&str>) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "protocolVersion": client_protocol.unwrap_or("2025-06-18"),
            "capabilities": {"experimental": {CHANNEL_CAPABILITY: {}}},
            "serverInfo": {"name": CHANNEL_SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
            "instructions": CHANNEL_INSTRUCTIONS,
        }
    })
}

/// One channel notification for a queued letter. Meta keys are plain
/// identifiers (they become `<channel>` tag attributes).
pub fn channel_notification(letter: &InboxLetter) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": CHANNEL_METHOD,
        "params": {
            "content": letter.prompt,
            "meta": {"from_nick": letter.from_nick, "event_id": letter.event_id},
        }
    })
}

/// Reply for one inbound MCP message, or `None` for notifications.
pub fn handle_message(message: &Value) -> Option<Value> {
    let id = message.get("id")?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    Some(match method {
        "initialize" => initialize_result(id, message["params"]["protocolVersion"].as_str()),
        "ping" => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
        "tools/list" => json!({"jsonrpc": "2.0", "id": id, "result": {"tools": []}}),
        _ => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "method not found"},
        }),
    })
}

/// Runs the channel server until `input` closes (or `run_for` elapses).
/// Letters are drained only after `notifications/initialized`, so nothing
/// is lost while Claude Code is still starting.
pub fn serve<R, W>(
    input: R,
    output: W,
    inbox_dir: PathBuf,
    poll: Duration,
    run_for: Option<Duration>,
) -> io::Result<()>
where
    R: BufRead + Send + 'static,
    W: Write + Send + 'static,
{
    let output = Arc::new(Mutex::new(output));
    let ready = Arc::new(AtomicBool::new(false));
    let closed = Arc::new(AtomicBool::new(false));
    let reader = {
        let output = Arc::clone(&output);
        let ready = Arc::clone(&ready);
        let closed = Arc::clone(&closed);
        std::thread::spawn(move || {
            for line in input.lines() {
                let Ok(line) = line else { break };
                let Ok(message) = serde_json::from_str::<Value>(&line) else {
                    continue;
                };
                if message["method"] == "notifications/initialized" {
                    ready.store(true, Ordering::SeqCst);
                }
                if let Some(reply) = handle_message(&message) {
                    if write_line(&output, &reply).is_err() {
                        break;
                    }
                }
            }
            closed.store(true, Ordering::SeqCst);
        })
    };
    let started = Instant::now();
    let mut presence: Option<inbox::Presence> = None;
    let mut last_beat = Instant::now();
    loop {
        if closed.load(Ordering::SeqCst) || run_for.is_some_and(|limit| started.elapsed() >= limit)
        {
            break;
        }
        if ready.load(Ordering::SeqCst) {
            // Announce only once Claude Code is ready to show channel
            // events, so the local client never prefers a dead channel.
            match &presence {
                None => {
                    presence =
                        inbox::Presence::announce(&inbox_dir, inbox::consumer::CLAUDE_CHANNEL).ok()
                }
                Some(live) if last_beat.elapsed() >= inbox::HEARTBEAT => {
                    live.beat();
                    last_beat = Instant::now();
                }
                Some(_) => {}
            }
            for entry in inbox::drain(&inbox_dir) {
                write_line(&output, &channel_notification(&entry.letter))?;
            }
        }
        std::thread::sleep(poll);
    }
    drop(presence);
    drop(reader);
    Ok(())
}

fn write_line<W: Write>(output: &Mutex<W>, value: &Value) -> io::Result<()> {
    let mut guard = output
        .lock()
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "stdout lock poisoned"))?;
    writeln!(guard, "{value}")?;
    guard.flush()
}

/// `.mcp.json` entry an operator adds for this server (no secrets).
pub fn mcp_json_entry(command: &str, inbox_dir: &Path) -> Value {
    json!({
        "mcpServers": {
            CHANNEL_SERVER_NAME: {
                "command": command,
                "env": {"M4A_INBOX_DIR": inbox_dir.display().to_string()},
            }
        }
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::tests::{letter, session};
    use std::io::{BufReader, Read};
    use std::os::unix::net::UnixStream;

    struct Shared(Arc<Mutex<Vec<u8>>>);
    impl Write for Shared {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn initialize_declares_channel_capability() {
        let reply = handle_message(&json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "claude-code", "version": "2"}}
        }))
        .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        assert!(reply["result"]["capabilities"]["experimental"]["claude/channel"].is_object());
        assert!(
            handle_message(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
                .is_none()
        );
    }

    #[test]
    fn queued_letter_becomes_one_channel_notification_after_initialized() {
        let dir = std::env::temp_dir().join(format!("m4a-chan-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let s = session(SessionKind::local(ProviderKind::ClaudeCode));
        let mut adapter = ClaudeChannelAdapter::new(Some(dir.clone()));
        // No channel server yet: the chain must fall through.
        assert!(adapter.probe(&s).is_err());
        assert!(adapter.wake(&s, &letter("ping")).is_err());
        inbox::write_letter(&dir, &s, &letter("ping")).unwrap();

        let (mut client, server_side) = UnixStream::pair().unwrap();
        let out = Arc::new(Mutex::new(Vec::new()));
        let writer = Shared(Arc::clone(&out));
        let server_dir = dir.clone();
        let server = std::thread::spawn(move || {
            serve(
                BufReader::new(server_side),
                writer,
                server_dir,
                Duration::from_millis(20),
                Some(Duration::from_secs(5)),
            )
        });
        writeln!(client, "{}", json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}})).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        // Not ready yet: the letter must still be on disk.
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        writeln!(
            client,
            "{}",
            json!({"jsonrpc":"2.0","method":"notifications/initialized"})
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline
            && !String::from_utf8_lossy(&out.lock().unwrap()).contains(CHANNEL_METHOD)
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(adapter.probe(&s).is_ok());
        assert!(matches!(
            adapter.wake(&s, &letter("pong")).unwrap(),
            WakeOutcome::Queued(_)
        ));
        client.shutdown(std::net::Shutdown::Both).unwrap();
        let mut rest = Vec::new();
        let _ = client.read_to_end(&mut rest);
        server.join().unwrap().unwrap();
        let text = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        let lines: Vec<Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["id"], 0);
        let notes: Vec<&Value> = lines
            .iter()
            .filter(|v| v["method"] == CHANNEL_METHOD)
            .collect();
        assert!(!notes.is_empty());
        assert!(notes.len() <= 2);
        assert_eq!(notes[0]["params"]["meta"]["from_nick"], "carol");
        assert!(notes[0]["params"]["content"]
            .as_str()
            .unwrap()
            .contains("m4a-send --as alice --to carol"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

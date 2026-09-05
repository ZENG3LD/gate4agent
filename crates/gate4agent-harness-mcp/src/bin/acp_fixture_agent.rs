//! A minimal ACP-speaking fixture agent over stdio, built for
//! `windows_operator_started_harness_mcp_acp_e2e.rs`'s assertion (5): the
//! only ACP fixture agent this workspace ships (`gate4agent-testkit`'s
//! `acp_agent_spec`/`grok_acp_agent_spec`) deliberately ERRORS the handshake
//! the moment `session/new.mcpServers` is anything but `[]` (it exists to
//! prove the opposite case), so it cannot serve as the receiving end for a
//! live harness-MCP overlay. This binary does the minimum real ACP v1
//! handshake needed to receive that overlay and hand it back to the test
//! for inspection, nothing more.
//!
//! Wire framing: plain newline-delimited JSON-RPC 2.0, one message per
//! line, no `Content-Length` header -- the same framing every other ACP
//! fixture in this workspace uses (`gate4agent-testkit::acp_fixture_launch`)
//! and the shape `gate4agent`'s own `src/acp/session.rs` writes and reads.
//!
//! Handled methods, in the order a real handshake exercises them:
//! - `initialize` -> `protocolVersion: 1`, `agentCapabilities` (including a
//!   `promptCapabilities` block, matching the real ACP v1 shape).
//! - `session/new` -> writes the ENTIRE received `params` object verbatim to
//!   the file named by env `G4A_ACP_FIXTURE_CAPTURE` (when set) before
//!   replying, so a test driving this binary as a child process can inspect
//!   exactly what the host sent -- most importantly `mcpServers[0]`, which
//!   this binary itself never inspects or gates on (unlike the testkit
//!   fixture above). Replies with a session id and a `modes` block
//!   (`currentModeId: "auto"`, one `availableModes` entry `"auto"`/`"Auto"`).
//! - `session/set_mode` -> replies success with an empty result, ack-only;
//!   this binary does not track or enforce mode.
//! - `session/prompt` -> emits one `session/update` notification
//!   (`agent_message_chunk`, text `"READY"`), then replies
//!   `stopReason: "end_turn"`.
//!
//! Anything else (unrecognized method, notification) is ignored, matching
//! `acp_fixture_launch`'s own tolerance. Exits cleanly on stdin EOF.
//!
//! `--version` is handled BEFORE the JSON-RPC loop even starts: the Node's
//! own `ProviderRuntimeMonitor` probes `<launcher> --version` with a bounded
//! deadline as part of every runtime admission
//! (`gate4agent-runtime-native::vendor_contract::provider_version_command`,
//! `CODEX_VERSION_ARGV`), and this binary answering instantly here (instead
//! of sitting in the JSON-RPC read loop until that probe's own deadline
//! kills it) keeps every admission call cheap. The probe's own classification
//! of this output is irrelevant -- `require_policy`'s `Acp` arm admits on the
//! catalog's static `acp_transport` declaration alone, never on the probe's
//! verdict -- so this reply's exact shape does not have to mimic any real
//! vendor's `--version` output.

use std::io::{self, BufRead, Write};

use serde_json::{json, Value};

const SESSION_ID: &str = "fixture-acp-session";

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--version") {
        println!("acp-fixture-agent 0.0.0");
        return;
    }
    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(trimmed) else {
            continue;
        };
        let id = message.get("id").cloned();
        let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let Some(method) = method else { continue };
        match method.as_str() {
            "initialize" => {
                let Some(id) = id else { continue };
                write_response(&mut stdout, id, initialize_result());
            }
            "session/new" => {
                let Some(id) = id else { continue };
                capture_session_new_params(&params);
                write_response(&mut stdout, id, session_new_result());
            }
            "session/set_mode" => {
                let Some(id) = id else { continue };
                let mode_id = params.get("modeId").and_then(Value::as_str).map(str::to_owned);
                write_response(&mut stdout, id, json!({}));
                // Corroborates the mode switch `apply_acp_approval_mode`
                // (`gate4agent-shell-native/src/lib.rs`) waits for, bounded by
                // its own `ACP_MODE_CONFIRMATION_TIMEOUT` -- optional per the
                // ACP spec, but sending it keeps this fixture from stalling
                // every FullAuto/Moderate/ReadOnly dispatch for that whole
                // bound.
                if let Some(mode_id) = mode_id {
                    write_notification(&mut stdout, "session/update", current_mode_update(&mode_id));
                }
            }
            "session/prompt" => {
                let Some(id) = id else { continue };
                write_notification(&mut stdout, "session/update", session_update_ready());
                write_response(&mut stdout, id, json!({ "stopReason": "end_turn" }));
            }
            _ => continue,
        }
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": 1,
        "agentCapabilities": {
            "loadSession": false,
            "promptCapabilities": {
                "image": false,
                "audio": false,
                "embeddedContext": false,
            },
        },
        "agentInfo": {
            "name": "acp-fixture-agent",
            "title": "ACP Fixture Agent",
            "version": "0.0.0",
        },
        "authMethods": [],
    })
}

fn session_new_result() -> Value {
    json!({
        "sessionId": SESSION_ID,
        "modes": {
            "currentModeId": "auto",
            "availableModes": [
                { "id": "auto", "name": "Auto" },
                // `codex` at `ApprovalLevel::FullAuto` resolves `acp_mode_id:
                // Some("agent-full-access")` (`gate4agent-catalog::launch::
                // approval_level_resolution`) -- `apply_acp_approval_mode`
                // (`gate4agent-shell-native/src/lib.rs`) refuses the spawn
                // by name (`ApprovalLevelNotOfferedByAgent`) unless this
                // exact id is among the modes handed back here, BEFORE
                // `session/set_mode` is ever called. Advertised alongside
                // `auto` (this agent's own default), not in its place.
                { "id": "agent-full-access", "name": "Full Access" },
            ],
        },
    })
}

fn session_update_ready() -> Value {
    json!({
        "sessionId": SESSION_ID,
        "update": {
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": "READY" },
        },
    })
}

fn current_mode_update(mode_id: &str) -> Value {
    json!({
        "sessionId": SESSION_ID,
        "update": {
            "sessionUpdate": "current_mode_update",
            "currentModeId": mode_id,
        },
    })
}

/// Writes the received `session/new` params verbatim to the file named by
/// `G4A_ACP_FIXTURE_CAPTURE`, when that env var is set. Silently does
/// nothing otherwise or if the write fails -- a capture is a diagnostic aid
/// for the test driving this binary, never something this agent's own
/// handshake depends on completing.
fn capture_session_new_params(params: &Value) {
    let Some(path) = std::env::var_os("G4A_ACP_FIXTURE_CAPTURE") else {
        return;
    };
    let Ok(serialized) = serde_json::to_vec(params) else {
        return;
    };
    let _ = std::fs::write(path, serialized);
}

fn write_response(stdout: &mut io::Stdout, id: Value, result: Value) {
    write_line(stdout, &json!({ "jsonrpc": "2.0", "id": id, "result": result }));
}

fn write_notification(stdout: &mut io::Stdout, method: &str, params: Value) {
    write_line(stdout, &json!({ "jsonrpc": "2.0", "method": method, "params": params }));
}

fn write_line(stdout: &mut io::Stdout, value: &Value) {
    let Ok(serialized) = serde_json::to_string(value) else {
        return;
    };
    let mut handle = stdout.lock();
    let _ = writeln!(handle, "{serialized}");
    let _ = handle.flush();
}

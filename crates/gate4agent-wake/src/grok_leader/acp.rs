//! ACP JSON-RPC bodies carried inside a leader `Acp` frame, and the
//! inbound classifier.
//!
//! A reverse request (`session/request_permission` and the other shared
//! modals) is broadcast to every leader subscriber and the first answer
//! wins. This courier never answers one. Silence leaves the modal with the
//! TUI that owns the session.

use serde_json::{json, Value};

pub enum Inbound {
    Matched { ok: bool, error: Option<String> },
    Ignore,
}

pub fn register_message() -> Value {
    json!({
        "type": "register",
        "client_type": "mail4agent-grok",
        "mode": "stdio",
        "capabilities": {}
    })
}

/// Old leaders omit `ready`. They are already initialised, so missing means ready.
pub fn registered_is_ready(value: &Value) -> Option<bool> {
    if value.get("type").and_then(Value::as_str) != Some("registered") {
        return None;
    }
    Some(value.get("ready").and_then(Value::as_bool).unwrap_or(true))
}

pub fn server_type(value: &Value) -> Option<&str> {
    value.get("type").and_then(Value::as_str)
}

pub fn acp_request(id: i64, method: &str, params: Value) -> String {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": method,
        "params": params
    })
    .to_string()
}

pub fn initialize_params() -> Value {
    json!({
        "protocolVersion": 1,
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false
        },
        "clientInfo": { "name": "mail4agent-grok", "version": env!("CARGO_PKG_VERSION") }
    })
}

pub fn session_load_params(session_id: &str, cwd: &str) -> Value {
    json!({
        "sessionId": session_id,
        "cwd": cwd,
        "mcpServers": []
    })
}

pub fn session_prompt_params(session_id: &str, text: &str) -> Value {
    json!({
        "sessionId": session_id,
        "prompt": [{ "type": "text", "text": text }]
    })
}

pub fn acp_envelope(payload: &str) -> Value {
    json!({ "type": "acp", "payload": payload })
}

pub fn disconnect_message() -> Value {
    json!({ "type": "disconnect" })
}

/// `want` is the id we sent. A leader may hand it back as a number, as that
/// number's decimal string, or restored from a `client|id` namespace.
pub fn classify_inbound(value: &Value, want: i64) -> Inbound {
    if value.get("method").is_some() {
        return Inbound::Ignore;
    }
    let Some(id) = value.get("id") else {
        return Inbound::Ignore;
    };
    if !id_is(id, want) {
        return Inbound::Ignore;
    }
    if let Some(err) = value.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_string();
        return Inbound::Matched { ok: false, error: Some(message) };
    }
    if value.get("result").is_some() {
        return Inbound::Matched { ok: true, error: None };
    }
    Inbound::Ignore
}

fn id_is(value: &Value, want: i64) -> bool {
    match value {
        Value::Number(number) => number.as_i64() == Some(want),
        Value::String(text) => {
            let want = want.to_string();
            text == &want || text.rsplit_once('|').is_some_and(|(_, tail)| tail == want)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_is_a_stdio_client_with_empty_capabilities() {
        let message = register_message();
        assert_eq!(message["type"], "register");
        assert_eq!(message["mode"], "stdio");
        assert_eq!(message["client_type"], "mail4agent-grok");
        assert!(message["capabilities"].as_object().unwrap().is_empty());
    }

    #[test]
    fn missing_ready_means_the_leader_is_ready() {
        let old = serde_json::from_str(r#"{"type":"registered","client_id":7}"#).unwrap();
        assert_eq!(registered_is_ready(&old), Some(true));
        let starting = serde_json::json!({"type":"registered","client_id":7,"ready":false});
        assert_eq!(registered_is_ready(&starting), Some(false));
        assert_eq!(registered_is_ready(&serde_json::json!({"type":"pong"})), None);
    }

    #[test]
    fn initialize_uses_an_integer_protocol_version() {
        let params = initialize_params();
        let raw = serde_json::to_string(&params).unwrap();
        assert!(raw.contains(r#""protocolVersion":1"#));
        assert!(!raw.contains(r#""protocolVersion":"1""#));
    }

    #[test]
    fn session_load_carries_cwd_and_an_empty_mcp_list() {
        let params = session_load_params("sess-a", r"C:\work");
        assert_eq!(params["sessionId"], "sess-a");
        assert_eq!(params["cwd"], r"C:\work");
        assert_eq!(params["mcpServers"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn a_permission_request_is_not_answered_even_when_the_id_matches() {
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "session/request_permission",
            "params": {}
        });
        assert!(matches!(classify_inbound(&request, 2), Inbound::Ignore));
    }

    #[test]
    fn a_result_and_an_error_match_only_their_id() {
        let ok = serde_json::json!({"jsonrpc":"2.0","id":2,"result":{"stopReason":"end_turn"}});
        assert!(matches!(classify_inbound(&ok, 2), Inbound::Matched { ok: true, .. }));
        assert!(matches!(classify_inbound(&ok, 1), Inbound::Ignore));

        let namespaced = serde_json::json!({
            "jsonrpc": "2.0",
            "id": "7|2",
            "error": { "message": "leader_starting" }
        });
        match classify_inbound(&namespaced, 2) {
            Inbound::Matched { ok: false, error: Some(message) } => {
                assert_eq!(message, "leader_starting");
            }
            Inbound::Matched { .. } => panic!("expected the leader_starting error"),
            Inbound::Ignore => panic!("a namespaced id must still match"),
        }
    }
}

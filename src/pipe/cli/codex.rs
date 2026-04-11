//! Pipe-mode Codex bindings: NDJSON parser + spawn builder.

use super::traits::{CliEvent, NdjsonParser};
use crate::utils::truncate_str;
use crate::transport::SpawnOptions;

/// Codex CLI JSONL parser.
///
/// Expects output from: `codex exec --json "prompt"`
///
/// Event types: "thread.started", "turn.started", "turn.completed", "item.started", "item.completed"
pub struct CodexNdjsonParser {
    thread_id: Option<String>,
}

impl CodexNdjsonParser {
    pub fn new() -> Self {
        Self { thread_id: None }
    }
}

impl Default for CodexNdjsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonParser for CodexNdjsonParser {
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent> {
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }

        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                return vec![CliEvent::Error {
                    message: format!("invalid JSON: {}", truncate_str(line, 100)),
                }]
            }
        };

        let mut events = Vec::new();

        match v.get("type").and_then(|t| t.as_str()) {
            Some("thread.started") => {
                let tid = v
                    .get("thread_id")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                self.thread_id = Some(tid.clone());
                events.push(CliEvent::SessionStart {
                    session_id: tid,
                    model: "codex".to_string(),
                    tools: vec![],
                });
            }
            Some("turn.completed") => {
                if let Some(usage) = v.get("usage") {
                    let input = usage
                        .get("input_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    let output = usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0);
                    events.push(CliEvent::TurnComplete {
                        input_tokens: input,
                        output_tokens: output,
                        cache_read_tokens: 0,
                        cache_write_tokens: 0,
                        reasoning_tokens: 0,
                        context_window: None,
                        is_cumulative: false,
                    });
                }
            }
            Some("event_msg") => {
                if let Some(payload) = v.get("payload") {
                    if payload.get("type").and_then(|t| t.as_str()) == Some("token_count") {
                        if let Some(info) = payload.get("info") {
                            let usage = info.get("total_token_usage");
                            let input = usage
                                .and_then(|u| u.get("input_tokens"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let output = usage
                                .and_then(|u| u.get("output_tokens"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let cached = usage
                                .and_then(|u| u.get("cached_input_tokens"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let reasoning = usage
                                .and_then(|u| u.get("reasoning_output_tokens"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let ctx_window = info
                                .get("model_context_window")
                                .and_then(|v| v.as_u64());
                            // Codex `input_tokens` INCLUDES cached — subtract
                            // to match the normalized convention where input =
                            // uncached portion only (same as OpenCode/Claude).
                            // Formula: used = input + output + cache_read + cache_write
                            let net_input = input.saturating_sub(cached);
                            events.push(CliEvent::TurnComplete {
                                input_tokens: net_input,
                                output_tokens: output,
                                cache_read_tokens: cached,
                                cache_write_tokens: 0,
                                reasoning_tokens: reasoning,
                                context_window: ctx_window,
                                is_cumulative: true,
                            });
                        }
                    }
                }
            }
            Some("turn.failed") => {
                let msg = v
                    .get("error")
                    .and_then(|e| e.as_str())
                    .unwrap_or("turn failed")
                    .to_string();
                events.push(CliEvent::Error { message: msg });
            }
            Some("item.started") => {
                if let Some(item) = v.get("item") {
                    match item.get("type").and_then(|t| t.as_str()) {
                        Some("command_execution") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let cmd = item
                                .get("command")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CliEvent::ToolCallStart {
                                id,
                                name: "Bash".to_string(),
                                input: serde_json::json!({"command": cmd}),
                            });
                        }
                        // Both "agent_message" and "assistant_message" are valid names
                        // depending on the Codex version. Treat them as aliases.
                        Some("agent_message") | Some("assistant_message") => {
                            // Message starting — will get content in item.completed
                        }
                        Some("file_change") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CliEvent::ToolCallStart {
                                id,
                                name: "FileChange".to_string(),
                                input: item.clone(),
                            });
                        }
                        Some("reasoning") => {
                            if let Some(text) =
                                item.get("text").and_then(|s| s.as_str())
                            {
                                events.push(CliEvent::Thinking {
                                    text: text.to_string(),
                                });
                            }
                        }
                        // MCP tool, web search, and plan update items — surface as
                        // ToolCallStart so callers can log/display them.
                        Some("mcp_tool_call") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let name = item
                                .get("tool_name")
                                .and_then(|s| s.as_str())
                                .unwrap_or("mcp_tool_call")
                                .to_string();
                            events.push(CliEvent::ToolCallStart {
                                id,
                                name,
                                input: item.clone(),
                            });
                        }
                        Some("web_search") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let query = item
                                .get("query")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CliEvent::ToolCallStart {
                                id,
                                name: "web_search".to_string(),
                                input: serde_json::json!({"query": query}),
                            });
                        }
                        Some("plan_update") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CliEvent::ToolCallStart {
                                id,
                                name: "plan_update".to_string(),
                                input: item.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            Some("item.completed") => {
                if let Some(item) = v.get("item") {
                    match item.get("type").and_then(|t| t.as_str()) {
                        Some("command_execution") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            // [BUGFIX] Codex emits "aggregated_output", not "output".
                            // Reading "output" always returned an empty string for all
                            // shell command results in pipe mode.
                            let output = item
                                .get("aggregated_output")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let status =
                                item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                            events.push(CliEvent::ToolCallResult {
                                id,
                                output,
                                is_error: status == "failed",
                                duration_ms: None,
                            });
                        }
                        // Both "agent_message" and "assistant_message" are valid names
                        // depending on the Codex version. Treat them as aliases.
                        Some("agent_message") | Some("assistant_message") => {
                            let text = item
                                .get("text")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            if !text.is_empty() {
                                events.push(CliEvent::AssistantText {
                                    text,
                                    is_delta: false,
                                });
                            }
                        }
                        Some("file_change") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            events.push(CliEvent::ToolCallResult {
                                id,
                                output: "file changed".to_string(),
                                is_error: false,
                                duration_ms: None,
                            });
                        }
                        Some("mcp_tool_call") | Some("web_search") | Some("plan_update") => {
                            let id = item
                                .get("id")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let output = item
                                .get("output")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let status =
                                item.get("status").and_then(|s| s.as_str()).unwrap_or("");
                            events.push(CliEvent::ToolCallResult {
                                id,
                                output,
                                is_error: status == "failed",
                                duration_ms: None,
                            });
                        }
                        _ => {}
                    }
                }
            }
            Some("error") => {
                let msg = v
                    .get("message")
                    .and_then(|s| s.as_str())
                    .or_else(|| v.get("error").and_then(|s| s.as_str()))
                    .unwrap_or("unknown error")
                    .to_string();
                events.push(CliEvent::Error { message: msg });
            }
            _ => {}
        }

        events
    }

    fn session_id(&self) -> Option<&str> {
        self.thread_id.as_deref()
    }
}

/// Pipe-mode spawn builder for Codex.
///
/// Argv produced (fresh session):
///   `codex exec --json --full-auto [--model <m>] <prompt>`
///
/// Argv produced (resumed session by ID):
///   `codex exec resume <session_id> --json --full-auto [--model <m>] <prompt>`
///
/// Argv produced (resume last session via `continue_last`):
///   `codex exec resume --last --json --full-auto [--model <m>] <prompt>`
///
/// Available `--sandbox` policies (not added here — callers can pass via `extra_args`):
///   - `read-only` (default) — no file writes, no network
///   - `workspace-write` — files writable, network blocked
///   - `danger-full-access` — arbitrary shell + network (use only in isolated envs)
pub struct CodexPipeBuilder;

impl super::traits::CliCommandBuilder for CodexPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("codex");

        if let Some(ref session_id) = opts.resume_session_id {
            // Resume by ID: `codex exec resume <id> --json --full-auto ...`
            cmd.arg("exec");
            cmd.arg("resume");
            cmd.arg(session_id);
        } else if opts.continue_last {
            // Resume last: `codex exec resume --last --json --full-auto ...`
            cmd.arg("exec");
            cmd.arg("resume");
            cmd.arg("--last");
        } else {
            // Fresh: `codex exec --json --full-auto ...`
            cmd.arg("exec");
        }

        cmd.arg("--json");
        // Codex approval mode is a positional flag, not --permission-mode.
        // Map SpawnOptions::permission_mode to the correct Codex CLI flag.
        match opts.permission_mode.as_deref() {
            Some("suggest") => {
                cmd.arg("--suggest");
            }
            Some("auto-edit") => {
                cmd.arg("--auto-edit");
            }
            _ => {
                cmd.arg("--full-auto");
            }
        }
        cmd.arg("--skip-git-repo-check");

        if let Some(ref model) = opts.model {
            cmd.arg("--model");
            cmd.arg(model);
        }

        for arg in &opts.extra_args {
            cmd.arg(arg);
        }

        // Prompt is the final positional argument.
        cmd.arg(&opts.prompt);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_parses_thread_started() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"thread.started","thread_id":"thread_xyz"}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::SessionStart { session_id, .. } => {
                assert_eq!(session_id, "thread_xyz");
            }
            _ => panic!("expected SessionStart"),
        }
    }

    /// Regression test: Codex emits "aggregated_output" for command_execution results,
    /// NOT "output". Reading the wrong field returns an empty string — this test guards
    /// against regressing to that behavior.
    #[test]
    fn codex_aggregated_output_regression() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"item_1","type":"command_execution","command":"bash -lc 'ls -la'","aggregated_output":"total 48\ndrwxr-xr-x 12 user user 4096 Apr  9 12:00 .","exit_code":0,"status":"completed"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1, "expected exactly one ToolCallResult event");
        match &events[0] {
            CliEvent::ToolCallResult { output, is_error, .. } => {
                assert!(
                    !output.is_empty(),
                    "aggregated_output must not be empty — parser may be reading the wrong field"
                );
                assert!(output.contains("total 48"), "expected ls output in aggregated_output");
                assert!(!is_error, "status=completed should not be an error");
            }
            other => panic!("expected ToolCallResult, got {:?}", other),
        }
    }

    /// Test that "assistant_message" is accepted as an alias for "agent_message"
    /// in item.completed. Some Codex versions use one or the other.
    #[test]
    fn codex_assistant_message_alias_completed() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"item_3","type":"assistant_message","text":"Here is what I found: some result"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1, "expected exactly one AssistantText event");
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "Here is what I found: some result");
                assert!(!is_delta, "completed message should not be a delta");
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }
    }

    /// Test that "assistant_message" in item.started is silently consumed (no events).
    #[test]
    fn codex_assistant_message_alias_started() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.started","item":{"id":"item_3","type":"assistant_message","status":"in_progress"}}"#;
        let events = parser.parse_line(line);
        assert!(
            events.is_empty(),
            "item.started for assistant_message should produce no events"
        );
    }

    #[test]
    fn codex_mcp_tool_call_started() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.started","item":{"id":"mcp_1","type":"mcp_tool_call","tool_name":"get_file_info","status":"in_progress"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallStart { id, name, .. } => {
                assert_eq!(id, "mcp_1");
                assert_eq!(name, "get_file_info");
            }
            other => panic!("expected ToolCallStart for mcp_tool_call, got {:?}", other),
        }
    }

    #[test]
    fn codex_web_search_started() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.started","item":{"id":"ws_1","type":"web_search","query":"rust async patterns","status":"in_progress"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallStart { id, name, input } => {
                assert_eq!(id, "ws_1");
                assert_eq!(name, "web_search");
                assert_eq!(input.get("query").and_then(|q| q.as_str()), Some("rust async patterns"));
            }
            other => panic!("expected ToolCallStart for web_search, got {:?}", other),
        }
    }

    #[test]
    fn codex_plan_update_started() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.started","item":{"id":"plan_1","type":"plan_update","summary":"Step 1: analyze","status":"in_progress"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallStart { id, name, .. } => {
                assert_eq!(id, "plan_1");
                assert_eq!(name, "plan_update");
            }
            other => panic!("expected ToolCallStart for plan_update, got {:?}", other),
        }
    }

    #[test]
    fn codex_mcp_tool_call_completed() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"mcp_1","type":"mcp_tool_call","output":"file info: 4096 bytes","status":"completed"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallResult { id, output, is_error, .. } => {
                assert_eq!(id, "mcp_1");
                assert_eq!(output, "file info: 4096 bytes");
                assert!(!is_error);
            }
            other => panic!("expected ToolCallResult for mcp_tool_call, got {:?}", other),
        }
    }

    #[test]
    fn codex_web_search_completed() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"ws_1","type":"web_search","output":"Found 10 results","status":"completed"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallResult { id, output, is_error, .. } => {
                assert_eq!(id, "ws_1");
                assert_eq!(output, "Found 10 results");
                assert!(!is_error);
            }
            other => panic!("expected ToolCallResult for web_search, got {:?}", other),
        }
    }

    #[test]
    fn codex_plan_update_failed() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"plan_1","type":"plan_update","output":"","status":"failed"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::ToolCallResult { id, is_error, .. } => {
                assert_eq!(id, "plan_1");
                assert!(*is_error, "status=failed should be an error");
            }
            other => panic!("expected ToolCallResult for plan_update, got {:?}", other),
        }
    }

    /// Test that "agent_message" still works.
    #[test]
    fn codex_agent_message_original_still_works() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"item.completed","item":{"id":"item_2","type":"agent_message","text":"Original agent_message format still works"}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, .. } => {
                assert_eq!(text, "Original agent_message format still works");
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }
    }

    #[test]
    fn codex_parses_event_msg_token_count() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":5000,"output_tokens":200,"cached_input_tokens":300,"reasoning_output_tokens":50},"model_context_window":128000}}}"#;
        let events = parser.parse_line(line);
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::TurnComplete {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
                reasoning_tokens,
                context_window,
                is_cumulative,
            } => {
                // input = 5000 - 300 (cached) = 4700 net uncached
                assert_eq!(*input_tokens, 4700);
                assert_eq!(*output_tokens, 200);
                assert_eq!(*cache_read_tokens, 300);
                assert_eq!(*cache_write_tokens, 0);
                assert_eq!(*reasoning_tokens, 50);
                assert_eq!(*context_window, Some(128_000));
                assert!(*is_cumulative, "event_msg token_count must be cumulative");
            }
            other => panic!("expected TurnComplete, got {:?}", other),
        }
    }

    #[test]
    fn codex_event_msg_non_token_count_ignored() {
        let mut parser = CodexNdjsonParser::new();
        let line = r#"{"type":"event_msg","payload":{"type":"some_other_type","info":{}}}"#;
        let events = parser.parse_line(line);
        assert!(events.is_empty(), "non-token_count event_msg must produce no events, got {:?}", events);
    }

    // ── Builder tests ─────────────────────────────────────────────────────────

    fn make_opts(permission_mode: Option<&str>) -> SpawnOptions {
        SpawnOptions {
            permission_mode: permission_mode.map(|s| s.to_string()),
            prompt: "test".to_string(),
            ..SpawnOptions::default()
        }
    }

    fn args_of(cmd: std::process::Command) -> Vec<String> {
        cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn codex_pipe_builder_suggest_mode() {
        use super::super::traits::CliCommandBuilder;
        let builder = CodexPipeBuilder;
        let opts = make_opts(Some("suggest"));
        let args = args_of(builder.build_command(&opts));
        assert!(args.contains(&"--suggest".to_string()), "--suggest must be present, args: {args:?}");
        assert!(!args.contains(&"--full-auto".to_string()), "--full-auto must be absent, args: {args:?}");
        assert!(!args.contains(&"--auto-edit".to_string()), "--auto-edit must be absent, args: {args:?}");
    }

    #[test]
    fn codex_pipe_builder_auto_edit_mode() {
        use super::super::traits::CliCommandBuilder;
        let builder = CodexPipeBuilder;
        let opts = make_opts(Some("auto-edit"));
        let args = args_of(builder.build_command(&opts));
        assert!(args.contains(&"--auto-edit".to_string()), "--auto-edit must be present, args: {args:?}");
        assert!(!args.contains(&"--full-auto".to_string()), "--full-auto must be absent, args: {args:?}");
    }

    #[test]
    fn codex_pipe_builder_default_is_full_auto() {
        use super::super::traits::CliCommandBuilder;
        let builder = CodexPipeBuilder;
        let opts = make_opts(None);
        let args = args_of(builder.build_command(&opts));
        assert!(args.contains(&"--full-auto".to_string()), "--full-auto must be present by default, args: {args:?}");
    }

    #[test]
    fn codex_pipe_builder_full_auto_explicit() {
        use super::super::traits::CliCommandBuilder;
        let builder = CodexPipeBuilder;
        let opts = make_opts(Some("full-auto"));
        let args = args_of(builder.build_command(&opts));
        assert!(args.contains(&"--full-auto".to_string()), "--full-auto must be present for explicit 'full-auto', args: {args:?}");
    }
}

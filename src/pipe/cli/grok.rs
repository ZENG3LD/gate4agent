//! Pipe-mode Grok bindings — not fixture-verified.
//!
//! Grok's supported transport is ACP (`grok agent stdio`); the catalog never
//! populates `transports.pipe` for it (see `gate4agent-catalog::builtin`), so
//! `cli_builder`/`create_ndjson_parser` never dispatch to these types in
//! practice. They exist only to keep the `CliTool` match arms in
//! `pipe/cli/mod.rs` exhaustive, and follow the same minimal shape as the
//! other not-yet-verified pipe bindings.

use super::traits::{CliEvent, NdjsonParser};
use crate::transport::SpawnOptions;

/// Minimal NDJSON parser for Grok pipe output. No real wire format has been
/// captured — every non-empty line is surfaced as plain assistant text.
pub struct GrokNdjsonParser {
    session_id: Option<String>,
}

impl GrokNdjsonParser {
    pub fn new() -> Self {
        Self { session_id: None }
    }
}

impl Default for GrokNdjsonParser {
    fn default() -> Self {
        Self::new()
    }
}

impl NdjsonParser for GrokNdjsonParser {
    fn parse_line(&mut self, line: &str) -> Vec<CliEvent> {
        let line = line.trim();
        if line.is_empty() {
            return vec![];
        }
        vec![CliEvent::AssistantText {
            text: line.to_owned(),
            is_delta: false,
        }]
    }

    fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}

/// Pipe-mode spawn builder for Grok. Unreachable in practice — see the
/// module-level doc.
pub struct GrokPipeBuilder;

impl super::traits::CliCommandBuilder for GrokPipeBuilder {
    fn build_command(&self, opts: &SpawnOptions) -> std::process::Command {
        let mut cmd = std::process::Command::new("grok");
        crate::utils::hide_console_window(&mut cmd);
        cmd.arg(&opts.prompt);
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::traits::CliCommandBuilder;

    #[test]
    fn grok_parser_surfaces_plain_text() {
        let mut parser = GrokNdjsonParser::new();
        let events = parser.parse_line("hello from grok");
        assert_eq!(events.len(), 1);
        match &events[0] {
            CliEvent::AssistantText { text, is_delta } => {
                assert_eq!(text, "hello from grok");
                assert!(!is_delta);
            }
            other => panic!("expected AssistantText, got {:?}", other),
        }
        assert!(parser.session_id().is_none());
    }

    #[test]
    fn grok_parser_empty_line_produces_no_events() {
        let mut parser = GrokNdjsonParser::new();
        assert!(parser.parse_line("").is_empty());
        assert!(parser.parse_line("   ").is_empty());
    }

    #[test]
    fn grok_pipe_builder_uses_grok_binary() {
        let opts = SpawnOptions {
            prompt: "fixture prompt".to_owned(),
            ..Default::default()
        };
        let cmd = GrokPipeBuilder.build_command(&opts);
        assert_eq!(cmd.get_program().to_string_lossy(), "grok");
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, vec!["fixture prompt".to_owned()]);
    }
}

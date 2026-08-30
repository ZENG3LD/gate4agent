//! PTY-specific CLI adapters: output parsers, prompt submitters, and classification pipeline.

pub mod traits;
pub mod pipeline;
pub mod claude;
pub mod codex;

pub use traits::{
    CliCommandBuilder, MessageClass, MessageMetadata, OutputParser, ParsedMessage,
    PromptSubmitter, StartupAction,
};
pub use pipeline::ClassificationPipeline;

use crate::core::types::CliTool;
use std::io;

use self::claude::{ClaudeOutputParser, ClaudePromptSubmitter};
use self::codex::{CodexOutputParser, CodexPromptSubmitter};

struct RawOutputParser {
    tool: CliTool,
    buffer: String,
}

impl RawOutputParser {
    fn new(tool: CliTool) -> Self {
        Self {
            tool,
            buffer: String::new(),
        }
    }
}

impl OutputParser for RawOutputParser {
    fn feed(&mut self, data: &str) {
        self.buffer.push_str(data);
    }

    fn parse(&mut self) -> Vec<ParsedMessage> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        vec![ParsedMessage {
            class: MessageClass::Raw,
            content: std::mem::take(&mut self.buffer),
            metadata: MessageMetadata::for_tool(self.tool),
        }]
    }

    fn extract_ai_text(&self, _raw_cleaned: &str) -> String {
        String::new()
    }

    fn classify(&self, _text: &str) -> MessageClass {
        MessageClass::Raw
    }

    fn buffer(&self) -> &str {
        &self.buffer
    }

    fn clear(&mut self) {
        self.buffer.clear();
    }

    fn tool(&self) -> CliTool {
        self.tool
    }
}

struct UnsupportedPromptSubmitter {
    tool: CliTool,
}

impl UnsupportedPromptSubmitter {
    fn new(tool: CliTool) -> Self {
        Self { tool }
    }

    fn unsupported(&self) -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            format!("{} PTY semantic prompt submission is not fixture-verified", self.tool),
        )
    }
}

impl PromptSubmitter for UnsupportedPromptSubmitter {
    fn send_prompt(&self, _writer: &mut dyn io::Write, _prompt: &str) -> io::Result<()> {
        Err(self.unsupported())
    }

    fn send_command(&self, _writer: &mut dyn io::Write, _command: &str) -> io::Result<()> {
        Err(self.unsupported())
    }

    fn send_control(&self, writer: &mut dyn io::Write, bytes: &[u8]) -> io::Result<()> {
        writer.write_all(bytes)
    }

    fn handle_startup(&self, _output: &str) -> StartupAction {
        StartupAction::Ready
    }

    fn tool(&self) -> CliTool {
        self.tool
    }
}

/// Create an `OutputParser` for the given CLI tool.
pub fn create_parser(tool: CliTool) -> Box<dyn OutputParser> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudeOutputParser::new()),
        CliTool::Codex => Box::new(CodexOutputParser::new()),
        CliTool::KimiCode => Box::new(RawOutputParser::new(CliTool::KimiCode)),
        // Grok's supported transport is ACP, not PTY — unreachable via this
        // legacy `CliTool` PTY path in practice.
        CliTool::Grok => Box::new(RawOutputParser::new(CliTool::Grok)),
    }
}

/// Create a `PromptSubmitter` for the given CLI tool.
pub fn create_submitter(tool: CliTool) -> Box<dyn PromptSubmitter> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudePromptSubmitter::new()),
        CliTool::Codex => Box::new(CodexPromptSubmitter::new()),
        CliTool::KimiCode => Box::new(UnsupportedPromptSubmitter::new(CliTool::KimiCode)),
        // Grok's supported transport is ACP, not PTY — unreachable via this
        // legacy `CliTool` PTY path in practice.
        CliTool::Grok => Box::new(UnsupportedPromptSubmitter::new(CliTool::Grok)),
    }
}

/// Create a full `ClassificationPipeline` for the given CLI tool.
pub fn create_pipeline(tool: CliTool) -> ClassificationPipeline {
    ClassificationPipeline::new(create_parser(tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kimi_pty_parser_is_explicitly_raw() {
        let mut parser = create_parser(CliTool::KimiCode);
        parser.feed("Kimi terminal frame");
        let messages = parser.parse();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].class, MessageClass::Raw);
        assert_eq!(messages[0].metadata.tool, CliTool::KimiCode);
    }

    #[test]
    fn kimi_semantic_submission_is_explicitly_unsupported() {
        let mut output = Vec::new();
        let error = create_submitter(CliTool::KimiCode)
            .send_prompt(&mut output, "hello")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(output.is_empty());
    }
}

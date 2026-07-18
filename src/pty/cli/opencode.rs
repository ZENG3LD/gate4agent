//! Conservative OpenCode PTY adapters.
//!
//! OpenCode's structured Pipe and ACP transports are supported elsewhere. Its
//! interactive terminal protocol has not been fixture-verified, so PTY output
//! remains raw and semantic prompt submission is explicitly unsupported.

use super::traits::{
    MessageClass, MessageMetadata, OutputParser, ParsedMessage, PromptSubmitter, StartupAction,
};
use crate::core::types::CliTool;
use std::io;

pub struct OpenCodeRawParser {
    buffer: String,
}

impl OpenCodeRawParser {
    pub fn new() -> Self {
        Self {
            buffer: String::new(),
        }
    }
}

impl Default for OpenCodeRawParser {
    fn default() -> Self {
        Self::new()
    }
}

impl OutputParser for OpenCodeRawParser {
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
            metadata: MessageMetadata::for_tool(CliTool::OpenCode),
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
        CliTool::OpenCode
    }
}

pub struct OpenCodeUnsupportedSubmitter;

impl OpenCodeUnsupportedSubmitter {
    pub fn new() -> Self {
        Self
    }

    fn unsupported() -> io::Error {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "OpenCode PTY prompt submission is not fixture-verified",
        )
    }
}

impl Default for OpenCodeUnsupportedSubmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl PromptSubmitter for OpenCodeUnsupportedSubmitter {
    fn send_prompt(&self, _writer: &mut dyn io::Write, _prompt: &str) -> io::Result<()> {
        Err(Self::unsupported())
    }

    fn send_command(&self, _writer: &mut dyn io::Write, _command: &str) -> io::Result<()> {
        Err(Self::unsupported())
    }

    fn send_control(&self, writer: &mut dyn io::Write, bytes: &[u8]) -> io::Result<()> {
        writer.write_all(bytes)
    }

    fn handle_startup(&self, _output: &str) -> StartupAction {
        StartupAction::Ready
    }

    fn tool(&self) -> CliTool {
        CliTool::OpenCode
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_parser_never_claims_claude_semantics() {
        let mut parser = OpenCodeRawParser::new();
        parser.feed("some OpenCode terminal output");
        let messages = parser.parse();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].class, MessageClass::Raw);
        assert_eq!(messages[0].metadata.tool, CliTool::OpenCode);
    }

    #[test]
    fn semantic_submission_is_explicitly_unsupported() {
        let mut bytes = Vec::new();
        let error = OpenCodeUnsupportedSubmitter
            .send_prompt(&mut bytes, "hello")
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
        assert!(bytes.is_empty());
    }
}

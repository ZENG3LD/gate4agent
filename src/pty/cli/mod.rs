//! PTY-specific CLI adapters: output parsers, prompt submitters, and classification pipeline.

pub mod traits;
pub mod pipeline;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod opencode;

pub use traits::{
    CliCommandBuilder, MessageClass, MessageMetadata, OutputParser, ParsedMessage,
    PromptSubmitter, StartupAction,
};
pub use pipeline::ClassificationPipeline;

use crate::core::types::CliTool;

use self::claude::{ClaudeOutputParser, ClaudePromptSubmitter};
use self::codex::{CodexOutputParser, CodexPromptSubmitter};
use self::gemini::{GeminiOutputParser, GeminiPromptSubmitter};

/// Create an `OutputParser` for the given CLI tool.
///
/// OpenCode parser is not yet verified against live CLI output.
/// It falls back to the Claude parser as a structural stub.
pub fn create_parser(tool: CliTool) -> Box<dyn OutputParser> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudeOutputParser::new()),
        CliTool::Codex => Box::new(CodexOutputParser::new()),
        CliTool::Gemini => Box::new(GeminiOutputParser::new()),
        // Dedicated parser will be added after real CLI output capture.
        CliTool::OpenCode => Box::new(ClaudeOutputParser::new()),
    }
}

/// Create a `PromptSubmitter` for the given CLI tool.
///
/// OpenCode submitter is not yet verified against live CLI output.
/// It falls back to the Claude submitter as a structural stub.
pub fn create_submitter(tool: CliTool) -> Box<dyn PromptSubmitter> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudePromptSubmitter::new()),
        CliTool::Codex => Box::new(CodexPromptSubmitter::new()),
        CliTool::Gemini => Box::new(GeminiPromptSubmitter::new()),
        // Dedicated submitter will be added after real CLI output capture.
        CliTool::OpenCode => Box::new(ClaudePromptSubmitter::new()),
    }
}

/// Create a full `ClassificationPipeline` for the given CLI tool.
pub fn create_pipeline(tool: CliTool) -> ClassificationPipeline {
    ClassificationPipeline::new(create_parser(tool))
}

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
use self::opencode::{OpenCodeRawParser, OpenCodeUnsupportedSubmitter};

/// Create an `OutputParser` for the given CLI tool.
///
/// OpenCode PTY output remains raw until a semantic parser is fixture-verified.
pub fn create_parser(tool: CliTool) -> Box<dyn OutputParser> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudeOutputParser::new()),
        CliTool::Codex => Box::new(CodexOutputParser::new()),
        CliTool::Gemini => Box::new(GeminiOutputParser::new()),
        CliTool::OpenCode => Box::new(OpenCodeRawParser::new()),
    }
}

/// Create a `PromptSubmitter` for the given CLI tool.
///
/// OpenCode semantic submission returns `io::ErrorKind::Unsupported` until its
/// interactive composer behavior is fixture-verified.
pub fn create_submitter(tool: CliTool) -> Box<dyn PromptSubmitter> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudePromptSubmitter::new()),
        CliTool::Codex => Box::new(CodexPromptSubmitter::new()),
        CliTool::Gemini => Box::new(GeminiPromptSubmitter::new()),
        CliTool::OpenCode => Box::new(OpenCodeUnsupportedSubmitter::new()),
    }
}

/// Create a full `ClassificationPipeline` for the given CLI tool.
pub fn create_pipeline(tool: CliTool) -> ClassificationPipeline {
    ClassificationPipeline::new(create_parser(tool))
}

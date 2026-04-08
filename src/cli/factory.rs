//! Factory functions for creating CLI parsers, submitters, and pipelines.

use crate::types::CliTool;

use super::claude::{ClaudeOutputParser, ClaudePromptSubmitter};
use super::codex::{CodexOutputParser, CodexPromptSubmitter};
use super::gemini::{GeminiOutputParser, GeminiPromptSubmitter};
use super::pipeline::ClassificationPipeline;
use super::traits::{OutputParser, PromptSubmitter};

/// Create an `OutputParser` for the given CLI tool.
///
/// Cursor, OpenCode, and OpenClaw parsers are not yet implemented (Phase 3/4).
/// They fall back to the Claude parser as a structural stub.
pub fn create_parser(tool: CliTool) -> Box<dyn OutputParser> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudeOutputParser::new()),
        CliTool::Codex => Box::new(CodexOutputParser::new()),
        CliTool::Gemini => Box::new(GeminiOutputParser::new()),
        // Phase 3/4: dedicated parsers will be added after real CLI output capture.
        CliTool::Cursor | CliTool::OpenCode | CliTool::OpenClaw => {
            Box::new(ClaudeOutputParser::new())
        }
    }
}

/// Create a `PromptSubmitter` for the given CLI tool.
///
/// Cursor, OpenCode, and OpenClaw submitters are not yet implemented (Phase 3/4).
/// They fall back to the Claude submitter as a structural stub.
pub fn create_submitter(tool: CliTool) -> Box<dyn PromptSubmitter> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudePromptSubmitter::new()),
        CliTool::Codex => Box::new(CodexPromptSubmitter::new()),
        CliTool::Gemini => Box::new(GeminiPromptSubmitter::new()),
        // Phase 3/4: dedicated submitters will be added after real CLI output capture.
        CliTool::Cursor | CliTool::OpenCode | CliTool::OpenClaw => {
            Box::new(ClaudePromptSubmitter::new())
        }
    }
}

/// Create a full `ClassificationPipeline` for the given CLI tool.
pub fn create_pipeline(tool: CliTool) -> ClassificationPipeline {
    ClassificationPipeline::new(create_parser(tool))
}

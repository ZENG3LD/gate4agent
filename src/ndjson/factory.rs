//! Factory for creating per-CLI NDJSON parsers.

use crate::cli::claude::parser::ClaudeNdjsonParser;
use crate::cli::codex::parser::CodexNdjsonParser;
use crate::cli::cursor::parser::CursorNdjsonParser;
use crate::cli::gemini::parser::GeminiNdjsonParser;
use crate::cli::opencode::parser::OpenCodeNdjsonParser;
use crate::ndjson::traits::NdjsonParser;
use crate::types::CliTool;

/// Create an NDJSON parser for the given CLI tool.
pub fn create_ndjson_parser(tool: CliTool) -> Box<dyn NdjsonParser> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudeNdjsonParser::new()),
        CliTool::Codex => Box::new(CodexNdjsonParser::new()),
        CliTool::Gemini => Box::new(GeminiNdjsonParser::new()),
        CliTool::Cursor => Box::new(CursorNdjsonParser::new()),
        CliTool::OpenCode => Box::new(OpenCodeNdjsonParser::new()),
    }
}

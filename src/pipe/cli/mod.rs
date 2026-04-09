//! Pipe-mode CLI adapters: NDJSON parsers and command builders.

pub mod traits;
pub mod claude;
pub mod codex;
pub mod gemini;
pub mod cursor;
pub mod opencode;

pub use traits::{CliCommandBuilder, CliEvent, NdjsonParser};

use crate::core::types::CliTool;

use self::claude::{ClaudeNdjsonParser, ClaudePipeBuilder};
use self::codex::{CodexNdjsonParser, CodexPipeBuilder};
use self::gemini::{GeminiNdjsonParser, GeminiPipeBuilder};
use self::cursor::{CursorNdjsonParser, CursorPipeBuilder};
use self::opencode::{OpenCodeNdjsonParser, OpenCodePipeBuilder};

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

/// Return a boxed `CliCommandBuilder` for the given CLI tool.
///
/// This is the single dispatch point used by `pipe/process.rs` to delegate
/// command construction to the per-CLI builder.
pub fn cli_builder(tool: CliTool) -> Box<dyn CliCommandBuilder> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudePipeBuilder),
        CliTool::Codex => Box::new(CodexPipeBuilder),
        CliTool::Gemini => Box::new(GeminiPipeBuilder),
        CliTool::Cursor => Box::new(CursorPipeBuilder),
        CliTool::OpenCode => Box::new(OpenCodePipeBuilder),
    }
}

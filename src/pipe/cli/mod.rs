//! Pipe-mode CLI adapters: NDJSON parsers and command builders.

pub mod traits;
pub mod claude;
pub mod codex;
pub mod grok;
pub mod kimi;

pub use traits::{CliCommandBuilder, CliEvent, NdjsonParser};

use crate::core::types::CliTool;

use self::claude::{ClaudeNdjsonParser, ClaudePipeBuilder};
use self::codex::{CodexNdjsonParser, CodexPipeBuilder};
use self::grok::{GrokNdjsonParser, GrokPipeBuilder};
use self::kimi::{KimiNdjsonParser, KimiPipeBuilder};

/// Create an NDJSON parser for the given CLI tool.
///
/// Grok's supported transport is ACP, not pipe — the catalog never enables
/// `transports.pipe` for it, so this arm is unreachable in practice.
pub fn create_ndjson_parser(tool: CliTool) -> Box<dyn NdjsonParser> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudeNdjsonParser::new()),
        CliTool::Codex => Box::new(CodexNdjsonParser::new()),
        CliTool::KimiCode => Box::new(KimiNdjsonParser::new()),
        CliTool::Grok => Box::new(GrokNdjsonParser::new()),
    }
}

/// Return a boxed `CliCommandBuilder` for the given CLI tool.
///
/// This is the single dispatch point used by `pipe/process.rs` to delegate
/// command construction to the per-CLI builder.
///
/// Grok's supported transport is ACP, not pipe — this arm is unreachable in
/// practice, same reasoning as `create_ndjson_parser`.
pub fn cli_builder(tool: CliTool) -> Box<dyn CliCommandBuilder> {
    match tool {
        CliTool::ClaudeCode => Box::new(ClaudePipeBuilder),
        CliTool::Codex => Box::new(CodexPipeBuilder),
        CliTool::KimiCode => Box::new(KimiPipeBuilder),
        CliTool::Grok => Box::new(GrokPipeBuilder),
    }
}

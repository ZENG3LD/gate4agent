//! Pipe transport — NDJSON-streaming headless CLI sessions.

pub mod process;
pub mod session;
pub mod cli;

pub use process::{ClaudeOptions, PipeProcess, PipeProcessOptions};
pub use session::PipeSession;
pub use cli::{CliEvent, NdjsonParser, create_ndjson_parser, cli_builder};

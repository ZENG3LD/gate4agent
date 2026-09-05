//! Pipe transport — NDJSON-streaming headless CLI sessions.

pub mod process;
pub mod session;
pub mod cli;

pub use process::{ClaudeOptions, PipeProcess, PipeProcessOptions};
pub use session::{PipeSession, PipeStopOutcome, PIPE_GRACEFUL_STOP_BOUND_SECS};
pub use cli::{CliEvent, NdjsonParser, create_ndjson_parser, cli_builder};

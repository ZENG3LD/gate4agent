pub mod claude;
pub mod codex;
pub mod gemini;
pub mod cursor;
pub mod opencode;
pub mod openclaw;
pub mod traits;
pub mod pipeline;
pub mod factory;

// Convenience flat re-exports for common types
pub use traits::{CliCommandBuilder, MessageClass, MessageMetadata, OutputParser, ParsedMessage, PromptSubmitter, StartupAction};
pub use pipeline::ClassificationPipeline;
pub use factory::{create_parser, create_pipeline, create_submitter, cli_builder};

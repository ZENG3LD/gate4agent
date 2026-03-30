//! Classification pipeline that combines VTE stripping with CLI-specific parsing.

use crate::parser::VteParser;
use crate::types::CliTool;

use super::traits::{MessageClass, OutputParser, ParsedMessage};

/// A pipeline that processes raw terminal output through VTE stripping,
/// CLI-specific parsing, and turn tracking.
pub struct ClassificationPipeline {
    vte_parser: VteParser,
    cli_parser: Box<dyn OutputParser>,
    turn_counter: u32,
    in_response: bool,
}

impl ClassificationPipeline {
    /// Create a new pipeline wrapping the given CLI-specific parser.
    pub fn new(cli_parser: Box<dyn OutputParser>) -> Self {
        Self {
            vte_parser: VteParser::new(),
            cli_parser,
            turn_counter: 0,
            in_response: false,
        }
    }

    /// Process raw terminal output through VTE strip, CLI parser, and turn tracking.
    ///
    /// Returns classified messages with turn metadata populated.
    pub fn process(&mut self, raw: &str) -> Vec<ParsedMessage> {
        // 1. VTE strip the raw input
        let cleaned = self.vte_parser.parse(raw);

        // 2. Feed to CLI parser
        self.cli_parser.feed(&cleaned);

        // 3. Get classified messages
        let mut messages = self.cli_parser.parse();

        // 4. Track turn transitions
        for msg in &mut messages {
            match msg.class {
                MessageClass::AiResponse => {
                    self.in_response = true;
                    msg.metadata.turn = Some(self.turn_counter);
                }
                MessageClass::PromptReady => {
                    if self.in_response {
                        self.turn_counter += 1;
                        self.in_response = false;
                    }
                    msg.metadata.turn = Some(self.turn_counter);
                }
                _ => {
                    msg.metadata.turn = Some(self.turn_counter);
                }
            }
        }

        messages
    }

    /// Returns the current turn count.
    pub fn turn_count(&self) -> u32 {
        self.turn_counter
    }

    /// Returns whether the pipeline is currently inside an AI response.
    pub fn in_response(&self) -> bool {
        self.in_response
    }

    /// Returns the CLI tool this pipeline is configured for.
    pub fn tool(&self) -> CliTool {
        self.cli_parser.tool()
    }

    /// Delegate `extract_ai_text` to the underlying parser.
    pub fn extract_ai_text(&self, raw_cleaned: &str) -> String {
        self.cli_parser.extract_ai_text(raw_cleaned)
    }

    /// Delegate `classify` to the underlying parser.
    pub fn classify(&self, text: &str) -> MessageClass {
        self.cli_parser.classify(text)
    }

    /// Clear the underlying parser's buffer.
    pub fn clear(&mut self) {
        self.cli_parser.clear();
    }
}

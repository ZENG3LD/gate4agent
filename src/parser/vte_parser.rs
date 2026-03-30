//! VTE-based ANSI sequence parser.

use vte::{Parser, Perform};

/// VTE parser for stripping ANSI escape sequences.
pub struct VteParser {
    parser: Parser,
}

impl VteParser {
    pub fn new() -> Self {
        Self {
            parser: Parser::new(),
        }
    }

    /// Parse input and return cleaned text.
    pub fn parse(&mut self, input: &str) -> String {
        let mut performer = TextCollector::new();

        for byte in input.bytes() {
            self.parser.advance(&mut performer, byte);
        }

        performer.text
    }
}

impl Default for VteParser {
    fn default() -> Self {
        Self::new()
    }
}

/// Collects printable text, ignoring escape sequences.
struct TextCollector {
    text: String,
}

impl TextCollector {
    fn new() -> Self {
        Self {
            text: String::new(),
        }
    }
}

impl Perform for TextCollector {
    fn print(&mut self, c: char) {
        self.text.push(c);
    }

    fn execute(&mut self, byte: u8) {
        // Handle control characters
        match byte {
            0x0A => self.text.push('\n'), // LF
            0x0D => {}                    // CR - ignore
            0x09 => self.text.push('\t'), // Tab
            _ => {}
        }
    }

    fn hook(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn put(&mut self, _byte: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    fn csi_dispatch(&mut self, _params: &vte::Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

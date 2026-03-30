//! Virtual terminal screen parser using vt100.
//!
//! Unlike VteParser which only strips ANSI codes, this maintains a full
//! virtual screen grid with cursor tracking, so text placed at specific
//! screen coordinates retains proper spacing.

/// Virtual terminal screen that processes raw PTY output.
pub struct ScreenParser {
    parser: vt100::Parser,
}

impl ScreenParser {
    /// Create a new screen parser with the given dimensions.
    /// Dimensions should match what the PTY child process believes the terminal size to be.
    pub fn new(rows: u16, cols: u16) -> Self {
        Self {
            parser: vt100::Parser::new(rows, cols, 0),
        }
    }

    /// Create with default dimensions (24 rows, 80 cols).
    /// Matches the PTY size used by PtyWrapper (see wrapper.rs).
    pub fn default_size() -> Self {
        Self::new(24, 80)
    }

    /// Feed raw PTY bytes into the virtual terminal.
    pub fn process(&mut self, data: &[u8]) {
        self.parser.process(data);
    }

    /// Get the entire screen contents as properly-spaced text.
    /// Empty trailing rows are stripped. Each row has trailing whitespace trimmed.
    pub fn contents(&self) -> String {
        self.parser.screen().contents()
    }

    /// Get a specific row's content (0-indexed).
    pub fn row_text(&self, row: u16) -> String {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        if row >= rows {
            return String::new();
        }
        screen.contents_between(row, 0, row, cols - 1)
    }

    /// Get the current cursor position (row, col).
    pub fn cursor_position(&self) -> (u16, u16) {
        self.parser.screen().cursor_position()
    }

    /// Get screen dimensions (rows, cols).
    pub fn size(&self) -> (u16, u16) {
        self.parser.screen().size()
    }
}

impl Default for ScreenParser {
    fn default() -> Self {
        Self::default_size()
    }
}

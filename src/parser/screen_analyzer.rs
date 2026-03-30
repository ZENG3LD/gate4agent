//! Semantic analysis of vt100::Screen for Claude Code output.
//!
//! This analyzer is Claude Code TUI-specific. The row markers (U+276F, U+25CF),
//! spinner characters, and color-based classification are tailored to Claude Code's
//! Ink-based TUI output.
//!
//! Scans each row of the virtual screen to classify it by content type
//! (logo, divider, user input, assistant response, tool call, etc.).
//! This allows re-rendering with alternative UI styling.

/// What kind of content a screen row represents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowKind {
    /// Empty / blank row.
    Blank,
    /// Logo block characters (startup splash).
    Logo,
    /// Horizontal divider (repeated box-drawing characters).
    Divider,
    /// User input prompt (with leading U+276F).
    UserPrompt { text: String },
    /// Spinner / thinking animation.
    Spinner {
        asterisk: char,
        word: String,
        detail: String,
    },
    /// Assistant response text (with leading U+25CF).
    AssistantText { text: String },
    /// Tool call header (U+25CF + bold tool name + args).
    ToolCall { name: String, args: String },
    /// Tool output continuation (U+23BF prefix or indented under tool).
    ToolOutput { text: String },
    /// Tip / hint text.
    Tip { text: String },
    /// Status hint (e.g., "esc to interrupt", "? for shortcuts").
    StatusHint { text: String },
    /// Permission prompt ("Do you want to proceed?").
    PermissionPrompt { text: String },
    /// Permission option (numbered choice).
    PermissionOption {
        number: u8,
        text: String,
        selected: bool,
    },
    /// Slash command menu item.
    SlashCommand {
        command: String,
        description: String,
    },
    /// Warning banner (usage limit, etc.).
    Warning { text: String },
    /// Skill notification.
    SkillNotification { name: String, detail: String },
    /// Token/timing stats.
    Stats { text: String },
    /// Unclassified content row.
    Other { text: String },
}

/// Result of analyzing the full screen.
#[derive(Debug, Clone)]
pub struct ScreenAnalysis {
    /// Classification of each row.
    pub rows: Vec<RowKind>,
    /// Detected UI state.
    pub state: UiState,
    /// Window title (placeholder for OSC tracking).
    pub title: String,
}

/// High-level UI state of Claude Code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiState {
    /// Startup splash screen.
    Splash,
    /// Idle, waiting for user input.
    Idle,
    /// User is typing.
    UserTyping,
    /// Thinking / processing.
    Thinking,
    /// Showing response.
    Responding,
    /// Permission prompt active.
    PermissionPrompt,
    /// Slash command menu open.
    SlashMenu,
}

/// Analyze a `vt100::Screen` and classify each row.
pub fn analyze_screen(screen: &vt100::Screen) -> ScreenAnalysis {
    let (rows, cols) = screen.size();
    let mut row_kinds = Vec::with_capacity(rows as usize);

    let mut has_logo = false;
    let mut has_spinner = false;
    let mut has_permission = false;
    let mut has_slash_menu = false;
    let mut has_prompt = false;
    let mut has_response = false;

    for row in 0..rows {
        let kind = classify_row(screen, row, cols);
        match &kind {
            RowKind::Logo => has_logo = true,
            RowKind::Spinner { .. } => has_spinner = true,
            RowKind::PermissionPrompt { .. } | RowKind::PermissionOption { .. } => {
                has_permission = true;
            }
            RowKind::SlashCommand { .. } => has_slash_menu = true,
            RowKind::UserPrompt { .. } => has_prompt = true,
            RowKind::AssistantText { .. }
            | RowKind::ToolCall { .. }
            | RowKind::ToolOutput { .. } => has_response = true,
            _ => {}
        }
        row_kinds.push(kind);
    }

    let state = if has_slash_menu {
        UiState::SlashMenu
    } else if has_permission {
        UiState::PermissionPrompt
    } else if has_spinner {
        UiState::Thinking
    } else if has_response {
        UiState::Responding
    } else if has_logo && !has_prompt {
        UiState::Splash
    } else {
        UiState::Idle
    };

    ScreenAnalysis {
        rows: row_kinds,
        state,
        title: String::new(),
    }
}

/// Per-row metadata extracted during cell scanning.
struct RowScan {
    text: String,
    text_trimmed: String,
    first_nonspace_char: Option<char>,
    first_nonspace_col: u16,
    first_nonspace_fg: vt100::Color,
    first_nonspace_bold: bool,
    divider_count: u16,
    block_char_count: u16,
    has_reverse: bool,
    total_nonspace: u16,
}

/// Scan all cells in a row and collect metadata.
fn scan_row(screen: &vt100::Screen, row: u16, cols: u16) -> RowScan {
    let mut text = String::with_capacity(cols as usize);
    let mut first_nonspace_char: Option<char> = None;
    let mut first_nonspace_col: u16 = 0;
    let mut first_nonspace_fg = vt100::Color::Default;
    let mut first_nonspace_bold = false;
    let mut divider_count: u16 = 0;
    let mut block_char_count: u16 = 0;
    let mut has_reverse = false;
    let mut total_nonspace: u16 = 0;

    for col in 0..cols {
        if let Some(cell) = screen.cell(row, col) {
            let contents = cell.contents();
            let ch = contents.chars().next().unwrap_or(' ');
            text.push(if contents.is_empty() { ' ' } else { ch });

            if ch != ' ' && !contents.is_empty() {
                total_nonspace += 1;
                if first_nonspace_char.is_none() {
                    first_nonspace_char = Some(ch);
                    first_nonspace_col = col;
                    first_nonspace_fg = cell.fgcolor();
                    first_nonspace_bold = cell.bold();
                }
                if ch == '\u{2500}' || ch == '\u{2501}' || ch == '\u{2550}' {
                    divider_count += 1;
                }
                if is_block_char(ch) {
                    block_char_count += 1;
                }
                if cell.inverse() {
                    has_reverse = true;
                }
            }
        }
    }

    let text_trimmed = text.trim().to_string();

    RowScan {
        text,
        text_trimmed,
        first_nonspace_char,
        first_nonspace_col,
        first_nonspace_fg,
        first_nonspace_bold,
        divider_count,
        block_char_count,
        has_reverse,
        total_nonspace,
    }
}

/// Classify a single row by examining its cells.
fn classify_row(screen: &vt100::Screen, row: u16, cols: u16) -> RowKind {
    let scan = scan_row(screen, row, cols);

    // Blank row
    if scan.text_trimmed.is_empty() || scan.total_nonspace == 0 {
        return RowKind::Blank;
    }

    // Divider: row is mostly box-drawing horizontal characters
    if scan.divider_count > cols / 3 {
        return RowKind::Divider;
    }

    // Logo: contains block characters in orange color
    if scan.block_char_count >= 2 && is_orange_fg(scan.first_nonspace_fg) {
        return RowKind::Logo;
    }

    let first_ch = match scan.first_nonspace_char {
        Some(ch) => ch,
        None => return RowKind::Blank,
    };

    // User prompt: U+276F prefix
    if first_ch == '\u{276F}' {
        let prompt_text = extract_after_marker(&scan.text, '\u{276F}');
        return RowKind::UserPrompt { text: prompt_text };
    }

    // Assistant/Tool: U+25CF prefix
    if first_ch == '\u{25CF}' {
        let after = extract_after_marker(&scan.text, '\u{25CF}');

        // Check if this is a skill notification
        if scan.text_trimmed.contains("Skill") {
            if let Some(sk) = try_parse_skill(&scan.text_trimmed) {
                return sk;
            }
        }

        // Check if it is a tool call (bold text after the bullet)
        let name_start_col = scan.first_nonspace_col.saturating_add(2);
        if name_start_col < cols {
            if let Some(name_cell) = screen.cell(row, name_start_col) {
                if name_cell.bold() {
                    let (name, args) = parse_tool_call(&after);
                    if !name.is_empty() {
                        return RowKind::ToolCall { name, args };
                    }
                }
            }
        }

        return RowKind::AssistantText { text: after };
    }

    // Tool output: U+23BF prefix
    if first_ch == '\u{23BF}' {
        let output_text = extract_after_marker(&scan.text, '\u{23BF}');
        if output_text.trim_start().starts_with("Tip:") {
            return RowKind::Tip { text: output_text };
        }
        return RowKind::ToolOutput { text: output_text };
    }

    // Spinner: asterisk-like char in orange
    if is_spinner_char(first_ch) && is_orange_fg(scan.first_nonspace_fg) {
        if let Some(spinner) = try_parse_spinner(&scan.text, first_ch) {
            return spinner;
        }
    }

    // Permission prompt
    if scan.text_trimmed.contains("Do you want to proceed?") {
        return RowKind::PermissionPrompt {
            text: scan.text_trimmed,
        };
    }

    // Permission option
    if let Some(opt) =
        parse_permission_option(&scan.text_trimmed, scan.has_reverse, scan.first_nonspace_fg)
    {
        return opt;
    }

    // Slash command menu
    if first_ch == '/' && scan.first_nonspace_col <= 4 {
        let parts: Vec<&str> = scan.text_trimmed.splitn(2, char::is_whitespace).collect();
        let command = parts.first().unwrap_or(&"").to_string();
        let description = if parts.len() > 1 {
            parts[1].trim().to_string()
        } else {
            String::new()
        };
        if !description.is_empty() {
            return RowKind::SlashCommand {
                command,
                description,
            };
        }
    }

    // Warning: amber/yellow color
    if is_amber_fg(scan.first_nonspace_fg) {
        return RowKind::Warning {
            text: scan.text_trimmed,
        };
    }

    // Status hints
    if is_gray_fg(scan.first_nonspace_fg) && is_status_hint_text(&scan.text_trimmed) {
        return RowKind::StatusHint {
            text: scan.text_trimmed,
        };
    }

    // Tip
    if scan.text_trimmed.starts_with("Tip:")
        || (scan.text_trimmed.contains("Tip:") && is_gray_fg(scan.first_nonspace_fg))
    {
        return RowKind::Tip {
            text: scan.text_trimmed,
        };
    }

    // Stats (tokens, timing)
    if scan.text_trimmed.contains("tokens") && scan.text_trimmed.contains('\u{00B7}') {
        return RowKind::Stats {
            text: scan.text_trimmed,
        };
    }

    // Gray text = secondary UI element
    if is_gray_fg(scan.first_nonspace_fg) && !scan.first_nonspace_bold {
        if is_splash_info_text(&scan.text_trimmed) {
            return RowKind::Logo;
        }
        return RowKind::StatusHint {
            text: scan.text_trimmed,
        };
    }

    // Indented text after a tool call could be continuation
    if scan.first_nonspace_col >= 3 {
        return RowKind::ToolOutput {
            text: scan.text_trimmed,
        };
    }

    RowKind::Other {
        text: scan.text_trimmed,
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

fn is_block_char(ch: char) -> bool {
    matches!(
        ch,
        '\u{2588}'
            | '\u{2590}'
            | '\u{259B}'
            | '\u{259C}'
            | '\u{259D}'
            | '\u{2598}'
            | '\u{258C}'
            | '\u{2580}'
            | '\u{2584}'
            | '\u{2596}'
            | '\u{2597}'
            | '\u{2599}'
            | '\u{259E}'
            | '\u{259F}'
    )
}

fn is_spinner_char(ch: char) -> bool {
    matches!(ch, '*' | '\u{2722}' | '\u{00B7}' | '\u{2736}' | '\u{273B}' | '\u{273D}' | '\u{2733}')
}

fn is_orange_fg(color: vt100::Color) -> bool {
    match color {
        vt100::Color::Rgb(r, g, b) => {
            (210..=240).contains(&r) && (110..=165).contains(&g) && (80..=130).contains(&b)
        }
        _ => false,
    }
}

fn is_gray_fg(color: vt100::Color) -> bool {
    match color {
        vt100::Color::Rgb(r, g, b) => {
            let diff =
                (r as i16 - g as i16).unsigned_abs() + (g as i16 - b as i16).unsigned_abs();
            diff < 10 && r > 100 && r < 200
        }
        _ => false,
    }
}

fn is_amber_fg(color: vt100::Color) -> bool {
    match color {
        vt100::Color::Rgb(r, g, b) => r > 200 && g > 150 && b < 50,
        _ => false,
    }
}

fn is_lavender_fg(color: vt100::Color) -> bool {
    match color {
        vt100::Color::Rgb(r, g, b) => {
            (170..=190).contains(&r) && (180..=195).contains(&g) && (240..=255).contains(&b)
        }
        _ => false,
    }
}

/// Extract text after a marker character, trimming the marker and surrounding whitespace.
fn extract_after_marker(text: &str, marker: char) -> String {
    if let Some(pos) = text.find(marker) {
        let after = &text[pos + marker.len_utf8()..];
        after
            .trim_start_matches(|c: char| c == ' ' || c == '\u{00A0}')
            .trim_end()
            .to_string()
    } else {
        text.trim().to_string()
    }
}

/// Parse "ToolName(args)" from text after the bullet marker.
fn parse_tool_call(text: &str) -> (String, String) {
    let trimmed = text.trim();
    if let Some(paren_pos) = trimmed.find('(') {
        let name = trimmed[..paren_pos].trim().to_string();
        let rest = &trimmed[paren_pos + 1..];
        let args = if let Some(end) = rest.rfind(')') {
            rest[..end].to_string()
        } else {
            rest.to_string()
        };
        (name, args)
    } else {
        let parts: Vec<&str> = trimmed.splitn(2, char::is_whitespace).collect();
        let name = parts.first().unwrap_or(&"").to_string();
        let args = if parts.len() > 1 {
            parts[1].to_string()
        } else {
            String::new()
        };
        (name, args)
    }
}

/// Try to parse a spinner row from text after the asterisk character.
fn try_parse_spinner(text: &str, asterisk: char) -> Option<RowKind> {
    let after = extract_after_marker(text, asterisk);
    let trimmed = after.trim();
    let parts: Vec<&str> = trimmed
        .splitn(2, |c: char| c == '(' || c == '\u{2026}' || c == ' ')
        .collect();
    let word = parts.first().unwrap_or(&"").trim().to_string();
    let detail = if parts.len() > 1 {
        parts[1..].join(" ")
    } else {
        String::new()
    };
    if !word.is_empty() && word.starts_with(|c: char| c.is_uppercase()) {
        Some(RowKind::Spinner {
            asterisk,
            word,
            detail,
        })
    } else {
        None
    }
}

/// Try to parse a skill notification from trimmed text.
fn try_parse_skill(text: &str) -> Option<RowKind> {
    let start = text.find('(')?;
    let end = text.find(')')?;
    if start < end {
        let name = text[start + 1..end].to_string();
        Some(RowKind::SkillNotification {
            name,
            detail: text.to_string(),
        })
    } else {
        None
    }
}

/// Try to parse a permission option line like "1. Yes" or "U+276F 1. Yes".
fn parse_permission_option(
    text: &str,
    has_reverse: bool,
    fg: vt100::Color,
) -> Option<RowKind> {
    let stripped = text
        .trim()
        .trim_start_matches(|c: char| c == '\u{276F}' || c == ' ' || c == '\u{00A0}');

    let mut chars = stripped.chars();
    let digit = chars.next()?;
    if !digit.is_ascii_digit() {
        return None;
    }
    if chars.next() != Some('.') {
        return None;
    }
    let number = digit.to_digit(10)? as u8;
    let option_text = stripped[2..].trim().to_string();
    let selected = has_reverse || is_lavender_fg(fg);
    Some(RowKind::PermissionOption {
        number,
        text: option_text,
        selected,
    })
}

fn is_status_hint_text(text: &str) -> bool {
    text.contains("esc to")
        || text.contains("? for shortcuts")
        || text.contains("ctrl+")
        || text.contains("Tab to")
}

/// Detect splash info text for the startup screen.
///
/// Note: the original code included `"\\CODING\\"` which was a developer machine
/// path accidentally included. That pattern has been removed.
fn is_splash_info_text(text: &str) -> bool {
    text.contains("v2.")
        || text.contains("Opus")
        || text.contains("Claude Max")
        || text.starts_with("~\\")
        || text.starts_with("~/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_screen_is_all_blank() {
        let parser = vt100::Parser::new(24, 80, 0);
        let analysis = analyze_screen(parser.screen());
        assert_eq!(analysis.rows.len(), 24);
        for kind in &analysis.rows {
            assert_eq!(*kind, RowKind::Blank);
        }
        assert_eq!(analysis.state, UiState::Idle);
    }

    #[test]
    fn divider_detection() {
        let mut parser = vt100::Parser::new(3, 40, 0);
        // Write a row of box-drawing characters
        let divider: String = std::iter::repeat('\u{2500}').take(40).collect();
        parser.process(divider.as_bytes());
        let analysis = analyze_screen(parser.screen());
        assert_eq!(analysis.rows[0], RowKind::Divider);
    }

    #[test]
    fn user_prompt_detection() {
        let mut parser = vt100::Parser::new(3, 80, 0);
        let line = "\u{276F} hello world";
        parser.process(line.as_bytes());
        let analysis = analyze_screen(parser.screen());
        match &analysis.rows[0] {
            RowKind::UserPrompt { text } => {
                assert_eq!(text, "hello world");
            }
            other => panic!("expected UserPrompt, got {:?}", other),
        }
    }

    #[test]
    fn tool_output_detection() {
        let mut parser = vt100::Parser::new(3, 80, 0);
        let line = "\u{23BF} some output here";
        parser.process(line.as_bytes());
        let analysis = analyze_screen(parser.screen());
        match &analysis.rows[0] {
            RowKind::ToolOutput { text } => {
                assert_eq!(text, "some output here");
            }
            other => panic!("expected ToolOutput, got {:?}", other),
        }
    }

    #[test]
    fn splash_info_does_not_match_developer_path() {
        // The original code had "\\CODING\\" hardcoded. Verify it's gone.
        assert!(!is_splash_info_text("C:\\CODING\\my_project"));
        assert!(!is_splash_info_text("/home/user/projects"));
        // But real splash text still matches
        assert!(is_splash_info_text("Claude Code v2.1.0"));
        assert!(is_splash_info_text("~/projects"));
    }
}

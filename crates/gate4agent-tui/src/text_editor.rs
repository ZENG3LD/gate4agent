//! Small dependency-free text buffer used by the TUI file surface.
//!
//! Cursor columns are Unicode scalar-value columns, not terminal display cells.
//! This keeps every operation UTF-8 safe without coupling the editor core to a
//! renderer or a Unicode-width dependency.

use std::ops::Range;
use std::path::Path;

pub const MAX_TEXT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TextEditorError {
    ContentTooLarge { bytes: usize, limit: usize },
    InvalidCursor { line: usize, column: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SyncState {
    Clean,
    Dirty,
    Saving,
    Conflict(String),
    Error(String),
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CursorPosition {
    pub line: usize,
    pub column: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxLanguage {
    Plain,
    Rust,
    Markdown,
    Json,
    Toml,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SyntaxClass {
    Plain,
    Keyword,
    String,
    Number,
    Comment,
    Heading,
    Emphasis,
    Key,
    Boolean,
    Punctuation,
}

/// A syntax-colored scalar-column range within one logical line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SyntaxSpan {
    pub start_column: usize,
    pub end_column: usize,
    pub class: SyntaxClass,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RenderLine<'a> {
    pub text: &'a str,
    pub clipped_left: bool,
    pub clipped_right: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    fn as_str(self) -> &'static str {
        match self {
            Self::Lf => "\n",
            Self::CrLf => "\r\n",
            Self::Cr => "\r",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LineSpan {
    start: usize,
    end: usize,
    next_start: usize,
}

#[derive(Clone, Debug)]
pub struct TextEditor {
    buffer: String,
    line_spans: Vec<LineSpan>,
    saved_buffer: String,
    saved_revision: Option<String>,
    cursor: usize,
    selection_anchor: Option<usize>,
    preferred_column: Option<usize>,
    scroll_line: usize,
    scroll_column: usize,
    line_ending: LineEnding,
    sync_state: SyncState,
    #[cfg(test)]
    line_index_rebuilds: usize,
}

impl Default for TextEditor {
    fn default() -> Self {
        Self {
            buffer: String::new(),
            line_spans: vec![LineSpan {
                start: 0,
                end: 0,
                next_start: 0,
            }],
            saved_buffer: String::new(),
            saved_revision: None,
            cursor: 0,
            selection_anchor: None,
            preferred_column: None,
            scroll_line: 0,
            scroll_column: 0,
            line_ending: LineEnding::Lf,
            sync_state: SyncState::Clean,
            #[cfg(test)]
            line_index_rebuilds: 0,
        }
    }
}

impl TextEditor {
    pub fn from_text(content: String) -> Result<Self, TextEditorError> {
        Self::new(content, None)
    }

    pub fn new(content: String, revision: Option<String>) -> Result<Self, TextEditorError> {
        let mut editor = Self::default();
        editor.replace_content(content, revision)?;
        Ok(editor)
    }

    pub fn text(&self) -> &str {
        &self.buffer
    }

    pub fn byte_len(&self) -> usize {
        self.buffer.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }

    pub fn is_dirty(&self) -> bool {
        self.buffer != self.saved_buffer
    }

    pub fn dirty(&self) -> bool {
        self.is_dirty()
    }

    pub fn saved_revision(&self) -> Option<&str> {
        self.saved_revision.as_deref()
    }

    pub fn sync_state(&self) -> &SyncState {
        &self.sync_state
    }

    pub fn cursor_byte_offset(&self) -> usize {
        self.cursor
    }

    pub fn caret_position(&self) -> CursorPosition {
        self.cursor_position()
    }

    pub fn cursor_position(&self) -> CursorPosition {
        position_for_cursor(&self.buffer, &self.line_spans, self.cursor)
    }

    pub fn selection_anchor_byte_offset(&self) -> Option<usize> {
        self.selection_anchor
    }

    pub fn selection_anchor_position(&self) -> Option<CursorPosition> {
        let anchor = self.selection_anchor?;
        Some(position_for_cursor(&self.buffer, &self.line_spans, anchor))
    }

    pub fn selection_range(&self) -> Option<Range<usize>> {
        let anchor = self.selection_anchor?;
        if anchor == self.cursor {
            return None;
        }
        Some(anchor.min(self.cursor)..anchor.max(self.cursor))
    }

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    pub fn selected_text(&self) -> Option<&str> {
        let range = self.selection_range()?;
        Some(&self.buffer[range])
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    pub fn select_all(&mut self) {
        self.selection_anchor = Some(0);
        self.cursor = self.buffer.len();
        self.preferred_column = None;
    }

    pub fn line_count(&self) -> usize {
        self.line_spans.len()
    }

    pub fn line_byte_start(&self, line: usize) -> Option<usize> {
        self.line_spans.get(line).map(|span| span.start)
    }

    pub fn scroll_line(&self) -> usize {
        self.scroll_line
    }

    pub fn scroll_column(&self) -> usize {
        self.scroll_column
    }

    pub fn replace_content(
        &mut self,
        content: String,
        revision: Option<String>,
    ) -> Result<(), TextEditorError> {
        ensure_size(content.len())?;
        self.line_ending = detect_line_ending(&content);
        self.saved_buffer = content.clone();
        self.buffer = content;
        self.rebuild_line_index();
        self.saved_revision = revision;
        self.cursor = 0;
        self.selection_anchor = None;
        self.preferred_column = None;
        self.scroll_line = 0;
        self.scroll_column = 0;
        self.sync_state = SyncState::Clean;
        Ok(())
    }

    pub fn mark_saving(&mut self) {
        self.sync_state = SyncState::Saving;
    }

    pub fn mark_save_success(&mut self, revision: Option<String>) {
        self.saved_buffer.clone_from(&self.buffer);
        self.saved_revision = revision;
        self.sync_state = SyncState::Clean;
    }

    pub fn mark_saved(&mut self) {
        self.saved_buffer.clone_from(&self.buffer);
        self.sync_state = SyncState::Clean;
    }

    pub fn mark_conflict(&mut self, message: impl Into<String>) {
        self.sync_state = SyncState::Conflict(message.into());
    }

    pub fn mark_error(&mut self, message: impl Into<String>) {
        self.sync_state = SyncState::Error(message.into());
    }

    pub fn clear_problem(&mut self) {
        self.sync_state = if self.is_dirty() {
            SyncState::Dirty
        } else {
            SyncState::Clean
        };
    }

    pub fn set_cursor_position(
        &mut self,
        position: CursorPosition,
    ) -> Result<(), TextEditorError> {
        self.set_cursor_position_internal(position, false)
    }

    pub fn start_selection(
        &mut self,
        position: CursorPosition,
    ) -> Result<(), TextEditorError> {
        self.set_cursor_position_internal(position, false)?;
        self.selection_anchor = Some(self.cursor);
        Ok(())
    }

    pub fn update_selection(
        &mut self,
        position: CursorPosition,
    ) -> Result<(), TextEditorError> {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        }
        self.set_cursor_position_internal(position, true)
    }

    pub fn hit_test_position(
        &self,
        viewport_row: usize,
        viewport_column: usize,
    ) -> CursorPosition {
        self.hit_test(viewport_row, viewport_column).0
    }

    fn hit_test(&self, viewport_row: usize, viewport_column: usize) -> (CursorPosition, usize) {
        let line = self
            .scroll_line
            .saturating_add(viewport_row)
            .min(self.line_spans.len().saturating_sub(1));
        let span = self.line_spans[line];
        let content = &self.buffer[span.start..span.end];
        let available = content.chars().count();
        let position = CursorPosition {
            line,
            column: self
                .scroll_column
                .saturating_add(viewport_column)
                .min(available),
        };
        let byte = span.start + byte_index_at_column(content, position.column);
        (position, byte)
    }

    pub fn set_cursor_from_viewport_hit(
        &mut self,
        viewport_row: usize,
        viewport_column: usize,
        extend_selection: bool,
    ) {
        let (_, byte) = self.hit_test(viewport_row, viewport_column);
        self.set_cursor_byte(byte, extend_selection);
    }

    pub fn start_drag_selection(&mut self, viewport_row: usize, viewport_column: usize) {
        let (_, byte) = self.hit_test(viewport_row, viewport_column);
        self.cursor = byte;
        self.selection_anchor = Some(byte);
        self.preferred_column = None;
    }

    pub fn update_drag_selection(&mut self, viewport_row: usize, viewport_column: usize) {
        let (_, byte) = self.hit_test(viewport_row, viewport_column);
        if byte == self.cursor {
            return;
        }
        self.set_cursor_byte(byte, true);
    }

    fn set_cursor_byte(&mut self, byte: usize, extend_selection: bool) {
        if extend_selection && self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        } else if !extend_selection {
            self.selection_anchor = None;
        }
        self.cursor = byte;
        self.preferred_column = None;
    }

    fn set_cursor_position_internal(
        &mut self,
        position: CursorPosition,
        extend_selection: bool,
    ) -> Result<(), TextEditorError> {
        let Some(span) = self.line_spans.get(position.line).copied() else {
            return Err(TextEditorError::InvalidCursor {
                line: position.line,
                column: position.column,
            });
        };
        let Some(byte) = byte_for_column(&self.buffer, span, position.column) else {
            return Err(TextEditorError::InvalidCursor {
                line: position.line,
                column: position.column,
            });
        };
        if extend_selection && self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor);
        } else if !extend_selection {
            self.selection_anchor = None;
        }
        self.cursor = byte;
        self.preferred_column = None;
        Ok(())
    }

    pub fn insert_char(&mut self, value: char) -> Result<(), TextEditorError> {
        let mut encoded = [0_u8; 4];
        self.insert_str(value.encode_utf8(&mut encoded))
    }

    pub fn insert_str(&mut self, value: &str) -> Result<(), TextEditorError> {
        self.replace_selection(value)
    }

    pub fn replace_selection(&mut self, value: &str) -> Result<(), TextEditorError> {
        let range = self.selection_range().unwrap_or(self.cursor..self.cursor);
        let retained = self.buffer.len().saturating_sub(range.len());
        let new_len = retained.checked_add(value.len()).unwrap_or(usize::MAX);
        ensure_size(new_len)?;
        let start = range.start;
        self.buffer.replace_range(range, value);
        self.rebuild_line_index();
        self.cursor = start + value.len();
        self.cursor = canonical_cursor(&self.buffer, self.cursor, true);
        self.selection_anchor = None;
        self.preferred_column = None;
        self.after_edit();
        Ok(())
    }

    pub fn insert_newline(&mut self) -> Result<(), TextEditorError> {
        self.insert_str(self.line_ending.as_str())
    }

    pub fn backspace(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor == 0 {
            return false;
        }
        let position = position_for_cursor(&self.buffer, &self.line_spans, self.cursor);
        let start = if position.column == 0 && position.line > 0 {
            self.line_spans[position.line - 1].end
        } else {
            previous_char_boundary(&self.buffer, self.cursor)
        };
        self.buffer.drain(start..self.cursor);
        self.rebuild_line_index();
        self.cursor = start;
        self.preferred_column = None;
        self.after_edit();
        true
    }

    pub fn delete(&mut self) -> bool {
        if self.delete_selection() {
            return true;
        }
        if self.cursor >= self.buffer.len() {
            return false;
        }
        let position = position_for_cursor(&self.buffer, &self.line_spans, self.cursor);
        let span = self.line_spans[position.line];
        let end = if self.cursor == span.end && span.next_start > span.end {
            span.next_start
        } else {
            next_char_boundary(&self.buffer, self.cursor)
        };
        self.buffer.drain(self.cursor..end);
        self.rebuild_line_index();
        self.preferred_column = None;
        self.after_edit();
        true
    }

    fn delete_selection(&mut self) -> bool {
        let Some(range) = self.selection_range() else {
            self.selection_anchor = None;
            return false;
        };
        let start = range.start;
        self.buffer.drain(range);
        self.rebuild_line_index();
        self.cursor = start;
        self.selection_anchor = None;
        self.preferred_column = None;
        self.after_edit();
        true
    }

    pub fn move_left(&mut self) {
        self.move_left_with_selection(false);
    }

    pub fn move_left_extending(&mut self) {
        self.move_left_with_selection(true);
    }

    pub fn move_left_with_selection(&mut self, extend_selection: bool) {
        if !extend_selection {
            if let Some(range) = self.selection_range() {
                self.cursor = range.start;
                self.selection_anchor = None;
                self.preferred_column = None;
                return;
            }
        }
        self.prepare_selection_extension(extend_selection);
        if self.cursor == 0 {
            return;
        }
        let position = position_for_cursor(&self.buffer, &self.line_spans, self.cursor);
        self.cursor = if position.column == 0 && position.line > 0 {
            self.line_spans[position.line - 1].end
        } else {
            previous_char_boundary(&self.buffer, self.cursor)
        };
        self.preferred_column = None;
    }

    pub fn move_right(&mut self) {
        self.move_right_with_selection(false);
    }

    pub fn move_right_extending(&mut self) {
        self.move_right_with_selection(true);
    }

    pub fn move_right_with_selection(&mut self, extend_selection: bool) {
        if !extend_selection {
            if let Some(range) = self.selection_range() {
                self.cursor = range.end;
                self.selection_anchor = None;
                self.preferred_column = None;
                return;
            }
        }
        self.prepare_selection_extension(extend_selection);
        if self.cursor >= self.buffer.len() {
            return;
        }
        let position = position_for_cursor(&self.buffer, &self.line_spans, self.cursor);
        let span = self.line_spans[position.line];
        self.cursor = if self.cursor == span.end && span.next_start > span.end {
            span.next_start
        } else {
            next_char_boundary(&self.buffer, self.cursor)
        };
        self.preferred_column = None;
    }

    pub fn move_up(&mut self) {
        self.move_vertical(-1, false);
    }

    pub fn move_up_extending(&mut self) {
        self.move_vertical(-1, true);
    }

    pub fn move_down(&mut self) {
        self.move_vertical(1, false);
    }

    pub fn move_down_extending(&mut self) {
        self.move_vertical(1, true);
    }

    pub fn page_up(&mut self, rows: usize) {
        self.move_vertical(-(rows.max(1) as isize), false);
    }

    pub fn page_up_extending(&mut self, rows: usize) {
        self.move_vertical(-(rows.max(1) as isize), true);
    }

    pub fn page_down(&mut self, rows: usize) {
        self.move_vertical(rows.max(1) as isize, false);
    }

    pub fn page_down_extending(&mut self, rows: usize) {
        self.move_vertical(rows.max(1) as isize, true);
    }

    pub fn move_home(&mut self) {
        self.move_home_with_selection(false);
    }

    pub fn move_home_extending(&mut self) {
        self.move_home_with_selection(true);
    }

    pub fn move_home_with_selection(&mut self, extend_selection: bool) {
        self.prepare_selection_extension(extend_selection);
        let position = position_for_cursor(&self.buffer, &self.line_spans, self.cursor);
        self.cursor = self.line_spans[position.line].start;
        self.preferred_column = None;
    }

    pub fn move_end(&mut self) {
        self.move_end_with_selection(false);
    }

    pub fn move_end_extending(&mut self) {
        self.move_end_with_selection(true);
    }

    pub fn move_end_with_selection(&mut self, extend_selection: bool) {
        self.prepare_selection_extension(extend_selection);
        let position = position_for_cursor(&self.buffer, &self.line_spans, self.cursor);
        self.cursor = self.line_spans[position.line].end;
        self.preferred_column = None;
    }

    pub fn set_scroll(&mut self, line: usize, column: usize) {
        self.scroll_line = line.min(self.line_count().saturating_sub(1));
        self.scroll_column = column;
    }

    pub fn scroll_vertical(&mut self, lines: isize) {
        let last = self.line_count().saturating_sub(1);
        self.scroll_line = offset_clamped(self.scroll_line, lines, last);
    }

    pub fn scroll_horizontal(&mut self, columns: isize) {
        self.scroll_column = if columns < 0 {
            self.scroll_column.saturating_sub(columns.unsigned_abs())
        } else {
            self.scroll_column.saturating_add(columns as usize)
        };
    }

    pub fn ensure_cursor_visible(&mut self, rows: usize, columns: usize) {
        let position = self.cursor_position();
        let rows = rows.max(1);
        let columns = columns.max(1);
        if position.line < self.scroll_line {
            self.scroll_line = position.line;
        } else if position.line >= self.scroll_line.saturating_add(rows) {
            self.scroll_line = position.line.saturating_add(1).saturating_sub(rows);
        }
        if position.column < self.scroll_column {
            self.scroll_column = position.column;
        } else if position.column >= self.scroll_column.saturating_add(columns) {
            self.scroll_column = position.column.saturating_add(1).saturating_sub(columns);
        }
    }

    pub fn render_line_slice(
        &self,
        line: usize,
        start_column: usize,
        max_columns: usize,
    ) -> Option<RenderLine<'_>> {
        let span = self.line_spans.get(line).copied()?;
        let content = &self.buffer[span.start..span.end];
        let char_count = content.chars().count();
        let start_column = start_column.min(char_count);
        let end_column = start_column.saturating_add(max_columns).min(char_count);
        let start = byte_index_at_column(content, start_column);
        let end = byte_index_at_column(content, end_column);
        Some(RenderLine {
            text: &content[start..end],
            clipped_left: start_column > 0,
            clipped_right: end_column < char_count,
        })
    }

    pub fn visible_lines(&self, rows: usize, columns: usize) -> Vec<RenderLine<'_>> {
        let count = rows.min(self.line_count().saturating_sub(self.scroll_line));
        (0..count)
            .filter_map(|offset| {
                self.render_line_slice(
                    self.scroll_line + offset,
                    self.scroll_column,
                    columns,
                )
            })
            .collect()
    }

    pub fn syntax_spans_for_line(&self, path: &str, line: usize) -> Option<Vec<SyntaxSpan>> {
        let span = self.line_spans.get(line).copied()?;
        Some(syntax_spans_for_line(
            syntax_language_for_path(path),
            &self.buffer[span.start..span.end],
        ))
    }

    fn move_vertical(&mut self, delta: isize, extend_selection: bool) {
        self.prepare_selection_extension(extend_selection);
        let position = position_for_cursor(&self.buffer, &self.line_spans, self.cursor);
        let target_column = self.preferred_column.unwrap_or(position.column);
        let target_line = offset_clamped(
            position.line,
            delta,
            self.line_spans.len().saturating_sub(1),
        );
        let target_span = self.line_spans[target_line];
        let available = self.buffer[target_span.start..target_span.end].chars().count();
        self.cursor = byte_for_column(&self.buffer, target_span, target_column.min(available))
            .unwrap_or(target_span.end);
        self.preferred_column = Some(target_column);
    }

    fn prepare_selection_extension(&mut self, extend_selection: bool) {
        if extend_selection {
            if self.selection_anchor.is_none() {
                self.selection_anchor = Some(self.cursor);
            }
        } else {
            self.selection_anchor = None;
        }
    }

    fn after_edit(&mut self) {
        if self.buffer == self.saved_buffer {
            self.sync_state = SyncState::Clean;
        } else if !matches!(self.sync_state, SyncState::Conflict(_)) {
            self.sync_state = SyncState::Dirty;
        }
    }

    fn rebuild_line_index(&mut self) {
        self.line_spans = line_spans(&self.buffer);
        #[cfg(test)]
        {
            self.line_index_rebuilds = self.line_index_rebuilds.saturating_add(1);
        }
    }
}

pub fn syntax_language_for_path(path: &str) -> SyntaxLanguage {
    let path = Path::new(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "rs" => SyntaxLanguage::Rust,
        "md" | "markdown" => SyntaxLanguage::Markdown,
        "json" | "jsonl" => SyntaxLanguage::Json,
        "toml" => SyntaxLanguage::Toml,
        _ if file_name == "cargo.lock" => SyntaxLanguage::Toml,
        _ => SyntaxLanguage::Plain,
    }
}

pub fn syntax_spans_for_line(language: SyntaxLanguage, line: &str) -> Vec<SyntaxSpan> {
    match language {
        SyntaxLanguage::Plain => Vec::new(),
        SyntaxLanguage::Rust => rust_syntax_spans(line),
        SyntaxLanguage::Markdown => markdown_syntax_spans(line),
        SyntaxLanguage::Json => json_syntax_spans(line),
        SyntaxLanguage::Toml => toml_syntax_spans(line),
    }
}

fn rust_syntax_spans(line: &str) -> Vec<SyntaxSpan> {
    const KEYWORDS: &[&str] = &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else",
        "enum", "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop",
        "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static",
        "struct", "super", "trait", "true", "type", "unsafe", "use", "where", "while",
    ];
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index..].starts_with(b"//") {
            push_syntax_span(&mut spans, line, index, bytes.len(), SyntaxClass::Comment);
            break;
        }
        if bytes[index..].starts_with(b"/*") {
            let end = line[index + 2..]
                .find("*/")
                .map(|offset| index + 2 + offset + 2)
                .unwrap_or(bytes.len());
            push_syntax_span(&mut spans, line, index, end, SyntaxClass::Comment);
            index = end;
            continue;
        }
        if bytes[index] == b'"' || bytes[index] == b'\'' {
            let end = quoted_end(line, index, bytes[index]);
            push_syntax_span(&mut spans, line, index, end, SyntaxClass::String);
            index = end;
            continue;
        }
        if bytes[index].is_ascii_digit() {
            let end = ascii_token_end(bytes, index, |byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.')
            });
            push_syntax_span(&mut spans, line, index, end, SyntaxClass::Number);
            index = end;
            continue;
        }
        if is_identifier_start(bytes[index]) {
            let end = ascii_token_end(bytes, index, is_identifier_continue);
            let word = &line[index..end];
            if KEYWORDS.contains(&word) {
                let class = if matches!(word, "true" | "false") {
                    SyntaxClass::Boolean
                } else {
                    SyntaxClass::Keyword
                };
                push_syntax_span(&mut spans, line, index, end, class);
            }
            index = end;
            continue;
        }
        if is_ascii_punctuation(bytes[index]) {
            push_syntax_span(
                &mut spans,
                line,
                index,
                index + 1,
                SyntaxClass::Punctuation,
            );
            index += 1;
            continue;
        }
        index += next_char_len(line, index);
    }
    spans
}

fn markdown_syntax_spans(line: &str) -> Vec<SyntaxSpan> {
    let mut spans = Vec::new();
    let trimmed = line.trim_start();
    let leading = line.len() - trimmed.len();
    let heading_marks = trimmed.bytes().take_while(|byte| *byte == b'#').count();
    if (1..=6).contains(&heading_marks)
        && trimmed.as_bytes().get(heading_marks).is_some_and(u8::is_ascii_whitespace)
    {
        push_syntax_span(
            &mut spans,
            line,
            leading,
            line.len(),
            SyntaxClass::Heading,
        );
        return spans;
    }
    let bytes = line.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let (marker, class) = if bytes[index] == b'`' {
            (b'`', SyntaxClass::String)
        } else if matches!(bytes[index], b'*' | b'_') {
            (bytes[index], SyntaxClass::Emphasis)
        } else {
            index += next_char_len(line, index);
            continue;
        };
        if let Some(offset) = bytes[index + 1..].iter().position(|byte| *byte == marker) {
            let end = index + 1 + offset + 1;
            push_syntax_span(&mut spans, line, index, end, class);
            index = end;
        } else {
            index += 1;
        }
    }
    spans
}

fn json_syntax_spans(line: &str) -> Vec<SyntaxSpan> {
    const WORDS: &[(&str, SyntaxClass)] = &[
        ("true", SyntaxClass::Boolean),
        ("false", SyntaxClass::Boolean),
        ("null", SyntaxClass::Keyword),
    ];
    let bytes = line.as_bytes();
    let mut spans = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'"' {
            let end = quoted_end(line, index, b'"');
            let class = if line[end..].trim_start().starts_with(':') {
                SyntaxClass::Key
            } else {
                SyntaxClass::String
            };
            push_syntax_span(&mut spans, line, index, end, class);
            index = end;
            continue;
        }
        if bytes[index].is_ascii_digit() || bytes[index] == b'-' {
            let end = ascii_token_end(bytes, index, |byte| {
                byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E')
            });
            push_syntax_span(&mut spans, line, index, end, SyntaxClass::Number);
            index = end;
            continue;
        }
        if is_identifier_start(bytes[index]) {
            let end = ascii_token_end(bytes, index, is_identifier_continue);
            if let Some((_, class)) = WORDS.iter().find(|(word, _)| *word == &line[index..end]) {
                push_syntax_span(&mut spans, line, index, end, *class);
            }
            index = end;
            continue;
        }
        if matches!(bytes[index], b'{' | b'}' | b'[' | b']' | b':' | b',') {
            push_syntax_span(
                &mut spans,
                line,
                index,
                index + 1,
                SyntaxClass::Punctuation,
            );
            index += 1;
            continue;
        }
        index += next_char_len(line, index);
    }
    spans
}

fn toml_syntax_spans(line: &str) -> Vec<SyntaxSpan> {
    let trimmed = line.trim_start();
    let leading = line.len() - trimmed.len();
    if trimmed.starts_with('[') {
        let end = trimmed.find(']').map(|offset| leading + offset + 1).unwrap_or(line.len());
        let mut spans = Vec::new();
        push_syntax_span(&mut spans, line, leading, end, SyntaxClass::Heading);
        return spans;
    }
    let mut spans = Vec::new();
    let bytes = line.as_bytes();
    let mut index = 0;
    let mut saw_equals = false;
    while index < bytes.len() {
        if bytes[index] == b'#' {
            push_syntax_span(&mut spans, line, index, bytes.len(), SyntaxClass::Comment);
            break;
        }
        if bytes[index] == b'"' || bytes[index] == b'\'' {
            let end = quoted_end(line, index, bytes[index]);
            push_syntax_span(&mut spans, line, index, end, SyntaxClass::String);
            index = end;
            continue;
        }
        if !saw_equals && bytes[index] == b'=' {
            let key_start = line[..index].find(|value: char| !value.is_whitespace()).unwrap_or(index);
            let key_end = line[..index].trim_end().len();
            if key_start < key_end {
                push_syntax_span(&mut spans, line, key_start, key_end, SyntaxClass::Key);
            }
            push_syntax_span(
                &mut spans,
                line,
                index,
                index + 1,
                SyntaxClass::Punctuation,
            );
            saw_equals = true;
            index += 1;
            continue;
        }
        if saw_equals && (bytes[index].is_ascii_digit() || bytes[index] == b'-') {
            let end = ascii_token_end(bytes, index, |byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b'-' | b'+' | b'.' | b'_' | b':' | b'T' | b'Z')
            });
            push_syntax_span(&mut spans, line, index, end, SyntaxClass::Number);
            index = end;
            continue;
        }
        if saw_equals && is_identifier_start(bytes[index]) {
            let end = ascii_token_end(bytes, index, is_identifier_continue);
            if matches!(&line[index..end], "true" | "false") {
                push_syntax_span(&mut spans, line, index, end, SyntaxClass::Boolean);
            }
            index = end;
            continue;
        }
        index += next_char_len(line, index);
    }
    spans
}

fn push_syntax_span(
    spans: &mut Vec<SyntaxSpan>,
    line: &str,
    start: usize,
    end: usize,
    class: SyntaxClass,
) {
    if start >= end || end > line.len() {
        return;
    }
    spans.push(SyntaxSpan {
        start_column: line[..start].chars().count(),
        end_column: line[..end].chars().count(),
        class,
    });
}

fn quoted_end(line: &str, start: usize, quote: u8) -> usize {
    let bytes = line.as_bytes();
    let mut index = start + 1;
    let mut escaped = false;
    while index < bytes.len() {
        if !escaped && bytes[index] == quote {
            return index + 1;
        }
        if quote == b'"' && !escaped && bytes[index] == b'\\' {
            escaped = true;
        } else {
            escaped = false;
        }
        index += next_char_len(line, index);
    }
    bytes.len()
}

fn ascii_token_end(bytes: &[u8], start: usize, accepts: impl Fn(u8) -> bool) -> usize {
    let mut end = start;
    while end < bytes.len() && accepts(bytes[end]) {
        end += 1;
    }
    end
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_identifier_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_ascii_punctuation(byte: u8) -> bool {
    matches!(
        byte,
        b'{' | b'}' | b'[' | b']' | b'(' | b')' | b':' | b';' | b',' | b'.' | b'=' | b'!'
            | b'<' | b'>' | b'&' | b'|' | b'+' | b'-' | b'*' | b'/' | b'%' | b'#'
    )
}

fn next_char_len(content: &str, byte: usize) -> usize {
    content[byte..].chars().next().map(char::len_utf8).unwrap_or(1)
}

fn ensure_size(bytes: usize) -> Result<(), TextEditorError> {
    if bytes > MAX_TEXT_BYTES {
        Err(TextEditorError::ContentTooLarge {
            bytes,
            limit: MAX_TEXT_BYTES,
        })
    } else {
        Ok(())
    }
}

fn detect_line_ending(content: &str) -> LineEnding {
    let bytes = content.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => return LineEnding::CrLf,
            b'\r' => return LineEnding::Cr,
            b'\n' => return LineEnding::Lf,
            _ => index += 1,
        }
    }
    LineEnding::Lf
}

fn line_spans(content: &str) -> Vec<LineSpan> {
    let bytes = content.as_bytes();
    let mut spans = Vec::new();
    let mut start = 0;
    let mut index = 0;
    while index < bytes.len() {
        let separator_len = match bytes[index] {
            b'\r' if bytes.get(index + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => {
                index += 1;
                continue;
            }
        };
        spans.push(LineSpan {
            start,
            end: index,
            next_start: index + separator_len,
        });
        index += separator_len;
        start = index;
    }
    spans.push(LineSpan {
        start,
        end: content.len(),
        next_start: content.len(),
    });
    spans
}

fn position_for_cursor(content: &str, spans: &[LineSpan], cursor: usize) -> CursorPosition {
    let line = spans
        .partition_point(|span| cursor > span.end)
        .min(spans.len().saturating_sub(1));
    let span = spans[line];
    let column_end = cursor.min(span.end);
    CursorPosition {
        line,
        column: content[span.start..column_end].chars().count(),
    }
}

fn byte_for_column(content: &str, span: LineSpan, column: usize) -> Option<usize> {
    let line = &content[span.start..span.end];
    let count = line.chars().count();
    if column > count {
        return None;
    }
    Some(span.start + byte_index_at_column(line, column))
}

fn byte_index_at_column(content: &str, column: usize) -> usize {
    if column == 0 {
        return 0;
    }
    content
        .char_indices()
        .nth(column)
        .map(|(index, _)| index)
        .unwrap_or(content.len())
}

fn previous_char_boundary(content: &str, cursor: usize) -> usize {
    content[..cursor]
        .char_indices()
        .next_back()
        .map(|(index, _)| index)
        .unwrap_or(0)
}

fn next_char_boundary(content: &str, cursor: usize) -> usize {
    content[cursor..]
        .chars()
        .next()
        .map(|value| cursor + value.len_utf8())
        .unwrap_or(content.len())
}

fn canonical_cursor(content: &str, cursor: usize, forward: bool) -> usize {
    let cursor = cursor.min(content.len());
    if cursor > 0
        && cursor < content.len()
        && content.as_bytes()[cursor - 1] == b'\r'
        && content.as_bytes()[cursor] == b'\n'
    {
        if forward { cursor + 1 } else { cursor - 1 }
    } else {
        cursor
    }
}

fn offset_clamped(value: usize, delta: isize, maximum: usize) -> usize {
    if delta < 0 {
        value.saturating_sub(delta.unsigned_abs())
    } else {
        value.saturating_add(delta as usize).min(maximum)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_cursor_and_render_slices_stay_on_char_boundaries() {
        let mut editor = TextEditor::new("aЖ🙂z\nβeta".to_string(), Some("r1".to_string()))
            .expect("editor");
        editor
            .set_cursor_position(CursorPosition { line: 0, column: 3 })
            .expect("unicode cursor");
        assert_eq!(editor.cursor_position(), CursorPosition { line: 0, column: 3 });
        editor.backspace();
        assert_eq!(editor.text(), "aЖz\nβeta");
        assert_eq!(
            editor.render_line_slice(0, 1, 1),
            Some(RenderLine {
                text: "Ж",
                clipped_left: true,
                clipped_right: true,
            })
        );
    }

    #[test]
    fn crlf_is_one_navigation_and_deletion_boundary() {
        let mut editor = TextEditor::new("one\r\ntwo\r\n".to_string(), None).expect("editor");
        editor
            .set_cursor_position(CursorPosition { line: 1, column: 0 })
            .expect("second line");
        editor.move_left();
        assert_eq!(editor.cursor_position(), CursorPosition { line: 0, column: 3 });
        editor.move_right();
        assert_eq!(editor.cursor_position(), CursorPosition { line: 1, column: 0 });
        assert!(editor.backspace());
        assert_eq!(editor.text(), "onetwo\r\n");
        editor.move_end();
        editor.insert_newline().expect("newline");
        assert_eq!(editor.text(), "onetwo\r\n\r\n");
    }

    #[test]
    fn vertical_motion_preserves_requested_column() {
        let mut editor = TextEditor::new("abcdef\nx\nuvwxyz".to_string(), None).expect("editor");
        editor
            .set_cursor_position(CursorPosition { line: 0, column: 5 })
            .expect("cursor");
        editor.move_down();
        assert_eq!(editor.cursor_position(), CursorPosition { line: 1, column: 1 });
        editor.move_down();
        assert_eq!(editor.cursor_position(), CursorPosition { line: 2, column: 5 });
        editor.page_up(2);
        assert_eq!(editor.cursor_position(), CursorPosition { line: 0, column: 5 });
    }

    #[test]
    fn failed_oversize_mutation_is_atomic() {
        let mut editor = TextEditor::new("ok".to_string(), Some("old".to_string()))
            .expect("editor");
        let oversized = "x".repeat(MAX_TEXT_BYTES);
        assert_eq!(
            editor.insert_str(&oversized),
            Err(TextEditorError::ContentTooLarge {
                bytes: MAX_TEXT_BYTES + 2,
                limit: MAX_TEXT_BYTES,
            })
        );
        assert_eq!(editor.text(), "ok");
        assert_eq!(editor.sync_state(), &SyncState::Clean);
    }

    #[test]
    fn save_conflict_and_error_states_are_explicit() {
        let mut editor = TextEditor::new("base".to_string(), Some("r1".to_string()))
            .expect("editor");
        editor.insert_char('!').expect("insert");
        assert!(editor.is_dirty());
        assert_eq!(editor.sync_state(), &SyncState::Dirty);
        editor.mark_saving();
        assert_eq!(editor.sync_state(), &SyncState::Saving);
        editor.mark_conflict("changed on disk");
        assert_eq!(
            editor.sync_state(),
            &SyncState::Conflict("changed on disk".to_string())
        );
        editor.mark_save_success(Some("r2".to_string()));
        assert!(!editor.is_dirty());
        assert_eq!(editor.saved_revision(), Some("r2"));
        editor.mark_error("write failed");
        editor.clear_problem();
        assert_eq!(editor.sync_state(), &SyncState::Clean);
    }

    #[test]
    fn cursor_visibility_updates_both_scroll_axes() {
        let mut editor = TextEditor::new(
            "zero\none\ntwo\n0123456789".to_string(),
            None,
        )
        .expect("editor");
        editor
            .set_cursor_position(CursorPosition { line: 3, column: 9 })
            .expect("cursor");
        editor.ensure_cursor_visible(2, 4);
        assert_eq!(editor.scroll_line(), 2);
        assert_eq!(editor.scroll_column(), 6);
    }

    #[test]
    fn selection_replaces_unicode_and_deletes_crlf_atomically() {
        let mut editor = TextEditor::new("aЖ🙂\r\nnext".to_string(), None).expect("editor");
        editor
            .start_selection(CursorPosition { line: 0, column: 1 })
            .expect("anchor");
        editor
            .update_selection(CursorPosition { line: 1, column: 0 })
            .expect("caret");
        assert_eq!(editor.selected_text(), Some("Ж🙂\r\n"));
        editor.replace_selection("β").expect("replace selection");
        assert_eq!(editor.text(), "aβnext");
        assert_eq!(editor.cursor_position(), CursorPosition { line: 0, column: 2 });
        assert!(!editor.has_selection());

        editor.select_all();
        assert!(editor.backspace());
        assert!(editor.is_empty());
    }

    #[test]
    fn extending_movement_preserves_anchor_and_plain_movement_collapses() {
        let mut editor = TextEditor::new("abcdef".to_string(), None).expect("editor");
        editor
            .set_cursor_position(CursorPosition { line: 0, column: 2 })
            .expect("cursor");
        editor.move_right_extending();
        editor.move_right_extending();
        assert_eq!(editor.selected_text(), Some("cd"));
        assert_eq!(editor.selection_anchor_position(), Some(CursorPosition { line: 0, column: 2 }));
        editor.move_left();
        assert_eq!(editor.cursor_position(), CursorPosition { line: 0, column: 2 });
        assert!(!editor.has_selection());
    }

    #[test]
    fn viewport_drag_hit_testing_is_unicode_safe_and_scroll_aware() {
        let mut editor = TextEditor::new("zero\nЖ🙂tail\nlast".to_string(), None).expect("editor");
        editor.set_scroll(1, 1);
        editor.start_drag_selection(0, 0);
        editor.update_drag_selection(0, 2);
        assert_eq!(editor.selection_anchor_position(), Some(CursorPosition { line: 1, column: 1 }));
        assert_eq!(editor.caret_position(), CursorPosition { line: 1, column: 3 });
        assert_eq!(editor.selected_text(), Some("🙂t"));
    }

    #[test]
    fn repeated_drag_hit_at_same_byte_is_a_no_op() {
        let mut editor = TextEditor::new("abcdef\nlast".to_string(), None).expect("editor");
        let cursor = editor.cursor_byte_offset();
        let rebuilds = editor.line_index_rebuilds;

        editor.update_drag_selection(0, 0);

        assert_eq!(editor.cursor_byte_offset(), cursor);
        assert_eq!(editor.selection_anchor_byte_offset(), None);
        assert_eq!(editor.line_index_rebuilds, rebuilds);
    }

    #[test]
    fn syntax_classification_is_path_driven_and_column_safe() {
        assert_eq!(syntax_language_for_path("src/main.rs"), SyntaxLanguage::Rust);
        assert_eq!(syntax_language_for_path("Cargo.lock"), SyntaxLanguage::Toml);
        assert_eq!(syntax_language_for_path("notes.txt"), SyntaxLanguage::Plain);

        let rust = syntax_spans_for_line(
            SyntaxLanguage::Rust,
            "let привет = \"мир\"; // note",
        );
        assert!(rust.contains(&SyntaxSpan {
            start_column: 0,
            end_column: 3,
            class: SyntaxClass::Keyword,
        }));
        assert!(rust.iter().any(|span| span.class == SyntaxClass::String));
        assert_eq!(rust.last().map(|span| span.class), Some(SyntaxClass::Comment));

        let json = syntax_spans_for_line(SyntaxLanguage::Json, "  \"ключ\": true");
        assert_eq!(json[0].class, SyntaxClass::Key);
        assert_eq!(json[0].start_column, 2);
        assert_eq!(json[0].end_column, 8);
        assert!(json.iter().any(|span| span.class == SyntaxClass::Boolean));
    }

    #[test]
    fn large_file_viewport_operations_reuse_cached_line_index() {
        let mut content = String::new();
        let mut line = 0;
        while content.len() < 90 * 1024 {
            content.push_str(&format!(
                "pub fn line_{line:04}() {{ let value = {line}; }} // sample\n"
            ));
            line += 1;
        }
        assert!(content.len() < 100 * 1024);
        let mut editor = TextEditor::from_text(content).expect("large editor");
        assert_eq!(editor.line_index_rebuilds, 1);
        assert_eq!(editor.line_count(), line + 1);
        assert_eq!(editor.line_byte_start(0), Some(0));

        const VIEWPORT_ROWS: usize = 32;
        editor.set_scroll(line.saturating_sub(40), 4);
        assert_eq!(
            editor.line_byte_start(editor.scroll_line()),
            Some(editor.line_spans[editor.scroll_line()].start)
        );
        let visible = editor.visible_lines(VIEWPORT_ROWS, 36);
        assert_eq!(visible.len(), VIEWPORT_ROWS);
        assert!(visible.iter().all(|rendered| !rendered.text.is_empty()));
        for visible_line in editor.scroll_line()..editor.scroll_line() + VIEWPORT_ROWS {
            let syntax = editor
                .syntax_spans_for_line("src/large.rs", visible_line)
                .expect("visible syntax");
            assert!(syntax.iter().any(|span| span.class == SyntaxClass::Keyword));
        }

        let hit = editor.hit_test_position(5, 10);
        assert_eq!(hit.line, editor.scroll_line() + 5);
        assert_eq!(hit.column, 14);
        editor.start_drag_selection(5, 10);
        editor.update_drag_selection(7, 12);
        assert!(editor.has_selection());
        assert_eq!(
            editor.selection_anchor_position(),
            Some(CursorPosition { line: hit.line, column: hit.column })
        );
        assert_eq!(editor.caret_position().line, hit.line + 2);
        editor.scroll_vertical(3);
        editor.scroll_horizontal(2);

        // Before the cached index, this read-only sequence rebuilt the complete
        // ~90 KiB line index 44 times. It must now perform no rebuilds.
        assert_eq!(editor.line_index_rebuilds, 1);

        editor.clear_selection();
        editor.insert_char('x').expect("bounded edit");
        assert_eq!(editor.line_index_rebuilds, 2);
        assert_eq!(editor.line_count(), line + 1);
    }
}

//! Snapshot types for agent rendering — no OS handles, safe to clone and send to UI.

use serde::{Deserialize, Serialize};

/// Which AI CLI agent to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentCli {
    Claude,
    Codex,
    Gemini,
}

impl AgentCli {
    /// Lowercase name used for file paths and CLI invocation.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentCli::Claude => "claude",
            AgentCli::Codex => "codex",
            AgentCli::Gemini => "gemini",
        }
    }

    /// Human-readable label for this CLI.
    pub fn label(self) -> &'static str {
        match self {
            AgentCli::Claude => "Claude",
            AgentCli::Codex => "Codex",
            AgentCli::Gemini => "Gemini",
        }
    }

    /// Returns the next CLI in cycle order.
    pub fn cycle(self) -> Self {
        match self {
            AgentCli::Claude => AgentCli::Codex,
            AgentCli::Codex => AgentCli::Gemini,
            AgentCli::Gemini => AgentCli::Claude,
        }
    }
}

/// A single terminal cell with character and colors.
#[derive(Clone, Debug)]
pub struct TermCell {
    pub ch: String,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
    pub bold: bool,
}

impl Default for TermCell {
    fn default() -> Self {
        Self {
            ch: " ".to_string(),
            fg: [204, 204, 204],
            bg: [0, 0, 0],
            bold: false,
        }
    }
}

/// Terminal grid — rows x cols of cells.
#[derive(Clone, Debug)]
pub struct TermGrid {
    pub cells: Vec<Vec<TermCell>>,
    pub cols: u16,
    pub rows: u16,
    pub cursor_row: u16,
    pub cursor_col: u16,
    /// Whether the terminal cursor should be visible (mirrors vt100's
    /// `hide_cursor()` flag — TUIs toggle this off while drawing custom UI).
    pub cursor_visible: bool,
    /// Optional ASCII-art "buddy" extracted from the right side of the grid.
    /// When `Some`, the cells covered by the buddy have already been blanked
    /// inside `cells`, so the main grid renders cleanly across the full width.
    /// The renderer should draw the buddy as a separate top-most layer.
    pub buddy: Option<BuddyArt>,
}

/// An ASCII-art block extracted from the right side of the terminal grid
/// (e.g. Claude Code's "Vellumwise" companion).
#[derive(Clone, Debug)]
pub struct BuddyArt {
    /// Rows of the art (each row may contain styled cells).
    pub rows: Vec<Vec<TermCell>>,
    /// Width of the art block in cells.
    pub width: u16,
    /// Original starting column on the source grid (informational).
    pub anchor_col: u16,
    /// Original starting row on the source grid (informational).
    pub anchor_row: u16,
}

impl TermGrid {
    pub fn empty(cols: u16, rows: u16) -> Self {
        Self {
            cells: vec![vec![TermCell::default(); cols as usize]; rows as usize],
            cols,
            rows,
            cursor_row: 0,
            cursor_col: 0,
            cursor_visible: true,
            buddy: None,
        }
    }

    /// Detect a right-anchored ASCII-art "buddy" block and extract it.
    ///
    /// Heuristic:
    /// 1. Look at the rightmost `scan_width` columns (default 30).
    /// 2. Find the leftmost column `lc` such that columns `lc..cols` contain a
    ///    contiguous non-blank region surrounded by ≥ `gap` blank columns on
    ///    its left side.
    /// 3. The region must span ≥ `min_rows` rows and ≤ `max_rows` rows.
    /// 4. Once found, copy those cells into `BuddyArt`, blank them in the
    ///    grid, and set `self.buddy`.
    ///
    /// No-op if nothing matches.
    pub fn detect_and_extract_buddy(&mut self) {
        let cols = self.cols as usize;
        let rows = self.rows as usize;
        if cols < 12 || rows < 3 {
            return;
        }
        let scan_width: usize = 30.min(cols / 2);
        let gap: usize = 2;
        let min_rows: usize = 2;

        // For each candidate left-column from (cols - scan_width) up to (cols - 4),
        // check whether [lc..cols] forms a buddy region.
        let scan_start = cols.saturating_sub(scan_width);
        let mut chosen: Option<(usize, usize, usize)> = None; // (lc, top_row, bot_row)

        // Try left-columns from far-left first so we capture the widest buddy.
        for lc in scan_start..cols.saturating_sub(3) {
            // Require `gap` columns of blanks immediately to the left.
            if lc < gap { continue; }
            let mut left_blank = true;
            for gc in (lc - gap)..lc {
                for r in 0..rows {
                    if !is_blank_cell(&self.cells[r][gc]) {
                        left_blank = false;
                        break;
                    }
                }
                if !left_blank { break; }
            }
            if !left_blank { continue; }

            // Determine row span of non-blank cells inside [lc..cols].
            let mut top: Option<usize> = None;
            let mut bot: Option<usize> = None;
            let mut total_filled = 0usize;
            for r in 0..rows {
                let row_has_content = (lc..cols).any(|c| !is_blank_cell(&self.cells[r][c]));
                if row_has_content {
                    if top.is_none() { top = Some(r); }
                    bot = Some(r);
                    total_filled += (lc..cols).filter(|&c| !is_blank_cell(&self.cells[r][c])).count();
                }
            }
            let (Some(top), Some(bot)) = (top, bot) else { continue; };
            let span = bot - top + 1;
            if span < min_rows { continue; }
            // Sanity: must contain at least a few non-blank cells overall.
            if total_filled < 4 { continue; }
            // Avoid grabbing huge text blocks: cap row span.
            if span > 8 { continue; }

            chosen = Some((lc, top, bot));
            break;
        }

        let Some((lc, top, bot)) = chosen else { return; };
        let width = (cols - lc) as u16;
        let mut art_rows: Vec<Vec<TermCell>> = Vec::with_capacity(bot - top + 1);
        for r in top..=bot {
            let mut row_cells = Vec::with_capacity(width as usize);
            for c in lc..cols {
                row_cells.push(self.cells[r][c].clone());
                // Blank the source cell.
                self.cells[r][c] = TermCell::default();
            }
            art_rows.push(row_cells);
        }
        self.buddy = Some(BuddyArt {
            rows: art_rows,
            width,
            anchor_col: lc as u16,
            anchor_row: top as u16,
        });
    }
}

fn is_blank_cell(cell: &TermCell) -> bool {
    cell.ch.is_empty() || cell.ch.chars().all(|c| c == ' ' || c == '\u{a0}')
}

/// Chat message role.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ChatRole {
    User,
    Assistant,
    Tool,
    Thinking,
    Error,
}

/// A single chat message.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: ChatRole,
    pub content: String,
    pub tool_name: Option<String>,
}

/// The rendering mode of the agent panel.
#[derive(Clone, Debug)]
pub enum AgentSnapshotMode {
    Pty(TermGrid),
    Chat(Vec<ChatMessage>),
    Idle,
}

/// Live in-progress status of the current agent turn.
///
/// Rendered as a single animated line at the bottom of the chat view.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum LiveStatus {
    /// No turn in progress.
    #[default]
    Idle,
    /// Agent is reasoning / waiting for first output.
    Thinking,
    /// Agent is executing a tool.
    RunningTool {
        /// Name of the tool currently executing.
        name: String,
        /// Number of tool calls completed so far in this turn.
        done: u32,
    },
}

/// Snapshot of agent state for rendering — no OS handles.
#[derive(Clone, Debug)]
pub struct AgentRenderSnapshot {
    pub mode: AgentSnapshotMode,
    pub session_active: bool,
    /// Live in-progress status (spinner text at bottom of chat).
    pub live_status: LiveStatus,
}

//! Multi-CLI agent session manager.
//!
//! `MultiCliManager` owns PTY and pipe sessions for Claude, Codex, and Gemini
//! simultaneously. Call `drain_events` every frame; call `snapshot(cli)` to
//! get a render-safe snapshot for the UI.

use std::fs;
use std::path::PathBuf;

use tokio::sync::broadcast;

use crate::pipe::{PipeSession, PipeProcessOptions};
use crate::pty::PtySession;
use crate::snapshot::{
    AgentCli, AgentRenderSnapshot, AgentSnapshotMode, ChatMessage, ChatRole, LiveStatus, TermCell, TermGrid,
};
use crate::types::{AgentEvent, CliTool, SessionConfig};

// =============================================================================
// ManagerConfig
// =============================================================================

/// Configuration passed to `MultiCliManager::new`.
///
/// Each CLI runs in `{sessions_dir}/{cli_name}/` as cwd, so its dotfolder ends
/// up there (e.g. `.claude/` for Claude Code).
pub struct ManagerConfig {
    /// Root directory. Each CLI gets a subdirectory: `{sessions_dir}/{cli_name}/`.
    pub sessions_dir: PathBuf,
    /// Initial PTY width in columns.
    pub default_cols: u16,
    /// Initial PTY height in rows.
    pub default_rows: u16,
}

impl Default for ManagerConfig {
    fn default() -> Self {
        Self {
            sessions_dir: default_sessions_dir(),
            default_cols: 80,
            default_rows: 24,
        }
    }
}

/// Returns `%APPDATA%/zengeld/agent-sessions` on Windows, or a `data/zengeld/agent-sessions`
/// fallback on other platforms.
fn default_sessions_dir() -> PathBuf {
    #[cfg(target_os = "windows")]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("zengeld").join("agent-sessions");
        }
    }
    PathBuf::from("data").join("zengeld").join("agent-sessions")
}

// =============================================================================
// Color helpers
// =============================================================================

fn vt100_color_to_rgb(color: vt100::Color, default_rgb: [u8; 3]) -> [u8; 3] {
    match color {
        vt100::Color::Default => default_rgb,
        vt100::Color::Rgb(r, g, b) => [r, g, b],
        vt100::Color::Idx(idx) => ansi_idx_to_rgb(idx),
    }
}

fn ansi_idx_to_rgb(idx: u8) -> [u8; 3] {
    const STANDARD_16: [[u8; 3]; 16] = [
        [0, 0, 0],
        [128, 0, 0],
        [0, 128, 0],
        [128, 128, 0],
        [0, 0, 128],
        [128, 0, 128],
        [0, 128, 128],
        [192, 192, 192],
        [128, 128, 128],
        [255, 0, 0],
        [0, 255, 0],
        [255, 255, 0],
        [0, 0, 255],
        [255, 0, 255],
        [0, 255, 255],
        [255, 255, 255],
    ];
    if (idx as usize) < STANDARD_16.len() {
        return STANDARD_16[idx as usize];
    }
    if idx >= 16 && idx <= 231 {
        let v = idx - 16;
        let b = v % 6;
        let g = (v / 6) % 6;
        let r = v / 36;
        let to_u8 = |x: u8| if x == 0 { 0 } else { 55 + x * 40 };
        return [to_u8(r), to_u8(g), to_u8(b)];
    }
    let gray = 8 + (idx - 232) * 10;
    [gray, gray, gray]
}

// =============================================================================
// Per-CLI state
// =============================================================================

/// State for a single CLI (Claude, Codex, or Gemini).
pub struct PerCliState {
    pub cli: AgentCli,
    pty_session: Option<PtySession>,
    pipe_session: Option<PipeSession>,
    pty_rx: Option<broadcast::Receiver<AgentEvent>>,
    pipe_rx: Option<broadcast::Receiver<AgentEvent>>,
    pty_parser: vt100::Parser,
    /// Transient live-stream buffer for the currently running session.
    /// Cleared when a past session is loaded for display.
    pub chat_messages: Vec<ChatMessage>,
    pub session_active: bool,
    pub pipe_session_id: Option<String>,
    /// Live status of the current turn — drives the animated spinner line in the chat view.
    pub live_status: LiveStatus,
}

impl PerCliState {
    fn new(cli: AgentCli, rows: u16, cols: u16) -> Self {
        Self {
            cli,
            pty_session: None,
            pipe_session: None,
            pty_rx: None,
            pipe_rx: None,
            pty_parser: vt100::Parser::new(rows, cols, 0),
            chat_messages: Vec::new(),
            session_active: false,
            pipe_session_id: None,
            live_status: LiveStatus::Idle,
        }
    }
}

// =============================================================================
// MultiCliManager
// =============================================================================

/// Manages agent sessions for all 3 CLIs simultaneously.
///
/// Each CLI has independent PTY and pipe sessions. Call `drain_events` every
/// frame. Call `snapshot(cli)` to get rendering state for the active CLI.
pub struct MultiCliManager {
    config: ManagerConfig,
    states: [PerCliState; 3],
    cols: u16,
    rows: u16,
}

impl MultiCliManager {
    /// Create a new manager with the given configuration.
    pub fn new(config: ManagerConfig) -> Self {
        let cols = config.default_cols;
        let rows = config.default_rows;
        Self {
            config,
            states: [
                PerCliState::new(AgentCli::Claude, rows, cols),
                PerCliState::new(AgentCli::Codex, rows, cols),
                PerCliState::new(AgentCli::Gemini, rows, cols),
            ],
            cols,
            rows,
        }
    }

    fn idx(cli: AgentCli) -> usize {
        match cli {
            AgentCli::Claude => 0,
            AgentCli::Codex => 1,
            AgentCli::Gemini => 2,
        }
    }

    fn state(&self, cli: AgentCli) -> &PerCliState {
        &self.states[Self::idx(cli)]
    }

    fn state_mut(&mut self, cli: AgentCli) -> &mut PerCliState {
        &mut self.states[Self::idx(cli)]
    }

    /// Returns the working directory for a given CLI.
    ///
    /// Each CLI runs isolated in `{sessions_dir}/{cli_name}/` so its dotfolder
    /// (`.claude/`, `.codex/`, `.gemini/`) lives there.
    pub fn cli_workdir(&self, cli: AgentCli) -> PathBuf {
        self.config.sessions_dir.join(cli.as_str())
    }

    // =========================================================================
    // Session lifecycle
    // =========================================================================

    /// Start a PTY session for the given CLI.
    pub async fn start_pty(&mut self, cli: AgentCli, config: SessionConfig) -> Result<(), String> {
        eprintln!("[gate4agent] start_pty cli={:?} cwd={}", cli, config.working_dir.display());
        let rows = self.rows;
        let cols = self.cols;
        let st = self.state_mut(cli);
        if st.session_active {
            return Err(format!("{:?} session already active", cli));
        }
        match PtySession::spawn_with_size(config, rows, cols).await {
            Ok(session) => {
                st.pty_rx = Some(session.subscribe());
                st.pty_parser = vt100::Parser::new(rows, cols, 0);
                st.pty_session = Some(session);
                st.session_active = true;
                Ok(())
            }
            Err(e) => Err(format!("Failed to spawn PTY for {:?}: {}", cli, e)),
        }
    }

    /// Start a Pipe/Chat session for the given CLI.
    ///
    /// Deprecated: prefer `send_chat` which lazy-spawns on the first message.
    pub async fn start_pipe(
        &mut self,
        cli: AgentCli,
        config: SessionConfig,
        prompt: &str,
    ) -> Result<(), String> {
        let st = self.state_mut(cli);
        if st.session_active {
            return Err(format!("{:?} session already active", cli));
        }
        let msg = ChatMessage {
            role: ChatRole::User,
            content: prompt.to_string(),
            tool_name: None,
        };
        st.chat_messages.push(msg);
        match PipeSession::spawn(config, prompt, PipeProcessOptions::default()).await {
            Ok(session) => {
                let st = self.state_mut(cli);
                st.pipe_rx = Some(session.subscribe());
                st.pipe_session = Some(session);
                st.session_active = true;
                Ok(())
            }
            Err(e) => Err(format!("Failed to spawn pipe for {:?}: {}", cli, e)),
        }
    }

    /// Stop the session for a single CLI.
    pub async fn stop(&mut self, cli: AgentCli) {
        let st = self.state_mut(cli);
        if let Some(session) = st.pty_session.take() {
            let _ = session.kill().await;
        }
        if let Some(session) = st.pipe_session.take() {
            let _ = session.kill().await;
        }
        st.pty_rx = None;
        st.pipe_rx = None;
        st.session_active = false;
    }

    /// Stop all active sessions (called on app shutdown).
    pub async fn stop_all(&mut self) {
        for cli in [AgentCli::Claude, AgentCli::Codex, AgentCli::Gemini] {
            self.stop(cli).await;
        }
    }

    // =========================================================================
    // I/O
    // =========================================================================

    /// Write a string to the active PTY for the given CLI.
    ///
    /// Lazy-spawns a PTY session on the first call for this CLI.
    pub async fn write_pty(&mut self, cli: AgentCli, text: &str) -> Result<(), String> {
        let need_spawn = self.state(cli).pty_session.is_none();
        if need_spawn {
            let workdir = self.cli_workdir(cli);
            fs::create_dir_all(&workdir).map_err(|e| format!("mkdir error: {}", e))?;
            eprintln!("[gate4agent] write_pty lazy-spawn cli={:?} cwd={}", cli, workdir.display());
            let tool = cli_to_tool(cli);
            let config = SessionConfig {
                tool,
                working_dir: workdir,
                ..SessionConfig::default()
            };
            self.start_pty(cli, config).await?;
        }
        eprintln!("[gate4agent] write_pty cli={:?} bytes={}", cli, text.len());
        let st = self.state(cli);
        if let Some(ref session) = st.pty_session {
            session
                .write(text)
                .await
                .map_err(|e| format!("PTY write error ({:?}): {}", cli, e))
        } else {
            Err(format!("No active PTY session for {:?}", cli))
        }
    }

    /// Send a chat prompt to the pipe session for the given CLI.
    ///
    /// Lazy-spawns a pipe session on the first call for this CLI.
    pub async fn send_chat(&mut self, cli: AgentCli, prompt: &str) -> Result<(), String> {
        let need_spawn = self.state(cli).pipe_session.is_none();
        eprintln!("[gate4agent] send_chat cli={:?} need_spawn={} prompt_len={}", cli, need_spawn, prompt.len());
        if need_spawn {
            let workdir = self.cli_workdir(cli);
            fs::create_dir_all(&workdir).map_err(|e| format!("mkdir error: {}", e))?;
            eprintln!("[gate4agent] send_chat lazy-spawn cli={:?} cwd={}", cli, workdir.display());
            let tool = cli_to_tool(cli);
            let config = SessionConfig {
                tool,
                working_dir: workdir,
                ..SessionConfig::default()
            };
            // Push the user message into the transient buffer immediately so UI shows it.
            self.state_mut(cli).chat_messages.push(ChatMessage {
                role: ChatRole::User,
                content: prompt.to_string(),
                tool_name: None,
            });
            // Switch to Thinking state so the animated spinner shows immediately.
            self.state_mut(cli).live_status = LiveStatus::Thinking;
            // Resume the previous Claude session if we have its id captured.
            // This makes a sequence of `send_chat` calls feel like a continuous
            // chat thread instead of independent one-shots.
            let resume_id = self.state(cli).pipe_session_id.clone();
            let opts = PipeProcessOptions {
                claude: crate::pipe::process::ClaudeOptions {
                    resume_session_id: resume_id,
                    ..Default::default()
                },
                ..PipeProcessOptions::default()
            };
            match PipeSession::spawn(config, prompt, opts).await {
                Ok(session) => {
                    let st = self.state_mut(cli);
                    st.pipe_rx = Some(session.subscribe());
                    st.pipe_session = Some(session);
                    st.session_active = true;
                    Ok(())
                }
                Err(e) => Err(format!("Failed to spawn pipe for {:?}: {}", cli, e)),
            }
        } else {
            // Existing session — push message and forward.
            self.state_mut(cli).chat_messages.push(ChatMessage {
                role: ChatRole::User,
                content: prompt.to_string(),
                tool_name: None,
            });
            self.state_mut(cli).live_status = LiveStatus::Thinking;
            let st = self.state(cli);
            if let Some(ref session) = st.pipe_session {
                session
                    .send_prompt(prompt)
                    .await
                    .map_err(|e| format!("Pipe send error ({:?}): {}", cli, e))
            } else {
                Err(format!("No active pipe session for {:?}", cli))
            }
        }
    }

    // =========================================================================
    // Resize
    // =========================================================================

    /// Resize all active PTY sessions to the new dimensions.
    pub async fn resize(&mut self, cols: u16, rows: u16) {
        if self.cols == cols && self.rows == rows {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        for st in &mut self.states {
            st.pty_parser.set_size(rows, cols);
            if let Some(ref session) = st.pty_session {
                let _ = session.resize(rows, cols).await;
            }
        }
    }

    // =========================================================================
    // Drain events (call every frame)
    // =========================================================================

    /// Drain events for all 3 CLIs. Returns `true` if any events were processed.
    pub fn drain_events(&mut self) -> bool {
        let mut had_events = false;
        for i in 0..3 {
            had_events |= Self::drain_one(&mut self.states[i]);
        }
        had_events
    }

    fn drain_one(st: &mut PerCliState) -> bool {
        let mut had_events = false;

        // Drain PTY events
        if let Some(ref mut rx) = st.pty_rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        had_events = true;
                        match event {
                            AgentEvent::PtyRaw { data } => {
                                st.pty_parser.process(&data);
                            }
                            AgentEvent::Exited { .. } => {
                                st.session_active = false;
                            }
                            _ => {}
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Closed) => {
                        st.session_active = false;
                        break;
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                }
            }
        }

        // Drain Pipe events
        if let Some(ref mut rx) = st.pipe_rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        had_events = true;
                        match event {
                            AgentEvent::PipeSessionStart { session_id, .. } => {
                                // Capture id for --resume on the next prompt.
                                // Do NOT push a ChatMessage — that was the source
                                // of "[tool] unknown · session XXXX" spam.
                                st.pipe_session_id = Some(session_id);
                            }
                            AgentEvent::PipeText { text, is_delta: _ } => {
                                // Finalize any RunningTool status: push a "done" history entry.
                                if let LiveStatus::RunningTool { ref name, done } = st.live_status.clone() {
                                    st.chat_messages.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: format!("✓ {} · {} done", name, done),
                                        tool_name: Some(name.clone()),
                                    });
                                }
                                st.live_status = LiveStatus::Idle;

                                if let Some(last) = st.chat_messages.last_mut() {
                                    if last.role == ChatRole::Assistant {
                                        last.content.push_str(&text);
                                        continue;
                                    }
                                }
                                st.chat_messages.push(ChatMessage {
                                    role: ChatRole::Assistant,
                                    content: text,
                                    tool_name: None,
                                });
                            }
                            AgentEvent::PipeToolStart { name, .. } => {
                                // Finalize previous tool if any.
                                if let LiveStatus::RunningTool { name: ref prev, done } = st.live_status.clone() {
                                    st.chat_messages.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: format!("✓ {} · {} done", prev, done),
                                        tool_name: Some(prev.clone()),
                                    });
                                }
                                // Start tracking the new tool.
                                st.live_status = LiveStatus::RunningTool { name, done: 0 };
                            }
                            AgentEvent::PipeToolResult { id: _, output: _, is_error: _, .. } => {
                                if let LiveStatus::RunningTool { done, .. } = &mut st.live_status {
                                    *done = done.saturating_add(1);
                                }
                            }
                            AgentEvent::PipeThinking { text: _ } => {
                                // Keep Thinking status as-is; suppress bubble noise.
                            }
                            AgentEvent::Error { message } => {
                                st.live_status = LiveStatus::Idle;
                                st.chat_messages.push(ChatMessage {
                                    role: ChatRole::Error,
                                    content: message,
                                    tool_name: None,
                                });
                            }
                            AgentEvent::PipeTurnComplete { .. } => {
                                // Finalize any in-flight tool and clear live status.
                                if let LiveStatus::RunningTool { ref name, done } = st.live_status.clone() {
                                    st.chat_messages.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: format!("✓ {} · {} done", name, done),
                                        tool_name: Some(name.clone()),
                                    });
                                }
                                st.live_status = LiveStatus::Idle;
                            }
                            AgentEvent::PipeSessionEnd { result, is_error, .. } => {
                                if is_error {
                                    st.chat_messages.push(ChatMessage {
                                        role: ChatRole::Error,
                                        content: format!("Session error · {}", result),
                                        tool_name: None,
                                    });
                                }
                                // Finalize any in-flight tool.
                                if let LiveStatus::RunningTool { ref name, done } = st.live_status.clone() {
                                    st.chat_messages.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: format!("✓ {} · {} done", name, done),
                                        tool_name: Some(name.clone()),
                                    });
                                }
                                st.live_status = LiveStatus::Idle;
                            }
                            AgentEvent::Exited { .. } => {
                                st.session_active = false;
                                st.live_status = LiveStatus::Idle;
                            }
                            _ => {}
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Closed) => {
                        st.session_active = false;
                        break;
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                }
            }
        }

        had_events
    }

    // =========================================================================
    // Snapshot + query
    // =========================================================================

    /// Build a render snapshot for the given CLI, honoring the UI's requested mode.
    ///
    /// `want_pty=true` → return PTY grid if a PTY parser/session exists (even if exited),
    /// otherwise Idle. `want_pty=false` → return chat messages if any, otherwise Idle.
    /// This decouples snapshot mode from session liveness so mode switches don't
    /// destroy the other view.
    pub fn snapshot_mode(&self, cli: AgentCli, want_pty: bool) -> AgentRenderSnapshot {
        let st = self.state(cli);
        if want_pty {
            // Render PTY grid as long as we ever spawned one OR the parser has content.
            if st.pty_session.is_some() || self.pty_has_content(cli) {
                return self.build_pty_snapshot(st);
            }
            return AgentRenderSnapshot {
                mode: AgentSnapshotMode::Idle,
                session_active: st.session_active,
                live_status: st.live_status.clone(),
            };
        }
        // Chat mode requested
        if !st.chat_messages.is_empty() {
            return AgentRenderSnapshot {
                mode: AgentSnapshotMode::Chat(st.chat_messages.clone()),
                session_active: st.session_active,
                live_status: st.live_status.clone(),
            };
        }
        AgentRenderSnapshot {
            mode: AgentSnapshotMode::Idle,
            session_active: st.session_active,
            live_status: st.live_status.clone(),
        }
    }

    fn pty_has_content(&self, cli: AgentCli) -> bool {
        let st = self.state(cli);
        let screen = st.pty_parser.screen();
        for row in 0..self.rows {
            for col in 0..self.cols {
                if let Some(cell) = screen.cell(row, col) {
                    if !cell.contents().is_empty() {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn build_pty_snapshot(&self, st: &PerCliState) -> AgentRenderSnapshot {
        let screen = st.pty_parser.screen();
        let mut grid = TermGrid::empty(self.cols, self.rows);
        for row in 0..self.rows {
            for col in 0..self.cols {
                if let Some(cell) = screen.cell(row, col) {
                    let fg = vt100_color_to_rgb(cell.fgcolor(), [204, 204, 204]);
                    let bg = vt100_color_to_rgb(cell.bgcolor(), [0, 0, 0]);
                    let contents = cell.contents();
                    grid.cells[row as usize][col as usize] = TermCell {
                        ch: if contents.is_empty() { " ".to_string() } else { contents },
                        fg,
                        bg,
                        bold: cell.bold(),
                    };
                }
            }
        }
        let (cur_row, cur_col) = screen.cursor_position();
        grid.cursor_row = cur_row;
        grid.cursor_col = cur_col;
        grid.cursor_visible = !screen.hide_cursor();
        // Buddy extraction disabled — heuristic doesn't reliably catch the
        // companion ASCII art yet. Kept in snapshot.rs for future experiments.
        // grid.detect_and_extract_buddy();
        AgentRenderSnapshot {
            mode: AgentSnapshotMode::Pty(grid),
            session_active: st.session_active,
            live_status: st.live_status.clone(),
        }
    }

    /// Legacy snapshot — inferred from state. Prefer `snapshot_mode`.
    pub fn snapshot(&self, cli: AgentCli) -> AgentRenderSnapshot {
        let st = self.state(cli);

        if !st.session_active {
            if !st.chat_messages.is_empty() {
                return AgentRenderSnapshot {
                    mode: AgentSnapshotMode::Chat(st.chat_messages.clone()),
                    session_active: false,
                    live_status: st.live_status.clone(),
                };
            }
            return AgentRenderSnapshot {
                mode: AgentSnapshotMode::Idle,
                session_active: false,
                live_status: st.live_status.clone(),
            };
        }

        if st.pty_session.is_some() {
            let screen = st.pty_parser.screen();
            let mut grid = TermGrid::empty(self.cols, self.rows);
            for row in 0..self.rows {
                for col in 0..self.cols {
                    if let Some(cell) = screen.cell(row, col) {
                        let fg = vt100_color_to_rgb(cell.fgcolor(), [204, 204, 204]);
                        let bg = vt100_color_to_rgb(cell.bgcolor(), [0, 0, 0]);
                        let contents = cell.contents();
                        grid.cells[row as usize][col as usize] = TermCell {
                            ch: if contents.is_empty() {
                                " ".to_string()
                            } else {
                                contents
                            },
                            fg,
                            bg,
                            bold: cell.bold(),
                        };
                    }
                }
            }
            let (cur_row, cur_col) = screen.cursor_position();
            grid.cursor_row = cur_row;
            grid.cursor_col = cur_col;
            grid.cursor_visible = !screen.hide_cursor();
            // Buddy extraction disabled — heuristic doesn't reliably catch the
        // companion ASCII art yet. Kept in snapshot.rs for future experiments.
        // grid.detect_and_extract_buddy();
            AgentRenderSnapshot {
                mode: AgentSnapshotMode::Pty(grid),
                session_active: true,
                live_status: st.live_status.clone(),
            }
        } else {
            AgentRenderSnapshot {
                mode: AgentSnapshotMode::Chat(st.chat_messages.clone()),
                session_active: true,
                live_status: st.live_status.clone(),
            }
        }
    }

    /// Returns `true` if either PTY or pipe session is alive for this CLI.
    pub fn is_active(&self, cli: AgentCli) -> bool {
        self.state(cli).session_active
    }

    /// Returns `true` if any CLI has an active session.
    pub fn any_active(&self) -> bool {
        self.states.iter().any(|s| s.session_active)
    }

    /// Number of past sessions for this CLI (live disk read).
    pub fn past_session_count(&self, cli: AgentCli) -> usize {
        let workdir = self.cli_workdir(cli);
        let n = crate::history::reader_for(cli).list_sessions(&workdir).len();
        n
    }

    /// List past sessions (newest first, live disk read).
    pub fn list_past_sessions(&self, cli: AgentCli) -> Vec<crate::history::SessionMeta> {
        let workdir = self.cli_workdir(cli);
        crate::history::reader_for(cli).list_sessions(&workdir)
    }

    /// Load the latest past session into the chat view (display only — does NOT resume the CLI process).
    ///
    /// Returns `true` if anything was loaded.
    pub fn load_latest_history(&mut self, cli: AgentCli) -> bool {
        let workdir = self.cli_workdir(cli);
        let reader = crate::history::reader_for(cli);
        eprintln!("[gate4agent] load_latest_history cli={:?} workdir={}", cli, workdir.display());
        let latest = match reader.latest_session(&workdir) {
            Some(id) => {
                eprintln!("[gate4agent]   latest session id={}", id);
                id
            }
            None => {
                eprintln!("[gate4agent]   no past sessions found");
                return false;
            }
        };
        let messages = reader.load_session(&workdir, &latest);
        eprintln!("[gate4agent]   loaded {} messages", messages.len());
        if messages.is_empty() {
            return false;
        }
        let st = self.state_mut(cli);
        st.chat_messages = messages;
        st.pipe_session_id = Some(latest.clone());
        st.pipe_session = None;
        st.pipe_rx = None;
        true
    }

    /// Load a specific past session into the chat view AND mark it as the
    /// resume target — the next `send_chat` will respawn the pipe with
    /// `--resume <session_id>`, so the user keeps writing into the chosen
    /// thread instead of accidentally starting a fresh session.
    pub fn load_history(&mut self, cli: AgentCli, session_id: &str) -> bool {
        let workdir = self.cli_workdir(cli);
        let reader = crate::history::reader_for(cli);
        let messages = reader.load_session(&workdir, session_id);
        if messages.is_empty() {
            return false;
        }
        let st = self.state_mut(cli);
        st.chat_messages = messages;
        st.pipe_session_id = Some(session_id.to_string());
        // Drop any stale live pipe handle so the next send_chat goes through
        // the spawn branch and picks up the resume id.
        st.pipe_session = None;
        st.pipe_rx = None;
        true
    }

    /// Backwards-compatible wrapper — loads the latest past session.
    ///
    /// Chart-app callers using `load_next_past_session` continue to work unchanged.
    pub fn load_next_past_session(&mut self, cli: AgentCli) -> bool {
        self.load_latest_history(cli)
    }
}

impl Default for MultiCliManager {
    fn default() -> Self {
        Self::new(ManagerConfig::default())
    }
}

// =============================================================================
// Helpers
// =============================================================================

fn cli_to_tool(cli: AgentCli) -> CliTool {
    match cli {
        AgentCli::Claude => CliTool::ClaudeCode,
        AgentCli::Codex => CliTool::Codex,
        AgentCli::Gemini => CliTool::Gemini,
    }
}

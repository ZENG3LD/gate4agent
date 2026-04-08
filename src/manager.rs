//! Multi-CLI agent session manager.
//!
//! `MultiCliManager` owns PTY and pipe sessions for Claude, Codex, and Gemini
//! simultaneously. Call `drain_events` every frame; call `snapshot(cli)` to
//! get a render-safe snapshot for the active CLI.
//!
//! ## Architecture
//!
//! Internally the manager stores instances in a `HashMap<InstanceId, AgentInstance>`.
//! Three "legacy" instances (one per `AgentCli`) are pre-created in `new()` so that
//! all existing callers using the `cli: AgentCli` API continue to work unchanged.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use tokio::sync::broadcast;
use uuid::Uuid;

use crate::pty::PtySession;
use crate::snapshot::{
    AgentCli, AgentRenderSnapshot, AgentSnapshotMode, ChatMessage, ChatRole, LiveStatus, TermCell, TermGrid,
};
use crate::transport::{SpawnOptions, TransportSession};
use crate::types::{AgentEvent, CliTool, SessionConfig};

// =============================================================================
// InstanceId
// =============================================================================

/// Opaque identifier for a per-instance agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct InstanceId(pub Uuid);

impl InstanceId {
    /// Create a new random instance ID.
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for InstanceId {
    fn default() -> Self {
        Self::new()
    }
}

// =============================================================================
// InstanceMode
// =============================================================================

/// Operating mode for an agent instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum InstanceMode {
    /// PTY mirror mode — spawns agent in a real pseudo-terminal.
    Pty,
    /// Chat/pipe mode — communicates via stdin/stdout with NDJSON events.
    /// Supported for all 6 CLIs via `TransportSession`.
    Chat,
}

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
    if (16..=231).contains(&idx) {
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
// AgentInstance (internal)
// =============================================================================

/// Internal state for a single agent instance. Not public.
struct AgentInstance {
    /// Stored for diagnostic / future introspection — the canonical id lives as the map key.
    _id: InstanceId,
    cli: AgentCli,
    mode: InstanceMode,
    workdir: PathBuf,
    pty_session: Option<PtySession>,
    /// Phase 5: unified pipe/daemon session via `TransportSession`.
    transport_session: Option<TransportSession>,
    pty_rx: Option<broadcast::Receiver<AgentEvent>>,
    pipe_rx: Option<broadcast::Receiver<AgentEvent>>,
    pty_parser: vt100::Parser,
    /// Transient live-stream buffer for the currently running session.
    /// Cleared when a past session is loaded for display.
    chat_messages: Vec<ChatMessage>,
    session_active: bool,
    pipe_session_id: Option<String>,
    /// Live status of the current turn — drives the animated spinner line in the chat view.
    live_status: LiveStatus,
}

impl AgentInstance {
    fn new(id: InstanceId, cli: AgentCli, mode: InstanceMode, workdir: PathBuf, rows: u16, cols: u16) -> Self {
        Self {
            _id: id,
            cli,
            mode,
            workdir,
            pty_session: None,
            transport_session: None,
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

/// Manages agent sessions for all CLIs simultaneously.
///
/// Internally uses a `HashMap<InstanceId, AgentInstance>`. Three legacy
/// instances (one per `AgentCli`) are pre-created in `new()` so that all
/// existing callers using the `cli: AgentCli` API continue to work unchanged.
pub struct MultiCliManager {
    config: ManagerConfig,
    instances: HashMap<InstanceId, AgentInstance>,
    /// Pre-created legacy slots: index 0 = Claude, 1 = Codex, 2 = Gemini.
    legacy_instances: [InstanceId; 3],
    cols: u16,
    rows: u16,
}

impl MultiCliManager {
    /// Create a new manager with the given configuration.
    pub fn new(config: ManagerConfig) -> Self {
        let cols = config.default_cols;
        let rows = config.default_rows;

        let mut instances = HashMap::new();

        let claude_id = InstanceId::new();
        let codex_id = InstanceId::new();
        let gemini_id = InstanceId::new();

        let claude_workdir = config.sessions_dir.join(AgentCli::Claude.as_str());
        let codex_workdir = config.sessions_dir.join(AgentCli::Codex.as_str());
        let gemini_workdir = config.sessions_dir.join(AgentCli::Gemini.as_str());

        instances.insert(claude_id, AgentInstance::new(claude_id, AgentCli::Claude, InstanceMode::Pty, claude_workdir, rows, cols));
        instances.insert(codex_id, AgentInstance::new(codex_id, AgentCli::Codex, InstanceMode::Pty, codex_workdir, rows, cols));
        instances.insert(gemini_id, AgentInstance::new(gemini_id, AgentCli::Gemini, InstanceMode::Pty, gemini_workdir, rows, cols));

        Self {
            config,
            instances,
            legacy_instances: [claude_id, codex_id, gemini_id],
            cols,
            rows,
        }
    }

    // =========================================================================
    // Legacy index helpers
    // =========================================================================

    fn legacy_idx(cli: AgentCli) -> usize {
        match cli {
            AgentCli::Claude => 0,
            AgentCli::Codex => 1,
            AgentCli::Gemini => 2,
            // Cursor and OpenCode do not have legacy slots — they are
            // managed through the per-instance API. Map to slot 0 as a safe
            // fallback; callers must not pass these to legacy methods.
            AgentCli::Cursor | AgentCli::OpenCode => 0,
        }
    }

    fn legacy_id(&self, cli: AgentCli) -> InstanceId {
        self.legacy_instances[Self::legacy_idx(cli)]
    }

    // =========================================================================
    // New per-instance API
    // =========================================================================

    /// Register a new agent instance. Does NOT spawn any process.
    ///
    /// All 6 CLIs support Chat mode via `TransportSession` (pipe/daemon).
    pub fn create_instance(
        &mut self,
        cli: AgentCli,
        mode: InstanceMode,
        workdir: PathBuf,
    ) -> Result<InstanceId, String> {
        let id = InstanceId::new();
        let inst = AgentInstance::new(id, cli, mode, workdir, self.rows, self.cols);
        self.instances.insert(id, inst);
        Ok(id)
    }

    /// Stop and remove an instance from the manager.
    pub async fn remove_instance(&mut self, id: InstanceId) {
        self.stop_instance(id).await;
        self.instances.remove(&id);
    }

    /// Start a PTY session for the given instance.
    pub async fn start_pty_instance(
        &mut self,
        id: InstanceId,
        config: SessionConfig,
    ) -> Result<(), String> {
        let rows = self.rows;
        let cols = self.cols;
        let inst = self
            .instances
            .get_mut(&id)
            .ok_or_else(|| format!("Instance {:?} not found", id))?;
        if inst.session_active {
            return Err(format!("Instance {:?} session already active", id));
        }
        match PtySession::spawn_with_size(config, rows, cols).await {
            Ok(session) => {
                inst.pty_rx = Some(session.subscribe());
                inst.pty_parser = vt100::Parser::new(rows, cols, 0);
                inst.pty_session = Some(session);
                inst.session_active = true;
                Ok(())
            }
            Err(e) => Err(format!("Failed to spawn PTY for instance {:?}: {}", id, e)),
        }
    }

    /// Write a string to the active PTY for the given instance.
    ///
    /// Lazy-spawns a PTY session on the first call.
    pub async fn write_pty_instance(&mut self, id: InstanceId, text: &str) -> Result<(), String> {
        let need_spawn = self
            .instances
            .get(&id)
            .ok_or_else(|| format!("Instance {:?} not found", id))?
            .pty_session
            .is_none();

        if need_spawn {
            let (cli, workdir) = {
                let inst = self
                    .instances
                    .get(&id)
                    .ok_or_else(|| format!("Instance {:?} not found", id))?;
                (inst.cli, inst.workdir.clone())
            };
            fs::create_dir_all(&workdir).map_err(|e| format!("mkdir error: {}", e))?;
            eprintln!(
                "[gate4agent] write_pty_instance lazy-spawn id={:?} cwd={}",
                id,
                workdir.display()
            );
            let tool = cli_to_tool(cli);
            let config = SessionConfig {
                tool,
                working_dir: workdir,
                ..SessionConfig::default()
            };
            self.start_pty_instance(id, config).await?;
        }

        let inst = self
            .instances
            .get(&id)
            .ok_or_else(|| format!("Instance {:?} not found", id))?;
        eprintln!(
            "[gate4agent] write_pty_instance id={:?} bytes={}",
            id,
            text.len()
        );
        if let Some(ref session) = inst.pty_session {
            session
                .write(text)
                .await
                .map_err(|e| format!("PTY write error (instance {:?}): {}", id, e))
        } else {
            Err(format!("No active PTY session for instance {:?}", id))
        }
    }

    /// Send a chat prompt to the pipe session for the given instance.
    ///
    /// Only valid if the instance mode is `Chat`. Lazy-spawns on first call.
    pub async fn send_chat_instance(&mut self, id: InstanceId, prompt: &str) -> Result<(), String> {
        {
            let inst = self
                .instances
                .get(&id)
                .ok_or_else(|| format!("Instance {:?} not found", id))?;
            if inst.mode != InstanceMode::Chat {
                return Err(format!(
                    "Instance {:?} is in {:?} mode, not Chat",
                    id, inst.mode
                ));
            }
        }

        let need_spawn = self
            .instances
            .get(&id)
            .ok_or_else(|| format!("Instance {:?} not found", id))?
            .transport_session
            .is_none();

        eprintln!(
            "[gate4agent] send_chat_instance id={:?} need_spawn={} prompt_len={}",
            id,
            need_spawn,
            prompt.len()
        );

        if need_spawn {
            let (cli, workdir, resume_id) = {
                let inst = self
                    .instances
                    .get(&id)
                    .ok_or_else(|| format!("Instance {:?} not found", id))?;
                (inst.cli, inst.workdir.clone(), inst.pipe_session_id.clone())
            };
            fs::create_dir_all(&workdir).map_err(|e| format!("mkdir error: {}", e))?;
            eprintln!(
                "[gate4agent] send_chat_instance lazy-spawn id={:?} cwd={}",
                id,
                workdir.display()
            );
            let tool = cli_to_tool(cli);
            {
                let inst = self
                    .instances
                    .get_mut(&id)
                    .ok_or_else(|| format!("Instance {:?} not found", id))?;
                inst.chat_messages.push(ChatMessage {
                    role: ChatRole::User,
                    content: prompt.to_string(),
                    tool_name: None,
                });
                inst.live_status = LiveStatus::Thinking;
            }
            let opts = SpawnOptions {
                resume_session_id: resume_id,
                ..SpawnOptions::default()
            };
            match TransportSession::spawn(tool, &workdir, prompt, opts).await {
                Ok(session) => {
                    let inst = self
                        .instances
                        .get_mut(&id)
                        .ok_or_else(|| format!("Instance {:?} not found", id))?;
                    inst.pipe_rx = Some(session.subscribe());
                    inst.transport_session = Some(session);
                    inst.session_active = true;
                    Ok(())
                }
                Err(e) => Err(format!("Failed to spawn pipe for instance {:?}: {}", id, e)),
            }
        } else {
            let inst = self
                .instances
                .get_mut(&id)
                .ok_or_else(|| format!("Instance {:?} not found", id))?;
            inst.chat_messages.push(ChatMessage {
                role: ChatRole::User,
                content: prompt.to_string(),
                tool_name: None,
            });
            inst.live_status = LiveStatus::Thinking;
            if let Some(ref session) = inst.transport_session {
                session
                    .send_prompt(prompt)
                    .await
                    .map_err(|e| format!("Pipe send error (instance {:?}): {}", id, e))
            } else {
                Err(format!("No active pipe session for instance {:?}", id))
            }
        }
    }

    /// Stop the session for a single instance.
    pub async fn stop_instance(&mut self, id: InstanceId) {
        if let Some(inst) = self.instances.get_mut(&id) {
            if let Some(session) = inst.pty_session.take() {
                let _ = session.kill().await;
            }
            if let Some(session) = inst.transport_session.take() {
                let _ = session.kill().await;
            }
            inst.pty_rx = None;
            inst.pipe_rx = None;
            inst.session_active = false;
        }
    }

    /// Build a render snapshot for the given instance.
    ///
    /// Returns `None` if the instance doesn't exist. Otherwise builds a PTY or
    /// Chat snapshot based on the instance's mode.
    pub fn snapshot_instance(&self, id: InstanceId) -> Option<AgentRenderSnapshot> {
        let inst = self.instances.get(&id)?;
        let snap = match inst.mode {
            InstanceMode::Pty => {
                if inst.pty_session.is_some() || self.instance_pty_has_content(inst) {
                    self.build_pty_snapshot_from_instance(inst)
                } else {
                    AgentRenderSnapshot {
                        mode: AgentSnapshotMode::Idle,
                        session_active: inst.session_active,
                        live_status: inst.live_status.clone(),
                    }
                }
            }
            InstanceMode::Chat => {
                if !inst.chat_messages.is_empty() {
                    AgentRenderSnapshot {
                        mode: AgentSnapshotMode::Chat(inst.chat_messages.clone()),
                        session_active: inst.session_active,
                        live_status: inst.live_status.clone(),
                    }
                } else {
                    AgentRenderSnapshot {
                        mode: AgentSnapshotMode::Idle,
                        session_active: inst.session_active,
                        live_status: inst.live_status.clone(),
                    }
                }
            }
        };
        Some(snap)
    }

    /// Returns `true` if the instance is active.
    pub fn is_instance_active(&self, id: InstanceId) -> bool {
        self.instances
            .get(&id)
            .map(|i| i.session_active)
            .unwrap_or(false)
    }

    /// Returns all registered instance IDs.
    pub fn list_instances(&self) -> Vec<InstanceId> {
        self.instances.keys().copied().collect()
    }

    /// Returns the `AgentCli` for an instance.
    pub fn instance_cli(&self, id: InstanceId) -> Option<AgentCli> {
        self.instances.get(&id).map(|i| i.cli)
    }

    /// Returns the `InstanceMode` for an instance.
    pub fn instance_mode(&self, id: InstanceId) -> Option<InstanceMode> {
        self.instances.get(&id).map(|i| i.mode)
    }

    /// Returns the working directory for an instance.
    pub fn instance_workdir(&self, id: InstanceId) -> Option<&Path> {
        self.instances.get(&id).map(|i| i.workdir.as_path())
    }

    /// Resize the PTY for a single instance.
    pub async fn resize_instance(&mut self, id: InstanceId, cols: u16, rows: u16) {
        if let Some(inst) = self.instances.get_mut(&id) {
            inst.pty_parser.set_size(rows, cols);
            if let Some(ref session) = inst.pty_session {
                let _ = session.resize(rows, cols).await;
            }
        }
    }

    /// Load the latest history for an instance. Chat-mode only.
    pub fn load_latest_history_instance(&mut self, id: InstanceId) -> bool {
        let workdir = match self.instances.get(&id).map(|i| i.workdir.clone()) {
            Some(w) => w,
            None => return false,
        };
        let cli = match self.instances.get(&id).map(|i| i.cli) {
            Some(c) => c,
            None => return false,
        };
        let reader = crate::history::reader_for(cli);
        let latest = match reader.latest_session(&workdir) {
            Some(id_str) => id_str,
            None => return false,
        };
        let messages = reader.load_session(&workdir, &latest);
        if messages.is_empty() {
            return false;
        }
        if let Some(inst) = self.instances.get_mut(&id) {
            inst.chat_messages = messages;
            inst.pipe_session_id = Some(latest);
            inst.transport_session = None;
            inst.pipe_rx = None;
        }
        true
    }

    /// Load a specific history session for an instance. Chat-mode only.
    pub fn load_history_instance(&mut self, id: InstanceId, session_id: &str) -> bool {
        let workdir = match self.instances.get(&id).map(|i| i.workdir.clone()) {
            Some(w) => w,
            None => return false,
        };
        let cli = match self.instances.get(&id).map(|i| i.cli) {
            Some(c) => c,
            None => return false,
        };
        let reader = crate::history::reader_for(cli);
        let messages = reader.load_session(&workdir, session_id);
        if messages.is_empty() {
            return false;
        }
        if let Some(inst) = self.instances.get_mut(&id) {
            inst.chat_messages = messages;
            inst.pipe_session_id = Some(session_id.to_string());
            inst.transport_session = None;
            inst.pipe_rx = None;
        }
        true
    }

    // =========================================================================
    // Legacy 3-CLI public API (shims onto per-instance API)
    // =========================================================================

    /// Returns the working directory for a given CLI.
    ///
    /// Each CLI runs isolated in `{sessions_dir}/{cli_name}/` so its dotfolder
    /// (`.claude/`, `.codex/`, `.gemini/`) lives there.
    pub fn cli_workdir(&self, cli: AgentCli) -> PathBuf {
        self.config.sessions_dir.join(cli.as_str())
    }

    /// Start a PTY session for the given CLI.
    pub async fn start_pty(&mut self, cli: AgentCli, config: SessionConfig) -> Result<(), String> {
        eprintln!(
            "[gate4agent] start_pty cli={:?} cwd={}",
            cli,
            config.working_dir.display()
        );
        let id = self.legacy_id(cli);
        let rows = self.rows;
        let cols = self.cols;
        let inst = self
            .instances
            .get_mut(&id)
            .ok_or_else(|| format!("Legacy instance for {:?} not found", cli))?;
        if inst.session_active {
            return Err(format!("{:?} session already active", cli));
        }
        match PtySession::spawn_with_size(config, rows, cols).await {
            Ok(session) => {
                inst.pty_rx = Some(session.subscribe());
                inst.pty_parser = vt100::Parser::new(rows, cols, 0);
                inst.pty_session = Some(session);
                inst.session_active = true;
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
        let id = self.legacy_id(cli);
        let inst = self
            .instances
            .get_mut(&id)
            .ok_or_else(|| format!("Legacy instance for {:?} not found", cli))?;
        if inst.session_active {
            return Err(format!("{:?} session already active", cli));
        }
        let msg = ChatMessage {
            role: ChatRole::User,
            content: prompt.to_string(),
            tool_name: None,
        };
        inst.chat_messages.push(msg);
        let workdir = config.working_dir.clone();
        let tool = config.tool;
        match TransportSession::spawn(tool, &workdir, prompt, SpawnOptions::default()).await {
            Ok(session) => {
                let inst = self
                    .instances
                    .get_mut(&id)
                    .ok_or_else(|| format!("Legacy instance for {:?} not found", cli))?;
                inst.pipe_rx = Some(session.subscribe());
                inst.transport_session = Some(session);
                inst.session_active = true;
                Ok(())
            }
            Err(e) => Err(format!("Failed to spawn pipe for {:?}: {}", cli, e)),
        }
    }

    /// Stop the session for a single CLI.
    pub async fn stop(&mut self, cli: AgentCli) {
        let id = self.legacy_id(cli);
        self.stop_instance(id).await;
    }

    /// Stop all active sessions (called on app shutdown).
    pub async fn stop_all(&mut self) {
        let all_ids: Vec<InstanceId> = self.instances.keys().copied().collect();
        for id in all_ids {
            self.stop_instance(id).await;
        }
    }

    /// Write a string to the active PTY for the given CLI.
    ///
    /// Lazy-spawns a PTY session on the first call for this CLI.
    pub async fn write_pty(&mut self, cli: AgentCli, text: &str) -> Result<(), String> {
        let need_spawn = {
            let id = self.legacy_id(cli);
            self.instances
                .get(&id)
                .map(|i| i.pty_session.is_none())
                .unwrap_or(false)
        };
        if need_spawn {
            let workdir = self.cli_workdir(cli);
            fs::create_dir_all(&workdir).map_err(|e| format!("mkdir error: {}", e))?;
            eprintln!(
                "[gate4agent] write_pty lazy-spawn cli={:?} cwd={}",
                cli,
                workdir.display()
            );
            let tool = cli_to_tool(cli);
            let config = SessionConfig {
                tool,
                working_dir: workdir,
                ..SessionConfig::default()
            };
            self.start_pty(cli, config).await?;
        }
        eprintln!(
            "[gate4agent] write_pty cli={:?} bytes={}",
            cli,
            text.len()
        );
        let id = self.legacy_id(cli);
        let inst = self
            .instances
            .get(&id)
            .ok_or_else(|| format!("Legacy instance for {:?} not found", cli))?;
        if let Some(ref session) = inst.pty_session {
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
    ///
    /// Routes all 6 CLIs through `TransportSession`. Legacy Claude-only restriction
    /// is lifted: all CLIs with a legacy slot can now use Chat/pipe mode.
    pub async fn send_chat(&mut self, cli: AgentCli, prompt: &str) -> Result<(), String> {
        let id = self.legacy_id(cli);
        let need_spawn = self
            .instances
            .get(&id)
            .map(|i| i.transport_session.is_none())
            .unwrap_or(false);

        eprintln!(
            "[gate4agent] send_chat cli={:?} need_spawn={} prompt_len={}",
            cli,
            need_spawn,
            prompt.len()
        );

        if need_spawn {
            let workdir = self.cli_workdir(cli);
            fs::create_dir_all(&workdir).map_err(|e| format!("mkdir error: {}", e))?;
            eprintln!(
                "[gate4agent] send_chat lazy-spawn cli={:?} cwd={}",
                cli,
                workdir.display()
            );
            let tool = cli_to_tool(cli);
            // Push the user message into the transient buffer immediately so UI shows it.
            {
                let inst = self
                    .instances
                    .get_mut(&id)
                    .ok_or_else(|| format!("Legacy instance for {:?} not found", cli))?;
                inst.chat_messages.push(ChatMessage {
                    role: ChatRole::User,
                    content: prompt.to_string(),
                    tool_name: None,
                });
                // Switch to Thinking state so the animated spinner shows immediately.
                inst.live_status = LiveStatus::Thinking;
            }
            // Resume the previous session if we have its id captured.
            let resume_id = self
                .instances
                .get(&id)
                .and_then(|i| i.pipe_session_id.clone());
            let opts = SpawnOptions {
                resume_session_id: resume_id,
                ..SpawnOptions::default()
            };
            match TransportSession::spawn(tool, &workdir, prompt, opts).await {
                Ok(session) => {
                    let inst = self
                        .instances
                        .get_mut(&id)
                        .ok_or_else(|| format!("Legacy instance for {:?} not found", cli))?;
                    inst.pipe_rx = Some(session.subscribe());
                    inst.transport_session = Some(session);
                    inst.session_active = true;
                    Ok(())
                }
                Err(e) => Err(format!("Failed to spawn pipe for {:?}: {}", cli, e)),
            }
        } else {
            // Existing session — push message and forward.
            {
                let inst = self
                    .instances
                    .get_mut(&id)
                    .ok_or_else(|| format!("Legacy instance for {:?} not found", cli))?;
                inst.chat_messages.push(ChatMessage {
                    role: ChatRole::User,
                    content: prompt.to_string(),
                    tool_name: None,
                });
                inst.live_status = LiveStatus::Thinking;
            }
            let inst = self
                .instances
                .get(&id)
                .ok_or_else(|| format!("Legacy instance for {:?} not found", cli))?;
            if let Some(ref session) = inst.transport_session {
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
        for inst in self.instances.values_mut() {
            inst.pty_parser.set_size(rows, cols);
            if let Some(ref session) = inst.pty_session {
                let _ = session.resize(rows, cols).await;
            }
        }
    }

    // =========================================================================
    // Drain events (call every frame)
    // =========================================================================

    /// Drain events for all instances. Returns `true` if any events were processed.
    pub fn drain_events(&mut self) -> bool {
        let mut had_events = false;
        for inst in self.instances.values_mut() {
            had_events |= Self::drain_one(inst);
        }
        had_events
    }

    fn drain_one(inst: &mut AgentInstance) -> bool {
        let mut had_events = false;

        // Drain PTY events
        if let Some(ref mut rx) = inst.pty_rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        had_events = true;
                        match event {
                            AgentEvent::PtyRaw { data } => {
                                inst.pty_parser.process(&data);
                            }
                            AgentEvent::Exited { .. } => {
                                inst.session_active = false;
                            }
                            _ => {}
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Closed) => {
                        inst.session_active = false;
                        break;
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                }
            }
        }

        // Drain Pipe events
        if let Some(ref mut rx) = inst.pipe_rx {
            loop {
                match rx.try_recv() {
                    Ok(event) => {
                        had_events = true;
                        match event {
                            AgentEvent::SessionStart { session_id, .. } => {
                                // Capture id for --resume on the next prompt.
                                // Do NOT push a ChatMessage — that was the source
                                // of "[tool] unknown · session XXXX" spam.
                                inst.pipe_session_id = Some(session_id);
                            }
                            AgentEvent::Text { text, is_delta: _ } => {
                                // Finalize any RunningTool status: push a "done" history entry.
                                if let LiveStatus::RunningTool { ref name, done } =
                                    inst.live_status.clone()
                                {
                                    inst.chat_messages.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: format!("✓ {} · {} done", name, done),
                                        tool_name: Some(name.clone()),
                                    });
                                }
                                inst.live_status = LiveStatus::Idle;

                                if let Some(last) = inst.chat_messages.last_mut() {
                                    if last.role == ChatRole::Assistant {
                                        last.content.push_str(&text);
                                        continue;
                                    }
                                }
                                inst.chat_messages.push(ChatMessage {
                                    role: ChatRole::Assistant,
                                    content: text,
                                    tool_name: None,
                                });
                            }
                            AgentEvent::ToolStart { name, .. } => {
                                // Finalize previous tool if any.
                                if let LiveStatus::RunningTool {
                                    name: ref prev,
                                    done,
                                } = inst.live_status.clone()
                                {
                                    inst.chat_messages.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: format!("✓ {} · {} done", prev, done),
                                        tool_name: Some(prev.clone()),
                                    });
                                }
                                // Start tracking the new tool.
                                inst.live_status = LiveStatus::RunningTool { name, done: 0 };
                            }
                            AgentEvent::ToolResult {
                                id: _,
                                output: _,
                                is_error: _,
                                ..
                            } => {
                                if let LiveStatus::RunningTool { done, .. } = &mut inst.live_status
                                {
                                    *done = done.saturating_add(1);
                                }
                            }
                            AgentEvent::Thinking { text: _ } => {
                                // Keep Thinking status as-is; suppress bubble noise.
                            }
                            AgentEvent::Error { message } => {
                                inst.live_status = LiveStatus::Idle;
                                inst.chat_messages.push(ChatMessage {
                                    role: ChatRole::Error,
                                    content: message,
                                    tool_name: None,
                                });
                            }
                            AgentEvent::TurnComplete { .. } => {
                                // Finalize any in-flight tool and clear live status.
                                if let LiveStatus::RunningTool { ref name, done } =
                                    inst.live_status.clone()
                                {
                                    inst.chat_messages.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: format!("✓ {} · {} done", name, done),
                                        tool_name: Some(name.clone()),
                                    });
                                }
                                inst.live_status = LiveStatus::Idle;
                            }
                            AgentEvent::SessionEnd {
                                result, is_error, ..
                            } => {
                                if is_error {
                                    inst.chat_messages.push(ChatMessage {
                                        role: ChatRole::Error,
                                        content: format!("Session error · {}", result),
                                        tool_name: None,
                                    });
                                }
                                // Finalize any in-flight tool.
                                if let LiveStatus::RunningTool { ref name, done } =
                                    inst.live_status.clone()
                                {
                                    inst.chat_messages.push(ChatMessage {
                                        role: ChatRole::Tool,
                                        content: format!("✓ {} · {} done", name, done),
                                        tool_name: Some(name.clone()),
                                    });
                                }
                                inst.live_status = LiveStatus::Idle;
                            }
                            AgentEvent::Exited { .. } => {
                                inst.session_active = false;
                                inst.live_status = LiveStatus::Idle;
                            }
                            _ => {}
                        }
                    }
                    Err(broadcast::error::TryRecvError::Empty) => break,
                    Err(broadcast::error::TryRecvError::Closed) => {
                        inst.session_active = false;
                        break;
                    }
                    Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                }
            }
        }

        had_events
    }

    // =========================================================================
    // Snapshot + query (legacy CLI-based)
    // =========================================================================

    /// Build a render snapshot for the given CLI, honoring the UI's requested mode.
    ///
    /// `want_pty=true` → return PTY grid if a PTY parser/session exists (even if exited),
    /// otherwise Idle. `want_pty=false` → return chat messages if any, otherwise Idle.
    /// This decouples snapshot mode from session liveness so mode switches don't
    /// destroy the other view.
    pub fn snapshot_mode(&self, cli: AgentCli, want_pty: bool) -> AgentRenderSnapshot {
        let id = self.legacy_id(cli);
        let inst = match self.instances.get(&id) {
            Some(i) => i,
            None => {
                return AgentRenderSnapshot {
                    mode: AgentSnapshotMode::Idle,
                    session_active: false,
                    live_status: LiveStatus::Idle,
                }
            }
        };
        if want_pty {
            // Render PTY grid as long as we ever spawned one OR the parser has content.
            if inst.pty_session.is_some() || self.instance_pty_has_content(inst) {
                return self.build_pty_snapshot_from_instance(inst);
            }
            return AgentRenderSnapshot {
                mode: AgentSnapshotMode::Idle,
                session_active: inst.session_active,
                live_status: inst.live_status.clone(),
            };
        }
        // Chat mode requested
        if !inst.chat_messages.is_empty() {
            return AgentRenderSnapshot {
                mode: AgentSnapshotMode::Chat(inst.chat_messages.clone()),
                session_active: inst.session_active,
                live_status: inst.live_status.clone(),
            };
        }
        AgentRenderSnapshot {
            mode: AgentSnapshotMode::Idle,
            session_active: inst.session_active,
            live_status: inst.live_status.clone(),
        }
    }

    fn instance_pty_has_content(&self, inst: &AgentInstance) -> bool {
        let screen = inst.pty_parser.screen();
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

    fn build_pty_snapshot_from_instance(&self, inst: &AgentInstance) -> AgentRenderSnapshot {
        let screen = inst.pty_parser.screen();
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
        // Respect DECTCEM. Empirically verified with
        // `cargo run --example pty_cursor_probe -p gate4agent`: Ink-based
        // TUIs like Claude Code
        //   - hide the terminal cursor in steady state and only unhide it
        //     briefly while echoing raw input;
        //   - park the vt100 cursor on whichever cell they are currently
        //     repainting — for Claude that is typically the top-right
        //     buddy ASCII-art animation, NOT the edit caret;
        //   - draw their own fake caret in the framebuffer at the real
        //     input position (reverse video / block glyph).
        // Drawing our own white block at the raw vt100 position therefore
        // causes two visible bugs: (1) the caret drifts into the buddy
        // area when idle, and (2) it sits one cell past the real input
        // caret while typing. Respecting `hide_cursor()` means the visible
        // caret comes entirely from the framebuffer (what Ink drew), and
        // both symptoms disappear.
        grid.cursor_visible = !screen.hide_cursor();
        AgentRenderSnapshot {
            mode: AgentSnapshotMode::Pty(grid),
            session_active: inst.session_active,
            live_status: inst.live_status.clone(),
        }
    }

    /// Legacy snapshot — inferred from state. Prefer `snapshot_mode`.
    pub fn snapshot(&self, cli: AgentCli) -> AgentRenderSnapshot {
        let id = self.legacy_id(cli);
        let inst = match self.instances.get(&id) {
            Some(i) => i,
            None => {
                return AgentRenderSnapshot {
                    mode: AgentSnapshotMode::Idle,
                    session_active: false,
                    live_status: LiveStatus::Idle,
                }
            }
        };

        if !inst.session_active {
            if !inst.chat_messages.is_empty() {
                return AgentRenderSnapshot {
                    mode: AgentSnapshotMode::Chat(inst.chat_messages.clone()),
                    session_active: false,
                    live_status: inst.live_status.clone(),
                };
            }
            return AgentRenderSnapshot {
                mode: AgentSnapshotMode::Idle,
                session_active: false,
                live_status: inst.live_status.clone(),
            };
        }

        if inst.pty_session.is_some() {
            let screen = inst.pty_parser.screen();
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
            // See build_pty_snapshot_from_instance() for the rationale: Ink-based TUIs
            // draw their own fake caret in the framebuffer and park the
            // real vt100 cursor on animation cells, so we must respect
            // DECTCEM to avoid a drifting ghost caret.
            grid.cursor_visible = !screen.hide_cursor();
            // Buddy extraction disabled — heuristic doesn't reliably catch the
            // companion ASCII art yet. Kept in snapshot.rs for future experiments.
            // grid.detect_and_extract_buddy();
            AgentRenderSnapshot {
                mode: AgentSnapshotMode::Pty(grid),
                session_active: true,
                live_status: inst.live_status.clone(),
            }
        } else {
            AgentRenderSnapshot {
                mode: AgentSnapshotMode::Chat(inst.chat_messages.clone()),
                session_active: true,
                live_status: inst.live_status.clone(),
            }
        }
    }

    /// Returns `true` if either PTY or pipe session is alive for this CLI.
    pub fn is_active(&self, cli: AgentCli) -> bool {
        let id = self.legacy_id(cli);
        self.instances
            .get(&id)
            .map(|i| i.session_active)
            .unwrap_or(false)
    }

    /// Returns `true` if any instance has an active session.
    pub fn any_active(&self) -> bool {
        self.instances.values().any(|i| i.session_active)
    }

    /// Number of past sessions for this CLI (live disk read).
    pub fn past_session_count(&self, cli: AgentCli) -> usize {
        let workdir = self.cli_workdir(cli);
        crate::history::reader_for(cli).list_sessions(&workdir).len()
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
        eprintln!(
            "[gate4agent] load_latest_history cli={:?} workdir={}",
            cli,
            workdir.display()
        );
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
        let id = self.legacy_id(cli);
        if let Some(inst) = self.instances.get_mut(&id) {
            inst.chat_messages = messages;
            inst.pipe_session_id = Some(latest.clone());
            inst.transport_session = None;
            inst.pipe_rx = None;
        }
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
        let id = self.legacy_id(cli);
        if let Some(inst) = self.instances.get_mut(&id) {
            inst.chat_messages = messages;
            inst.pipe_session_id = Some(session_id.to_string());
            // Drop any stale live transport handle so the next send_chat goes through
            // the spawn branch and picks up the resume id.
            inst.transport_session = None;
            inst.pipe_rx = None;
        }
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
        AgentCli::Cursor => CliTool::Cursor,
        AgentCli::OpenCode => CliTool::OpenCode,
    }
}

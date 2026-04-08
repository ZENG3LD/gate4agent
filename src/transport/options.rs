/// Options controlling how a CLI agent process is spawned.
///
/// This struct replaces `PipeProcessOptions` + `ClaudeOptions` as the unified
/// spawn configuration carrier. It is tool-agnostic: each CLI builder reads
/// only the fields it understands and ignores the rest.
#[derive(Debug, Clone, Default)]
pub struct SpawnOptions {
    /// Absolute path to the working directory for the spawned process.
    pub working_dir: std::path::PathBuf,

    /// The initial prompt to deliver to the agent.
    pub prompt: String,

    /// Resume a previous CLI-native session by its ID.
    ///
    /// - Claude: `--resume <id>`
    /// - Codex: `exec resume <id>` sub-sub-command
    /// - Gemini: not supported (field is ignored)
    /// - Cursor: `--resume <id>`
    pub resume_session_id: Option<String>,

    /// Model override passed to the CLI.
    ///
    /// - Claude: `--model <model>`
    /// - Cursor: `--model <model>`
    /// - Others: ignored.
    pub model: Option<String>,

    /// Text appended to the system prompt via `--append-system-prompt`.
    ///
    /// - Claude: `--append-system-prompt "<text>"`
    /// - Others: ignored.
    pub append_system_prompt: Option<String>,

    /// Extra CLI arguments appended after all standard flags.
    pub extra_args: Vec<String>,

    /// Environment variables injected into the child process.
    pub env_vars: Vec<(String, String)>,
}

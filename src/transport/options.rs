/// Options controlling how a CLI agent process is spawned.
///
/// This struct replaces `PipeProcessOptions` + `ClaudeOptions` as the unified
/// spawn configuration carrier. It is tool-agnostic: each CLI builder reads
/// only the fields it understands and ignores the rest.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// Absolute path to the working directory for the spawned process.
    pub working_dir: std::path::PathBuf,

    /// The initial prompt to deliver to the agent.
    pub prompt: String,

    /// Resume a previous CLI-native session by its ID.
    ///
    /// - Claude: `--resume <id>`
    /// - Codex: `exec resume <id>` sub-sub-command
    /// - Gemini: `--resume <id>` (pass `"latest"` for most recent session)
    /// - Others: ignored.
    pub resume_session_id: Option<String>,

    /// Model override passed to the CLI.
    ///
    /// - Claude: `--model <model>`
    /// - Codex: `--model <model>`
    /// - OpenCode: `-m <model>`
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

    /// Resume the most recent session without knowing its ID.
    ///
    /// - Claude: `--continue`
    /// - Codex: `exec resume --last`
    /// - OpenCode: `--continue`
    /// - Gemini: NOT supported (use `resume_session_id` with `"latest"` instead).
    ///
    /// Ignored when `resume_session_id` is also set.
    pub continue_last: bool,

    /// Restrict which tools the agent can use.
    ///
    /// - Claude: `--allowedTools Edit,Read,Bash`
    /// - Others: ignored (no equivalent flag).
    pub allowed_tools: Vec<String>,

    /// Permission mode for tool execution.
    ///
    /// - Claude: `--permission-mode accept-all` or `--permission-mode default`
    /// - Others: ignored.
    ///
    /// When set, overrides the default `--dangerously-skip-permissions` flag.
    pub permission_mode: Option<String>,

    /// Path to MCP server configuration file.
    ///
    /// - Claude: `--mcp-config <path>`
    /// - Others: ignored (use their own config files).
    pub mcp_config: Option<std::path::PathBuf>,

    /// Maximum number of agentic turns (auto-loop iterations).
    ///
    /// - Claude: `--max-turns <N>`
    /// - Others: ignored.
    pub max_turns: Option<u32>,

    /// Run tool execution in a sandbox/container.
    ///
    /// - Gemini: `--sandbox`
    /// - Others: ignored.
    pub sandbox: bool,
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self {
            working_dir: std::path::PathBuf::default(),
            prompt: String::new(),
            resume_session_id: None,
            model: None,
            append_system_prompt: None,
            extra_args: Vec::new(),
            env_vars: Vec::new(),
            continue_last: false,
            allowed_tools: Vec::new(),
            permission_mode: None,
            mcp_config: None,
            max_turns: None,
            sandbox: false,
        }
    }
}

use crate::{normalize_executable_name, AdapterBinding, AgentId, AgentReadinessSpec};
use serde::{Deserialize, Serialize};

/// Runtime in which an executable is detected or launched.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RuntimePlatform {
    Windows,
    MacOs,
    Linux,
    Wsl,
}

impl RuntimePlatform {
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::MacOs
        } else {
            Self::Linux
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DetectionSpec {
    pub command: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub required_commands: Vec<String>,
    #[serde(default)]
    pub unsupported_platforms: Vec<RuntimePlatform>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LaunchSpec {
    /// Executable name or absolute path. It is never a shell command string.
    pub program: String,
    #[serde(default)]
    pub fixed_args: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum ProcessMatcher {
    Exact { name: String },
    Prefix { prefix: String },
}

impl ProcessMatcher {
    pub fn matches(&self, process_or_path: &str, platform: RuntimePlatform) -> bool {
        let process = normalize_executable_name(process_or_path, platform);
        match self {
            Self::Exact { name } => {
                let expected = normalize_executable_name(name, platform);
                process == expected
            }
            Self::Prefix { prefix } => {
                let expected = if platform == RuntimePlatform::Windows {
                    prefix.to_ascii_lowercase()
                } else {
                    prefix.clone()
                };
                process.starts_with(&expected)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum InitialPromptMode {
    None,
    Positional { option_terminator: bool },
    Flag { flag: String },
    InteractiveFlag { flag: String },
    AgentNativeQuery,
    AfterReady,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum NativeDraftMode {
    Flag { flag: String },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PromptSpec {
    pub initial: InitialPromptMode,
    #[serde(default)]
    pub native_draft: Option<NativeDraftMode>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AgentCommandMode {
    SlashLine,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentCapabilities {
    #[serde(default)]
    pub agent_commands: Option<AgentCommandMode>,
    #[serde(default)]
    pub transports: AgentTransportCapabilities,
    #[serde(default)]
    pub adapters: AgentAdapterCapabilities,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentAdapterCapabilities {
    #[serde(default)]
    pub hook: Option<AdapterBinding>,
    #[serde(default)]
    pub history: Option<AdapterBinding>,
    #[serde(default)]
    pub resume: Option<AdapterBinding>,
    #[serde(default)]
    pub session_options: Option<AdapterBinding>,
    #[serde(default)]
    pub capability_probe: Option<AdapterBinding>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PipePromptDelivery {
    None,
    StdinClose,
    Positional,
}

/// Optional catalog override used by controlled fixtures and providers whose
/// headless executable differs from their interactive PTY executable.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PipeTransportSpec {
    pub adapter: AdapterBinding,
    #[serde(default)]
    pub launch_override: Option<LaunchSpec>,
    #[serde(default = "default_pipe_prompt_delivery")]
    pub prompt_delivery: PipePromptDelivery,
}

fn default_pipe_prompt_delivery() -> PipePromptDelivery {
    PipePromptDelivery::None
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AcpTransportSpec {
    pub adapter: AdapterBinding,
    #[serde(default)]
    pub launch_override: Option<LaunchSpec>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentTransportCapabilities {
    #[serde(default = "default_true")]
    pub pty: bool,
    #[serde(default)]
    pub pty_adapter: Option<AdapterBinding>,
    #[serde(default)]
    pub pipe: Option<PipeTransportSpec>,
    #[serde(default)]
    pub acp: Option<AcpTransportSpec>,
}

impl Default for AgentTransportCapabilities {
    fn default() -> Self {
        Self {
            pty: true,
            pty_adapter: None,
            pipe: None,
            acp: None,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Provenance state for a built-in launch specification.
///
/// `Reference` means the shape still requires verification against a pinned
/// vendor CLI before a product presents it as fully supported.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SpecVerification {
    Gate4AgentVerified,
    Reference,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentSpec {
    pub id: AgentId,
    /// Revision of the launch/detection contract, not the installed CLI version.
    pub revision: String,
    pub display_name: String,
    pub detection: DetectionSpec,
    pub launch: LaunchSpec,
    pub expected_processes: Vec<ProcessMatcher>,
    pub prompt: PromptSpec,
    pub readiness: AgentReadinessSpec,
    #[serde(default)]
    pub capabilities: AgentCapabilities,
    pub verification: SpecVerification,
}

impl AgentSpec {
    pub fn supports_platform(&self, platform: RuntimePlatform) -> bool {
        !self.detection.unsupported_platforms.contains(&platform)
    }
}

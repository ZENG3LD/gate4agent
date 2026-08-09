use gate4agent_catalog::EnvMutation;
use gate4agent_types::{AgentId, AgentInstanceId, TransportKind};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};
use thiserror::Error;

const NATIVE_LAUNCH_PROFILE_ID_MAX_BYTES: usize = 64;
const NATIVE_LAUNCH_PROFILES_MAX: usize = 512;
const NATIVE_LAUNCH_PROFILE_SELECTIONS_MAX: usize = 512;
const NATIVE_LAUNCH_PROFILE_ENV_MUTATIONS_MAX: usize = 128;
const NATIVE_LAUNCH_PROFILE_ENV_KEY_MAX_BYTES: usize = 1_024;
const NATIVE_LAUNCH_PROFILE_ENV_VALUE_MAX_BYTES: usize = 65_536;
const NATIVE_LAUNCH_PROFILE_ENV_TOTAL_MAX_BYTES: usize = 1_048_576;
const NATIVE_INSTANCE_LAUNCH_ARGS_MAX: usize = 128;
const NATIVE_INSTANCE_LAUNCH_ARG_MAX_BYTES: usize = 65_536;
const NATIVE_INSTANCE_LAUNCH_ARGS_TOTAL_MAX_BYTES: usize = 262_144;
const RESERVED_HOOK_ENV_PREFIX: &str = "GATE4AGENT_HOOK_";
const RESERVED_CLAUDE_LAUNCH_FLAGS: &[&str] = &[
    "--continue",
    "--print",
    "--prompt",
    "--prompt-interactive",
    "--resume",
    "--session-id",
    "-c",
    "-p",
    "-r",
];

pub const ZAI_GLM_CLAUDE_PROFILE_ID: &str = "zai-glm";
pub const ZAI_GLM_CLAUDE_PROFILE_REVISION: &str = "zai-claude-env-2026-07-21";
pub const ZAI_GLM_ANTHROPIC_BASE_URL: &str = "https://api.z.ai/api/anthropic";
pub const ZAI_GLM_CLAUDE_REQUIRED_ENV_KEYS: &[&str] =
    &["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_BASE_URL"];
pub const ZAI_GLM_CLAUDE_OPTIONAL_ENV_KEYS: &[&str] = &[
    "API_TIMEOUT_MS",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
];
pub const ZAI_GLM_CLAUDE_OWNED_ENV_KEYS: &[&str] = &[
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
    "API_TIMEOUT_MS",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "CLAUDE_CODE_AUTO_COMPACT_WINDOW",
    "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
];

/// Revisioned non-secret descriptor for a built-in native launch profile.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeLaunchProfileDescriptor {
    id: &'static str,
    revision: &'static str,
    agent_id: &'static str,
    transport: TransportKind,
    required_env_keys: &'static [&'static str],
    optional_env_keys: &'static [&'static str],
    owned_env_keys: &'static [&'static str],
    contract: NativeLaunchProfileContract,
}

impl NativeLaunchProfileDescriptor {
    pub const fn id(&self) -> &'static str {
        self.id
    }

    pub const fn revision(&self) -> &'static str {
        self.revision
    }

    pub const fn agent_id(&self) -> &'static str {
        self.agent_id
    }

    pub const fn transport(&self) -> TransportKind {
        self.transport
    }

    pub const fn required_env_keys(&self) -> &'static [&'static str] {
        self.required_env_keys
    }

    pub const fn optional_env_keys(&self) -> &'static [&'static str] {
        self.optional_env_keys
    }

    pub const fn owned_env_keys(&self) -> &'static [&'static str] {
        self.owned_env_keys
    }

    pub fn instantiate(
        &self,
        resolver: Arc<dyn NativeChildEnvironmentResolver>,
    ) -> Result<NativeLaunchProfile, NativeLaunchProfileError> {
        let id = NativeLaunchProfileId::new(self.id)
            .expect("built-in native launch profile ID must be valid");
        let agent_id = AgentId::new(self.agent_id)
            .expect("built-in native launch profile agent ID must be valid");
        NativeLaunchProfile::new_with_contract(
            id,
            agent_id,
            self.transport,
            self.owned_env_keys.iter().map(OsString::from).collect(),
            resolver,
            self.contract,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeLaunchProfileContract {
    ExactOwnership,
    ZaiGlmClaude,
}

impl NativeLaunchProfileContract {
    fn validate(self, environment: &[EnvMutation]) -> Result<(), NativeLaunchProfileError> {
        match self {
            Self::ExactOwnership => Ok(()),
            Self::ZaiGlmClaude => validate_zai_glm_claude_environment(environment),
        }
    }
}

/// Z.AI GLM Coding Plan over the installed Claude Code CLI.
///
/// The key inventory follows the official Z.AI Claude Code environment example
/// reviewed on 2026-07-21. Only the token and base URL are required; a resolver
/// must still return every owned key and use `None` for optional values it does
/// not set. No model value is embedded because the current upstream example is
/// internally inconsistent and model selection remains host configuration.
pub const ZAI_GLM_CLAUDE_PROFILE: NativeLaunchProfileDescriptor =
    NativeLaunchProfileDescriptor {
        id: ZAI_GLM_CLAUDE_PROFILE_ID,
        revision: ZAI_GLM_CLAUDE_PROFILE_REVISION,
        agent_id: "claude",
        transport: TransportKind::Pty,
        required_env_keys: ZAI_GLM_CLAUDE_REQUIRED_ENV_KEYS,
        optional_env_keys: ZAI_GLM_CLAUDE_OPTIONAL_ENV_KEYS,
        owned_env_keys: ZAI_GLM_CLAUDE_OWNED_ENV_KEYS,
        contract: NativeLaunchProfileContract::ZaiGlmClaude,
    };

/// Host-local identifier for a non-wire native launch profile.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct NativeLaunchProfileId(String);

impl NativeLaunchProfileId {
    pub fn new(value: impl Into<String>) -> Result<Self, NativeLaunchProfileError> {
        let value = value.into();
        if value.is_empty() {
            return Err(NativeLaunchProfileError::EmptyId);
        }
        if value.len() > NATIVE_LAUNCH_PROFILE_ID_MAX_BYTES {
            return Err(NativeLaunchProfileError::IdTooLong {
                len: value.len(),
                max: NATIVE_LAUNCH_PROFILE_ID_MAX_BYTES,
            });
        }
        if !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
        }) || matches!(value.as_bytes().first(), Some(b'-' | b'_'))
            || matches!(value.as_bytes().last(), Some(b'-' | b'_'))
        {
            return Err(NativeLaunchProfileError::InvalidId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for NativeLaunchProfileId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Host-owned child-process environment policy, absent from the wire protocol.
///
/// The profile retains an opaque resolver rather than resolved values and does
/// not implement `Debug`, preventing routine diagnostics from exposing child
/// environment material.
#[derive(Clone)]
pub struct NativeLaunchProfile {
    id: NativeLaunchProfileId,
    agent_id: AgentId,
    transport: TransportKind,
    owned_env_keys: Vec<OsString>,
    resolver: Arc<dyn NativeChildEnvironmentResolver>,
    contract: NativeLaunchProfileContract,
}

/// Host-only launch mutations applied to one exact native PTY instance.
///
/// The overlay does not implement `Debug` because environment values and argv
/// may contain secret material. It is validated and retained without crossing
/// the wire or durable state boundaries.
pub struct NativeInstanceLaunchOverlay {
    agent_id: AgentId,
    transport: TransportKind,
    environment: Vec<EnvMutation>,
    extra_args: Vec<OsString>,
}

impl NativeInstanceLaunchOverlay {
    pub fn new(
        agent_id: AgentId,
        transport: TransportKind,
        environment: Vec<EnvMutation>,
        extra_args: Vec<OsString>,
    ) -> Result<Self, NativeLaunchProfileError> {
        if transport != TransportKind::Pty {
            return Err(NativeLaunchProfileError::InstanceOverlayUnsupportedTransport);
        }
        validate_environment_mutations(&environment)?;
        validate_launch_arguments(&agent_id, &extra_args)?;
        Ok(Self {
            agent_id,
            transport,
            environment,
            extra_args,
        })
    }
}

/// Backward-compatible environment-only overlay used by existing node flows.
///
/// This type also intentionally omits `Debug`. New PTY feature bundles should
/// use [`NativeInstanceLaunchOverlay`] when they need child argv.
pub struct NativeLaunchEnvironmentOverlay {
    agent_id: AgentId,
    transport: TransportKind,
    environment: Vec<EnvMutation>,
}

impl NativeLaunchEnvironmentOverlay {
    pub fn new(
        agent_id: AgentId,
        transport: TransportKind,
        environment: Vec<EnvMutation>,
    ) -> Result<Self, NativeLaunchProfileError> {
        if !matches!(transport, TransportKind::Pty | TransportKind::Pipe) {
            return Err(NativeLaunchProfileError::UnsupportedTransport);
        }
        validate_environment_mutations(&environment)?;
        Ok(Self {
            agent_id,
            transport,
            environment,
        })
    }

    fn into_instance_overlay(self) -> NativeInstanceLaunchOverlay {
        NativeInstanceLaunchOverlay {
            agent_id: self.agent_id,
            transport: self.transport,
            environment: self.environment,
            extra_args: Vec::new(),
        }
    }
}

impl NativeLaunchProfile {
    pub fn new(
        id: NativeLaunchProfileId,
        agent_id: AgentId,
        transport: TransportKind,
        owned_env_keys: Vec<OsString>,
        resolver: Arc<dyn NativeChildEnvironmentResolver>,
    ) -> Result<Self, NativeLaunchProfileError> {
        Self::new_with_contract(
            id,
            agent_id,
            transport,
            owned_env_keys,
            resolver,
            NativeLaunchProfileContract::ExactOwnership,
        )
    }

    fn new_with_contract(
        id: NativeLaunchProfileId,
        agent_id: AgentId,
        transport: TransportKind,
        owned_env_keys: Vec<OsString>,
        resolver: Arc<dyn NativeChildEnvironmentResolver>,
        contract: NativeLaunchProfileContract,
    ) -> Result<Self, NativeLaunchProfileError> {
        if !matches!(transport, TransportKind::Pty | TransportKind::Pipe) {
            return Err(NativeLaunchProfileError::UnsupportedTransport);
        }
        validate_owned_environment_keys(&owned_env_keys)?;
        Ok(Self {
            id,
            agent_id,
            transport,
            owned_env_keys,
            resolver,
            contract,
        })
    }

    pub fn id(&self) -> &NativeLaunchProfileId {
        &self.id
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn transport(&self) -> TransportKind {
        self.transport
    }

    fn resolve_environment(
        &self,
        agent_id: &AgentId,
        transport: TransportKind,
    ) -> Result<Vec<EnvMutation>, NativeLaunchProfileError> {
        if &self.agent_id != agent_id || self.transport != transport {
            return Err(NativeLaunchProfileError::BindingMismatch);
        }
        let environment = self
            .resolver
            .resolve_child_environment()
            .map_err(NativeLaunchProfileError::Resolve)?;
        validate_resolved_environment(&self.owned_env_keys, &environment)?;
        self.contract.validate(&environment)?;
        Ok(environment)
    }
}

/// Resolves an exact, declared environment overlay at native spawn dispatch.
///
/// Implementations should retain references to secret storage and resolve
/// values only when called; resolver implementations must not expose them via
/// `Debug` or error text.
pub trait NativeChildEnvironmentResolver: Send + Sync + 'static {
    fn resolve_child_environment(
        &self,
    ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError>;
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeChildEnvironmentResolveError {
    #[error("native child environment is temporarily unavailable")]
    TemporarilyUnavailable,
    #[error("native child environment resolution was denied")]
    Denied,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum NativeLaunchProfileError {
    #[error("native launch profile ID must not be empty")]
    EmptyId,
    #[error("native launch profile ID is {len} bytes; maximum is {max}")]
    IdTooLong { len: usize, max: usize },
    #[error("native launch profile ID must be a lowercase ASCII slug")]
    InvalidId,
    #[error("native launch profiles support PTY and exact OneShotText Pipe transports only")]
    UnsupportedTransport,
    #[error("native launch profile must own at least one environment key")]
    EmptyEnvironmentOwnership,
    #[error("native launch profile has {count} environment mutations; maximum is {max}")]
    TooManyEnvironmentMutations { count: usize, max: usize },
    #[error("native launch profile environment mutation {index} has an invalid key")]
    InvalidEnvironmentKey { index: usize },
    #[error("native launch profile environment mutation {index} uses a reserved hook key")]
    ReservedHookEnvironmentKey { index: usize },
    #[error("native launch profile environment mutation {index} duplicates an earlier key")]
    DuplicateEnvironmentKey { index: usize },
    #[error("native launch profile environment mutation {index} value exceeds {max} bytes")]
    EnvironmentValueTooLong { index: usize, max: usize },
    #[error("native launch profile environment mutation {index} has an invalid value")]
    InvalidEnvironmentValue { index: usize },
    #[error("native launch profile environment payload exceeds {max} bytes")]
    EnvironmentPayloadTooLarge { max: usize },
    #[error("native launch profile resolver returned a key outside its exact ownership set")]
    EnvironmentOwnershipMismatch,
    #[error("native launch profile resolver omitted a required environment value")]
    RequiredEnvironmentValueMissing,
    #[error("native launch profile resolver returned an invalid fixed environment value")]
    FixedEnvironmentValueMismatch,
    #[error("native launch profile capacity is {max}")]
    ProfileCapacityExceeded { max: usize },
    #[error("native launch profile '{profile_id}' is not installed")]
    UnknownProfile { profile_id: NativeLaunchProfileId },
    #[error("native launch profile does not match the exact agent and transport binding")]
    BindingMismatch,
    #[error("native instance launch overlay supports PTY transport only")]
    InstanceOverlayUnsupportedTransport,
    #[error("native launch profile selection capacity is {max}")]
    SelectionCapacityExceeded { max: usize },
    #[error("native launch environment overlay requires an existing profile selection")]
    EnvironmentOverlaySelectionMissing,
    #[error("native launch environment overlay does not match the selected profile binding")]
    EnvironmentOverlayBindingMismatch,
    #[error("native launch environment overlay conflicts with a selected profile-owned key")]
    EnvironmentOverlayKeyConflict,
    #[error("native launch environment overlay capacity is {max}")]
    EnvironmentOverlayCapacityExceeded { max: usize },
    #[error("native instance launch overlay has {count} arguments; maximum is {max}")]
    TooManyLaunchArguments { count: usize, max: usize },
    #[error("native instance launch overlay argument {index} is invalid")]
    InvalidLaunchArgument { index: usize },
    #[error("native instance launch overlay argument {index} exceeds {max} bytes")]
    LaunchArgumentTooLong { index: usize, max: usize },
    #[error("native instance launch overlay argument payload exceeds {max} bytes")]
    LaunchArgumentsPayloadTooLarge { max: usize },
    #[error("native instance launch overlay argument {index} conflicts with Claude session, resume, or prompt authority")]
    ReservedClaudeLaunchArgument { index: usize },
    #[error("native launch profile is selected by an instance; clear the selection first")]
    ProfileInUse,
    #[error(transparent)]
    Resolve(#[from] NativeChildEnvironmentResolveError),
}

pub(crate) struct NativeLaunchProfiles {
    profiles: BTreeMap<NativeLaunchProfileId, NativeLaunchProfile>,
    selections: BTreeMap<AgentInstanceId, NativeLaunchProfileId>,
    instance_overlays: BTreeMap<AgentInstanceId, Arc<NativeInstanceLaunchOverlay>>,
}

impl NativeLaunchProfiles {
    pub(crate) fn new() -> Self {
        Self {
            profiles: BTreeMap::new(),
            selections: BTreeMap::new(),
            instance_overlays: BTreeMap::new(),
        }
    }

    pub(crate) fn upsert(
        &mut self,
        profile: NativeLaunchProfile,
    ) -> Result<(), NativeLaunchProfileError> {
        if !self.profiles.contains_key(profile.id())
            && self.profiles.len() >= NATIVE_LAUNCH_PROFILES_MAX
        {
            return Err(NativeLaunchProfileError::ProfileCapacityExceeded {
                max: NATIVE_LAUNCH_PROFILES_MAX,
            });
        }
        for (instance_id, selected_profile_id) in &self.selections {
            if selected_profile_id == profile.id() {
                if let Some(overlay) = self.instance_overlays.get(instance_id) {
                    validate_instance_overlay_binding(&profile, overlay)?;
                }
            }
        }
        self.profiles.insert(profile.id.clone(), profile);
        Ok(())
    }

    pub(crate) fn remove(
        &mut self,
        profile_id: &NativeLaunchProfileId,
    ) -> Result<bool, NativeLaunchProfileError> {
        if self
            .selections
            .values()
            .any(|selected_id| selected_id == profile_id)
        {
            return Err(NativeLaunchProfileError::ProfileInUse);
        }
        Ok(self.profiles.remove(profile_id).is_some())
    }

    pub(crate) fn select(
        &mut self,
        instance_id: AgentInstanceId,
        profile_id: NativeLaunchProfileId,
    ) -> Result<(), NativeLaunchProfileError> {
        let profile = self.profiles.get(&profile_id).ok_or_else(|| {
            NativeLaunchProfileError::UnknownProfile {
                profile_id: profile_id.clone(),
            }
        })?;
        if let Some(overlay) = self.instance_overlays.get(&instance_id) {
            validate_instance_overlay_binding(profile, overlay)?;
        }
        if !self.selections.contains_key(&instance_id)
            && self.selections.len() >= NATIVE_LAUNCH_PROFILE_SELECTIONS_MAX
        {
            return Err(NativeLaunchProfileError::SelectionCapacityExceeded {
                max: NATIVE_LAUNCH_PROFILE_SELECTIONS_MAX,
            });
        }
        self.selections.insert(instance_id, profile_id);
        Ok(())
    }

    pub(crate) fn clear_selection(&mut self, instance_id: AgentInstanceId) -> bool {
        let selection_removed = self.selections.remove(&instance_id).is_some();
        let overlay_removed = self.instance_overlays.remove(&instance_id).is_some();
        selection_removed || overlay_removed
    }

    pub(crate) fn install_environment_overlay(
        &mut self,
        instance_id: AgentInstanceId,
        overlay: NativeLaunchEnvironmentOverlay,
    ) -> Result<(), NativeLaunchProfileError> {
        if !self.selections.contains_key(&instance_id) {
            return Err(NativeLaunchProfileError::EnvironmentOverlaySelectionMissing);
        }
        self.install_instance_overlay(instance_id, overlay.into_instance_overlay())
    }

    pub(crate) fn install_instance_overlay(
        &mut self,
        instance_id: AgentInstanceId,
        overlay: NativeInstanceLaunchOverlay,
    ) -> Result<(), NativeLaunchProfileError> {
        if let Some(profile_id) = self.selections.get(&instance_id) {
            let profile = self.profiles.get(profile_id).ok_or_else(|| {
                NativeLaunchProfileError::UnknownProfile {
                    profile_id: profile_id.clone(),
                }
            })?;
            validate_instance_overlay_binding(profile, &overlay)?;
        } else if !overlay.environment.is_empty() {
            return Err(NativeLaunchProfileError::EnvironmentOverlaySelectionMissing);
        }
        if !self.instance_overlays.contains_key(&instance_id)
            && self.instance_overlays.len() >= NATIVE_LAUNCH_PROFILE_SELECTIONS_MAX
        {
            return Err(
                NativeLaunchProfileError::EnvironmentOverlayCapacityExceeded {
                    max: NATIVE_LAUNCH_PROFILE_SELECTIONS_MAX,
                },
            );
        }
        self.instance_overlays
            .insert(instance_id, Arc::new(overlay));
        Ok(())
    }

    pub(crate) fn clear_instance_overlay(&mut self, instance_id: AgentInstanceId) -> bool {
        self.instance_overlays.remove(&instance_id).is_some()
    }

    fn profile_for_spawn(
        &self,
        instance_id: AgentInstanceId,
        agent_id: &AgentId,
        transport: TransportKind,
        pipe_binding_is_exact_one_shot: bool,
    ) -> Result<Option<NativeLaunchSpawnEnvironment>, NativeLaunchProfileError> {
        let overlay = self.instance_overlays.get(&instance_id).cloned();
        let profile = if let Some(profile_id) = self.selections.get(&instance_id) {
            let profile = self
                .profiles
                .get(profile_id)
                .ok_or_else(|| NativeLaunchProfileError::UnknownProfile {
                    profile_id: profile_id.clone(),
                })?;
            if profile.agent_id() != agent_id || profile.transport() != transport {
                return Err(NativeLaunchProfileError::BindingMismatch);
            }
            if transport == TransportKind::Pipe && !pipe_binding_is_exact_one_shot {
                return Err(NativeLaunchProfileError::UnsupportedTransport);
            }
            if let Some(overlay) = &overlay {
                validate_instance_overlay_binding(profile, overlay)?;
            }
            Some(profile.clone())
        } else {
            None
        };
        if let Some(overlay) = &overlay {
            validate_instance_overlay_spawn_binding(agent_id, transport, overlay)?;
            if profile.is_none() && !overlay.environment.is_empty() {
                return Err(NativeLaunchProfileError::EnvironmentOverlaySelectionMissing);
            }
        }
        if profile.is_none() && overlay.is_none() {
            return Ok(None);
        }
        Ok(Some(NativeLaunchSpawnEnvironment {
            profile,
            overlay,
        }))
    }
}

struct NativeLaunchSpawnEnvironment {
    profile: Option<NativeLaunchProfile>,
    overlay: Option<Arc<NativeInstanceLaunchOverlay>>,
}

pub(crate) struct ResolvedNativeLaunchOverlay {
    pub(crate) environment: Vec<EnvMutation>,
    pub(crate) extra_args: Vec<OsString>,
}

/// Clonable host-local control for selecting profiles without mutable runtime access.
///
/// The handle intentionally does not implement `Debug`: its shared registry retains
/// opaque environment resolvers that may own secret material.
#[derive(Clone)]
pub struct NativeLaunchProfileControl {
    launch_profiles: Arc<Mutex<NativeLaunchProfiles>>,
}

impl NativeLaunchProfileControl {
    pub(crate) fn new() -> Self {
        Self {
            launch_profiles: Arc::new(Mutex::new(NativeLaunchProfiles::new())),
        }
    }

    pub(crate) fn upsert(
        &self,
        profile: NativeLaunchProfile,
    ) -> Result<(), NativeLaunchProfileError> {
        self.lock().upsert(profile)
    }

    pub(crate) fn remove(
        &self,
        profile_id: &NativeLaunchProfileId,
    ) -> Result<bool, NativeLaunchProfileError> {
        self.lock().remove(profile_id)
    }

    /// Selects a profile for future spawns of one exact instance.
    ///
    /// Spawn dispatch snapshots the selection under the same registry lock, so
    /// a selection change does not alter a child that was already dispatched.
    pub fn select_native_launch_profile(
        &self,
        instance_id: AgentInstanceId,
        profile_id: NativeLaunchProfileId,
    ) -> Result<(), NativeLaunchProfileError> {
        self.lock().select(instance_id, profile_id)
    }

    /// Clears one instance selection for future spawns only.
    pub fn clear_native_launch_profile_selection(&self, instance_id: AgentInstanceId) -> bool {
        self.lock().clear_selection(instance_id)
    }

    /// Installs or replaces one host-only environment overlay for an exact selection.
    pub fn install_native_launch_environment_overlay(
        &self,
        instance_id: AgentInstanceId,
        overlay: NativeLaunchEnvironmentOverlay,
    ) -> Result<(), NativeLaunchProfileError> {
        self.lock().install_environment_overlay(instance_id, overlay)
    }

    /// Installs or replaces one host-only PTY launch overlay for an exact instance.
    ///
    /// An argv-only overlay does not require an environment profile selection.
    /// Environment mutations retain the existing exact-selection requirement.
    pub fn install_native_instance_launch_overlay(
        &self,
        instance_id: AgentInstanceId,
        overlay: NativeInstanceLaunchOverlay,
    ) -> Result<(), NativeLaunchProfileError> {
        self.lock().install_instance_overlay(instance_id, overlay)
    }

    /// Clears one host-only instance launch overlay for future spawns only.
    pub fn clear_native_instance_launch_overlay(&self, instance_id: AgentInstanceId) -> bool {
        self.lock().clear_instance_overlay(instance_id)
    }

    pub(crate) fn resolve_launch_overlay(
        &self,
        instance_id: AgentInstanceId,
        agent_id: &AgentId,
        transport: TransportKind,
        pipe_binding_is_exact_one_shot: bool,
    ) -> Result<ResolvedNativeLaunchOverlay, NativeLaunchProfileError> {
        let spawn_environment = {
            self.lock().profile_for_spawn(
                instance_id,
                agent_id,
                transport,
                pipe_binding_is_exact_one_shot,
            )?
        };
        let Some(spawn_environment) = spawn_environment else {
            return Ok(ResolvedNativeLaunchOverlay {
                environment: Vec::new(),
                extra_args: Vec::new(),
            });
        };
        let mut environment = if let Some(profile) = spawn_environment.profile {
            profile.resolve_environment(agent_id, transport)?
        } else {
            Vec::new()
        };
        let mut extra_args = Vec::new();
        if let Some(overlay) = spawn_environment.overlay {
            environment.extend(overlay.environment.iter().cloned());
            extra_args.extend(overlay.extra_args.iter().cloned());
        }
        validate_environment_mutations(&environment)?;
        validate_launch_arguments(agent_id, &extra_args)?;
        Ok(ResolvedNativeLaunchOverlay {
            environment,
            extra_args,
        })
    }

    fn lock(&self) -> MutexGuard<'_, NativeLaunchProfiles> {
        self.launch_profiles
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn validate_owned_environment_keys(keys: &[OsString]) -> Result<(), NativeLaunchProfileError> {
    if keys.is_empty() {
        return Err(NativeLaunchProfileError::EmptyEnvironmentOwnership);
    }
    if keys.len() > NATIVE_LAUNCH_PROFILE_ENV_MUTATIONS_MAX {
        return Err(NativeLaunchProfileError::TooManyEnvironmentMutations {
            count: keys.len(),
            max: NATIVE_LAUNCH_PROFILE_ENV_MUTATIONS_MAX,
        });
    }
    let mut normalized_keys = BTreeSet::new();
    for (index, key) in keys.iter().enumerate() {
        let Some(key) = key.to_str() else {
            return Err(NativeLaunchProfileError::InvalidEnvironmentKey { index });
        };
        if key.is_empty()
            || key.len() > NATIVE_LAUNCH_PROFILE_ENV_KEY_MAX_BYTES
            || key.contains(['\0', '='])
        {
            return Err(NativeLaunchProfileError::InvalidEnvironmentKey { index });
        }
        let normalized_key = key.to_ascii_uppercase();
        if normalized_key.starts_with(RESERVED_HOOK_ENV_PREFIX) {
            return Err(NativeLaunchProfileError::ReservedHookEnvironmentKey { index });
        }
        if !normalized_keys.insert(normalized_key) {
            return Err(NativeLaunchProfileError::DuplicateEnvironmentKey { index });
        }
    }
    Ok(())
}

fn validate_resolved_environment(
    owned_keys: &[OsString],
    environment: &[EnvMutation],
) -> Result<(), NativeLaunchProfileError> {
    if owned_keys.len() != environment.len() {
        return Err(NativeLaunchProfileError::EnvironmentOwnershipMismatch);
    }
    validate_owned_environment_keys(
        &environment
            .iter()
            .map(|mutation| mutation.key.clone())
            .collect::<Vec<_>>(),
    )?;
    let owned_keys = owned_keys.iter().cloned().collect::<BTreeSet<_>>();
    let resolved_keys = environment
        .iter()
        .map(|mutation| mutation.key.clone())
        .collect::<BTreeSet<_>>();
    if owned_keys != resolved_keys {
        return Err(NativeLaunchProfileError::EnvironmentOwnershipMismatch);
    }
    validate_environment_mutations(environment)
}

fn validate_environment_mutations(
    environment: &[EnvMutation],
) -> Result<(), NativeLaunchProfileError> {
    if environment.len() > NATIVE_LAUNCH_PROFILE_ENV_MUTATIONS_MAX {
        return Err(NativeLaunchProfileError::TooManyEnvironmentMutations {
            count: environment.len(),
            max: NATIVE_LAUNCH_PROFILE_ENV_MUTATIONS_MAX,
        });
    }
    let mut normalized_keys = BTreeSet::new();
    let mut total_bytes = 0usize;
    for (index, mutation) in environment.iter().enumerate() {
        let Some(key) = mutation.key.to_str() else {
            return Err(NativeLaunchProfileError::InvalidEnvironmentKey { index });
        };
        if key.is_empty()
            || key.len() > NATIVE_LAUNCH_PROFILE_ENV_KEY_MAX_BYTES
            || key.contains(['\0', '='])
        {
            return Err(NativeLaunchProfileError::InvalidEnvironmentKey { index });
        }
        let normalized_key = key.to_ascii_uppercase();
        if normalized_key.starts_with(RESERVED_HOOK_ENV_PREFIX) {
            return Err(NativeLaunchProfileError::ReservedHookEnvironmentKey { index });
        }
        if !normalized_keys.insert(normalized_key) {
            return Err(NativeLaunchProfileError::DuplicateEnvironmentKey { index });
        }
        total_bytes = total_bytes.saturating_add(os_string_bytes(&mutation.key));
        if let Some(value) = &mutation.value {
            let value_bytes = os_string_bytes(value);
            if contains_nul(value) {
                return Err(NativeLaunchProfileError::InvalidEnvironmentValue { index });
            }
            if value_bytes > NATIVE_LAUNCH_PROFILE_ENV_VALUE_MAX_BYTES {
                return Err(NativeLaunchProfileError::EnvironmentValueTooLong {
                    index,
                    max: NATIVE_LAUNCH_PROFILE_ENV_VALUE_MAX_BYTES,
                });
            }
            total_bytes = total_bytes.saturating_add(value_bytes);
        }
        if total_bytes > NATIVE_LAUNCH_PROFILE_ENV_TOTAL_MAX_BYTES {
            return Err(NativeLaunchProfileError::EnvironmentPayloadTooLarge {
                max: NATIVE_LAUNCH_PROFILE_ENV_TOTAL_MAX_BYTES,
            });
        }
    }
    Ok(())
}

fn validate_instance_overlay_binding(
    profile: &NativeLaunchProfile,
    overlay: &NativeInstanceLaunchOverlay,
) -> Result<(), NativeLaunchProfileError> {
    validate_instance_overlay_spawn_binding(profile.agent_id(), profile.transport(), overlay)?;
    let profile_keys = profile
        .owned_env_keys
        .iter()
        .filter_map(|key| key.to_str())
        .map(str::to_ascii_uppercase)
        .collect::<BTreeSet<_>>();
    if overlay.environment.iter().any(|mutation| {
        mutation
            .key
            .to_str()
            .is_some_and(|key| profile_keys.contains(&key.to_ascii_uppercase()))
    }) {
        return Err(NativeLaunchProfileError::EnvironmentOverlayKeyConflict);
    }
    Ok(())
}

fn validate_instance_overlay_spawn_binding(
    agent_id: &AgentId,
    transport: TransportKind,
    overlay: &NativeInstanceLaunchOverlay,
) -> Result<(), NativeLaunchProfileError> {
    if agent_id != &overlay.agent_id || transport != overlay.transport {
        return Err(NativeLaunchProfileError::EnvironmentOverlayBindingMismatch);
    }
    Ok(())
}

fn validate_launch_arguments(
    agent_id: &AgentId,
    arguments: &[OsString],
) -> Result<(), NativeLaunchProfileError> {
    if arguments.len() > NATIVE_INSTANCE_LAUNCH_ARGS_MAX {
        return Err(NativeLaunchProfileError::TooManyLaunchArguments {
            count: arguments.len(),
            max: NATIVE_INSTANCE_LAUNCH_ARGS_MAX,
        });
    }
    let mut total_bytes = 0usize;
    for (index, argument) in arguments.iter().enumerate() {
        if contains_nul(argument) {
            return Err(NativeLaunchProfileError::InvalidLaunchArgument { index });
        }
        let argument_bytes = os_string_bytes(argument);
        if argument_bytes > NATIVE_INSTANCE_LAUNCH_ARG_MAX_BYTES {
            return Err(NativeLaunchProfileError::LaunchArgumentTooLong {
                index,
                max: NATIVE_INSTANCE_LAUNCH_ARG_MAX_BYTES,
            });
        }
        total_bytes = total_bytes.saturating_add(argument_bytes);
        if total_bytes > NATIVE_INSTANCE_LAUNCH_ARGS_TOTAL_MAX_BYTES {
            return Err(NativeLaunchProfileError::LaunchArgumentsPayloadTooLarge {
                max: NATIVE_INSTANCE_LAUNCH_ARGS_TOTAL_MAX_BYTES,
            });
        }
        if agent_id.as_str() == "claude" && is_reserved_claude_launch_argument(argument) {
            return Err(NativeLaunchProfileError::ReservedClaudeLaunchArgument { index });
        }
    }
    Ok(())
}

fn is_reserved_claude_launch_argument(argument: &OsStr) -> bool {
    let Some(argument) = argument.to_str() else {
        return false;
    };
    RESERVED_CLAUDE_LAUNCH_FLAGS.iter().any(|reserved| {
        argument == *reserved
            || (reserved.starts_with("--")
                && argument
                    .strip_prefix(reserved)
                    .is_some_and(|suffix| suffix.starts_with('=')))
            || (reserved.len() == 2
                && argument
                    .strip_prefix(reserved)
                    .is_some_and(|suffix| !suffix.is_empty() && !suffix.starts_with('-')))
    })
}

fn validate_zai_glm_claude_environment(
    environment: &[EnvMutation],
) -> Result<(), NativeLaunchProfileError> {
    let token = environment
        .iter()
        .find(|mutation| mutation.key == OsStr::new("ANTHROPIC_AUTH_TOKEN"))
        .and_then(|mutation| mutation.value.as_deref())
        .and_then(OsStr::to_str)
        .filter(|value| !value.trim().is_empty());
    if token.is_none() {
        return Err(NativeLaunchProfileError::RequiredEnvironmentValueMissing);
    }

    let endpoint = environment
        .iter()
        .find(|mutation| mutation.key == OsStr::new("ANTHROPIC_BASE_URL"))
        .and_then(|mutation| mutation.value.as_deref());
    if endpoint != Some(OsStr::new(ZAI_GLM_ANTHROPIC_BASE_URL)) {
        return Err(NativeLaunchProfileError::FixedEnvironmentValueMismatch);
    }
    Ok(())
}

fn os_string_bytes(value: &OsStr) -> usize {
    value.to_string_lossy().len()
}

fn contains_nul(value: &OsStr) -> bool {
    value.to_string_lossy().contains('\0')
}

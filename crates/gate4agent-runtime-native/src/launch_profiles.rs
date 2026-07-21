use gate4agent_catalog::EnvMutation;
use gate4agent_types::{AgentId, AgentInstanceId, TransportKind};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::sync::Arc;
use thiserror::Error;

const NATIVE_LAUNCH_PROFILE_ID_MAX_BYTES: usize = 64;
const NATIVE_LAUNCH_PROFILES_MAX: usize = 512;
const NATIVE_LAUNCH_PROFILE_SELECTIONS_MAX: usize = 512;
const NATIVE_LAUNCH_PROFILE_ENV_MUTATIONS_MAX: usize = 128;
const NATIVE_LAUNCH_PROFILE_ENV_KEY_MAX_BYTES: usize = 1_024;
const NATIVE_LAUNCH_PROFILE_ENV_VALUE_MAX_BYTES: usize = 65_536;
const NATIVE_LAUNCH_PROFILE_ENV_TOTAL_MAX_BYTES: usize = 1_048_576;
const RESERVED_HOOK_ENV_PREFIX: &str = "GATE4AGENT_HOOK_";

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
pub struct NativeLaunchProfile {
    id: NativeLaunchProfileId,
    agent_id: AgentId,
    transport: TransportKind,
    owned_env_keys: Vec<OsString>,
    resolver: Arc<dyn NativeChildEnvironmentResolver>,
    contract: NativeLaunchProfileContract,
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
        if transport != TransportKind::Pty {
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
    #[error("native launch profiles currently support PTY transport only")]
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
    #[error("native launch profile selection capacity is {max}")]
    SelectionCapacityExceeded { max: usize },
    #[error("native launch profile is selected by an instance; clear the selection first")]
    ProfileInUse,
    #[error(transparent)]
    Resolve(#[from] NativeChildEnvironmentResolveError),
}

pub(crate) struct NativeLaunchProfiles {
    profiles: BTreeMap<NativeLaunchProfileId, NativeLaunchProfile>,
    selections: BTreeMap<AgentInstanceId, NativeLaunchProfileId>,
}

impl NativeLaunchProfiles {
    pub(crate) fn new() -> Self {
        Self {
            profiles: BTreeMap::new(),
            selections: BTreeMap::new(),
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
        self.profiles.get(&profile_id).ok_or_else(|| {
            NativeLaunchProfileError::UnknownProfile {
                profile_id: profile_id.clone(),
            }
        })?;
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
        self.selections.remove(&instance_id).is_some()
    }

    pub(crate) fn resolve_environment(
        &self,
        instance_id: AgentInstanceId,
        agent_id: &AgentId,
        transport: TransportKind,
    ) -> Result<Vec<EnvMutation>, NativeLaunchProfileError> {
        let Some(profile_id) = self.selections.get(&instance_id) else {
            return Ok(Vec::new());
        };
        let profile = self
            .profiles
            .get(profile_id)
            .ok_or_else(|| NativeLaunchProfileError::UnknownProfile {
                profile_id: profile_id.clone(),
            })?;
        profile.resolve_environment(agent_id, transport)
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
    let mut total_bytes = 0usize;
    for (index, mutation) in environment.iter().enumerate() {
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

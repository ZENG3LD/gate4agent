use crate::protocol::{
    AgentId, SessionMode, SpawnProfileDefaults, SpawnProfileId, SpawnProfileRevision,
};
use gate4agent_types::TerminalSize;
use std::collections::BTreeMap;
use thiserror::Error;

pub const DEFAULT_SPAWN_PROFILE_ID: &str = "default";
pub use crate::protocol::MAX_SPAWN_PROFILES;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpawnProfileRegistry {
    profiles: BTreeMap<SpawnProfileId, SpawnProfileDefaults>,
}

impl SpawnProfileRegistry {
    pub fn new(
        profiles: impl IntoIterator<Item = SpawnProfileDefaults>,
    ) -> Result<Self, SpawnProfileRegistryError> {
        let mut bounded = BTreeMap::new();
        for profile in profiles {
            if bounded.len() == MAX_SPAWN_PROFILES {
                return Err(SpawnProfileRegistryError::TooMany {
                    max: MAX_SPAWN_PROFILES,
                });
            }
            if !profile.terminal_size.is_valid() {
                return Err(SpawnProfileRegistryError::InvalidTerminalSize {
                    profile_id: profile.profile_id,
                });
            }
            let profile_id = profile.profile_id.clone();
            if bounded.insert(profile_id.clone(), profile).is_some() {
                return Err(SpawnProfileRegistryError::Duplicate { profile_id });
            }
        }
        if bounded.is_empty() {
            return Err(SpawnProfileRegistryError::Empty);
        }
        Ok(Self { profiles: bounded })
    }

    pub fn get(&self, profile_id: &SpawnProfileId) -> Option<&SpawnProfileDefaults> {
        self.profiles.get(profile_id)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &SpawnProfileDefaults> {
        self.profiles.values()
    }
}

impl Default for SpawnProfileRegistry {
    fn default() -> Self {
        Self::new([SpawnProfileDefaults {
            profile_id: SpawnProfileId::new(DEFAULT_SPAWN_PROFILE_ID)
                .expect("the built-in spawn profile ID is valid"),
            revision: SpawnProfileRevision::new("builtin-v1")
                .expect("the built-in spawn profile revision is valid"),
            provider: AgentId::new("claude")
                .expect("the built-in spawn profile provider is valid"),
            mode: SessionMode::Pty,
            terminal_size: TerminalSize {
                rows: 24,
                columns: 80,
            },
            prompt: None,
            bundle_id: None,
            context_id: None,
            environment_profile_id: None,
        }])
        .expect("the built-in spawn profile registry is valid")
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum SpawnProfileRegistryError {
    #[error("spawn profile registry cannot be empty")]
    Empty,
    #[error("spawn profile registry exceeds the {max}-profile limit")]
    TooMany { max: usize },
    #[error("spawn profile registry contains duplicate profile {profile_id}")]
    Duplicate { profile_id: SpawnProfileId },
    #[error("spawn profile {profile_id} has an invalid terminal size")]
    InvalidTerminalSize { profile_id: SpawnProfileId },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_spawn_profile_is_bounded_and_safe() {
        let registry = SpawnProfileRegistry::default();
        let profile_id = SpawnProfileId::new(DEFAULT_SPAWN_PROFILE_ID).unwrap();
        let profile = registry.get(&profile_id).unwrap();

        assert_eq!(profile.provider.as_str(), "claude");
        assert_eq!(profile.mode, SessionMode::Pty);
        assert!(profile.terminal_size.is_valid());
        assert!(profile.prompt.is_none());
        assert!(profile.bundle_id.is_none());
        assert!(profile.context_id.is_none());
        assert!(profile.environment_profile_id.is_none());
    }
}

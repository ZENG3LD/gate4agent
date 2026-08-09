use crate::protocol::{
    AgentId, SessionMode, SpawnEnvironmentProfileId, SpawnEnvironmentProfileRevision,
};
use gate4agent_runtime_native::{
    NativeLaunchProfile, NativeLaunchProfileId,
};
use gate4agent_types::TransportKind;
use thiserror::Error;
use crate::session_environment::NodeSessionMaterializationProfile;

pub const MAX_NODE_ENVIRONMENT_PROFILES: usize = 128;

/// Immutable host-local binding from one public profile revision to native
/// child-environment resolvers. Resolver values remain inside the native
/// profiles and are never serialized or formatted for diagnostics.
pub struct NodeEnvironmentProfile {
    id: SpawnEnvironmentProfileId,
    revision: SpawnEnvironmentProfileRevision,
    provider: AgentId,
    pty: Option<NativeLaunchProfile>,
    inline: Option<NativeLaunchProfile>,
    materialization: Option<NodeSessionMaterializationProfile>,
}

impl NodeEnvironmentProfile {
    pub fn new(
        id: SpawnEnvironmentProfileId,
        revision: SpawnEnvironmentProfileRevision,
        provider: AgentId,
        profiles: impl IntoIterator<Item = NativeLaunchProfile>,
    ) -> Result<Self, NodeEnvironmentProfileError> {
        Self::new_with_materialization(id, revision, provider, profiles, None)
    }

    pub fn new_with_materialization(
        id: SpawnEnvironmentProfileId,
        revision: SpawnEnvironmentProfileRevision,
        provider: AgentId,
        profiles: impl IntoIterator<Item = NativeLaunchProfile>,
        materialization: Option<NodeSessionMaterializationProfile>,
    ) -> Result<Self, NodeEnvironmentProfileError> {
        let mut pty = None;
        let mut inline = None;
        for profile in profiles {
            if profile.agent_id() != &provider {
                return Err(NodeEnvironmentProfileError::ProviderMismatch);
            }
            let destination = match profile.transport() {
                TransportKind::Pty => &mut pty,
                TransportKind::Pipe => &mut inline,
                TransportKind::Acp => {
                    return Err(NodeEnvironmentProfileError::UnsupportedTransport)
                }
            };
            if destination.replace(profile).is_some() {
                return Err(NodeEnvironmentProfileError::DuplicateTransport);
            }
        }
        if pty.is_none() && inline.is_none() {
            return Err(NodeEnvironmentProfileError::Empty);
        }
        if pty.as_ref().zip(inline.as_ref()).is_some_and(|(pty, inline)| {
            pty.id() == inline.id()
        }) {
            return Err(NodeEnvironmentProfileError::DuplicateNativeProfileId);
        }
        Ok(Self {
            id,
            revision,
            provider,
            pty,
            inline,
            materialization,
        })
    }

    pub fn id(&self) -> &SpawnEnvironmentProfileId {
        &self.id
    }

    pub fn revision(&self) -> &SpawnEnvironmentProfileRevision {
        &self.revision
    }

    pub fn provider(&self) -> &AgentId {
        &self.provider
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        EnvironmentProfileBinding,
        Vec<NativeLaunchProfile>,
        Option<NodeSessionMaterializationProfile>,
    ) {
        let pty_id = self.pty.as_ref().map(|profile| profile.id().clone());
        let inline_id = self.inline.as_ref().map(|profile| profile.id().clone());
        let mut profiles = Vec::with_capacity(usize::from(self.pty.is_some()) + usize::from(self.inline.is_some()));
        profiles.extend(self.pty);
        profiles.extend(self.inline);
        (
            EnvironmentProfileBinding {
                id: self.id,
                revision: self.revision,
                provider: self.provider,
                pty_id,
                inline_id,
            },
            profiles,
            self.materialization,
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct EnvironmentProfileBinding {
    pub(crate) id: SpawnEnvironmentProfileId,
    pub(crate) revision: SpawnEnvironmentProfileRevision,
    pub(crate) provider: AgentId,
    pty_id: Option<NativeLaunchProfileId>,
    inline_id: Option<NativeLaunchProfileId>,
}

impl EnvironmentProfileBinding {
    pub(crate) fn native_profile_id(
        &self,
        mode: SessionMode,
    ) -> Option<&NativeLaunchProfileId> {
        match mode {
            SessionMode::Pty => self.pty_id.as_ref(),
            SessionMode::Inline => self.inline_id.as_ref(),
        }
    }

    pub(crate) fn native_profile_ids(&self) -> impl Iterator<Item = &NativeLaunchProfileId> {
        self.pty_id.iter().chain(self.inline_id.iter())
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NodeEnvironmentProfileError {
    #[error("node environment profile requires at least one native transport binding")]
    Empty,
    #[error("node environment profile native binding targets another provider")]
    ProviderMismatch,
    #[error("node environment profile contains duplicate transport bindings")]
    DuplicateTransport,
    #[error("node environment profile transport is unsupported")]
    UnsupportedTransport,
    #[error("node environment profile reuses one native profile ID across transports")]
    DuplicateNativeProfileId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_catalog::EnvMutation;
    use gate4agent_runtime_native::{
        NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver,
    };
    use std::ffi::OsString;
    use std::sync::Arc;

    struct EmptyResolver;

    impl NativeChildEnvironmentResolver for EmptyResolver {
        fn resolve_child_environment(
            &self,
        ) -> Result<Vec<EnvMutation>, NativeChildEnvironmentResolveError> {
            Ok(vec![EnvMutation {
                key: OsString::from("GATE4AGENT_TEST_PROFILE"),
                value: None,
            }])
        }
    }

    fn native(id: &str, provider: &str, transport: TransportKind) -> NativeLaunchProfile {
        NativeLaunchProfile::new(
            NativeLaunchProfileId::new(id).unwrap(),
            AgentId::new(provider).unwrap(),
            transport,
            vec![OsString::from("GATE4AGENT_TEST_PROFILE")],
            Arc::new(EmptyResolver),
        )
        .unwrap()
    }

    #[test]
    fn node_environment_profile_requires_exact_provider_and_transport_bindings() {
        let id = SpawnEnvironmentProfileId::new("local-claude").unwrap();
        let revision = SpawnEnvironmentProfileRevision::new("local-claude-r1").unwrap();
        let provider = AgentId::new("claude").unwrap();
        let profile = NodeEnvironmentProfile::new(
            id.clone(),
            revision.clone(),
            provider.clone(),
            [
                native("local-claude-pty", "claude", TransportKind::Pty),
                native("local-claude-pipe", "claude", TransportKind::Pipe),
            ],
        )
        .unwrap();
        let (binding, native_profiles, materialization) = profile.into_parts();
        assert!(materialization.is_none());
        assert_eq!(binding.id, id);
        assert_eq!(binding.revision, revision);
        assert_eq!(binding.provider, provider);
        assert_eq!(native_profiles.len(), 2);
        assert_eq!(
            binding.native_profile_id(SessionMode::Pty).unwrap().as_str(),
            "local-claude-pty",
        );
        assert_eq!(
            binding.native_profile_id(SessionMode::Inline).unwrap().as_str(),
            "local-claude-pipe",
        );

        let mismatch = match NodeEnvironmentProfile::new(
            SpawnEnvironmentProfileId::new("mismatch").unwrap(),
            SpawnEnvironmentProfileRevision::new("r1").unwrap(),
            AgentId::new("claude").unwrap(),
            [native("mismatch-pty", "codex", TransportKind::Pty)],
        ) {
            Ok(_) => panic!("provider mismatch was accepted"),
            Err(error) => error,
        };
        assert_eq!(mismatch, NodeEnvironmentProfileError::ProviderMismatch);
    }
}

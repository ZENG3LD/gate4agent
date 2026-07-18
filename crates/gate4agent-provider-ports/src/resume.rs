use crate::{declared_binding, validate_working_directory, ProviderPortValidationError};
use gate4agent_adapters::{
    build_resume_plan, ResumeAdapterError, ResumePlan, RESUME_SESSION_ID_MAX_BYTES,
};
use gate4agent_types::{AdapterBinding, AdapterFamily, AgentId, AgentSpec};
use std::error::Error;
use thiserror::Error;

pub const RESUME_DENIAL_REASON_MAX_BYTES: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ResumeRequest {
    agent_id: AgentId,
    binding: AdapterBinding,
    session_id: String,
    working_directory: Option<String>,
}

impl ResumeRequest {
    pub fn from_spec(
        spec: &AgentSpec,
        session_id: impl Into<String>,
        working_directory: Option<String>,
    ) -> Result<Self, ProviderPortValidationError> {
        let session_id = normalize_session_id(session_id.into())?;
        Ok(Self {
            agent_id: spec.id.clone(),
            binding: declared_binding(spec, AdapterFamily::Resume)?,
            session_id,
            working_directory: validate_working_directory(working_directory)?,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn binding(&self) -> &AdapterBinding {
        &self.binding
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn working_directory(&self) -> Option<&str> {
        self.working_directory.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PreparedResume {
    agent_id: AgentId,
    binding: AdapterBinding,
    session_id: String,
    working_directory: Option<String>,
    plan: ResumePlan,
}

impl PreparedResume {
    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn binding(&self) -> &AdapterBinding {
        &self.binding
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn working_directory(&self) -> Option<&str> {
        self.working_directory.as_deref()
    }

    pub fn plan(&self) -> &ResumePlan {
        &self.plan
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeAuthorityDecision {
    Authorized,
    Denied { reason: String },
}

/// Policy authority implemented by an effect-owning shell.
///
/// This method authorizes a prepared plan only. It must not launch a process;
/// execution remains a separately ordered shell effect so this port cannot
/// become a second mutation door around the control plane.
pub trait ResumeAuthority {
    type Error: Error + 'static;

    fn authorize(
        &mut self,
        prepared: &PreparedResume,
    ) -> Result<ResumeAuthorityDecision, Self::Error>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResumeOutcome {
    Authorized(PreparedResume),
    Denied { reason: String },
}

pub fn prepare_resume<A: ResumeAuthority>(
    authority: &mut A,
    request: ResumeRequest,
) -> Result<ResumeOutcome, ResumePortError<A::Error>> {
    let plan = build_resume_plan(&request.binding.id, &request.session_id)
        .map_err(ResumePortError::Adapter)?
        .ok_or_else(|| ResumePortError::DeclaredAdapterHasNoPlan {
            agent_id: request.agent_id.clone(),
            adapter_id: request.binding.id.to_string(),
        })?;
    let prepared = PreparedResume {
        agent_id: request.agent_id,
        binding: request.binding,
        session_id: request.session_id,
        working_directory: request.working_directory,
        plan,
    };

    match authority
        .authorize(&prepared)
        .map_err(ResumePortError::Authority)?
    {
        ResumeAuthorityDecision::Authorized => Ok(ResumeOutcome::Authorized(prepared)),
        ResumeAuthorityDecision::Denied { reason } => {
            if reason.trim().is_empty()
                || reason.len() > RESUME_DENIAL_REASON_MAX_BYTES
                || reason.chars().any(char::is_control)
            {
                return Err(ResumePortError::InvalidDenialReason);
            }
            Ok(ResumeOutcome::Denied { reason })
        }
    }
}

fn normalize_session_id(value: String) -> Result<String, ProviderPortValidationError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > RESUME_SESSION_ID_MAX_BYTES
        || value.starts_with('-')
        || value.chars().any(char::is_control)
    {
        return Err(ProviderPortValidationError::InvalidResumeSessionId);
    }
    Ok(value.to_owned())
}

#[derive(Debug, Error)]
pub enum ResumePortError<E: Error + 'static> {
    #[error("resume authority failed: {0}")]
    Authority(#[source] E),
    #[error("resume adapter rejected the request: {0}")]
    Adapter(#[source] ResumeAdapterError),
    #[error("agent {agent_id} declares adapter {adapter_id}, but it has no live resume plan")]
    DeclaredAdapterHasNoPlan {
        agent_id: AgentId,
        adapter_id: String,
    },
    #[error("resume authority returned an empty, oversized, or control-bearing denial reason")]
    InvalidDenialReason,
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_catalog::builtin_registry;
    use std::fmt;

    #[derive(Debug)]
    struct FakeError;

    impl fmt::Display for FakeError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fake authority failure")
        }
    }

    impl Error for FakeError {}

    struct FakeResumeAuthority {
        calls: usize,
        decision: ResumeAuthorityDecision,
    }

    impl ResumeAuthority for FakeResumeAuthority {
        type Error = FakeError;

        fn authorize(
            &mut self,
            _prepared: &PreparedResume,
        ) -> Result<ResumeAuthorityDecision, Self::Error> {
            self.calls += 1;
            Ok(self.decision.clone())
        }
    }

    #[test]
    fn declared_resume_is_built_then_authorized_without_execution() {
        let registry = builtin_registry();
        let grok = registry.get_by_id("grok").unwrap();
        let request =
            ResumeRequest::from_spec(grok, "session-1", Some("/fixture/repo".to_owned())).unwrap();
        let mut authority = FakeResumeAuthority {
            calls: 0,
            decision: ResumeAuthorityDecision::Authorized,
        };

        let ResumeOutcome::Authorized(prepared) = prepare_resume(&mut authority, request).unwrap()
        else {
            panic!("expected authorization")
        };
        assert_eq!(prepared.plan().program, "grok");
        assert_eq!(prepared.plan().args, ["--resume", "session-1"]);
        assert_eq!(authority.calls, 1);
    }

    #[test]
    fn negative_capability_fails_before_the_authority_port() {
        let registry = builtin_registry();
        let kimi = registry.get_by_id("kimi").unwrap();
        assert!(matches!(
            ResumeRequest::from_spec(kimi, "session-1", None),
            Err(ProviderPortValidationError::UnsupportedFamily { .. })
        ));
    }

    #[test]
    fn bounded_denial_is_data_and_invalid_denial_is_rejected() {
        let registry = builtin_registry();
        let grok = registry.get_by_id("grok").unwrap();
        let request = ResumeRequest::from_spec(grok, "session-1", None).unwrap();
        let mut authority = FakeResumeAuthority {
            calls: 0,
            decision: ResumeAuthorityDecision::Denied {
                reason: "vendor login is required".to_owned(),
            },
        };
        assert_eq!(
            prepare_resume(&mut authority, request).unwrap(),
            ResumeOutcome::Denied {
                reason: "vendor login is required".to_owned(),
            }
        );

        let request = ResumeRequest::from_spec(grok, "session-2", None).unwrap();
        authority.decision = ResumeAuthorityDecision::Denied {
            reason: "bad\nreason".to_owned(),
        };
        assert!(matches!(
            prepare_resume(&mut authority, request),
            Err(ResumePortError::InvalidDenialReason)
        ));
    }
}

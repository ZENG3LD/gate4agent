use crate::{declared_binding, validate_working_directory, ProviderPortValidationError};
use gate4agent_adapters::{
    parse_history, HistoryAdapterError, HistoryDocument, HistorySession,
    RESUME_SESSION_ID_MAX_BYTES,
};
use gate4agent_types::{AdapterBinding, AdapterFamily, AgentId, AgentSpec};
use std::error::Error;
use thiserror::Error;

pub const HISTORY_DISCOVERY_LIMIT_MAX: u16 = 256;
pub const HISTORY_CANDIDATE_ID_MAX_BYTES: usize = 1_024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryDiscoveryRequest {
    agent_id: AgentId,
    binding: AdapterBinding,
    working_directory: Option<String>,
    limit: u16,
}

impl HistoryDiscoveryRequest {
    pub fn from_spec(
        spec: &AgentSpec,
        working_directory: Option<String>,
        limit: u16,
    ) -> Result<Self, ProviderPortValidationError> {
        if !(1..=HISTORY_DISCOVERY_LIMIT_MAX).contains(&limit) {
            return Err(ProviderPortValidationError::InvalidHistoryLimit {
                max: HISTORY_DISCOVERY_LIMIT_MAX,
            });
        }
        Ok(Self {
            agent_id: spec.id.clone(),
            binding: declared_binding(spec, AdapterFamily::History)?,
            working_directory: validate_working_directory(working_directory)?,
            limit,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn binding(&self) -> &AdapterBinding {
        &self.binding
    }

    pub fn working_directory(&self) -> Option<&str> {
        self.working_directory.as_deref()
    }

    pub fn limit(&self) -> u16 {
        self.limit
    }
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct HistoryCandidateId(String);

impl HistoryCandidateId {
    pub fn new(value: impl Into<String>) -> Result<Self, ProviderPortValidationError> {
        let value = value.into();
        if value.trim().is_empty()
            || value.len() > HISTORY_CANDIDATE_ID_MAX_BYTES
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
        {
            return Err(ProviderPortValidationError::InvalidHistoryCandidateId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryCandidate {
    agent_id: AgentId,
    binding: AdapterBinding,
    id: HistoryCandidateId,
    session_id_hint: String,
    modified_at_unix_ms: Option<u64>,
}

impl HistoryCandidate {
    pub fn new(
        request: &HistoryDiscoveryRequest,
        id: HistoryCandidateId,
        session_id_hint: impl Into<String>,
        modified_at_unix_ms: Option<u64>,
    ) -> Result<Self, ProviderPortValidationError> {
        let session_id_hint = session_id_hint.into();
        validate_session_id_hint(&session_id_hint)?;
        Ok(Self {
            agent_id: request.agent_id.clone(),
            binding: request.binding.clone(),
            id,
            session_id_hint,
            modified_at_unix_ms,
        })
    }

    pub fn id(&self) -> &HistoryCandidateId {
        &self.id
    }

    pub fn session_id_hint(&self) -> &str {
        &self.session_id_hint
    }

    pub fn modified_at_unix_ms(&self) -> Option<u64> {
        self.modified_at_unix_ms
    }

    fn matches(&self, request: &HistoryDiscoveryRequest) -> bool {
        self.agent_id == request.agent_id && self.binding == request.binding
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoryLoadRequest {
    agent_id: AgentId,
    binding: AdapterBinding,
    working_directory: Option<String>,
    candidate: HistoryCandidate,
}

impl HistoryLoadRequest {
    pub fn new(
        discovery: &HistoryDiscoveryRequest,
        candidate: HistoryCandidate,
    ) -> Result<Self, ProviderPortValidationError> {
        if !candidate.matches(discovery) {
            return Err(ProviderPortValidationError::HistoryCandidateSourceMismatch);
        }
        Ok(Self {
            agent_id: discovery.agent_id.clone(),
            binding: discovery.binding.clone(),
            working_directory: discovery.working_directory.clone(),
            candidate,
        })
    }

    pub fn agent_id(&self) -> &AgentId {
        &self.agent_id
    }

    pub fn binding(&self) -> &AdapterBinding {
        &self.binding
    }

    pub fn working_directory(&self) -> Option<&str> {
        self.working_directory.as_deref()
    }

    pub fn candidate(&self) -> &HistoryCandidate {
        &self.candidate
    }
}

/// Authority implemented by an effect-owning shell.
///
/// The candidate ID is opaque. It should index shell-private filesystem or
/// database state instead of exposing a host path to the core or a browser.
pub trait HistoryAuthority {
    type Error: Error + 'static;

    fn discover(
        &mut self,
        request: &HistoryDiscoveryRequest,
    ) -> Result<Vec<HistoryCandidate>, Self::Error>;

    fn load(&mut self, request: &HistoryLoadRequest) -> Result<HistoryDocument, Self::Error>;
}

pub fn discover_history<A: HistoryAuthority>(
    authority: &mut A,
    request: &HistoryDiscoveryRequest,
) -> Result<Vec<HistoryCandidate>, HistoryPortError<A::Error>> {
    let candidates = authority
        .discover(request)
        .map_err(HistoryPortError::Authority)?;
    if candidates.len() > usize::from(request.limit) {
        return Err(HistoryPortError::TooManyCandidates {
            count: candidates.len(),
            max: request.limit,
        });
    }
    if candidates
        .iter()
        .any(|candidate| !candidate.matches(request))
    {
        return Err(HistoryPortError::CandidateSourceMismatch);
    }
    Ok(candidates)
}

pub fn load_history_session<A: HistoryAuthority>(
    authority: &mut A,
    request: &HistoryLoadRequest,
) -> Result<HistorySession, HistoryPortError<A::Error>> {
    let document = authority
        .load(request)
        .map_err(HistoryPortError::Authority)?;
    if document.session_id_hint.trim() != request.candidate.session_id_hint {
        return Err(HistoryPortError::SessionHintMismatch);
    }
    parse_history(&request.binding.id, &document).map_err(HistoryPortError::Adapter)
}

fn validate_session_id_hint(value: &str) -> Result<(), ProviderPortValidationError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > RESUME_SESSION_ID_MAX_BYTES
        || value.starts_with('-')
        || value.chars().any(char::is_control)
    {
        return Err(ProviderPortValidationError::InvalidHistorySessionId);
    }
    Ok(())
}

#[derive(Debug, Error)]
pub enum HistoryPortError<E: Error + 'static> {
    #[error("history authority failed: {0}")]
    Authority(#[source] E),
    #[error("history authority returned {count} candidates; request limit is {max}")]
    TooManyCandidates { count: usize, max: u16 },
    #[error("history authority returned a candidate for another agent or adapter")]
    CandidateSourceMismatch,
    #[error("history authority changed the candidate session ID hint")]
    SessionHintMismatch,
    #[error("history adapter rejected the supplied document: {0}")]
    Adapter(#[source] HistoryAdapterError),
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

    #[derive(Default)]
    struct FakeHistoryAuthority {
        discover_calls: usize,
        load_calls: usize,
    }

    impl HistoryAuthority for FakeHistoryAuthority {
        type Error = FakeError;

        fn discover(
            &mut self,
            request: &HistoryDiscoveryRequest,
        ) -> Result<Vec<HistoryCandidate>, Self::Error> {
            self.discover_calls += 1;
            Ok(vec![HistoryCandidate::new(
                request,
                HistoryCandidateId::new("opaque-history-1").unwrap(),
                "grok-session-1",
                Some(42),
            )
            .unwrap()])
        }

        fn load(&mut self, request: &HistoryLoadRequest) -> Result<HistoryDocument, Self::Error> {
            self.load_calls += 1;
            Ok(HistoryDocument {
                session_id_hint: request.candidate().session_id_hint().to_owned(),
                metadata_json: Some(
                    r#"{"info":{"id":"grok-session-1","cwd":"/fixture/repo"}}"#.to_owned(),
                ),
                transcript: concat!(
                    r#"{"type":"user","content":"hello"}"#,
                    "\n",
                    r#"{"type":"assistant","content":"world"}"#,
                )
                .to_owned(),
            })
        }
    }

    #[test]
    fn shell_authority_supplies_bytes_but_pure_adapter_owns_parsing() {
        let registry = builtin_registry();
        let spec = registry.get_by_id("grok").unwrap();
        let discovery =
            HistoryDiscoveryRequest::from_spec(spec, Some("/fixture/repo".to_owned()), 4).unwrap();
        let mut authority = FakeHistoryAuthority::default();

        let mut candidates = discover_history(&mut authority, &discovery).unwrap();
        let load = HistoryLoadRequest::new(&discovery, candidates.remove(0)).unwrap();
        let session = load_history_session(&mut authority, &load).unwrap();

        assert_eq!(session.session_id, "grok-session-1");
        assert_eq!(session.messages.len(), 2);
        assert_eq!(authority.discover_calls, 1);
        assert_eq!(authority.load_calls, 1);
    }

    #[test]
    fn launch_only_agents_and_unbounded_requests_fail_before_authority() {
        let registry = builtin_registry();
        let qwen = registry.get_by_id("qwen-code").unwrap();
        assert!(matches!(
            HistoryDiscoveryRequest::from_spec(qwen, None, 1),
            Err(ProviderPortValidationError::UnsupportedFamily { .. })
        ));

        let grok = registry.get_by_id("grok").unwrap();
        assert!(matches!(
            HistoryDiscoveryRequest::from_spec(grok, None, 0),
            Err(ProviderPortValidationError::InvalidHistoryLimit { .. })
        ));
        assert!(matches!(
            HistoryCandidateId::new(r"C:\Users\fixture\history.jsonl"),
            Err(ProviderPortValidationError::InvalidHistoryCandidateId)
        ));
    }
}

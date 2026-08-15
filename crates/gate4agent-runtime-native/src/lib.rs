//! Tick-driven native runtime for embedding gate4agent in an owning app core.

mod launch_profiles;
mod vendor_contract;

pub use launch_profiles::{
    NativeChildEnvironmentResolveError, NativeChildEnvironmentResolver, NativeLaunchProfile,
    NativeHarnessMcpLaunchOverlay, NativeInstanceLaunchOverlay, NativeLaunchEnvironmentOverlay,
    NativeLaunchProfileControl,
    NativeLaunchProfileDescriptor, NativeLaunchProfileError, NativeLaunchProfileId,
    HARNESS_MCP_PROGRAM_ENV, HARNESS_MCP_SESSION_ENDPOINT_ENV,
    HARNESS_MCP_SESSION_TOKEN_ENV, LEGACY_HARNESS_READ_CREDENTIAL_ENV,
    LEGACY_HARNESS_READ_ENDPOINT_ENV,
    ZAI_GLM_ANTHROPIC_BASE_URL, ZAI_GLM_CLAUDE_OPTIONAL_ENV_KEYS,
    ZAI_GLM_CLAUDE_OWNED_ENV_KEYS, ZAI_GLM_CLAUDE_PROFILE, ZAI_GLM_CLAUDE_PROFILE_ID,
    ZAI_GLM_CLAUDE_PROFILE_REVISION, ZAI_GLM_CLAUDE_REQUIRED_ENV_KEYS,
};
pub use vendor_contract::{
    probe_installed_vendor_version, resolve_vendor_contract, VendorCapabilitySet,
    VendorCapabilityUnavailableReason, VendorCapabilityVerdict, VendorCliFamily,
    VendorContractResolution, VendorFallbackReason, VendorLauncherIdentity, VendorPlatform,
    VendorRuntimeMode, VendorVersionProbeCache, VendorVersionProbeResult,
    VendorVersionProbeStatus, VendorVersionStatus,
    CLAUDE_WINDOWS_X86_64_2_1_223_CONTRACT_ID,
    CLAUDE_WINDOWS_X86_64_2_1_224_CONTRACT_ID,
};

use gate4agent_catalog::{builtin_registry, AgentRegistry, EnvMutation};
use gate4agent_handle::{
    bounded_control_plane, ControlPlaneKernelPort, Gate4AgentHandle,
    ProviderRuntimeError, PublishReport, ToolAuthorityHandle,
};
use gate4agent_kernel::{CommandOutcome, Gate4AgentKernel};
use gate4agent_provider_ports::{
    discover_history, load_history_session, prepare_resume, HistoryCandidate,
    HistoryDiscoveryRequest, HistoryLoadRequest, PreparedResume, ResumeAuthority,
    ResumeAuthorityDecision, ResumeOutcome, ResumeRequest,
    HISTORY_DISCOVERY_LIMIT_MAX as PROVIDER_HISTORY_DISCOVERY_LIMIT_MAX,
};
use gate4agent_shell_capabilities::NativeCapabilityProbeAuthority;
use gate4agent_shell_history::NativeHistoryAuthority;
pub use gate4agent_shell_history::{orca_home_roots, NativeHistoryConfig, NativeHistoryRoot};
pub use gate4agent_adapters::{HistorySourceLayout, OneShotSessionPersistence};
pub use gate4agent_shell_hooks::{HookIngressConfig, HookIngressEndpoint};
use gate4agent_shell_hooks::{HookIngressControl, HookIngressServer, HookIngressStartError};
pub use gate4agent_shell_native::{
    NativeProviderExecutor, NativeProviderExit, NativeProviderOperation,
    NativeProviderOperationError, NativeProviderResultPoll, PhysicalExitAck,
    ProviderOperationKey, ProviderStopCause, ProviderSupervisorFault,
    ProviderSupervisorFaultKind, ProviderSupervisorSnapshot, ProviderSupervisorState,
};
use gate4agent_shell_native::{
    NativeEffectShell, ProviderSupervisor, ProviderSupervisorBuildError, QwenDualOutputLaunch,
    MAX_PROVIDER_SUPERVISOR_EVENTS,
};
use gate4agent_tool_engine::{
    CapabilityOwner, CapabilityProviderDescriptor, ProviderBindingId, ToolEngineError,
    ToolProviderId,
};
use gate4agent_types::{
    AgentId, AgentInstanceId, CapabilityProbeFailure, ControlEffect, ControlObservation,
    EffectEnvelope, HistoryCandidateSummary, HistoryMessageRecord, HistoryMessageRole,
    HistorySessionRecord, NativeSessionCatalogEntry, NativeSessionCatalogScope,
    NativeSessionCatalogWindow, NativeSessionExternalGroup, NativeSessionExternalGroupKind,
    NativeSessionPreview,
    NativeSessionPreviewMessage, ObservationEnvelope, ProviderSessionIdentity,
    ProviderRuntimeCapability, ProviderRuntimePolicy,
    PipeProtocol, ResumeAuthorityTarget, ResumeLaunchRequest, SessionGeneration, SessionStatus,
    TransportKind, CONTROL_PROTOCOL_VERSION,
};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::convert::Infallible;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use thiserror::Error;
use tokio::sync::mpsc::{self, error::TrySendError, Receiver, Sender};
use tokio::time::MissedTickBehavior;

type TerminalFrameKey = (AgentInstanceId, SessionGeneration);
const PROVIDER_EVENT_DRAIN_QUANTUM: usize = 64;
const MAX_PROVIDER_SHUTDOWN_TIMEOUT_MS: u64 = 86_400_000;
const NATIVE_SESSION_INDEX_REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const NATIVE_SESSION_RECENT_WINDOW_MS: u64 = 7 * 24 * 60 * 60 * 1_000;

/// Independent read-only authority for projecting provider-native history into
/// bounded session metadata. It does not create kernel or provider sessions.
pub struct NativeSessionCatalogAuthority {
    catalog: AgentRegistry,
    history_config: NativeHistoryConfig,
    authorities: HashMap<AgentId, NativeHistoryAuthority>,
    indexes: HashMap<AgentId, NativeProviderSessionIndex>,
}

struct NativeProviderSessionIndex {
    refreshed_at: Instant,
    revision: u64,
    recent_cutoff_unix_ms: u64,
    sessions: Vec<IndexedNativeSession>,
}

#[derive(Clone, Eq, PartialEq)]
struct IndexedNativeSession {
    candidate: HistoryCandidate,
    selection_id: String,
    modified_at_unix_ms: Option<u64>,
    cwd_key: Option<NativePathKey>,
    session_id: String,
    title: Option<String>,
    model: Option<String>,
    project_label: String,
    provider_session: ProviderSessionIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedNativeSessionCatalogEntry {
    pub metadata: NativeSessionCatalogEntry,
    pub external_group: Option<NativeSessionExternalGroup>,
    pub provider_session: ProviderSessionIdentity,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScopedNativeSessionCatalogPage {
    pub revision: u64,
    pub cutoff_unix_ms: u64,
    pub window: NativeSessionCatalogWindow,
    pub entries: Vec<ScopedNativeSessionCatalogEntry>,
    pub recent_total_count: u32,
    pub older_total_count: u32,
    pub next_after_selection_id: Option<String>,
    pub remaining_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSessionSelectionResolution {
    pub identity: ProviderSessionIdentity,
    pub external_group: Option<NativeSessionExternalGroup>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeSessionCatalogPage {
    pub revision: u64,
    pub cutoff_unix_ms: u64,
    pub window: NativeSessionCatalogWindow,
    pub entries: Vec<NativeSessionCatalogEntry>,
    pub recent_total_count: u32,
    pub older_total_count: u32,
    pub next_after_selection_id: Option<String>,
    pub remaining_count: u32,
}

impl NativeSessionCatalogAuthority {
    pub fn new(config: NativeHistoryConfig) -> Self {
        Self {
            catalog: builtin_registry().clone(),
            history_config: config,
            authorities: HashMap::new(),
            indexes: HashMap::new(),
        }
    }

    pub fn catalog(
        &mut self,
        provider: &AgentId,
        canonical_workspace: &Path,
        limit: u16,
    ) -> Result<Vec<NativeSessionCatalogEntry>, NativeSessionCatalogError> {
        self.catalog_for_workspace(
            provider,
            canonical_workspace,
            &[canonical_workspace.to_path_buf()],
            limit,
        )
    }

    pub fn catalog_for_workspace(
        &mut self,
        provider: &AgentId,
        canonical_workspace: &Path,
        registered_workspace_roots: &[PathBuf],
        limit: u16,
    ) -> Result<Vec<NativeSessionCatalogEntry>, NativeSessionCatalogError> {
        self.catalog_initial_for_workspace(
            provider,
            canonical_workspace,
            registered_workspace_roots,
            limit,
        )
        .map(|page| page.entries)
    }

    pub fn catalog_initial_for_workspace(
        &mut self,
        provider: &AgentId,
        canonical_workspace: &Path,
        registered_workspace_roots: &[PathBuf],
        limit: u16,
    ) -> Result<NativeSessionCatalogPage, NativeSessionCatalogError> {
        self.catalog_initial_for_scope(
            provider,
            NativeSessionCatalogScope::Workspace,
            Some(canonical_workspace),
            registered_workspace_roots,
            limit,
        )
        .map(unscoped_native_catalog_page)
    }

    pub fn catalog_initial_for_scope(
        &mut self,
        provider: &AgentId,
        scope: NativeSessionCatalogScope,
        canonical_workspace: Option<&Path>,
        registered_workspace_roots: &[PathBuf],
        limit: u16,
    ) -> Result<ScopedNativeSessionCatalogPage, NativeSessionCatalogError> {
        let ownership = NativeSessionOwnership::new(
            scope,
            canonical_workspace,
            registered_workspace_roots,
        )
        .map_err(|_| NativeSessionCatalogError::WorkspaceUnavailable)?;
        self.ensure_provider_index(provider)
            .map_err(map_catalog_index_error)?;
        let (index_revision, cutoff_unix_ms) = self
            .indexes
            .get(provider)
            .map(|index| (index.revision, index.recent_cutoff_unix_ms))
            .ok_or(NativeSessionCatalogError::CatalogUnavailable)?;
        let revision = scoped_native_catalog_snapshot_revision(
            index_revision,
            cutoff_unix_ms,
            ownership.fingerprint(),
        );
        self.catalog_page_for_scope(
            provider,
            scope,
            canonical_workspace,
            registered_workspace_roots,
            NativeSessionCatalogWindow::Recent,
            revision,
            cutoff_unix_ms,
            None,
            limit,
        )
    }

    pub fn catalog_page_for_workspace(
        &mut self,
        provider: &AgentId,
        canonical_workspace: &Path,
        registered_workspace_roots: &[PathBuf],
        window: NativeSessionCatalogWindow,
        revision: u64,
        cutoff_unix_ms: u64,
        after_selection_id: Option<&str>,
        limit: u16,
    ) -> Result<NativeSessionCatalogPage, NativeSessionCatalogError> {
        self.catalog_page_for_scope(
            provider,
            NativeSessionCatalogScope::Workspace,
            Some(canonical_workspace),
            registered_workspace_roots,
            window,
            revision,
            cutoff_unix_ms,
            after_selection_id,
            limit,
        )
        .map(unscoped_native_catalog_page)
    }

    pub fn catalog_page_for_scope(
        &mut self,
        provider: &AgentId,
        scope: NativeSessionCatalogScope,
        canonical_workspace: Option<&Path>,
        registered_workspace_roots: &[PathBuf],
        window: NativeSessionCatalogWindow,
        revision: u64,
        cutoff_unix_ms: u64,
        after_selection_id: Option<&str>,
        limit: u16,
    ) -> Result<ScopedNativeSessionCatalogPage, NativeSessionCatalogError> {
        if !(1..=gate4agent_types::NATIVE_SESSION_CATALOG_LIMIT_MAX).contains(&limit) {
            return Err(NativeSessionCatalogError::InvalidLimit);
        }
        let ownership = NativeSessionOwnership::new(
            scope,
            canonical_workspace,
            registered_workspace_roots,
        )
            .map_err(|_| NativeSessionCatalogError::WorkspaceUnavailable)?;
        self.ensure_provider_index(provider)
            .map_err(map_catalog_index_error)?;
        let index = self
            .indexes
            .get(provider)
            .expect("ensured native provider index must exist");
        if scoped_native_catalog_snapshot_revision(
            index.revision,
            cutoff_unix_ms,
            ownership.fingerprint(),
        ) != revision
        {
            return Err(NativeSessionCatalogError::StaleCatalog);
        }
        let external_group_ids = ownership.external_group_ids(&index.sessions);
        let mut seen_sessions = std::collections::HashSet::new();
        let mut recent = Vec::new();
        let mut older = Vec::new();
        for indexed in &index.sessions {
            if !ownership.owns(&indexed.cwd_key)
                || !seen_sessions.insert(indexed.session_id.clone())
            {
                continue;
            }
            let entry = NativeSessionCatalogEntry {
                selection_id: indexed.selection_id.clone(),
                session_id: indexed.session_id.clone(),
                title: indexed.title.clone(),
                modified_at_unix_ms: indexed.modified_at_unix_ms,
                model: indexed.model.clone(),
                message_count: 0,
                completed_turn_count: None,
            };
            let external_group = ownership.external_group(indexed, &external_group_ids);
            if entry.validate().is_ok()
                && external_group
                    .as_ref()
                    .map_or(true, |group| group.validate().is_ok())
            {
                let entry = ScopedNativeSessionCatalogEntry {
                    metadata: entry,
                    external_group,
                    provider_session: indexed.provider_session.clone(),
                };
                match native_catalog_window_for_modified(
                    indexed.modified_at_unix_ms,
                    cutoff_unix_ms,
                ) {
                    NativeSessionCatalogWindow::Recent => recent.push(entry),
                    NativeSessionCatalogWindow::Older => older.push(entry),
                }
            }
        }
        let recent_total_count = u32::try_from(recent.len()).unwrap_or(u32::MAX);
        let older_total_count = u32::try_from(older.len()).unwrap_or(u32::MAX);
        let selected = match window {
            NativeSessionCatalogWindow::Recent => recent,
            NativeSessionCatalogWindow::Older => older,
        };
        let start = match after_selection_id {
            Some(cursor) => selected
                .iter()
                .position(|entry| entry.metadata.selection_id == cursor)
                .map(|index| index + 1)
                .ok_or(NativeSessionCatalogError::StaleCatalog)?,
            None => 0,
        };
        let end = start.saturating_add(usize::from(limit)).min(selected.len());
        let entries = selected[start..end].to_vec();
        let next_after_selection_id = (end < selected.len())
            .then(|| entries.last().map(|entry| entry.metadata.selection_id.clone()))
            .flatten();
        let remaining_count = u32::try_from(selected.len().saturating_sub(end))
            .unwrap_or(u32::MAX);
        Ok(ScopedNativeSessionCatalogPage {
            revision,
            cutoff_unix_ms,
            window,
            entries,
            recent_total_count,
            older_total_count,
            next_after_selection_id,
            remaining_count,
        })
    }

    fn ensure_provider_index(
        &mut self,
        provider: &AgentId,
    ) -> Result<(), NativeSessionIndexError> {
        if self.indexes.get(provider).is_some_and(|index| {
            index.refreshed_at.elapsed() < NATIVE_SESSION_INDEX_REFRESH_INTERVAL
        }) {
            return Ok(());
        }
        let spec = self
            .catalog
            .get(provider)
            .cloned()
            .ok_or(NativeSessionIndexError::UnsupportedProvider)?;
        let request = HistoryDiscoveryRequest::from_spec(
            &spec,
            None,
            PROVIDER_HISTORY_DISCOVERY_LIMIT_MAX,
        )
        .map_err(|_| NativeSessionIndexError::UnsupportedProvider)?;
        let authority = self
            .authorities
            .entry(provider.clone())
            .or_insert_with(|| NativeHistoryAuthority::new(self.history_config.clone()));
        let locators = authority.discover_locator_index(&request)
            .map_err(|_| NativeSessionIndexError::Unavailable)?;
        if !authority.take_discovery_issues().is_empty() {
            return Err(NativeSessionIndexError::Unavailable);
        }
        let mut sessions = Vec::with_capacity(locators.len());
        for locator in locators {
            let indexed_candidate = locator.candidate().clone();
            let candidate = locator.candidate();
            let selection_id = candidate.id().as_str().to_owned();
            let modified_at_unix_ms = candidate.modified_at_unix_ms();
            let cwd_key = locator.cwd().and_then(NativePathKey::from_declared_cwd);
            let project_label = if cwd_key.is_some() {
                locator
                    .cwd()
                    .map(native_external_group_label)
                    .unwrap_or_else(|| "Global".to_owned())
            } else {
                "Global".to_owned()
            };
            let load = HistoryLoadRequest::new(&request, indexed_candidate.clone())
                .map_err(|_| NativeSessionIndexError::Unavailable)?;
            let provider_session = authority
                .resume_provider_session(&load, locator.session_id().to_owned())
                .map_err(|_| NativeSessionIndexError::Unavailable)?;
            sessions.push(IndexedNativeSession {
                candidate: indexed_candidate,
                selection_id,
                modified_at_unix_ms,
                cwd_key,
                session_id: locator.session_id().to_owned(),
                title: locator.title().map(str::to_owned),
                model: locator.model().map(str::to_owned),
                project_label,
                provider_session,
            });
        }
        sessions.sort_by(|left, right| {
            right
                .modified_at_unix_ms
                .cmp(&left.modified_at_unix_ms)
                .then_with(|| left.session_id.cmp(&right.session_id))
                .then_with(|| left.selection_id.cmp(&right.selection_id))
        });
        if let Some(current) = self.indexes.get_mut(provider) {
            if current.sessions == sessions {
                current.refreshed_at = Instant::now();
                return Ok(());
            }
        }
        let revision = self
            .indexes
            .get(provider)
            .map(|index| index.revision.saturating_add(1))
            .unwrap_or(1);
        let recent_cutoff_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|duration| u64::try_from(duration.as_millis()).ok())
            .map(|now| now.saturating_sub(NATIVE_SESSION_RECENT_WINDOW_MS))
            .ok_or(NativeSessionIndexError::Unavailable)?;
        self.indexes.insert(
            provider.clone(),
            NativeProviderSessionIndex {
                refreshed_at: Instant::now(),
                revision,
                recent_cutoff_unix_ms,
                sessions,
            },
        );
        Ok(())
    }

    pub fn preview(
        &mut self,
        provider: &AgentId,
        canonical_workspace: &Path,
        selection_id: &str,
        message_limit: u16,
    ) -> Result<NativeSessionPreview, NativeSessionPreviewError> {
        self.preview_for_workspace(
            provider,
            canonical_workspace,
            &[canonical_workspace.to_path_buf()],
            selection_id,
            message_limit,
        )
    }

    pub fn preview_for_workspace(
        &mut self,
        provider: &AgentId,
        canonical_workspace: &Path,
        registered_workspace_roots: &[PathBuf],
        selection_id: &str,
        message_limit: u16,
    ) -> Result<NativeSessionPreview, NativeSessionPreviewError> {
        self.preview_selected(
            provider,
            NativeSessionCatalogScope::Workspace,
            Some(canonical_workspace),
            registered_workspace_roots,
            None,
            NativeSessionPreviewSelector::Candidate(selection_id),
            message_limit,
        )
    }

    pub fn preview_for_scope(
        &mut self,
        provider: &AgentId,
        scope: NativeSessionCatalogScope,
        canonical_workspace: Option<&Path>,
        registered_workspace_roots: &[PathBuf],
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        selection_id: &str,
        message_limit: u16,
    ) -> Result<NativeSessionPreview, NativeSessionPreviewError> {
        self.preview_selected(
            provider,
            scope,
            canonical_workspace,
            registered_workspace_roots,
            Some((catalog_revision, recent_cutoff_unix_ms)),
            NativeSessionPreviewSelector::Candidate(selection_id),
            message_limit,
        )
    }

    pub fn preview_session_id(
        &mut self,
        provider: &AgentId,
        canonical_workspace: &Path,
        session_id: &str,
        message_limit: u16,
    ) -> Result<NativeSessionPreview, NativeSessionPreviewError> {
        self.preview_session_id_for_workspace(
            provider,
            canonical_workspace,
            &[canonical_workspace.to_path_buf()],
            session_id,
            message_limit,
        )
    }

    pub fn preview_session_id_for_workspace(
        &mut self,
        provider: &AgentId,
        canonical_workspace: &Path,
        registered_workspace_roots: &[PathBuf],
        session_id: &str,
        message_limit: u16,
    ) -> Result<NativeSessionPreview, NativeSessionPreviewError> {
        self.preview_selected(
            provider,
            NativeSessionCatalogScope::Workspace,
            Some(canonical_workspace),
            registered_workspace_roots,
            None,
            NativeSessionPreviewSelector::Session(session_id),
            message_limit,
        )
    }

    fn preview_selected(
        &mut self,
        provider: &AgentId,
        scope: NativeSessionCatalogScope,
        canonical_workspace: Option<&Path>,
        registered_workspace_roots: &[PathBuf],
        catalog_snapshot: Option<(u64, u64)>,
        selector: NativeSessionPreviewSelector<'_>,
        message_limit: u16,
    ) -> Result<NativeSessionPreview, NativeSessionPreviewError> {
        if !(1..=gate4agent_types::NATIVE_SESSION_PREVIEW_MESSAGE_LIMIT_MAX)
            .contains(&message_limit)
        {
            return Err(NativeSessionPreviewError::InvalidLimit);
        }
        let ownership = NativeSessionOwnership::new(
            scope,
            canonical_workspace,
            registered_workspace_roots,
        )
            .map_err(|_| NativeSessionPreviewError::WorkspaceUnavailable)?;
        let cached_candidate = match selector {
            NativeSessionPreviewSelector::Candidate(selection_id) => self
                .indexes
                .get(provider)
                .is_some_and(|index| {
                    index.sessions.iter().any(|session| {
                        session.selection_id == selection_id
                            && ownership.owns(&session.cwd_key)
                    })
                }),
            NativeSessionPreviewSelector::Session(_) => false,
        };
        if !cached_candidate {
            self.ensure_provider_index(provider)
                .map_err(|error| match error {
                    NativeSessionIndexError::UnsupportedProvider => {
                        NativeSessionPreviewError::UnsupportedProvider
                    }
                    NativeSessionIndexError::Unavailable => {
                        NativeSessionPreviewError::PreviewUnavailable
                    }
                })?;
        }
        let index = self
            .indexes
            .get(provider)
            .ok_or(NativeSessionPreviewError::SessionNotFound)?;
        if let Some((catalog_revision, recent_cutoff_unix_ms)) = catalog_snapshot {
            if index.recent_cutoff_unix_ms != recent_cutoff_unix_ms
                || scoped_native_catalog_snapshot_revision(
                    index.revision,
                    recent_cutoff_unix_ms,
                    ownership.fingerprint(),
                ) != catalog_revision
            {
                return Err(NativeSessionPreviewError::StaleCatalog);
            }
        }
        let mut matched = None;
        for indexed in &index.sessions {
            if matches!(selector, NativeSessionPreviewSelector::Candidate(selection_id)
                if indexed.selection_id != selection_id)
                || matches!(selector, NativeSessionPreviewSelector::Session(session_id)
                    if indexed.session_id != session_id)
                || !ownership.owns(&indexed.cwd_key)
            {
                continue;
            }
            if matched.is_some() {
                return Err(NativeSessionPreviewError::AmbiguousSession);
            }
            matched = Some((
                indexed.candidate.clone(),
                indexed.session_id.clone(),
                indexed.cwd_key.clone(),
                indexed.modified_at_unix_ms,
            ));
        }
        let (candidate, expected_session_id, expected_cwd, modified_at_unix_ms) =
            matched.ok_or(NativeSessionPreviewError::SessionNotFound)?;
        let spec = self
            .catalog
            .get(provider)
            .cloned()
            .ok_or(NativeSessionPreviewError::UnsupportedProvider)?;
        let request = HistoryDiscoveryRequest::from_spec(
            &spec,
            None,
            PROVIDER_HISTORY_DISCOVERY_LIMIT_MAX,
        )
        .map_err(|_| NativeSessionPreviewError::UnsupportedProvider)?;
        let load = HistoryLoadRequest::new(&request, candidate)
            .map_err(|_| NativeSessionPreviewError::PreviewUnavailable)?;
        let authority = self
            .authorities
            .get_mut(provider)
            .ok_or(NativeSessionPreviewError::PreviewUnavailable)?;
        let preview_load = authority.load_preview_session(&load)
            .map_err(|_| NativeSessionPreviewError::PreviewUnavailable)?;
        let source_truncated = preview_load.source_truncated();
        let session = preview_load.session();
        let current_cwd = session
            .cwd
            .as_deref()
            .and_then(NativePathKey::from_declared_cwd);
        if session.session_id != expected_session_id
            || current_cwd != expected_cwd
            || !ownership.owns(&current_cwd)
        {
            return Err(NativeSessionPreviewError::PreviewUnavailable);
        }
        project_native_session_preview(
            session,
            modified_at_unix_ms,
            source_truncated,
            message_limit,
        )
    }

    pub fn resolve_selection_for_scope(
        &mut self,
        provider: &AgentId,
        scope: NativeSessionCatalogScope,
        canonical_workspace: Option<&Path>,
        registered_workspace_roots: &[PathBuf],
        catalog_revision: u64,
        recent_cutoff_unix_ms: u64,
        selection_id: &str,
    ) -> Result<NativeSessionSelectionResolution, NativeSessionPreviewError> {
        gate4agent_types::validate_candidate_id(selection_id)
            .map_err(|_| NativeSessionPreviewError::SessionNotFound)?;
        let ownership = NativeSessionOwnership::new(
            scope,
            canonical_workspace,
            registered_workspace_roots,
        )
        .map_err(|_| NativeSessionPreviewError::WorkspaceUnavailable)?;
        self.ensure_provider_index(provider)
            .map_err(|error| match error {
                NativeSessionIndexError::UnsupportedProvider => {
                    NativeSessionPreviewError::UnsupportedProvider
                }
                NativeSessionIndexError::Unavailable => {
                    NativeSessionPreviewError::PreviewUnavailable
                }
            })?;
        let index = self
            .indexes
            .get(provider)
            .ok_or(NativeSessionPreviewError::SessionNotFound)?;
        if index.recent_cutoff_unix_ms != recent_cutoff_unix_ms
            || scoped_native_catalog_snapshot_revision(
                index.revision,
                recent_cutoff_unix_ms,
                ownership.fingerprint(),
            ) != catalog_revision
        {
            return Err(NativeSessionPreviewError::StaleCatalog);
        }
        let external_group_ids = ownership.external_group_ids(&index.sessions);
        let mut matched = None;
        for indexed in &index.sessions {
            if indexed.selection_id != selection_id || !ownership.owns(&indexed.cwd_key) {
                continue;
            }
            if matched.is_some() {
                return Err(NativeSessionPreviewError::AmbiguousSession);
            }
            matched = Some((
                indexed.candidate.clone(),
                indexed.session_id.clone(),
                indexed.cwd_key.clone(),
                ownership.external_group(indexed, &external_group_ids),
            ));
        }
        let (candidate, expected_session_id, expected_cwd, external_group) =
            matched.ok_or(NativeSessionPreviewError::SessionNotFound)?;
        let spec = self
            .catalog
            .get(provider)
            .cloned()
            .ok_or(NativeSessionPreviewError::UnsupportedProvider)?;
        let request = HistoryDiscoveryRequest::from_spec(
            &spec,
            None,
            PROVIDER_HISTORY_DISCOVERY_LIMIT_MAX,
        )
        .map_err(|_| NativeSessionPreviewError::UnsupportedProvider)?;
        let load = HistoryLoadRequest::new(&request, candidate)
            .map_err(|_| NativeSessionPreviewError::PreviewUnavailable)?;
        let authority = self
            .authorities
            .get_mut(provider)
            .ok_or(NativeSessionPreviewError::PreviewUnavailable)?;
        let session = authority
            .load_preview_session(&load)
            .map_err(|_| NativeSessionPreviewError::PreviewUnavailable)?
            .session();
        let current_cwd = session
            .cwd
            .as_deref()
            .and_then(NativePathKey::from_declared_cwd);
        if session.session_id != expected_session_id
            || current_cwd != expected_cwd
            || !ownership.owns(&current_cwd)
        {
            return Err(NativeSessionPreviewError::PreviewUnavailable);
        }
        let identity = authority
            .resume_provider_session(&load, session.session_id)
            .map_err(|_| NativeSessionPreviewError::PreviewUnavailable)?;
        identity
            .validate()
            .map_err(|_| NativeSessionPreviewError::PreviewUnavailable)?;
        if external_group
            .as_ref()
            .is_some_and(|group| group.validate().is_err())
        {
            return Err(NativeSessionPreviewError::PreviewUnavailable);
        }
        Ok(NativeSessionSelectionResolution {
            identity,
            external_group,
        })
    }
}

struct NativeSessionOwnership {
    scope: NativeSessionCatalogScope,
    selected: Option<NativePathKey>,
    registered: Vec<NativePathKey>,
}

impl NativeSessionOwnership {
    fn new(
        scope: NativeSessionCatalogScope,
        selected: Option<&Path>,
        registered: &[PathBuf],
    ) -> Result<Self, ()> {
        let mut registered = registered
            .iter()
            .filter(|root| root.is_absolute())
            .map(std::fs::canonicalize)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| ())?
            .into_iter()
            .map(|root| NativePathKey::from_canonical_root(&root).ok_or(()))
            .collect::<Result<Vec<_>, _>>()?;
        let selected = match (scope, selected) {
            (NativeSessionCatalogScope::Workspace, Some(selected)) => {
                let selected = std::fs::canonicalize(selected).map_err(|_| ())?;
                if !selected.is_dir() {
                    return Err(());
                }
                let selected = NativePathKey::from_canonical_root(&selected).ok_or(())?;
                if !registered.iter().any(|root| root == &selected) {
                    registered.push(selected.clone());
                }
                Some(selected)
            }
            (NativeSessionCatalogScope::Unregistered, None) => None,
            _ => return Err(()),
        };
        registered.sort();
        registered.dedup();
        Ok(Self {
            scope,
            selected,
            registered,
        })
    }

    fn owns(&self, cwd: &Option<NativePathKey>) -> bool {
        let Some(cwd) = cwd.as_ref() else {
            return matches!(self.scope, NativeSessionCatalogScope::Unregistered);
        };
        let owner = self.registered
            .iter()
            .filter(|root| cwd.starts_with(root))
            .max_by_key(|root| root.components.len());
        match self.scope {
            NativeSessionCatalogScope::Workspace => owner == self.selected.as_ref(),
            NativeSessionCatalogScope::Unregistered => owner.is_none(),
        }
    }

    fn external_group(
        &self,
        indexed: &IndexedNativeSession,
        group_ids: &BTreeMap<NativeExternalGroupKey, String>,
    ) -> Option<NativeSessionExternalGroup> {
        matches!(self.scope, NativeSessionCatalogScope::Unregistered).then(|| {
            let key = NativeExternalGroupKey::from_cwd(indexed.cwd_key.as_ref());
            NativeSessionExternalGroup {
                group_id: group_ids
                    .get(&key)
                    .cloned()
                    .expect("owned unregistered session must have an opaque group token"),
                kind: match key {
                    NativeExternalGroupKey::Project(_) => NativeSessionExternalGroupKind::Project,
                    NativeExternalGroupKey::Global => NativeSessionExternalGroupKind::Global,
                },
                display_name: indexed.project_label.clone(),
            }
        })
    }

    fn external_group_ids(
        &self,
        sessions: &[IndexedNativeSession],
    ) -> BTreeMap<NativeExternalGroupKey, String> {
        if !matches!(self.scope, NativeSessionCatalogScope::Unregistered) {
            return BTreeMap::new();
        }
        let mut paths = sessions
            .iter()
            .filter(|session| self.owns(&session.cwd_key))
            .map(|session| NativeExternalGroupKey::from_cwd(session.cwd_key.as_ref()))
            .collect::<Vec<_>>();
        paths.sort();
        paths.dedup();
        paths
            .into_iter()
            .enumerate()
            .map(|(index, path)| (path, format!("external-{:04}", index + 1)))
            .collect()
    }

    fn fingerprint(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        hash = native_hash_bytes(
            hash,
            &[match self.scope {
                NativeSessionCatalogScope::Workspace => 1,
                NativeSessionCatalogScope::Unregistered => 2,
            }],
        );
        if let Some(selected) = self.selected.as_ref() {
            hash = native_hash_path(hash, selected);
        }
        for root in &self.registered {
            hash = native_hash_bytes(hash, &[0xff]);
            hash = native_hash_path(hash, root);
        }
        hash.max(1)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum NativeExternalGroupKey {
    Global,
    Project(NativePathKey),
}

impl NativeExternalGroupKey {
    fn from_cwd(cwd: Option<&NativePathKey>) -> Self {
        match cwd {
            Some(cwd) => Self::Project(cwd.clone()),
            None => Self::Global,
        }
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct NativePathKey {
    components: Vec<String>,
}

impl NativePathKey {
    fn from_canonical_root(path: &Path) -> Option<Self> {
        Self::from_absolute_path(path)
    }

    fn from_declared_cwd(value: &str) -> Option<Self> {
        let path = Path::new(value);
        if !path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return None;
        }
        if let Ok(canonical) = std::fs::canonicalize(path) {
            return Self::from_absolute_path(&canonical);
        }
        let mut existing = path;
        let mut missing = Vec::<OsString>::new();
        while !existing.exists() {
            missing.push(existing.file_name()?.to_os_string());
            existing = existing.parent()?;
        }
        let mut resolved = std::fs::canonicalize(existing).ok()?;
        for component in missing.into_iter().rev() {
            resolved.push(component);
        }
        Self::from_absolute_path(&resolved)
    }

    fn from_absolute_path(path: &Path) -> Option<Self> {
        if !path.is_absolute() {
            return None;
        }
        let mut components = Vec::new();
        for component in path.components() {
            match component {
                Component::Prefix(prefix) => {
                    components.push(normalize_native_path_component(
                        &prefix.as_os_str().to_string_lossy(),
                    ));
                }
                Component::RootDir => {}
                Component::CurDir => {}
                Component::ParentDir => {
                    if components.len() <= 1 {
                        return None;
                    }
                    components.pop();
                }
                Component::Normal(value) => components.push(normalize_native_path_component(
                    &value.to_string_lossy(),
                )),
            }
        }
        (!components.is_empty()).then_some(Self { components })
    }

    fn starts_with(&self, root: &Self) -> bool {
        self.components.starts_with(&root.components)
    }
}

fn native_external_group_label(cwd: &str) -> String {
    let raw = Path::new(cwd)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "project".to_owned());
    let sanitized = raw
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>();
    let sanitized = sanitized.trim();
    let sanitized = if sanitized.is_empty() { "project" } else { sanitized };
    let mut end = sanitized
        .len()
        .min(gate4agent_types::NATIVE_SESSION_EXTERNAL_GROUP_LABEL_MAX_BYTES);
    while !sanitized.is_char_boundary(end) {
        end -= 1;
    }
    sanitized[..end].to_owned()
}

fn native_hash_path(mut hash: u64, path: &NativePathKey) -> u64 {
    for component in &path.components {
        hash = native_hash_bytes(hash, &(component.len() as u64).to_le_bytes());
        hash = native_hash_bytes(hash, component.as_bytes());
    }
    hash
}

fn native_hash_bytes(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(windows)]
fn normalize_native_path_component(value: &str) -> String {
    if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{unc}").to_lowercase()
    } else {
        value.trim_start_matches(r"\\?\").to_lowercase()
    }
}

#[cfg(not(windows))]
fn normalize_native_path_component(value: &str) -> String {
    value.to_owned()
}

#[derive(Clone, Copy)]
enum NativeSessionPreviewSelector<'a> {
    Candidate(&'a str),
    Session(&'a str),
}

fn normalize_preview_text(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut end = normalized.len().min(gate4agent_types::NATIVE_SESSION_PREVIEW_TEXT_MAX_BYTES);
    while !normalized.is_char_boundary(end) {
        end -= 1;
    }
    normalized[..end]
        .chars()
        .filter(|character| {
            !character.is_control() || matches!(character, '\n' | '\t')
        })
        .collect()
}

fn project_native_session_preview(
    session: gate4agent_adapters::HistorySession,
    modified_at_unix_ms: Option<u64>,
    source_truncated: bool,
    message_limit: u16,
) -> Result<NativeSessionPreview, NativeSessionPreviewError> {
    let skip = session
        .messages
        .len()
        .saturating_sub(usize::from(message_limit));
    let messages: Vec<NativeSessionPreviewMessage> = session
        .messages
        .into_iter()
        .skip(skip)
        .map(|message| NativeSessionPreviewMessage {
            role: match message.role {
                gate4agent_adapters::HistoryRole::User => HistoryMessageRole::User,
                gate4agent_adapters::HistoryRole::Assistant => HistoryMessageRole::Assistant,
            },
            text: normalize_preview_text(&message.text),
        })
        .collect();
    let truncated = source_truncated || session.message_count > messages.len() as u64;
    let preview = NativeSessionPreview {
        session_id: session.session_id,
        title: session.title,
        modified_at_unix_ms,
        model: session.model,
        message_count: session.message_count,
        message_count_exact: !source_truncated,
        completed_turn_count: session.completed_turn_count,
        total_tokens: session
            .total_tokens_observed
            .then_some(session.total_tokens),
        truncated,
        messages,
    };
    preview
        .validate()
        .map_err(|_| NativeSessionPreviewError::PreviewUnavailable)?;
    Ok(preview)
}

fn native_catalog_window_for_modified(
    modified_at_unix_ms: Option<u64>,
    recent_cutoff: u64,
) -> NativeSessionCatalogWindow {
    if modified_at_unix_ms.is_some_and(|modified| modified >= recent_cutoff) {
        NativeSessionCatalogWindow::Recent
    } else {
        NativeSessionCatalogWindow::Older
    }
}

fn native_catalog_snapshot_revision(index_revision: u64, cutoff_unix_ms: u64) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in index_revision
        .to_le_bytes()
        .into_iter()
        .chain(cutoff_unix_ms.to_le_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash.max(1)
}

fn scoped_native_catalog_snapshot_revision(
    index_revision: u64,
    cutoff_unix_ms: u64,
    ownership_fingerprint: u64,
) -> u64 {
    native_hash_bytes(
        native_catalog_snapshot_revision(index_revision, cutoff_unix_ms),
        &ownership_fingerprint.to_le_bytes(),
    )
    .max(1)
}

fn unscoped_native_catalog_page(
    page: ScopedNativeSessionCatalogPage,
) -> NativeSessionCatalogPage {
    NativeSessionCatalogPage {
        revision: page.revision,
        cutoff_unix_ms: page.cutoff_unix_ms,
        window: page.window,
        entries: page
            .entries
            .into_iter()
            .map(|entry| entry.metadata)
            .collect(),
        recent_total_count: page.recent_total_count,
        older_total_count: page.older_total_count,
        next_after_selection_id: page.next_after_selection_id,
        remaining_count: page.remaining_count,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NativeSessionIndexError {
    UnsupportedProvider,
    Unavailable,
}

fn map_catalog_index_error(error: NativeSessionIndexError) -> NativeSessionCatalogError {
    match error {
        NativeSessionIndexError::UnsupportedProvider => {
            NativeSessionCatalogError::UnsupportedProvider
        }
        NativeSessionIndexError::Unavailable => NativeSessionCatalogError::CatalogUnavailable,
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeSessionCatalogError {
    #[error("native session catalog limit is outside the supported bounded range")]
    InvalidLimit,
    #[error("native session catalog provider is unsupported")]
    UnsupportedProvider,
    #[error("native session catalog workspace is unavailable")]
    WorkspaceUnavailable,
    #[error("native session catalog is unavailable")]
    CatalogUnavailable,
    #[error("native session catalog revision or cursor is stale")]
    StaleCatalog,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum NativeSessionPreviewError {
    #[error("native session preview limit is outside the supported bounded range")]
    InvalidLimit,
    #[error("native session preview provider is unsupported")]
    UnsupportedProvider,
    #[error("native session preview workspace is unavailable")]
    WorkspaceUnavailable,
    #[error("native session preview is unavailable")]
    PreviewUnavailable,
    #[error("native session preview was not found")]
    SessionNotFound,
    #[error("native session catalog revision is stale")]
    StaleCatalog,
    #[error("native session preview is ambiguous")]
    AmbiguousSession,
}

fn drain_queue<T>(queue: &mut VecDeque<T>, limit: usize) -> Vec<T> {
    let count = limit.min(queue.len());
    queue.drain(..count).collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NativeRuntimeConfig {
    pub command_capacity: usize,
    pub max_commands_per_tick: usize,
    pub effect_capacity_per_session: usize,
    pub observation_capacity: usize,
    pub max_observations_per_tick: usize,
    pub provider_stop_grace_ms: u64,
    pub provider_shutdown_timeout_ms: u64,
    pub worker_poll_interval_ms: u64,
    pub worker_idle_timeout_ms: u64,
}

impl Default for NativeRuntimeConfig {
    fn default() -> Self {
        Self {
            command_capacity: 256,
            max_commands_per_tick: 64,
            effect_capacity_per_session: 16,
            observation_capacity: 1_024,
            max_observations_per_tick: 256,
            provider_stop_grace_ms: 5_000,
            provider_shutdown_timeout_ms: 15_000,
            worker_poll_interval_ms: 20,
            worker_idle_timeout_ms: 60_000,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NativeRuntimeTick {
    pub command_outcomes: Vec<CommandOutcome>,
    pub effects_dispatched: usize,
    pub observations_applied: usize,
    pub terminal_frames_collected: usize,
    pub snapshot_revision: u64,
    pub publish_report: PublishReport,
}

/// Owns the kernel tick and all native effect workers. Product apps retain
/// only the bounded handle returned by [`NativeRuntime::new`].
pub struct NativeRuntime {
    config: NativeRuntimeConfig,
    kernel: Gate4AgentKernel,
    handle: Gate4AgentHandle,
    tool_authority: ToolAuthorityHandle,
    tool_providers: BTreeMap<ToolProviderId, CapabilityProviderDescriptor>,
    provider_supervisors: BTreeMap<ToolProviderId, ProviderSupervisor>,
    port: ControlPlaneKernelPort,
    provider_exit_acks: VecDeque<PhysicalExitAck>,
    provider_faults: VecDeque<ProviderSupervisorFault>,
    provider_ack_cursor: Option<ToolProviderId>,
    provider_fault_cursor: Option<ToolProviderId>,
    effects: NativeEffectDispatcher,
    hook_ingress: Option<HookIngressServer>,
}

impl NativeRuntime {
    pub fn new(catalog: AgentRegistry, config: NativeRuntimeConfig) -> (Gate4AgentHandle, Self) {
        Self::new_with_optional_history(catalog, config, None)
    }

    pub fn new_with_history(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history: NativeHistoryConfig,
    ) -> (Gate4AgentHandle, Self) {
        Self::new_with_optional_history(catalog, config, Some(history))
    }

    pub fn new_with_tool_providers(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        providers: impl IntoIterator<Item = CapabilityProviderDescriptor>,
    ) -> Result<(Gate4AgentHandle, Self), ToolEngineError> {
        let providers = providers.into_iter().collect::<Vec<_>>();
        let kernel = Gate4AgentKernel::with_tool_providers(catalog.clone(), providers.clone())?;
        Ok(Self::new_with_kernel_and_optional_history(
            catalog, config, None, kernel, providers,
        ))
    }

    pub fn new_with_history_and_tool_providers(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history: NativeHistoryConfig,
        providers: impl IntoIterator<Item = CapabilityProviderDescriptor>,
    ) -> Result<(Gate4AgentHandle, Self), ToolEngineError> {
        let providers = providers.into_iter().collect::<Vec<_>>();
        let kernel = Gate4AgentKernel::with_tool_providers(catalog.clone(), providers.clone())?;
        Ok(Self::new_with_kernel_and_optional_history(
            catalog,
            config,
            Some(history),
            kernel,
            providers,
        ))
    }

    fn new_with_optional_history(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history: Option<NativeHistoryConfig>,
    ) -> (Gate4AgentHandle, Self) {
        let kernel = Gate4AgentKernel::new(catalog.clone());
        Self::new_with_kernel_and_optional_history(catalog, config, history, kernel, Vec::new())
    }

    fn new_with_kernel_and_optional_history(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history: Option<NativeHistoryConfig>,
        kernel: Gate4AgentKernel,
        providers: Vec<CapabilityProviderDescriptor>,
    ) -> (Gate4AgentHandle, Self) {
        let (handle, tool_authority, port) = bounded_control_plane(config.command_capacity);
        let runtime = Self {
            config,
            kernel,
            handle: handle.clone(),
            tool_authority,
            tool_providers: providers
                .into_iter()
                .map(|descriptor| (descriptor.id.clone(), descriptor))
                .collect(),
            provider_supervisors: BTreeMap::new(),
            port,
            provider_exit_acks: VecDeque::new(),
            provider_faults: VecDeque::new(),
            provider_ack_cursor: None,
            provider_fault_cursor: None,
            effects: NativeEffectDispatcher::new(catalog, config, history),
            hook_ingress: None,
        };
        (handle, runtime)
    }

    pub fn tool_authority(&self) -> ToolAuthorityHandle {
        self.tool_authority.clone()
    }

    pub fn install_native_provider(
        &mut self,
        provider_id: &ToolProviderId,
        work_capacity: usize,
        executor: Box<dyn NativeProviderExecutor>,
    ) -> Result<ProviderBindingId, NativeProviderControlError> {
        self.collect_provider_supervisor_events();
        if let Some(existing) = self.provider_supervisors.get(provider_id) {
            let snapshot = existing.snapshot();
            if snapshot.state != ProviderSupervisorState::Closed {
                return Err(NativeProviderControlError::AlreadyInstalled {
                    state: snapshot.state,
                });
            }
            if snapshot.buffered_exit_acks != 0 || snapshot.buffered_faults != 0 {
                return Err(NativeProviderControlError::PendingSupervisorEvents);
            }
        }
        self.provider_supervisors.remove(provider_id);

        let descriptor = self
            .tool_providers
            .get(provider_id)
            .cloned()
            .ok_or_else(|| NativeProviderControlError::UnknownProvider {
                provider_id: provider_id.clone(),
            })?;
        if !matches!(&descriptor.owner, CapabilityOwner::Gate) {
            return Err(NativeProviderControlError::UnsupportedOwner {
                provider_id: provider_id.clone(),
            });
        }
        let runtime = self
            .port
            .provider_authority()
            .bind_provider(provider_id.clone(), work_capacity)?;
        let binding_id = runtime.binding_id();
        let supervisor = ProviderSupervisor::new_with_stop_grace(
            descriptor,
            runtime,
            executor,
            Duration::from_millis(self.config.provider_stop_grace_ms.max(1)),
        )
        .map_err(NativeProviderControlError::Build)?;
        self.provider_supervisors
            .insert(provider_id.clone(), supervisor);
        Ok(binding_id)
    }

    pub fn retire_native_provider(
        &mut self,
        provider_id: &ToolProviderId,
    ) -> Result<(), NativeProviderControlError> {
        let supervisor = self
            .provider_supervisors
            .get_mut(provider_id)
            .ok_or_else(|| NativeProviderControlError::NotInstalled {
                provider_id: provider_id.clone(),
            })?;
        supervisor.begin_retirement()?;
        Ok(())
    }

    /// Begins non-blocking retirement for every installed native provider.
    ///
    /// The owner must keep calling [`NativeRuntime::tick`] until
    /// [`NativeRuntime::native_provider_shutdown_complete`] returns `true`
    /// before dropping the runtime. Dropping the runtime does not acknowledge
    /// physical provider exit or complete coordinated shutdown.
    pub fn retire_all_native_providers(&mut self) -> Result<(), NativeProviderControlError> {
        let mut first_error = None;
        for supervisor in self.provider_supervisors.values_mut() {
            if let Err(error) = supervisor.begin_retirement() {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error.into()),
            None => Ok(()),
        }
    }

    /// Returns `true` only after every installed provider supervisor is closed.
    pub fn native_provider_shutdown_complete(&self) -> bool {
        self.provider_supervisors
            .values()
            .all(|supervisor| supervisor.state() == ProviderSupervisorState::Closed)
    }

    /// Drives the canonical runtime path until every native provider has
    /// detached after physical teardown, or returns the still-owned snapshots
    /// at the configured shutdown deadline. Lifecycle events remain buffered
    /// for explicit operator drain after this method returns.
    pub async fn shutdown_native_providers(
        &mut self,
    ) -> Result<(), NativeProviderShutdownError> {
        let retirement_error = self.retire_all_native_providers().err();
        let shutdown_timeout_ms = self
            .config
            .provider_shutdown_timeout_ms
            .clamp(1, MAX_PROVIDER_SHUTDOWN_TIMEOUT_MS);
        let deadline = Instant::now() + Duration::from_millis(shutdown_timeout_ms);
        while !self.native_provider_shutdown_complete() {
            self.tick().await;
            if self.native_provider_shutdown_complete() {
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(NativeProviderShutdownError::TimedOut {
                    pending: self
                        .provider_supervisors
                        .values()
                        .filter(|supervisor| {
                            supervisor.state() != ProviderSupervisorState::Closed
                        })
                        .map(ProviderSupervisor::snapshot)
                        .collect(),
                });
            }
            let poll_interval =
                Duration::from_millis(self.config.worker_poll_interval_ms.max(1));
            tokio::time::sleep(poll_interval.min(deadline.duration_since(now))).await;
        }
        match retirement_error {
            Some(error) => Err(NativeProviderShutdownError::Control(error)),
            None => Ok(()),
        }
    }

    pub fn native_provider_snapshot(
        &self,
        provider_id: &ToolProviderId,
    ) -> Option<ProviderSupervisorSnapshot> {
        self.provider_supervisors
            .get(provider_id)
            .map(ProviderSupervisor::snapshot)
    }

    pub fn drain_provider_exit_acks(&mut self, limit: usize) -> Vec<PhysicalExitAck> {
        self.collect_provider_supervisor_events();
        drain_queue(&mut self.provider_exit_acks, limit)
    }

    pub fn drain_provider_faults(&mut self, limit: usize) -> Vec<ProviderSupervisorFault> {
        self.collect_provider_supervisor_events();
        drain_queue(&mut self.provider_faults, limit)
    }

    pub fn history_enabled(&self) -> bool {
        self.effects.history_config.is_some()
    }

    /// Run one non-blocking host tick. Effects are dispatched in session order
    /// to per-instance workers; their observations enter later ticks.
    pub async fn tick(&mut self) -> NativeRuntimeTick {
        let (observations, terminal_frames_collected) = self
            .effects
            .drain_observations(self.config.max_observations_per_tick.max(1));
        let observations_applied = observations.len();
        let ingress = self
            .port
            .drain_ingress(self.config.max_commands_per_tick.max(1));
        let step = self.kernel.step_control_plane(ingress, observations);
        let effects_dispatched = step.effects.len();
        for effect in step.effects.iter().cloned() {
            self.effects.dispatch(effect);
        }

        let snapshot_revision = step.snapshot.revision;
        let publish_report = self.port.publish_step(&step).control_events;
        for supervisor in self.provider_supervisors.values_mut() {
            supervisor.tick();
        }
        self.collect_provider_supervisor_events();

        NativeRuntimeTick {
            command_outcomes: step.command_outcomes,
            effects_dispatched,
            observations_applied,
            terminal_frames_collected,
            snapshot_revision,
            publish_report,
        }
    }

    fn collect_provider_supervisor_events(&mut self) {
        self.collect_provider_exit_acks();
        self.collect_provider_faults();
    }

    fn collect_provider_exit_acks(&mut self) {
        let remaining =
            MAX_PROVIDER_SUPERVISOR_EVENTS.saturating_sub(self.provider_exit_acks.len());
        if remaining == 0 {
            return;
        }
        let provider_ids = provider_ids_after(
            &self.provider_supervisors,
            self.provider_ack_cursor.as_ref(),
        );
        let mut remaining = remaining;
        for provider_id in provider_ids {
            let limit = remaining.min(PROVIDER_EVENT_DRAIN_QUANTUM);
            let drained = self
                .provider_supervisors
                .get_mut(&provider_id)
                .map_or_else(Vec::new, |supervisor| supervisor.drain_exit_acks(limit));
            if !drained.is_empty() {
                remaining -= drained.len();
                self.provider_exit_acks.extend(drained);
                self.provider_ack_cursor = Some(provider_id);
            }
            if remaining == 0 {
                break;
            }
        }
    }

    fn collect_provider_faults(&mut self) {
        let remaining =
            MAX_PROVIDER_SUPERVISOR_EVENTS.saturating_sub(self.provider_faults.len());
        if remaining == 0 {
            return;
        }
        let provider_ids = provider_ids_after(
            &self.provider_supervisors,
            self.provider_fault_cursor.as_ref(),
        );
        let mut remaining = remaining;
        for provider_id in provider_ids {
            let limit = remaining.min(PROVIDER_EVENT_DRAIN_QUANTUM);
            let drained = self
                .provider_supervisors
                .get_mut(&provider_id)
                .map_or_else(Vec::new, |supervisor| supervisor.drain_faults(limit));
            if !drained.is_empty() {
                remaining -= drained.len();
                self.provider_faults.extend(drained);
                self.provider_fault_cursor = Some(provider_id);
            }
            if remaining == 0 {
                break;
            }
        }
    }

    pub fn active_native_sessions(&self) -> usize {
        self.effects.active_sessions.load(Ordering::Acquire)
    }

    /// Installs or replaces a bounded host-only profile for future spawns.
    pub fn upsert_native_launch_profile(
        &mut self,
        profile: NativeLaunchProfile,
    ) -> Result<(), NativeLaunchProfileError> {
        self.effects.launch_profiles.upsert(profile)
    }

    /// Removes an unselected host-only profile.
    pub fn remove_native_launch_profile(
        &mut self,
        profile_id: &NativeLaunchProfileId,
    ) -> Result<bool, NativeLaunchProfileError> {
        self.effects.launch_profiles.remove(profile_id)
    }

    /// Selects a profile for future spawns of one exact instance.
    ///
    /// Spawn dispatch is the linearization point: a selection change does not
    /// alter a child that was already dispatched or started.
    pub fn select_native_launch_profile(
        &mut self,
        instance_id: AgentInstanceId,
        profile_id: NativeLaunchProfileId,
    ) -> Result<(), NativeLaunchProfileError> {
        self.effects
            .launch_profiles
            .select_native_launch_profile(instance_id, profile_id)
    }

    /// Clears one instance selection for future spawns only.
    pub fn clear_native_launch_profile_selection(
        &mut self,
        instance_id: AgentInstanceId,
    ) -> bool {
        self.effects
            .launch_profiles
            .clear_native_launch_profile_selection(instance_id)
    }

    /// Returns a clonable selector for future native launch-profile spawns.
    pub fn native_launch_profile_control(&self) -> NativeLaunchProfileControl {
        self.effects.launch_profiles.clone()
    }

    /// Start the process-global loopback Hook listener. Call this before
    /// starting agent sessions so their PTY environments receive route-scoped
    /// endpoint coordinates.
    pub async fn start_hook_ingress(
        &mut self,
        config: HookIngressConfig,
    ) -> Result<HookIngressEndpoint, NativeHookIngressError> {
        if let Some(server) = &self.hook_ingress {
            if server.is_running() {
                return Ok(server.endpoint().clone());
            }
        }
        let has_started_session = self.handle.snapshot().sessions.iter().any(|session| {
            matches!(
                session.status,
                SessionStatus::Starting | SessionStatus::Running | SessionStatus::Stopping
            )
        });
        if self.active_native_sessions() != 0 || has_started_session {
            return Err(NativeHookIngressError::ActiveSessions);
        }
        self.hook_ingress = None;
        let server = HookIngressServer::start(self.handle.clone(), config).await?;
        let endpoint = server.endpoint().clone();
        self.effects.set_hook_ingress(Some(server.control()));
        self.hook_ingress = Some(server);
        Ok(endpoint)
    }

    pub async fn stop_hook_ingress(&mut self) {
        self.effects.set_hook_ingress(None);
        if let Some(server) = self.hook_ingress.take() {
            server.stop().await;
        }
    }

    pub fn hook_ingress_endpoint(&self) -> Option<&HookIngressEndpoint> {
        self.hook_ingress
            .as_ref()
            .filter(|server| server.is_running())
            .map(HookIngressServer::endpoint)
    }

    pub fn active_hook_routes(&self) -> usize {
        self.hook_ingress
            .as_ref()
            .map_or(0, |server| server.control().active_route_count())
    }
}

fn provider_ids_after(
    supervisors: &BTreeMap<ToolProviderId, ProviderSupervisor>,
    cursor: Option<&ToolProviderId>,
) -> Vec<ToolProviderId> {
    let mut provider_ids = supervisors.keys().cloned().collect::<Vec<_>>();
    let Some(cursor) = cursor else {
        return provider_ids;
    };
    let start = provider_ids
        .iter()
        .position(|provider_id| provider_id > cursor)
        .unwrap_or(0);
    provider_ids.rotate_left(start);
    provider_ids
}

#[derive(Debug, Error)]
pub enum NativeProviderControlError {
    #[error("tool provider '{provider_id}' is not registered in this native runtime")]
    UnknownProvider { provider_id: ToolProviderId },
    #[error("tool provider '{provider_id}' is not owned by gate4agent")]
    UnsupportedOwner { provider_id: ToolProviderId },
    #[error("native provider supervisor is already installed in state {state:?}")]
    AlreadyInstalled { state: ProviderSupervisorState },
    #[error("native provider supervisor is not installed for '{provider_id}'")]
    NotInstalled { provider_id: ToolProviderId },
    #[error("retired provider supervisor still has undelivered lifecycle events")]
    PendingSupervisorEvents,
    #[error("native provider runtime failed: {0}")]
    Runtime(#[from] ProviderRuntimeError),
    #[error("native provider supervisor build failed: {0:?}")]
    Build(ProviderSupervisorBuildError),
}

#[derive(Debug, Error)]
pub enum NativeProviderShutdownError {
    #[error("native provider retirement reported an error: {0}")]
    Control(NativeProviderControlError),
    #[error("native provider shutdown timed out with physical owners retained")]
    TimedOut { pending: Vec<ProviderSupervisorSnapshot> },
}

#[derive(Debug, Error)]
pub enum NativeHookIngressError {
    #[error("hook ingress must start before native agent sessions")]
    ActiveSessions,
    #[error(transparent)]
    Start(#[from] HookIngressStartError),
}

struct EffectWorker {
    sender: Sender<NativeEffectRequest>,
}

struct NativeSpawnOverlay {
    environment: Vec<EnvMutation>,
    extra_args: Vec<OsString>,
    one_shot_session_persistence: OneShotSessionPersistence,
    qwen_sidecar: Option<QwenDualOutputLaunch>,
}

impl Default for NativeSpawnOverlay {
    fn default() -> Self {
        Self {
            environment: Vec::new(),
            extra_args: Vec::new(),
            one_shot_session_persistence: OneShotSessionPersistence::Ephemeral,
            qwen_sidecar: None,
        }
    }
}

struct NativeEffectRequest {
    effect: EffectEnvelope,
    spawn_env: Vec<EnvMutation>,
    spawn_extra_args: Vec<OsString>,
    one_shot_session_persistence: OneShotSessionPersistence,
    qwen_sidecar: Option<QwenDualOutputLaunch>,
}

#[derive(Clone)]
struct NativeWorkerContext {
    control_tx: Sender<ObservationEnvelope>,
    terminal_frames: Arc<Mutex<BTreeMap<TerminalFrameKey, ObservationEnvelope>>>,
    active_sessions: Arc<AtomicUsize>,
    hook_ingress: Arc<RwLock<Option<HookIngressControl>>>,
    poll_interval: Duration,
    idle_timeout: Duration,
}

struct NativeEffectDispatcher {
    catalog: AgentRegistry,
    config: NativeRuntimeConfig,
    launch_profiles: NativeLaunchProfileControl,
    workers: HashMap<AgentInstanceId, EffectWorker>,
    authority_worker: Option<EffectWorker>,
    capability_worker: Option<EffectWorker>,
    history_config: Option<NativeHistoryConfig>,
    control_tx: Sender<ObservationEnvelope>,
    control_rx: Receiver<ObservationEnvelope>,
    pending_failures: VecDeque<ObservationEnvelope>,
    terminal_frames: Arc<Mutex<BTreeMap<TerminalFrameKey, ObservationEnvelope>>>,
    active_sessions: Arc<AtomicUsize>,
    hook_ingress: Arc<RwLock<Option<HookIngressControl>>>,
}

impl NativeEffectDispatcher {
    fn new(
        catalog: AgentRegistry,
        config: NativeRuntimeConfig,
        history_config: Option<NativeHistoryConfig>,
    ) -> Self {
        let (control_tx, control_rx) = mpsc::channel(config.observation_capacity.max(1));
        Self {
            catalog,
            config,
            launch_profiles: NativeLaunchProfileControl::new(),
            workers: HashMap::new(),
            authority_worker: None,
            capability_worker: None,
            history_config,
            control_tx,
            control_rx,
            pending_failures: VecDeque::new(),
            terminal_frames: Arc::new(Mutex::new(BTreeMap::new())),
            active_sessions: Arc::new(AtomicUsize::new(0)),
            hook_ingress: Arc::new(RwLock::new(None)),
        }
    }

    fn set_hook_ingress(&self, control: Option<HookIngressControl>) {
        *self
            .hook_ingress
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = control;
    }

    fn dispatch(&mut self, effect: EffectEnvelope) {
        if matches!(effect.effect, ControlEffect::ProbeCapabilities { .. }) {
            self.dispatch_capability(effect);
            return;
        }
        if matches!(
            effect.effect,
            ControlEffect::DiscoverHistory { .. }
                | ControlEffect::LoadHistory { .. }
                | ControlEffect::AuthorizeResume { .. }
        ) {
            self.dispatch_authority(effect);
            return;
        }
        self.workers.retain(|_, worker| !worker.sender.is_closed());
        if let Err(message) = validate_effect_runtime_policy(&effect.effect) {
            self.pending_failures
                .push_back(effect_failure(effect, message));
            return;
        }
        let instance_id = effect.instance_id;
        let spawn_overlay = match self.compose_spawn_overlay(&effect) {
            Ok(overlay) => overlay,
            Err(message) => {
                self.pending_failures
                    .push_back(effect_failure(effect, message));
                return;
            }
        };
        let mut pending = NativeEffectRequest {
            effect,
            spawn_env: spawn_overlay.environment,
            spawn_extra_args: spawn_overlay.extra_args,
            one_shot_session_persistence: spawn_overlay.one_shot_session_persistence,
            qwen_sidecar: spawn_overlay.qwen_sidecar,
        };
        for _ in 0..2 {
            let sender = self.worker_sender(instance_id);
            match sender.try_send(pending) {
                Ok(()) => return,
                Err(TrySendError::Closed(request)) => {
                    self.workers.remove(&instance_id);
                    pending = request;
                }
                Err(TrySendError::Full(request)) => {
                    self.remove_hook_route(&request.effect);
                    self.pending_failures.push_back(effect_failure(
                        request.effect,
                        "native session effect queue is full".to_owned(),
                    ));
                    return;
                }
            }
        }
        self.remove_hook_route(&pending.effect);
        self.pending_failures.push_back(effect_failure(
            pending.effect,
            "native session effect worker is unavailable".to_owned(),
        ));
    }

    fn dispatch_capability(&mut self, effect: EffectEnvelope) {
        let mut pending = NativeEffectRequest {
            effect,
            spawn_env: Vec::new(),
            spawn_extra_args: Vec::new(),
            one_shot_session_persistence: OneShotSessionPersistence::Ephemeral,
            qwen_sidecar: None,
        };
        for _ in 0..2 {
            let sender = self.capability_sender();
            match sender.try_send(pending) {
                Ok(()) => return,
                Err(TrySendError::Closed(request)) => {
                    self.capability_worker = None;
                    pending = request;
                }
                Err(TrySendError::Full(request)) => {
                    self.pending_failures.push_back(effect_failure(
                        request.effect,
                        "native capability effect queue is full".to_owned(),
                    ));
                    return;
                }
            }
        }
        self.pending_failures.push_back(effect_failure(
            pending.effect,
            "native capability effect worker is unavailable".to_owned(),
        ));
    }

    fn capability_sender(&mut self) -> Sender<NativeEffectRequest> {
        if let Some(worker) = &self.capability_worker {
            if !worker.sender.is_closed() {
                return worker.sender.clone();
            }
        }
        let (sender, receiver) = mpsc::channel(self.config.effect_capacity_per_session.max(1));
        tokio::spawn(run_capability_worker(
            self.catalog.clone(),
            receiver,
            self.control_tx.clone(),
        ));
        self.capability_worker = Some(EffectWorker {
            sender: sender.clone(),
        });
        sender
    }

    fn dispatch_authority(&mut self, effect: EffectEnvelope) {
        if self.history_config.is_none()
            && matches!(
                effect.effect,
                ControlEffect::DiscoverHistory { .. } | ControlEffect::LoadHistory { .. }
            )
        {
            self.pending_failures.push_back(effect_failure(
                effect,
                "native history authority is not configured".to_owned(),
            ));
            return;
        }
        let mut pending = NativeEffectRequest {
            effect,
            spawn_env: Vec::new(),
            spawn_extra_args: Vec::new(),
            one_shot_session_persistence: OneShotSessionPersistence::Ephemeral,
            qwen_sidecar: None,
        };
        for _ in 0..2 {
            let sender = self.authority_sender();
            match sender.try_send(pending) {
                Ok(()) => return,
                Err(TrySendError::Closed(request)) => {
                    self.authority_worker = None;
                    pending = request;
                }
                Err(TrySendError::Full(request)) => {
                    self.pending_failures.push_back(effect_failure(
                        request.effect,
                        "native authority effect queue is full".to_owned(),
                    ));
                    return;
                }
            }
        }
        self.pending_failures.push_back(effect_failure(
            pending.effect,
            "native authority effect worker is unavailable".to_owned(),
        ));
    }

    fn authority_sender(&mut self) -> Sender<NativeEffectRequest> {
        if let Some(worker) = &self.authority_worker {
            if !worker.sender.is_closed() {
                return worker.sender.clone();
            }
        }
        let (sender, receiver) = mpsc::channel(self.config.effect_capacity_per_session.max(1));
        tokio::spawn(run_authority_worker(
            self.catalog.clone(),
            self.history_config.clone(),
            receiver,
            self.control_tx.clone(),
        ));
        self.authority_worker = Some(EffectWorker {
            sender: sender.clone(),
        });
        sender
    }

    fn hook_pty_env(&self, effect: &EffectEnvelope) -> Result<Vec<EnvMutation>, String> {
        let agent_id = match &effect.effect {
            ControlEffect::Spawn {
                agent_id,
                transport: gate4agent_types::TransportKind::Pty,
                ..
            }
            | ControlEffect::SpawnResume {
                agent_id,
                transport: gate4agent_types::TransportKind::Pty,
                ..
            } => agent_id,
            _ => return Ok(Vec::new()),
        };
        let control = self
            .hook_ingress
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        let Some(control) = control else {
            return Ok(Vec::new());
        };
        let binding = self
            .catalog
            .get(agent_id)
            .and_then(|spec| spec.capabilities.adapters.hook.clone());
        let Some(binding) = binding else {
            return Ok(Vec::new());
        };
        let route = control
            .register_route(effect.instance_id, effect.generation, binding)
            .map_err(|error| error.to_string())?;
        Ok(route
            .environment()
            .into_iter()
            .map(|(key, value)| EnvMutation {
                key: key.into(),
                value: Some(value.into()),
            })
            .collect())
    }

    fn compose_spawn_overlay(
        &self,
        effect: &EffectEnvelope,
    ) -> Result<NativeSpawnOverlay, String> {
        let mut overlay = self.profile_spawn_overlay(effect)?;
        overlay.environment.extend(self.hook_pty_env(effect)?);
        overlay.qwen_sidecar = self.qwen_pty_sidecar(effect);
        Ok(overlay)
    }

    fn qwen_pty_sidecar(&self, effect: &EffectEnvelope) -> Option<QwenDualOutputLaunch> {
        let agent_id = match &effect.effect {
            ControlEffect::Spawn {
                agent_id,
                transport: TransportKind::Pty,
                ..
            }
            | ControlEffect::SpawnResume {
                agent_id,
                transport: TransportKind::Pty,
                ..
            } => agent_id,
            _ => return None,
        };
        let binding = self
            .catalog
            .get(agent_id)?
            .capabilities
            .adapters
            .pty_sidecar
            .clone()?;
        Some(
            QwenDualOutputLaunch::prepare(binding.clone())
                .unwrap_or_else(|_| QwenDualOutputLaunch::unavailable(binding)),
        )
    }

    fn profile_spawn_overlay(&self, effect: &EffectEnvelope) -> Result<NativeSpawnOverlay, String> {
        let (agent_id, transport) = match &effect.effect {
            ControlEffect::Spawn {
                agent_id,
                transport,
                ..
            } => (agent_id, *transport),
            ControlEffect::SpawnResume {
                agent_id,
                transport,
                ..
            } => (agent_id, *transport),
            _ => return Ok(NativeSpawnOverlay::default()),
        };
        let pipe_binding_is_exact_one_shot = transport != TransportKind::Pipe
            || self.catalog.get(agent_id).is_some_and(|spec| {
                spec.capabilities.transports.pipe.as_ref().is_some_and(|pipe| {
                    pipe.protocol == PipeProtocol::OneShotText
                        && spec.capabilities.adapters.one_shot.as_ref() == Some(&pipe.adapter)
                })
            });
        self.launch_profiles
            .resolve_launch_overlay(
                effect.instance_id,
                agent_id,
                transport,
                pipe_binding_is_exact_one_shot,
            )
            .map(|overlay| NativeSpawnOverlay {
                environment: overlay.environment,
                extra_args: overlay.extra_args,
                one_shot_session_persistence: overlay.one_shot_session_persistence,
                qwen_sidecar: None,
            })
            .map_err(|error| error.to_string())
    }

    fn remove_hook_route(&self, effect: &EffectEnvelope) {
        if !matches!(
            effect.effect,
            ControlEffect::Spawn { .. } | ControlEffect::SpawnResume { .. }
        ) {
            return;
        }
        if let Some(control) = self
            .hook_ingress
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            control.remove_route(effect.instance_id, effect.generation);
        }
    }

    fn worker_sender(&mut self, instance_id: AgentInstanceId) -> Sender<NativeEffectRequest> {
        if let Some(worker) = self.workers.get(&instance_id) {
            return worker.sender.clone();
        }
        let (sender, receiver) = mpsc::channel(self.config.effect_capacity_per_session.max(1));
        tokio::spawn(run_effect_worker(
            self.catalog.clone(),
            receiver,
            NativeWorkerContext {
                control_tx: self.control_tx.clone(),
                terminal_frames: Arc::clone(&self.terminal_frames),
                active_sessions: Arc::clone(&self.active_sessions),
                hook_ingress: Arc::clone(&self.hook_ingress),
                poll_interval: Duration::from_millis(self.config.worker_poll_interval_ms.max(1)),
                idle_timeout: Duration::from_millis(self.config.worker_idle_timeout_ms.max(1)),
            },
        ));
        self.workers.insert(
            instance_id,
            EffectWorker {
                sender: sender.clone(),
            },
        );
        sender
    }

    fn drain_observations(&mut self, limit: usize) -> (Vec<ObservationEnvelope>, usize) {
        self.workers.retain(|_, worker| !worker.sender.is_closed());
        let mut observations = Vec::new();
        while observations.len() < limit {
            if let Some(observation) = self.pending_failures.pop_front() {
                observations.push(observation);
                continue;
            }
            match self.control_rx.try_recv() {
                Ok(observation) => observations.push(observation),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }

        let remaining = limit.saturating_sub(observations.len());
        let mut frames = self
            .terminal_frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let keys: Vec<_> = frames.keys().copied().take(remaining).collect();
        let terminal_frames_collected = keys.len();
        for key in keys {
            if let Some(frame) = frames.remove(&key) {
                observations.push(frame);
            }
        }
        (observations, terminal_frames_collected)
    }
}

const HISTORY_INSTANCE_DISCOVERIES_MAX: usize = 1_024;

struct InstanceHistoryDiscovery {
    generation: SessionGeneration,
    agent_id: AgentId,
    request: HistoryDiscoveryRequest,
    candidates: HashMap<String, HistoryCandidate>,
}

struct NativeAuthorityWorkerState {
    catalog: AgentRegistry,
    authority: Option<NativeHistoryAuthority>,
    discoveries: HashMap<AgentInstanceId, InstanceHistoryDiscovery>,
    discovery_order: VecDeque<AgentInstanceId>,
}

impl NativeAuthorityWorkerState {
    fn new(catalog: AgentRegistry, config: Option<NativeHistoryConfig>) -> Self {
        Self {
            catalog,
            authority: config.map(NativeHistoryAuthority::new),
            discoveries: HashMap::new(),
            discovery_order: VecDeque::new(),
        }
    }

    fn execute(&mut self, envelope: EffectEnvelope) -> ObservationEnvelope {
        let EffectEnvelope {
            protocol_version,
            operation_id,
            instance_id,
            generation,
            effect,
        } = envelope;
        let is_resume = matches!(effect, ControlEffect::AuthorizeResume { .. });
        let observation = if protocol_version != CONTROL_PROTOCOL_VERSION {
            authority_failure(
                is_resume,
                format!(
                    "authority effect protocol version {protocol_version} is unsupported; expected {CONTROL_PROTOCOL_VERSION}"
                ),
            )
        } else {
            match effect {
                ControlEffect::DiscoverHistory { agent_id, query } => {
                    self.discover(instance_id, generation, agent_id, query)
                }
                ControlEffect::LoadHistory {
                    agent_id,
                    candidate_id,
                } => self.load(instance_id, generation, agent_id, candidate_id),
                ControlEffect::AuthorizeResume {
                    agent_id,
                    target,
                    request,
                } => self.authorize_resume(instance_id, generation, agent_id, target, request),
                _ => {
                    history_failure("native authority worker received an invalid effect".to_owned())
                }
            }
        };
        ObservationEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: Some(operation_id),
            instance_id,
            generation,
            observation,
        }
    }

    fn discover(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        agent_id: AgentId,
        query: gate4agent_types::HistoryQuery,
    ) -> ControlObservation {
        let Some(spec) = self.catalog.get(&agent_id).cloned() else {
            return history_failure(format!("agent '{agent_id}' is absent from native catalog"));
        };
        let request =
            match HistoryDiscoveryRequest::from_spec(&spec, query.working_directory, query.limit) {
                Ok(request) => request,
                Err(error) => return history_failure(error.to_string()),
            };
        let Some(authority) = self.authority.as_mut() else {
            return history_failure("native history authority is not configured".to_owned());
        };
        let candidates = match discover_history(authority, &request) {
            Ok(candidates) => candidates,
            Err(error) => return history_failure(error.to_string()),
        };
        let summaries = candidates
            .iter()
            .map(|candidate| HistoryCandidateSummary {
                id: candidate.id().as_str().to_owned(),
                session_id_hint: candidate.session_id_hint().to_owned(),
                modified_at_unix_ms: candidate.modified_at_unix_ms(),
            })
            .collect::<Vec<_>>();
        if summaries
            .iter()
            .any(|candidate| candidate.validate().is_err())
        {
            return history_failure(
                "native history authority returned an invalid candidate".to_owned(),
            );
        }
        let candidates = candidates
            .into_iter()
            .map(|candidate| (candidate.id().as_str().to_owned(), candidate))
            .collect();
        self.discoveries.insert(
            instance_id,
            InstanceHistoryDiscovery {
                generation,
                agent_id,
                request,
                candidates,
            },
        );
        self.touch_discovery(instance_id);
        ControlObservation::HistoryDiscovered {
            candidates: summaries,
        }
    }

    fn load(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        agent_id: AgentId,
        candidate_id: String,
    ) -> ControlObservation {
        let Some(discovery) = self.discoveries.get(&instance_id) else {
            return history_failure("history candidate discovery is expired".to_owned());
        };
        if discovery.generation != generation || discovery.agent_id != agent_id {
            return history_failure("history candidate generation is stale".to_owned());
        }
        let Some(candidate) = discovery.candidates.get(&candidate_id).cloned() else {
            return history_failure("history candidate is expired or unknown".to_owned());
        };
        let request = discovery.request.clone();
        self.touch_discovery(instance_id);
        let load = match HistoryLoadRequest::new(&request, candidate) {
            Ok(load) => load,
            Err(error) => return history_failure(error.to_string()),
        };
        let Some(authority) = self.authority.as_mut() else {
            return history_failure("native history authority is not configured".to_owned());
        };
        match load_history_session(authority, &load) {
            Ok(session) => {
                let session = history_session_record(session);
                if let Err(error) = session.validate() {
                    history_failure(error.to_string())
                } else {
                    ControlObservation::HistoryLoaded { session }
                }
            }
            Err(error) => history_failure(error.to_string()),
        }
    }

    fn authorize_resume(
        &mut self,
        instance_id: AgentInstanceId,
        generation: SessionGeneration,
        agent_id: AgentId,
        target: ResumeAuthorityTarget,
        request: ResumeLaunchRequest,
    ) -> ControlObservation {
        if let Err(error) = request.validate() {
            return resume_failure(error.to_string());
        }
        let Some(spec) = self.catalog.get(&agent_id).cloned() else {
            return resume_failure(format!("agent '{agent_id}' is absent from native catalog"));
        };

        let provider_session = match target {
            ResumeAuthorityTarget::ProviderSession { identity } => identity,
            ResumeAuthorityTarget::HistoryCandidate { candidate_id } => {
                let Some(discovery) = self.discoveries.get(&instance_id) else {
                    return resume_failure("history candidate discovery is expired".to_owned());
                };
                if discovery.generation != generation || discovery.agent_id != agent_id {
                    return resume_failure("history candidate generation is stale".to_owned());
                }
                let Some(candidate) = discovery.candidates.get(&candidate_id).cloned() else {
                    return resume_failure("history candidate is expired or unknown".to_owned());
                };
                let discovery_request = discovery.request.clone();
                self.touch_discovery(instance_id);
                let load = match HistoryLoadRequest::new(&discovery_request, candidate) {
                    Ok(load) => load,
                    Err(error) => return resume_failure(error.to_string()),
                };
                let Some(authority) = self.authority.as_mut() else {
                    return resume_failure("native history authority is not configured".to_owned());
                };
                let session = match load_history_session(authority, &load) {
                    Ok(session) => session,
                    Err(error) => return resume_failure(error.to_string()),
                };
                match authority.resume_provider_session(&load, session.session_id) {
                    Ok(identity) => identity,
                    Err(error) => return resume_failure(error.to_string()),
                }
            }
        };
        let resume_request = match ResumeRequest::from_provider_session(
            &spec,
            provider_session,
            Some(request.working_directory.clone()),
        ) {
            Ok(request) => request,
            Err(error) => return resume_failure(error.to_string()),
        };
        match prepare_resume(&mut ExplicitResumeAuthority, resume_request) {
            Ok(ResumeOutcome::Authorized(prepared)) => {
                if prepared.working_directory() != Some(request.working_directory.as_str()) {
                    return resume_failure(
                        "resume authority changed the requested working directory".to_owned(),
                    );
                }
                ControlObservation::ResumeAuthorized {
                    provider_session: prepared.provider_session().clone(),
                }
            }
            Ok(ResumeOutcome::Denied { reason }) => ControlObservation::ResumeDenied { reason },
            Err(error) => resume_failure(error.to_string()),
        }
    }

    fn touch_discovery(&mut self, instance_id: AgentInstanceId) {
        self.discovery_order
            .retain(|candidate| *candidate != instance_id);
        self.discovery_order.push_back(instance_id);
        while self.discoveries.len() > HISTORY_INSTANCE_DISCOVERIES_MAX {
            if let Some(expired) = self.discovery_order.pop_front() {
                self.discoveries.remove(&expired);
            }
        }
    }
}

async fn run_authority_worker(
    catalog: AgentRegistry,
    config: Option<NativeHistoryConfig>,
    mut effects: Receiver<NativeEffectRequest>,
    control_tx: Sender<ObservationEnvelope>,
) {
    let state = Arc::new(Mutex::new(NativeAuthorityWorkerState::new(catalog, config)));
    while let Some(request) = effects.recv().await {
        let fallback = request.effect.clone();
        let worker_state = Arc::clone(&state);
        let completion = match tokio::task::spawn_blocking(move || {
            worker_state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .execute(request.effect)
        })
        .await
        {
            Ok(completion) => completion,
            Err(_) => effect_failure(fallback, "native authority worker task failed".to_owned()),
        };
        if control_tx.send(completion).await.is_err() {
            break;
        }
    }
}

async fn run_capability_worker(
    catalog: AgentRegistry,
    mut effects: Receiver<NativeEffectRequest>,
    control_tx: Sender<ObservationEnvelope>,
) {
    let mut authority = NativeCapabilityProbeAuthority::default();
    while let Some(request) = effects.recv().await {
        let EffectEnvelope {
            protocol_version,
            operation_id,
            instance_id,
            generation,
            effect,
        } = request.effect;
        let observation = if protocol_version != CONTROL_PROTOCOL_VERSION {
            ControlObservation::CapabilityProbeFailed {
                failure: CapabilityProbeFailure::AuthorityRejected,
            }
        } else if let ControlEffect::ProbeCapabilities { agent_id, request } = effect {
            if request.validate().is_err() {
                ControlObservation::CapabilityProbeFailed {
                    failure: CapabilityProbeFailure::AuthorityRejected,
                }
            } else if let Some(spec) = catalog.get(&agent_id) {
                match authority.probe(spec, &request.working_directory).await {
                    Ok(session_option_models) => ControlObservation::CapabilitiesProbed {
                        session_option_models,
                    },
                    Err(failure) => ControlObservation::CapabilityProbeFailed { failure },
                }
            } else {
                ControlObservation::CapabilityProbeFailed {
                    failure: CapabilityProbeFailure::AuthorityRejected,
                }
            }
        } else {
            ControlObservation::CapabilityProbeFailed {
                failure: CapabilityProbeFailure::AuthorityRejected,
            }
        };
        if control_tx
            .send(ObservationEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: Some(operation_id),
                instance_id,
                generation,
                observation,
            })
            .await
            .is_err()
        {
            break;
        }
    }
}

fn history_failure(message: String) -> ControlObservation {
    ControlObservation::HistoryFailed { message }
}

fn resume_failure(message: String) -> ControlObservation {
    ControlObservation::ResumeFailed { message }
}

fn authority_failure(is_resume: bool, message: String) -> ControlObservation {
    if is_resume {
        resume_failure(message)
    } else {
        history_failure(message)
    }
}

struct ExplicitResumeAuthority;

impl ResumeAuthority for ExplicitResumeAuthority {
    type Error = Infallible;

    fn authorize(
        &mut self,
        _prepared: &PreparedResume,
    ) -> Result<ResumeAuthorityDecision, Self::Error> {
        Ok(ResumeAuthorityDecision::Authorized)
    }
}

fn history_session_record(session: gate4agent_adapters::HistorySession) -> HistorySessionRecord {
    HistorySessionRecord {
        session_id: session.session_id,
        title: session.title,
        cwd: session.cwd,
        model: session.model,
        message_count: session.message_count,
        completed_turn_count: session.completed_turn_count,
        total_tokens: session.total_tokens,
        messages: session
            .messages
            .into_iter()
            .map(|message| HistoryMessageRecord {
                role: match message.role {
                    gate4agent_adapters::HistoryRole::User => HistoryMessageRole::User,
                    gate4agent_adapters::HistoryRole::Assistant => HistoryMessageRole::Assistant,
                },
                text: message.text,
            })
            .collect(),
    }
}

async fn run_effect_worker(
    catalog: AgentRegistry,
    mut effects: Receiver<NativeEffectRequest>,
    context: NativeWorkerContext,
) {
    let mut shell = NativeEffectShell::new(catalog);
    let mut interval = tokio::time::interval(context.poll_interval);
    interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut last_effect = Instant::now();

    loop {
        tokio::select! {
            request = effects.recv() => {
                let Some(request) = request else {
                    break;
                };
                last_effect = Instant::now();
                let before = shell.active_session_count();
                let completion = shell
                    .execute_with_launch_context(
                        request.effect,
                        request.spawn_env,
                        request.spawn_extra_args,
                        request.one_shot_session_persistence,
                        request.qwen_sidecar,
                    )
                    .await;
                update_active_count(&context.active_sessions, before, shell.active_session_count());
                remove_hook_route_for_observation(&context.hook_ingress, &completion);
                if closes_terminal_session(&completion.observation) {
                    context
                        .terminal_frames
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .remove(&(completion.instance_id, completion.generation));
                }
                if context.control_tx.send(completion).await.is_err() {
                    break;
                }
                if !publish_shell_observations(&mut shell, &context).await {
                    break;
                }
            }
            _ = interval.tick() => {
                if !publish_shell_observations(&mut shell, &context).await {
                    break;
                }
                if shell.active_session_count() == 0
                    && last_effect.elapsed() >= context.idle_timeout
                {
                    break;
                }
            }
        }
    }

    let remaining = shell.active_session_count();
    if remaining > 0 {
        context
            .active_sessions
            .fetch_sub(remaining, Ordering::AcqRel);
    }
}

async fn publish_shell_observations(
    shell: &mut NativeEffectShell,
    context: &NativeWorkerContext,
) -> bool {
    for observation in shell.collect_provider_events() {
        if context.control_tx.send(observation).await.is_err() {
            return false;
        }
    }

    for observation in shell.collect_terminal_frames() {
        if matches!(
            &observation.observation,
            ControlObservation::TerminalFrame { .. }
        ) {
            let key = (observation.instance_id, observation.generation);
            context
                .terminal_frames
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(key, observation);
        } else if context.control_tx.send(observation).await.is_err() {
            return false;
        }
    }

    let before = shell.active_session_count();
    for observation in shell.collect_exits().await {
        remove_hook_route_for_observation(&context.hook_ingress, &observation);
        context
            .terminal_frames
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(&(observation.instance_id, observation.generation));
        if context.control_tx.send(observation).await.is_err() {
            return false;
        }
    }
    update_active_count(
        &context.active_sessions,
        before,
        shell.active_session_count(),
    );
    true
}

fn remove_hook_route_for_observation(
    hook_ingress: &Arc<RwLock<Option<HookIngressControl>>>,
    observation: &ObservationEnvelope,
) {
    if !matches!(
        observation.observation,
        ControlObservation::SpawnFailed { .. }
            | ControlObservation::StopCompleted { .. }
            | ControlObservation::ProcessExited { .. }
    ) {
        return;
    }
    if let Some(control) = hook_ingress
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
    {
        control.remove_route(observation.instance_id, observation.generation);
    }
}

fn closes_terminal_session(observation: &ControlObservation) -> bool {
    matches!(
        observation,
        ControlObservation::StopCompleted { .. }
            | ControlObservation::StopFailed { .. }
            | ControlObservation::ProcessExited { .. }
    )
}

fn update_active_count(counter: &AtomicUsize, before: usize, after: usize) {
    if after > before {
        counter.fetch_add(after - before, Ordering::AcqRel);
    } else if before > after {
        counter.fetch_sub(before - after, Ordering::AcqRel);
    }
}

fn validate_effect_runtime_policy(effect: &ControlEffect) -> Result<(), String> {
    let (transport, policy, has_initial_prompt, is_resume) = match effect {
        ControlEffect::Spawn {
            transport,
            runtime_policy,
            request,
            ..
        } => (*transport, *runtime_policy, request.initial_prompt.is_some(), false),
        ControlEffect::SpawnResume {
            transport,
            runtime_policy,
            request,
            ..
        } => (*transport, *runtime_policy, request.initial_prompt.is_some(), true),
        ControlEffect::Stop { .. }
        | ControlEffect::WriteInput { .. }
        | ControlEffect::SubmitPrompt { .. }
        | ControlEffect::Interrupt
        | ControlEffect::Resize { .. }
        | ControlEffect::ObserveForeground
        | ControlEffect::ProbeCapabilities { .. }
        | ControlEffect::DiscoverHistory { .. }
        | ControlEffect::LoadHistory { .. }
        | ControlEffect::AuthorizeResume { .. }
        | ControlEffect::ResolveInteraction { .. } => return Ok(()),
    };
    policy
        .validate()
        .map_err(|error| format!("provider runtime policy is invalid: {error}"))?;
    require_runtime_capability(policy, ProviderRuntimeCapability::RawPtyLifecycle)?;
    if transport != TransportKind::Pty {
        require_runtime_capability(policy, ProviderRuntimeCapability::SemanticReadiness)?;
    }
    if has_initial_prompt {
        require_runtime_capability(policy, ProviderRuntimeCapability::SemanticReadiness)?;
        require_runtime_capability(policy, ProviderRuntimeCapability::StructuredPrompt)?;
    }
    if is_resume && has_initial_prompt {
        require_runtime_capability(policy, ProviderRuntimeCapability::ProviderSessionIdentity)?;
        require_runtime_capability(policy, ProviderRuntimeCapability::SemanticResume)?;
    }
    Ok(())
}

fn require_runtime_capability(
    policy: ProviderRuntimePolicy,
    capability: ProviderRuntimeCapability,
) -> Result<(), String> {
    if policy.admits(capability) {
        Ok(())
    } else {
        Err(format!(
            "provider runtime capability {capability:?} is not admitted"
        ))
    }
}

fn effect_failure(effect: EffectEnvelope, message: String) -> ObservationEnvelope {
    let observation = match effect.effect {
        ControlEffect::Spawn { .. } | ControlEffect::SpawnResume { .. } => {
            ControlObservation::SpawnFailed { message }
        }
        ControlEffect::Stop { .. } => ControlObservation::StopFailed { message },
        ControlEffect::WriteInput { .. } => ControlObservation::InputFailed { message },
        ControlEffect::SubmitPrompt { .. } | ControlEffect::Interrupt => {
            ControlObservation::InputFailed { message }
        }
        ControlEffect::ResolveInteraction { target, .. } => {
            ControlObservation::InteractionResolutionFailed {
                interaction_id: target.interaction_id,
                message,
            }
        }
        ControlEffect::Resize { .. } => ControlObservation::ResizeFailed { message },
        ControlEffect::ObserveForeground => ControlObservation::ForegroundFailed { message },
        ControlEffect::ProbeCapabilities { .. } => ControlObservation::CapabilityProbeFailed {
            failure: CapabilityProbeFailure::ExecutorUnavailable,
        },
        ControlEffect::DiscoverHistory { .. } | ControlEffect::LoadHistory { .. } => {
            ControlObservation::HistoryFailed { message }
        }
        ControlEffect::AuthorizeResume { .. } => ControlObservation::ResumeFailed { message },
    };
    ObservationEnvelope {
        protocol_version: CONTROL_PROTOCOL_VERSION,
        operation_id: Some(effect.operation_id),
        instance_id: effect.instance_id,
        generation: effect.generation,
        observation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_shell_hooks::{
        HOOK_PORT_ENV, HOOK_ROUTE_ENV, HOOK_TOKEN_ENV, HOOK_URL_ENV, HOOK_VERSION_ENV,
    };
    use gate4agent_testkit::{
        hook_posting_agent_spec, interactive_agent_spec, CONTROL_FIXTURE_ID,
        HOOK_POSTING_FIXTURE_ID,
    };
    use gate4agent_types::{OperationId, StartRequest, TerminalSize};

    #[test]
    fn history_record_preserves_completed_turn_count() {
        let record = history_session_record(gate4agent_adapters::HistorySession {
            session_id: "session-1".to_owned(),
            title: None,
            cwd: None,
            model: None,
            message_count: 5,
            completed_turn_count: Some(2),
            total_tokens: 8,
            total_tokens_observed: true,
            messages: Vec::new(),
        });

        assert_eq!(record.completed_turn_count, Some(2));
    }

    #[test]
    fn runtime_preview_maps_known_and_unknown_token_totals_exactly() {
        let session = |total_tokens_observed, total_tokens| {
            gate4agent_adapters::HistorySession {
                session_id: "session-1".to_owned(),
                title: None,
                cwd: None,
                model: None,
                message_count: 0,
                completed_turn_count: None,
                total_tokens,
                total_tokens_observed,
                messages: Vec::new(),
            }
        };

        let unknown = project_native_session_preview(session(false, 0), None, false, 1).unwrap();
        let observed_zero =
            project_native_session_preview(session(true, 0), None, false, 1).unwrap();
        let observed_nonzero =
            project_native_session_preview(session(true, 42), None, false, 1).unwrap();

        assert_eq!(unknown.total_tokens, None);
        assert_eq!(observed_zero.total_tokens, Some(0));
        assert_eq!(observed_nonzero.total_tokens, Some(42));
    }

    #[test]
    fn native_catalog_high_cardinality_outside_workspace_does_not_hide_match() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gate4agent-native-catalog-prelimit-{}-{unique}",
            std::process::id(),
        ));
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        let history = root.join("central-history");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(&history).unwrap();

        let transcript = |session_id: &str, cwd: &Path| {
            let cwd = cwd
                .to_str()
                .unwrap()
                .replace('\\', "\\\\")
                .replace('"', "\\\"");
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"{session_id}\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"question\"}}}}\n\
                 {{\"type\":\"assistant\",\"sessionId\":\"{session_id}\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"answer\"}}}}"
            )
        };
        std::fs::write(
            history.join("zz-workspace-session.jsonl"),
            transcript("workspace-session", &workspace),
        )
        .unwrap();
        for index in 0..1_100 {
            let session_id = format!("outside-session-{index:04}");
            std::fs::write(
                history.join(format!("aa-outside-session-{index:04}.jsonl")),
                transcript(&session_id, &outside),
            )
            .unwrap();
        }

        let config = NativeHistoryConfig::new(vec![
            NativeHistoryRoot::new(
                gate4agent_types::AdapterId::new("claude-code").unwrap(),
                HistorySourceLayout::SingleNdjson,
                &history,
            )
            .unwrap(),
        ])
        .unwrap();
        let mut catalog = NativeSessionCatalogAuthority::new(config);
        let entries = catalog
            .catalog(&AgentId::new("claude").unwrap(), &workspace, 1)
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "workspace-session");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_catalog_attributes_deleted_historical_cwd_lexically() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gate4agent-native-catalog-deleted-cwd-{}-{unique}",
            std::process::id(),
        ));
        let workspace = root.join("workspace");
        let deleted_cwd = workspace.join("removed-subdirectory");
        let history = root.join("central-history");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&history).unwrap();
        let cwd = deleted_cwd
            .to_str()
            .unwrap()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        let workspace_key = NativePathKey::from_canonical_root(
            &std::fs::canonicalize(&workspace).unwrap(),
        )
        .unwrap();
        let deleted_key = NativePathKey::from_declared_cwd(
            deleted_cwd.to_str().unwrap(),
        )
        .unwrap();
        assert!(
            deleted_key.starts_with(&workspace_key),
            "declared cwd {deleted_key:?} must remain beneath workspace {workspace_key:?}",
        );
        std::fs::write(
            history.join("deleted-cwd-session.jsonl"),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"deleted-cwd-session\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"question\"}}}}\n\
                 {{\"type\":\"assistant\",\"sessionId\":\"deleted-cwd-session\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"answer\"}}}}"
            ),
        )
        .unwrap();

        let config = NativeHistoryConfig::new(vec![
            NativeHistoryRoot::new(
                gate4agent_types::AdapterId::new("claude-code").unwrap(),
                HistorySourceLayout::SingleNdjson,
                &history,
            )
            .unwrap(),
        ])
        .unwrap();
        let mut catalog = NativeSessionCatalogAuthority::new(config);
        let entries = catalog
            .catalog(&AgentId::new("claude").unwrap(), &workspace, 10)
            .unwrap();

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].session_id, "deleted-cwd-session");
        let preview = catalog
            .preview(
                &AgentId::new("claude").unwrap(),
                &workspace,
                &entries[0].selection_id,
                10,
            )
            .unwrap();
        assert_eq!(preview.session_id, "deleted-cwd-session");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_catalog_keeps_provider_candidate_indexes_isolated() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gate4agent-native-catalog-provider-isolation-{}-{unique}",
            std::process::id(),
        ));
        let workspace = root.join("workspace");
        let claude_history = root.join("claude-history");
        let qwen_history = root.join("qwen-history");
        let qwen_chats = qwen_history.join("project").join("chats");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&claude_history).unwrap();
        std::fs::create_dir_all(&qwen_chats).unwrap();
        let cwd = workspace
            .to_str()
            .unwrap()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        std::fs::write(
            claude_history.join("claude-session.jsonl"),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"claude-session\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"question\"}}}}\n\
                 {{\"type\":\"assistant\",\"sessionId\":\"claude-session\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"answer\"}}}}"
            ),
        )
        .unwrap();
        let qwen_session = "11111111-1111-4111-8111-111111111111";
        std::fs::write(
            qwen_chats.join(format!("{qwen_session}.jsonl")),
            format!(
                "{{\"uuid\":\"u1\",\"parentUuid\":null,\"sessionId\":\"{qwen_session}\",\"type\":\"user\",\"provenance\":\"real_user\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"user\",\"parts\":[{{\"text\":\"question\"}}]}}}}\n\
                 {{\"uuid\":\"a1\",\"parentUuid\":\"u1\",\"sessionId\":\"{qwen_session}\",\"type\":\"assistant\",\"provenance\":\"assistant_output\",\"cwd\":\"{cwd}\",\"message\":{{\"role\":\"model\",\"parts\":[{{\"text\":\"answer\"}}]}}}}"
            ),
        )
        .unwrap();
        let limits = gate4agent_shell_history::NativeHistoryLimits {
            max_candidates: 1,
            ..gate4agent_shell_history::NativeHistoryLimits::default()
        };
        let config = NativeHistoryConfig::with_limits(
            vec![
                NativeHistoryRoot::new(
                    gate4agent_types::AdapterId::new("claude-code").unwrap(),
                    HistorySourceLayout::SingleNdjson,
                    &claude_history,
                )
                .unwrap(),
                NativeHistoryRoot::new(
                    gate4agent_types::AdapterId::new("qwen-code").unwrap(),
                    HistorySourceLayout::SingleNdjson,
                    &qwen_history,
                )
                .unwrap(),
            ],
            limits,
        )
        .unwrap();
        let mut catalog = NativeSessionCatalogAuthority::new(config);
        let claude = catalog
            .catalog(&AgentId::new("claude").unwrap(), &workspace, 1)
            .unwrap();
        let qwen = catalog
            .catalog(&AgentId::new("qwen-code").unwrap(), &workspace, 1)
            .unwrap();

        assert_eq!(claude.len(), 1);
        assert_eq!(qwen.len(), 1);
        let preview = catalog
            .preview(
                &AgentId::new("claude").unwrap(),
                &workspace,
                &claude[0].selection_id,
                10,
            )
            .unwrap();
        assert_eq!(preview.session_id, "claude-session");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_catalog_rejects_incomplete_provider_scan() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gate4agent-native-catalog-incomplete-{}-{unique}",
            std::process::id(),
        ));
        let workspace = root.join("workspace");
        let history = root.join("history");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&history).unwrap();
        let cwd = workspace
            .to_str()
            .unwrap()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        for index in 0..2 {
            std::fs::write(
                history.join(format!("session-{index}.jsonl")),
                format!(
                    "{{\"type\":\"user\",\"sessionId\":\"session-{index}\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"question\"}}}}"
                ),
            )
            .unwrap();
        }
        let limits = gate4agent_shell_history::NativeHistoryLimits {
            max_walk_entries: 1,
            ..gate4agent_shell_history::NativeHistoryLimits::default()
        };
        let config = NativeHistoryConfig::with_limits(
            vec![NativeHistoryRoot::new(
                gate4agent_types::AdapterId::new("claude-code").unwrap(),
                HistorySourceLayout::SingleNdjson,
                &history,
            )
            .unwrap()],
            limits,
        )
        .unwrap();
        let mut catalog = NativeSessionCatalogAuthority::new(config);

        assert_eq!(
            catalog.catalog(&AgentId::new("claude").unwrap(), &workspace, 10),
            Err(NativeSessionCatalogError::CatalogUnavailable),
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_catalog_initial_window_excludes_older_until_explicit_page() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let workspace = std::env::temp_dir().join(format!(
            "gate4agent-native-catalog-window-{}-{unique}",
            std::process::id(),
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let provider = AgentId::new("claude").unwrap();
        let mut catalog = NativeSessionCatalogAuthority::new(
            NativeHistoryConfig::new(Vec::new()).unwrap(),
        );
        let spec = catalog.catalog.get(&provider).unwrap().clone();
        let request = HistoryDiscoveryRequest::from_spec(&spec, None, 2).unwrap();
        let cwd_key = NativePathKey::from_canonical_root(
            &std::fs::canonicalize(&workspace).unwrap(),
        )
        .unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let indexed = |id: &str, modified_at_unix_ms| IndexedNativeSession {
            candidate: HistoryCandidate::new(
                &request,
                gate4agent_provider_ports::HistoryCandidateId::new(id).unwrap(),
                id,
                Some(modified_at_unix_ms),
            )
            .unwrap(),
            selection_id: id.to_owned(),
            modified_at_unix_ms: Some(modified_at_unix_ms),
            cwd_key: Some(cwd_key.clone()),
            session_id: id.to_owned(),
            title: Some(id.to_owned()),
            model: None,
            project_label: "workspace".to_owned(),
            provider_session: ProviderSessionIdentity {
                key: gate4agent_types::ProviderSessionKey::SessionId,
                id: id.to_owned(),
                transcript_path: None,
            },
        };
        catalog.indexes.insert(
            provider.clone(),
            NativeProviderSessionIndex {
                refreshed_at: Instant::now(),
                revision: 1,
                recent_cutoff_unix_ms: now.saturating_sub(NATIVE_SESSION_RECENT_WINDOW_MS),
                sessions: vec![
                    indexed("recent-session", now),
                    indexed("recent-session-2", now.saturating_sub(1)),
                    indexed(
                        "older-session",
                        now.saturating_sub(NATIVE_SESSION_RECENT_WINDOW_MS + 1),
                    ),
                ],
            },
        );

        let recent = catalog
            .catalog_initial_for_workspace(
                &provider,
                &workspace,
                &[workspace.clone()],
                1,
            )
            .unwrap();
        let older = catalog
            .catalog_page_for_workspace(
                &provider,
                &workspace,
                &[workspace.clone()],
                NativeSessionCatalogWindow::Older,
                recent.revision,
                recent.cutoff_unix_ms,
                None,
                64,
            )
            .unwrap();

        assert_eq!(recent.entries.len(), 1);
        assert_eq!(recent.entries[0].session_id, "recent-session");
        assert_eq!(recent.recent_total_count, 2);
        assert_eq!(recent.older_total_count, 1);
        assert_eq!(recent.next_after_selection_id.as_deref(), Some("recent-session"));
        let next = catalog
            .catalog_page_for_workspace(
                &provider,
                &workspace,
                &[workspace.clone()],
                NativeSessionCatalogWindow::Recent,
                recent.revision,
                recent.cutoff_unix_ms,
                recent.next_after_selection_id.as_deref(),
                1,
            )
            .unwrap();
        assert_eq!(next.entries[0].session_id, "recent-session-2");
        assert_eq!(next.next_after_selection_id, None);
        assert_eq!(older.entries.len(), 1);
        assert_eq!(older.entries[0].session_id, "older-session");
        assert_eq!(
            catalog.catalog_page_for_workspace(
                &provider,
                &workspace,
                &[workspace.clone()],
                NativeSessionCatalogWindow::Recent,
                recent.revision + 1,
                recent.cutoff_unix_ms,
                None,
                1,
            ),
            Err(NativeSessionCatalogError::StaleCatalog),
        );
        assert_eq!(
            catalog.catalog_page_for_workspace(
                &provider,
                &workspace,
                &[workspace.clone()],
                NativeSessionCatalogWindow::Recent,
                recent.revision,
                recent.cutoff_unix_ms + 1,
                None,
                1,
            ),
            Err(NativeSessionCatalogError::StaleCatalog),
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[test]
    fn unregistered_catalog_uses_revision_bound_opaque_groups_for_same_basename() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gate4agent-native-external-groups-{}-{unique}",
            std::process::id(),
        ));
        let first = root.join("private-a").join("shared");
        let second = root.join("private-b").join("shared");
        std::fs::create_dir_all(&first).unwrap();
        std::fs::create_dir_all(&second).unwrap();
        let provider = AgentId::new("claude").unwrap();
        let mut catalog = NativeSessionCatalogAuthority::new(
            NativeHistoryConfig::new(Vec::new()).unwrap(),
        );
        let spec = catalog.catalog.get(&provider).unwrap().clone();
        let request = HistoryDiscoveryRequest::from_spec(&spec, None, 2).unwrap();
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let indexed = |id: &str, cwd: &Path| IndexedNativeSession {
            candidate: HistoryCandidate::new(
                &request,
                gate4agent_provider_ports::HistoryCandidateId::new(id).unwrap(),
                id,
                Some(now),
            )
            .unwrap(),
            selection_id: id.to_owned(),
            modified_at_unix_ms: Some(now),
            cwd_key: Some(
                NativePathKey::from_declared_cwd(cwd.to_str().unwrap()).unwrap(),
            ),
            session_id: id.to_owned(),
            title: None,
            model: None,
            project_label: "shared".to_owned(),
            provider_session: ProviderSessionIdentity {
                key: gate4agent_types::ProviderSessionKey::SessionId,
                id: id.to_owned(),
                transcript_path: None,
            },
        };
        catalog.indexes.insert(
            provider.clone(),
            NativeProviderSessionIndex {
                refreshed_at: Instant::now(),
                revision: 1,
                recent_cutoff_unix_ms: now.saturating_sub(NATIVE_SESSION_RECENT_WINDOW_MS),
                sessions: vec![indexed("session-a", &first), indexed("session-b", &second)],
            },
        );

        let page = catalog
            .catalog_initial_for_scope(
                &provider,
                NativeSessionCatalogScope::Unregistered,
                None,
                &[],
                64,
            )
            .unwrap();
        let groups = page
            .entries
            .iter()
            .map(|entry| entry.external_group.as_ref().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].display_name, "shared");
        assert_eq!(groups[1].display_name, "shared");
        assert_ne!(groups[0].group_id, groups[1].group_id);
        assert!(groups.iter().all(|group| group.group_id.starts_with("external-")));
        assert!(groups.iter().all(|group| !group.group_id.contains("private")));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_catalog_revision_changes_only_when_locator_metadata_changes() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gate4agent-native-catalog-revision-{}-{unique}",
            std::process::id(),
        ));
        let workspace = root.join("workspace");
        let history = root.join("history");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&history).unwrap();
        let cwd = workspace.to_str().unwrap().replace('\\', "\\\\");
        let transcript = |title: &str| {
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"revision-session\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"{title}\"}}}}"
            )
        };
        let path = history.join("revision-session.jsonl");
        std::fs::write(&path, transcript("first title")).unwrap();
        let config = NativeHistoryConfig::new(vec![
            NativeHistoryRoot::new(
                gate4agent_types::AdapterId::new("claude-code").unwrap(),
                HistorySourceLayout::SingleNdjson,
                &history,
            )
            .unwrap(),
        ])
        .unwrap();
        let provider = AgentId::new("claude").unwrap();
        let mut catalog = NativeSessionCatalogAuthority::new(config);

        let first = catalog
            .catalog_initial_for_workspace(&provider, &workspace, &[workspace.clone()], 64)
            .unwrap();
        catalog.indexes.get_mut(&provider).unwrap().refreshed_at =
            Instant::now() - NATIVE_SESSION_INDEX_REFRESH_INTERVAL;
        let unchanged = catalog
            .catalog_initial_for_workspace(&provider, &workspace, &[workspace.clone()], 64)
            .unwrap();
        assert_eq!(unchanged.revision, first.revision);

        std::fs::write(&path, transcript("changed title")).unwrap();
        catalog.indexes.get_mut(&provider).unwrap().refreshed_at =
            Instant::now() - NATIVE_SESSION_INDEX_REFRESH_INTERVAL;
        let changed = catalog
            .catalog_initial_for_workspace(&provider, &workspace, &[workspace.clone()], 64)
            .unwrap();
        assert_ne!(changed.revision, first.revision);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn native_catalog_high_cardinality_nested_worktree_uses_deepest_owner() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "gate4agent-native-catalog-worktree-owner-{}-{unique}",
            std::process::id(),
        ));
        let parent = root.join("workspace");
        let worktree = parent.join("linked-worktree");
        let history = root.join("central-history");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::create_dir_all(&history).unwrap();
        let cwd = worktree
            .to_str()
            .unwrap()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        std::fs::write(
            history.join("worktree-session.jsonl"),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"worktree-session\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"question\"}}}}\n\
                 {{\"type\":\"assistant\",\"sessionId\":\"worktree-session\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"answer\"}}}}"
            ),
        )
        .unwrap();
        let parent_cwd = parent
            .to_str()
            .unwrap()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        std::fs::write(
            history.join("parent-session.jsonl"),
            format!(
                "{{\"type\":\"user\",\"sessionId\":\"parent-session\",\"cwd\":\"{parent_cwd}\",\"message\":{{\"content\":\"parent question\"}}}}\n\
                 {{\"type\":\"assistant\",\"sessionId\":\"parent-session\",\"cwd\":\"{parent_cwd}\",\"message\":{{\"content\":\"parent answer\"}}}}"
            ),
        )
        .unwrap();
        for index in 0..1_100 {
            std::fs::write(
                history.join(format!("child-session-{index:04}.jsonl")),
                format!(
                    "{{\"type\":\"user\",\"sessionId\":\"child-session-{index:04}\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"child question\"}}}}\n\
                     {{\"type\":\"assistant\",\"sessionId\":\"child-session-{index:04}\",\"cwd\":\"{cwd}\",\"message\":{{\"content\":\"child answer\"}}}}"
                ),
            )
            .unwrap();
        }

        let config = NativeHistoryConfig::new(vec![
            NativeHistoryRoot::new(
                gate4agent_types::AdapterId::new("claude-code").unwrap(),
                HistorySourceLayout::SingleNdjson,
                &history,
            )
            .unwrap(),
        ])
        .unwrap();
        let registered = vec![parent.clone(), worktree.clone()];
        let mut catalog = NativeSessionCatalogAuthority::new(config);
        let parent_entries = catalog
            .catalog_for_workspace(
                &AgentId::new("claude").unwrap(),
                &parent,
                &registered,
                1,
            )
            .unwrap();
        let worktree_entries = catalog
            .catalog_for_workspace(
                &AgentId::new("claude").unwrap(),
                &worktree,
                &registered,
                10,
            )
            .unwrap();

        assert_eq!(parent_entries.len(), 1);
        assert_eq!(parent_entries[0].session_id, "parent-session");
        assert_eq!(worktree_entries.len(), 10);
        assert_eq!(
            catalog.preview_for_workspace(
                &AgentId::new("claude").unwrap(),
                &parent,
                &registered,
                &worktree_entries[0].selection_id,
                10,
            ),
            Err(NativeSessionPreviewError::SessionNotFound),
        );
        let preview = catalog
            .preview_for_workspace(
                &AgentId::new("claude").unwrap(),
                &worktree,
                &registered,
                &worktree_entries[0].selection_id,
                10,
            )
            .unwrap();
        assert_eq!(preview.session_id, worktree_entries[0].session_id);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn runtime_dispatcher_rejects_raw_semantic_prompt_before_worker_dispatch() {
        let mut dispatcher = NativeEffectDispatcher::new(
            AgentRegistry::new([]).unwrap(),
            NativeRuntimeConfig::default(),
            None,
        );
        dispatcher.dispatch(EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: OperationId(1),
            instance_id: AgentInstanceId(1),
            generation: SessionGeneration(1),
            effect: ControlEffect::Spawn {
                agent_id: AgentId::new("unknown-provider").unwrap(),
                transport: TransportKind::Pty,
                runtime_policy: ProviderRuntimePolicy::raw_pty(),
                request: StartRequest {
                    working_directory: ".".to_owned(),
                    terminal_size: TerminalSize { rows: 24, columns: 80 },
                    initial_prompt: Some("must-not-run".to_owned()),
                    session_options: None,
                },
            },
        });

        let (observations, terminal_frames) = dispatcher.drain_observations(1);
        assert_eq!(terminal_frames, 0);
        assert!(dispatcher.workers.is_empty());
        assert!(matches!(
            observations.as_slice(),
            [ObservationEnvelope {
                observation: ControlObservation::SpawnFailed { message },
                ..
            }] if message.contains("SemanticReadiness")
        ));
    }

    #[tokio::test]
    async fn pty_hook_spawn_attaches_route_environment_without_identity_authority() {
        let catalog = AgentRegistry::new([hook_posting_agent_spec()]).unwrap();
        let (_, mut runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        runtime
            .start_hook_ingress(HookIngressConfig::default())
            .await
            .unwrap();
        let hook_monitoring_policy =
            ProviderRuntimePolicy::new(true, true, true, false, false).unwrap();
        assert!(!hook_monitoring_policy.provider_session_identity);
        let spawn = |operation_id, instance_id, agent_id, transport, runtime_policy| {
            EffectEnvelope {
                protocol_version: CONTROL_PROTOCOL_VERSION,
                operation_id: OperationId(operation_id),
                instance_id: AgentInstanceId(instance_id),
                generation: SessionGeneration(1),
                effect: ControlEffect::Spawn {
                    agent_id: AgentId::new(agent_id).unwrap(),
                    transport,
                    runtime_policy,
                    request: StartRequest {
                        working_directory: ".".to_owned(),
                        terminal_size: TerminalSize { rows: 24, columns: 80 },
                        initial_prompt: None,
                        session_options: None,
                    },
                },
            }
        };

        let hook_spawn = spawn(
            10,
            10,
            HOOK_POSTING_FIXTURE_ID,
            TransportKind::Pty,
            hook_monitoring_policy,
        );
        let overlay = runtime
            .effects
            .compose_spawn_overlay(&hook_spawn)
            .unwrap();
        let environment_keys = overlay
            .environment
            .iter()
            .map(|mutation| mutation.key.to_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(overlay.environment.len(), 5);
        for key in [
            HOOK_PORT_ENV,
            HOOK_TOKEN_ENV,
            HOOK_ROUTE_ENV,
            HOOK_URL_ENV,
            HOOK_VERSION_ENV,
        ] {
            assert!(environment_keys.contains(&key), "missing hook environment key {key}");
        }
        assert!(overlay
            .environment
            .iter()
            .all(|mutation| mutation.value.is_some()));
        assert_eq!(runtime.active_hook_routes(), 1);

        runtime.effects.remove_hook_route(&hook_spawn);
        assert_eq!(runtime.active_hook_routes(), 0);

        let non_pty_hook_spawn = spawn(
            11,
            11,
            HOOK_POSTING_FIXTURE_ID,
            TransportKind::Pipe,
            ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
        );
        assert!(runtime
            .effects
            .compose_spawn_overlay(&non_pty_hook_spawn)
            .unwrap()
            .environment
            .is_empty());
        assert_eq!(runtime.active_hook_routes(), 0);

        runtime.stop_hook_ingress().await;

        let raw_pty_without_hook = spawn(
            12,
            12,
            CONTROL_FIXTURE_ID,
            TransportKind::Pty,
            ProviderRuntimePolicy::raw_pty(),
        );
        let no_hook_catalog = AgentRegistry::new([interactive_agent_spec()]).unwrap();
        let (_, mut no_hook_runtime) =
            NativeRuntime::new(no_hook_catalog, NativeRuntimeConfig::default());
        no_hook_runtime
            .start_hook_ingress(HookIngressConfig::default())
            .await
            .unwrap();
        assert!(no_hook_runtime
            .effects
            .compose_spawn_overlay(&raw_pty_without_hook)
            .unwrap()
            .environment
            .is_empty());
        assert_eq!(no_hook_runtime.active_hook_routes(), 0);

        no_hook_runtime.stop_hook_ingress().await;
    }

    #[test]
    fn qwen_dual_output_overlay_is_exactly_pty_only_and_hook_independent() {
        let catalog = builtin_registry().clone();
        let (_, runtime) = NativeRuntime::new(catalog, NativeRuntimeConfig::default());
        let spawn = |operation_id, instance_id, agent_id, transport| EffectEnvelope {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            operation_id: OperationId(operation_id),
            instance_id: AgentInstanceId(instance_id),
            generation: SessionGeneration(1),
            effect: ControlEffect::Spawn {
                agent_id: AgentId::new(agent_id).unwrap(),
                transport,
                runtime_policy: ProviderRuntimePolicy::raw_pty(),
                request: StartRequest {
                    working_directory: ".".to_owned(),
                    terminal_size: TerminalSize { rows: 24, columns: 80 },
                    initial_prompt: None,
                    session_options: None,
                },
            },
        };

        let qwen_pty = runtime
            .effects
            .compose_spawn_overlay(&spawn(21, 21, "qwen-code", TransportKind::Pty))
            .unwrap();
        assert!(qwen_pty.environment.is_empty());
        let mut sidecar_args = Vec::new();
        qwen_pty
            .qwen_sidecar
            .as_ref()
            .unwrap()
            .append_launch_arguments(&mut sidecar_args);
        assert_eq!(sidecar_args.len(), 2);
        assert_eq!(sidecar_args[0], OsString::from("--json-file"));
        assert!(sidecar_args[1].to_string_lossy().ends_with("events.jsonl"));

        let qwen_pipe = runtime
            .effects
            .compose_spawn_overlay(&spawn(22, 22, "qwen-code", TransportKind::Pipe))
            .unwrap();
        assert!(qwen_pipe.qwen_sidecar.is_none());
        let codex_pty = runtime
            .effects
            .compose_spawn_overlay(&spawn(23, 23, "codex", TransportKind::Pty))
            .unwrap();
        assert!(codex_pty.qwen_sidecar.is_none());
    }

    #[test]
    fn runtime_policy_admits_raw_native_resume_without_prompt_only() {
        let raw = ProviderRuntimePolicy::raw_pty();
        let raw_resume = ControlEffect::SpawnResume {
            agent_id: AgentId::new("unknown-provider").unwrap(),
            transport: TransportKind::Pty,
            provider_session: gate4agent_types::ProviderSessionIdentity {
                key: gate4agent_types::ProviderSessionKey::SessionId,
                id: "session-1".to_owned(),
                transcript_path: None,
            },
            runtime_policy: raw,
            request: ResumeLaunchRequest {
                working_directory: ".".to_owned(),
                terminal_size: gate4agent_types::TerminalSize { rows: 24, columns: 80 },
                initial_prompt: None,
            },
        };

        assert!(validate_effect_runtime_policy(&raw_resume).is_ok());

        let resume_with_prompt = ControlEffect::SpawnResume {
            agent_id: AgentId::new("unknown-provider").unwrap(),
            transport: TransportKind::Pty,
            provider_session: gate4agent_types::ProviderSessionIdentity {
                key: gate4agent_types::ProviderSessionKey::SessionId,
                id: "session-1".to_owned(),
                transcript_path: None,
            },
            runtime_policy: raw,
            request: ResumeLaunchRequest {
                working_directory: ".".to_owned(),
                terminal_size: gate4agent_types::TerminalSize { rows: 24, columns: 80 },
                initial_prompt: Some("must-not-run".to_owned()),
            },
        };
        assert!(validate_effect_runtime_policy(&resume_with_prompt)
            .unwrap_err()
            .contains("SemanticReadiness"));
    }
}

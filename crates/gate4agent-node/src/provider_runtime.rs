use crate::protocol::{
    ProviderRuntimeContractId, ProviderRuntimeMode, ProviderRuntimeStatus,
    ProviderRuntimeStatuses, ProviderRuntimeVersion,
};
use gate4agent_catalog::AgentRegistry;
use gate4agent_runtime_native::{
    VendorContractResolution, VendorRuntimeMode, VendorVersionProbeCache,
};
use gate4agent_types::{AgentId, ProviderRuntimePolicy};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const VERSION_PROBE_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderRuntimeRequirement {
    RawPty,
    SemanticPrompt,
    Inline,
    Resume,
    ResumeWithPrompt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderRuntimeAdmissionError {
    LauncherUnavailable,
    SemanticCapabilityUnverified,
    ProbeBusy,
}

pub(crate) struct ProviderRuntimeMonitor {
    providers: BTreeMap<AgentId, ProviderStaticCapabilities>,
    probe_cache: Mutex<VendorVersionProbeCache>,
}

#[derive(Clone, Debug)]
struct ProviderStaticCapabilities {
    launch_program: String,
    raw_pty: bool,
    semantic_pty_adapter: bool,
    resume_adapter: bool,
    pty_sidecar_observation: bool,
}

impl ProviderRuntimeMonitor {
    pub(crate) fn new(catalog: &AgentRegistry) -> Self {
        Self {
            providers: catalog
                .iter()
                .map(|spec| {
                    (
                        spec.id.clone(),
                        ProviderStaticCapabilities {
                            launch_program: spec.launch.program.clone(),
                            raw_pty: spec.capabilities.transports.pty,
                            semantic_pty_adapter: spec
                                .capabilities
                                .transports
                                .pty_adapter
                                .is_some(),
                            resume_adapter: spec.capabilities.adapters.resume.is_some(),
                            pty_sidecar_observation: spec
                                .capabilities
                                .adapters
                                .pty_sidecar
                                .is_some(),
                        },
                    )
                })
                .collect(),
            probe_cache: Mutex::new(VendorVersionProbeCache::default()),
        }
    }

    pub(crate) fn collect(&self) -> ProviderRuntimeStatuses {
        ProviderRuntimeStatuses::new(
            self.providers
                .keys()
                .map(|provider| {
                    self.evaluate(provider)
                        .0
                        .expect("startup provider probe cache is uncontended")
                }),
        )
        .expect("the validated node catalog remains within the provider identity limit")
    }

    pub(crate) fn evaluate(
        &self,
        provider: &AgentId,
    ) -> (
        Option<ProviderRuntimeStatus>,
        Result<ProviderRuntimePolicy, ProviderRuntimeAdmissionError>,
    ) {
        let Some(static_capabilities) = self.providers.get(provider) else {
            return (
                Some(ProviderRuntimeStatus::unavailable(provider.clone())),
                Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
            );
        };
        let Some(launcher) = resolve_local_launcher(&static_capabilities.launch_program) else {
            return (
                Some(ProviderRuntimeStatus::unavailable(provider.clone())),
                Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
            );
        };
        if static_capabilities.pty_sidecar_observation {
            let policy = ProviderRuntimePolicy::new(
                static_capabilities.raw_pty,
                true,
                false,
                false,
                false,
            )
            .expect("catalog-declared PTY sidecar policy is internally valid");
            return (
                Some(ProviderRuntimeStatus::raw_passthrough(
                    provider.clone(),
                    None,
                )),
                Ok(policy),
            );
        }
        let mut cache = match self.probe_cache.try_lock() {
            Ok(cache) => cache,
            Err(std::sync::TryLockError::WouldBlock) => {
                return (None, Err(ProviderRuntimeAdmissionError::ProbeBusy));
            }
            Err(std::sync::TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        let probe = cache.probe(
            provider.as_str(),
            &launcher,
            Instant::now() + VERSION_PROBE_DEADLINE,
        );
        let resolution = probe.resolution();
        let status = status_from_resolution(provider.clone(), resolution);
        let policy = policy_from_resolution(static_capabilities, resolution);
        (Some(status), Ok(policy))
    }
}

pub(crate) fn admit_status(
    statuses: &ProviderRuntimeStatuses,
    provider: &AgentId,
    requirement: ProviderRuntimeRequirement,
) -> Result<ProviderRuntimePolicy, ProviderRuntimeAdmissionError> {
    let Some(status) = statuses
        .iter()
        .find(|status| status.provider() == provider)
    else {
        return Err(ProviderRuntimeAdmissionError::LauncherUnavailable);
    };
    let policy = match status.mode() {
        ProviderRuntimeMode::Unavailable => {
            return Err(ProviderRuntimeAdmissionError::LauncherUnavailable);
        }
        ProviderRuntimeMode::RawPassthrough => ProviderRuntimePolicy::raw_pty(),
        ProviderRuntimeMode::VerifiedSemantic => ProviderRuntimePolicy::new(
            true,
            true,
            true,
            false,
            false,
        )
        .expect("coarse verified semantic fallback policy is internally valid"),
    };
    require_policy(policy, requirement).map(|()| policy)
}

pub(crate) fn require_policy(
    policy: ProviderRuntimePolicy,
    requirement: ProviderRuntimeRequirement,
) -> Result<(), ProviderRuntimeAdmissionError> {
    let admitted = match requirement {
        ProviderRuntimeRequirement::RawPty => policy.raw_pty_lifecycle,
        ProviderRuntimeRequirement::SemanticPrompt => {
            policy.semantic_readiness && policy.structured_prompt
        }
        ProviderRuntimeRequirement::Inline => false,
        // A provider-native PTY resume is still a raw PTY launch. The durable
        // session record supplies the exact provider identity and workspace;
        // the native shell separately requires a declared resume adapter before
        // it can construct the provider argv. Semantic resume is only needed
        // when Gate4Agent must also inject a prompt without operator input.
        ProviderRuntimeRequirement::Resume => policy.raw_pty_lifecycle,
        ProviderRuntimeRequirement::ResumeWithPrompt => {
            policy.provider_session_identity
                && policy.semantic_resume
                && policy.semantic_readiness
                && policy.structured_prompt
        }
    };
    if admitted {
        Ok(())
    } else {
        Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified)
    }
}

fn policy_from_resolution(
    static_capabilities: &ProviderStaticCapabilities,
    resolution: &VendorContractResolution,
) -> ProviderRuntimePolicy {
    let capabilities = resolution.capabilities();
    policy_from_capability_flags(
        static_capabilities,
        resolution.admits_raw_pty_lifecycle(),
        capabilities.semantic_readiness.is_verified(),
        capabilities.structured_prompt.is_verified(),
        capabilities.provider_session_identity.is_verified(),
        capabilities.semantic_resume.is_verified(),
    )
}

fn policy_from_capability_flags(
    static_capabilities: &ProviderStaticCapabilities,
    live_raw_pty_lifecycle: bool,
    live_semantic_readiness: bool,
    live_structured_prompt: bool,
    live_provider_session_identity: bool,
    live_semantic_resume: bool,
) -> ProviderRuntimePolicy {
    let raw_pty_lifecycle = static_capabilities.raw_pty && live_raw_pty_lifecycle;
    let semantic_readiness = raw_pty_lifecycle
        && static_capabilities.semantic_pty_adapter
        && live_semantic_readiness;
    let structured_prompt = semantic_readiness && live_structured_prompt;
    let provider_session_identity = raw_pty_lifecycle
        && static_capabilities.semantic_pty_adapter
        && live_provider_session_identity;
    let semantic_resume = provider_session_identity
        && static_capabilities.resume_adapter
        && live_semantic_resume;
    ProviderRuntimePolicy::new(
        raw_pty_lifecycle,
        semantic_readiness,
        structured_prompt,
        provider_session_identity,
        semantic_resume,
    )
    .expect("static and live provider capability intersection is internally valid")
}

fn status_from_resolution(
    provider: AgentId,
    resolution: &VendorContractResolution,
) -> ProviderRuntimeStatus {
    let version = resolution
        .normalized_version()
        .and_then(|version| ProviderRuntimeVersion::new(version).ok());
    if resolution.mode() == VendorRuntimeMode::VerifiedSemantic {
        match (
            version.clone(),
            resolution
                .contract_id()
                .and_then(|contract_id| ProviderRuntimeContractId::new(contract_id).ok()),
        ) {
            (Some(version), Some(contract_id)) => {
                ProviderRuntimeStatus::verified_semantic(provider, version, contract_id)
            }
            _ => ProviderRuntimeStatus::raw_passthrough(provider, version),
        }
    } else {
        ProviderRuntimeStatus::raw_passthrough(provider, version)
    }
}

fn resolve_local_launcher(program: &str) -> Option<PathBuf> {
    if program.is_empty() || program.contains('\0') {
        return None;
    }
    let program_path = Path::new(program);
    if program_path.is_absolute() {
        return program_path.is_file().then(|| program_path.to_path_buf());
    }
    if program_path.components().count() != 1 {
        return None;
    }
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path).filter(|entry| entry.is_absolute()) {
        for candidate in launcher_candidates(&directory, program) {
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn launcher_candidates(directory: &Path, program: &str) -> Vec<PathBuf> {
    vec![directory.join(program)]
}

#[cfg(windows)]
fn launcher_candidates(directory: &Path, program: &str) -> Vec<PathBuf> {
    if Path::new(program).extension().is_some() {
        return vec![directory.join(program)];
    }
    [".com", ".exe", ".bat", ".cmd"]
        .into_iter()
        .map(|extension| directory.join(format!("{program}{extension}")))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gate4agent_catalog::builtin_registry;
    use gate4agent_types::AgentId;

    #[test]
    fn startup_provider_runtime_inventory_preserves_catalog_availability() {
        let mut spec = builtin_registry().get_by_id("claude").unwrap().clone();
        spec.id = AgentId::new("claude").unwrap();
        spec.launch.program = std::env::temp_dir()
            .join(format!(
                "gate4agent-provider-runtime-missing-{}{}",
                std::process::id(),
                std::env::consts::EXE_SUFFIX,
            ))
            .to_string_lossy()
            .into_owned();
        let catalog = AgentRegistry::new([spec]).unwrap();

        let statuses = ProviderRuntimeMonitor::new(&catalog).collect();

        assert_eq!(statuses.as_slice().len(), 1);
        assert_eq!(statuses.as_slice()[0].provider(), &AgentId::new("claude").unwrap());
        assert_eq!(
            statuses.as_slice()[0].mode(),
            crate::protocol::ProviderRuntimeMode::Unavailable,
        );
        assert_eq!(
            catalog.iter().map(|spec| spec.id.as_str()).collect::<Vec<_>>(),
            vec!["claude"],
        );
    }

    #[test]
    fn spawn_admission_is_mode_aware_and_version_agnostic() {
        let unavailable = AgentId::new("unavailable").unwrap();
        let unknown = AgentId::new("unknown-version").unwrap();
        let future = AgentId::new("future-version").unwrap();
        let verified = AgentId::new("verified").unwrap();
        let statuses = ProviderRuntimeStatuses::new([
            ProviderRuntimeStatus::unavailable(unavailable.clone()),
            ProviderRuntimeStatus::raw_passthrough(unknown.clone(), None),
            ProviderRuntimeStatus::raw_passthrough(
                future.clone(),
                Some(ProviderRuntimeVersion::new("999.0.0").unwrap()),
            ),
            ProviderRuntimeStatus::verified_semantic(
                verified.clone(),
                ProviderRuntimeVersion::new("1.0.0").unwrap(),
                ProviderRuntimeContractId::new("verified.contract-v1").unwrap(),
            ),
        ])
        .unwrap();

        for provider in [&unknown, &future, &verified] {
            assert_eq!(
                admit_status(&statuses, provider, ProviderRuntimeRequirement::RawPty)
                    .map(|policy| policy.raw_pty_lifecycle),
                Ok(true),
            );
        }
        for provider in [&unknown, &future] {
            assert_eq!(
                admit_status(
                    &statuses,
                    provider,
                    ProviderRuntimeRequirement::SemanticPrompt,
                ),
                Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
            );
        }
        assert_eq!(
            admit_status(
                &statuses,
                &verified,
                ProviderRuntimeRequirement::SemanticPrompt,
            ),
            Ok(ProviderRuntimePolicy::new(true, true, true, false, false).unwrap()),
        );
        assert_eq!(
            admit_status(&statuses, &unavailable, ProviderRuntimeRequirement::RawPty),
            Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
        );
        assert_eq!(
            admit_status(
                &statuses,
                &AgentId::new("missing-status").unwrap(),
                ProviderRuntimeRequirement::RawPty,
            ),
            Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
        );
    }

    #[test]
    fn spawn_admission_reprobes_launcher_availability() {
        let launcher = std::env::temp_dir().join(format!(
            "gate4agent-runtime-monitor-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX,
        ));
        std::fs::write(&launcher, b"fixture launcher identity").unwrap();
        let mut spec = builtin_registry().get_by_id("grok").unwrap().clone();
        spec.launch.program = launcher.to_string_lossy().into_owned();
        let catalog = AgentRegistry::new([spec]).unwrap();
        let monitor = ProviderRuntimeMonitor::new(&catalog);
        let provider = AgentId::new("grok").unwrap();

        let (available, admitted) = monitor.evaluate(&provider);
        let available = available.unwrap();
        assert_eq!(available.mode(), ProviderRuntimeMode::RawPassthrough);
        assert!(admitted.unwrap().raw_pty_lifecycle);

        std::fs::remove_file(&launcher).unwrap();
        let (missing, rejected) = monitor.evaluate(&provider);
        let missing = missing.unwrap();
        assert_eq!(missing.mode(), ProviderRuntimeMode::Unavailable);
        assert_eq!(
            rejected,
            Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
        );
    }

    #[test]
    fn qwen_sidecar_admission_skips_version_probe_and_non_qwen_does_not() {
        let launcher = std::env::temp_dir().join(format!(
            "gate4agent-qwen-sidecar-runtime-monitor-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX,
        ));
        std::fs::write(&launcher, b"fixture launcher identity").unwrap();
        let mut qwen = builtin_registry().get_by_id("qwen-code").unwrap().clone();
        qwen.launch.program = launcher.to_string_lossy().into_owned();
        let mut grok = builtin_registry().get_by_id("grok").unwrap().clone();
        grok.launch.program = launcher.to_string_lossy().into_owned();
        let catalog = AgentRegistry::new([qwen, grok]).unwrap();
        let monitor = ProviderRuntimeMonitor::new(&catalog);
        let cache_guard = monitor.probe_cache.lock().unwrap();

        let (qwen_status, qwen_admission) =
            monitor.evaluate(&AgentId::new("qwen-code").unwrap());
        assert_eq!(qwen_status.unwrap().mode(), ProviderRuntimeMode::RawPassthrough);
        assert_eq!(
            qwen_admission,
            Ok(ProviderRuntimePolicy::new(true, true, false, false, false).unwrap())
        );
        let (grok_status, grok_admission) = monitor.evaluate(&AgentId::new("grok").unwrap());
        assert!(grok_status.is_none());
        assert_eq!(grok_admission, Err(ProviderRuntimeAdmissionError::ProbeBusy));

        drop(cache_guard);
        std::fs::remove_file(launcher).unwrap();
    }

    #[test]
    fn spawn_admission_fails_bounded_when_probe_is_busy() {
        let launcher = std::env::temp_dir().join(format!(
            "gate4agent-runtime-monitor-busy-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX,
        ));
        std::fs::write(&launcher, b"fixture launcher identity").unwrap();
        let mut spec = builtin_registry().get_by_id("grok").unwrap().clone();
        spec.launch.program = launcher.to_string_lossy().into_owned();
        let catalog = AgentRegistry::new([spec]).unwrap();
        let monitor = ProviderRuntimeMonitor::new(&catalog);
        let cache_guard = monitor.probe_cache.lock().unwrap();

        let (status, admission) = monitor.evaluate(&AgentId::new("grok").unwrap());
        assert!(status.is_none());
        assert_eq!(admission, Err(ProviderRuntimeAdmissionError::ProbeBusy));

        drop(cache_guard);
        std::fs::remove_file(launcher).unwrap();
    }

    #[test]
    fn composite_runtime_policy_intersects_live_and_static_capabilities() {
        let full_static = ProviderStaticCapabilities {
            launch_program: "fixture".to_owned(),
            raw_pty: true,
            semantic_pty_adapter: true,
            resume_adapter: true,
            pty_sidecar_observation: false,
        };
        let full = policy_from_capability_flags(
            &full_static,
            true,
            true,
            true,
            true,
            true,
        );
        assert_eq!(
            full,
            ProviderRuntimePolicy::new(true, true, true, true, true).unwrap(),
        );

        let no_static_resume = ProviderStaticCapabilities {
            resume_adapter: false,
            ..full_static.clone()
        };
        assert_eq!(
            policy_from_capability_flags(
                &no_static_resume,
                true,
                true,
                true,
                true,
                true,
            ),
            ProviderRuntimePolicy::new(true, true, true, true, false).unwrap(),
        );

        assert_eq!(
            policy_from_capability_flags(
                &full_static,
                true,
                false,
                true,
                true,
                true,
            ),
            ProviderRuntimePolicy::new(true, false, false, true, true).unwrap(),
        );

        let no_semantic_adapter = ProviderStaticCapabilities {
            semantic_pty_adapter: false,
            ..full_static
        };
        assert_eq!(
            policy_from_capability_flags(
                &no_semantic_adapter,
                true,
                true,
                true,
                true,
                true,
            ),
            ProviderRuntimePolicy::raw_pty(),
        );
    }

    #[test]
    fn raw_pty_admits_provider_native_resume_without_prompt_only() {
        let raw = ProviderRuntimePolicy::raw_pty();

        assert_eq!(
            require_policy(raw, ProviderRuntimeRequirement::Resume),
            Ok(()),
        );
        assert_eq!(
            require_policy(raw, ProviderRuntimeRequirement::ResumeWithPrompt),
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
        );
    }
}

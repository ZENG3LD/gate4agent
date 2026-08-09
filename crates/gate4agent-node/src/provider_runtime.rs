use crate::protocol::{
    ProviderRuntimeContractId, ProviderRuntimeMode, ProviderRuntimeStatus,
    ProviderRuntimeStatuses, ProviderRuntimeVersion,
};
use gate4agent_catalog::AgentRegistry;
use gate4agent_runtime_native::{
    VendorContractResolution, VendorRuntimeMode, VendorVersionProbeCache,
};
use gate4agent_types::AgentId;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const VERSION_PROBE_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderRuntimeRequirement {
    RawPty,
    SemanticPty,
    Inline,
    Resume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderRuntimeAdmissionError {
    LauncherUnavailable,
    SemanticCapabilityUnverified,
    ProbeBusy,
}

pub(crate) struct ProviderRuntimeMonitor {
    launch_programs: BTreeMap<AgentId, String>,
    probe_cache: Mutex<VendorVersionProbeCache>,
}

impl ProviderRuntimeMonitor {
    pub(crate) fn new(catalog: &AgentRegistry) -> Self {
        Self {
            launch_programs: catalog
                .iter()
                .map(|spec| (spec.id.clone(), spec.launch.program.clone()))
                .collect(),
            probe_cache: Mutex::new(VendorVersionProbeCache::default()),
        }
    }

    pub(crate) fn collect(&self) -> ProviderRuntimeStatuses {
        ProviderRuntimeStatuses::new(
            self.launch_programs
                .keys()
                .map(|provider| {
                    self.evaluate(provider, ProviderRuntimeRequirement::RawPty)
                        .0
                        .expect("startup provider probe cache is uncontended")
                }),
        )
        .expect("the validated node catalog remains within the provider identity limit")
    }

    pub(crate) fn evaluate(
        &self,
        provider: &AgentId,
        requirement: ProviderRuntimeRequirement,
    ) -> (Option<ProviderRuntimeStatus>, Result<(), ProviderRuntimeAdmissionError>) {
        let Some(program) = self.launch_programs.get(provider) else {
            return (
                Some(ProviderRuntimeStatus::unavailable(provider.clone())),
                Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
            );
        };
        let Some(launcher) = resolve_local_launcher(program) else {
            return (
                Some(ProviderRuntimeStatus::unavailable(provider.clone())),
                Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
            );
        };
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
        let admission = if resolution_admits(resolution, requirement) {
            Ok(())
        } else {
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified)
        };
        (Some(status), admission)
    }
}

pub(crate) fn admit_status(
    statuses: &ProviderRuntimeStatuses,
    provider: &AgentId,
    requirement: ProviderRuntimeRequirement,
) -> Result<(), ProviderRuntimeAdmissionError> {
    let Some(status) = statuses
        .iter()
        .find(|status| status.provider() == provider)
    else {
        return Err(ProviderRuntimeAdmissionError::LauncherUnavailable);
    };
    match (status.mode(), requirement) {
        (ProviderRuntimeMode::Unavailable, _) => {
            Err(ProviderRuntimeAdmissionError::LauncherUnavailable)
        }
        (ProviderRuntimeMode::RawPassthrough, ProviderRuntimeRequirement::RawPty)
        | (
            ProviderRuntimeMode::VerifiedSemantic,
            ProviderRuntimeRequirement::RawPty | ProviderRuntimeRequirement::SemanticPty,
        ) => Ok(()),
        (ProviderRuntimeMode::RawPassthrough, _)
        | (ProviderRuntimeMode::VerifiedSemantic, _) => {
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified)
        }
    }
}

fn resolution_admits(
    resolution: &VendorContractResolution,
    requirement: ProviderRuntimeRequirement,
) -> bool {
    let capabilities = resolution.capabilities();
    match requirement {
        ProviderRuntimeRequirement::RawPty => resolution.admits_raw_pty_lifecycle(),
        ProviderRuntimeRequirement::SemanticPty => {
            capabilities.semantic_readiness.is_verified()
                && capabilities.structured_prompt.is_verified()
        }
        ProviderRuntimeRequirement::Inline => false,
        ProviderRuntimeRequirement::Resume => capabilities.semantic_resume.is_verified(),
    }
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
                admit_status(&statuses, provider, ProviderRuntimeRequirement::RawPty),
                Ok(()),
            );
        }
        for provider in [&unknown, &future] {
            assert_eq!(
                admit_status(
                    &statuses,
                    provider,
                    ProviderRuntimeRequirement::SemanticPty,
                ),
                Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
            );
        }
        assert_eq!(
            admit_status(
                &statuses,
                &verified,
                ProviderRuntimeRequirement::SemanticPty,
            ),
            Ok(()),
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

        let (available, admitted) =
            monitor.evaluate(&provider, ProviderRuntimeRequirement::RawPty);
        let available = available.unwrap();
        assert_eq!(available.mode(), ProviderRuntimeMode::RawPassthrough);
        assert_eq!(admitted, Ok(()));

        std::fs::remove_file(&launcher).unwrap();
        let (missing, rejected) =
            monitor.evaluate(&provider, ProviderRuntimeRequirement::RawPty);
        let missing = missing.unwrap();
        assert_eq!(missing.mode(), ProviderRuntimeMode::Unavailable);
        assert_eq!(
            rejected,
            Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
        );
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

        let (status, admission) = monitor.evaluate(
            &AgentId::new("grok").unwrap(),
            ProviderRuntimeRequirement::RawPty,
        );
        assert!(status.is_none());
        assert_eq!(admission, Err(ProviderRuntimeAdmissionError::ProbeBusy));

        drop(cache_guard);
        std::fs::remove_file(launcher).unwrap();
    }
}

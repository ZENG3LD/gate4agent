use crate::protocol::{
    ProviderRuntimeContractId, ProviderRuntimeStatus,
    ProviderRuntimeStatuses, ProviderRuntimeVersion,
};
use gate4agent_catalog::AgentRegistry;
use gate4agent_runtime_native::{
    VendorRuntimeMode, VendorVersionProbeCache,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const VERSION_PROBE_DEADLINE: Duration = Duration::from_secs(2);

pub(crate) fn collect(catalog: &AgentRegistry) -> ProviderRuntimeStatuses {
    let mut cache = VendorVersionProbeCache::default();
    let statuses = catalog.iter().filter_map(|spec| {
        let provider = spec.id.clone();
        let Some(launcher) = resolve_local_launcher(&spec.launch.program) else {
            return Some(ProviderRuntimeStatus::unavailable(provider));
        };
        let probe = cache.probe(
            spec.id.as_str(),
            &launcher,
            Instant::now() + VERSION_PROBE_DEADLINE,
        );
        let resolution = probe.resolution();
        let version = resolution
            .normalized_version()
            .and_then(|version| ProviderRuntimeVersion::new(version).ok());
        let status = if resolution.mode() == VendorRuntimeMode::VerifiedSemantic {
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
        };
        Some(status)
    });
    ProviderRuntimeStatuses::new(statuses)
        .expect("the validated node catalog remains within the provider identity limit")
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

        let statuses = collect(&catalog);

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
}

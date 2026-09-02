use crate::protocol::{
    ProviderRuntimeContractId, ProviderRuntimeMode, ProviderRuntimeStatus,
    ProviderRuntimeStatuses, ProviderRuntimeVersion,
};
use gate4agent_catalog::AgentRegistry;
use gate4agent_runtime_native::{
    VendorContractResolution, VendorRuntimeMode, VendorVersionProbeCache,
};
use gate4agent_types::{AgentId, ProviderRuntimePolicy, TransportKind};
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
    Acp,
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
    hook_adapter: bool,
    pipe_transport: bool,
    acp_transport: bool,
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
                            hook_adapter: spec.capabilities.adapters.hook.is_some(),
                            pipe_transport: spec.capabilities.transports.pipe.is_some(),
                            acp_transport: spec.capabilities.transports.acp.is_some(),
                        },
                    )
                })
                .collect(),
            probe_cache: Mutex::new(VendorVersionProbeCache::default()),
        }
    }

    /// Whether the catalog declares a Pipe transport for this provider --
    /// `PipeSession` (NDJSON over stdio, no PTY) has no vendor terminal
    /// contract to verify, so unlike `raw_pty_lifecycle` this fact is never
    /// derived from a live probe, and it never changes once the catalog is
    /// loaded. It is the sole authority `require_policy` uses to admit or
    /// reject `ProviderRuntimeRequirement::Inline`.
    pub(crate) fn supports_pipe_transport(&self, provider: &AgentId) -> bool {
        self.providers
            .get(provider)
            .is_some_and(|capabilities| capabilities.pipe_transport)
    }

    /// Whether the catalog declares an ACP transport for this provider --
    /// ACP speaks structured JSON-RPC over stdio, not a PTY, so exactly like
    /// `pipe_transport` this is never derived from a live probe. It is the
    /// sole authority `require_policy` uses to admit or reject
    /// `ProviderRuntimeRequirement::Acp`.
    pub(crate) fn supports_acp_transport(&self, provider: &AgentId) -> bool {
        self.providers
            .get(provider)
            .is_some_and(|capabilities| capabilities.acp_transport)
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
            // The pipe sidecar observes structured events over its own
            // adapter, not a hook route -- honestly derive hook_semantics
            // the same way the primary path does rather than hardcoding it,
            // even though the catalog never declares both adapters for one
            // provider today.
            let hook_semantics = static_capabilities.raw_pty && static_capabilities.hook_adapter;
            let policy = ProviderRuntimePolicy::new(
                static_capabilities.raw_pty,
                true,
                false,
                false,
                false,
                hook_semantics,
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
    pipe_transport: bool,
    acp_transport: bool,
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
        // This coarse fallback answers only the requirements in
        // `ProviderRuntimeRequirement` (raw PTY / semantic prompt / resume),
        // none of which reference `HookSemantics` -- it carries no hook
        // signal because it is not derived from the catalog's adapter
        // declarations at all.
        ProviderRuntimeMode::VerifiedSemantic => ProviderRuntimePolicy::new(
            true,
            true,
            true,
            false,
            false,
            false,
        )
        .expect("coarse verified semantic fallback policy is internally valid"),
    };
    // `admit_status` has no catalog reference of its own to consult -- the
    // caller supplies whatever it already knows about the declared Pipe and
    // ACP transports, the same way `require_policy` takes them as bare
    // arguments rather than reading them off `policy`.
    require_policy(policy, pipe_transport, acp_transport, requirement).map(|()| policy)
}

pub(crate) fn require_policy(
    policy: ProviderRuntimePolicy,
    pipe_transport: bool,
    acp_transport: bool,
    requirement: ProviderRuntimeRequirement,
) -> Result<(), ProviderRuntimeAdmissionError> {
    let admitted = match requirement {
        ProviderRuntimeRequirement::RawPty => policy.raw_pty_lifecycle,
        ProviderRuntimeRequirement::SemanticPrompt => {
            policy.semantic_readiness && policy.structured_prompt
        }
        // `PipeSession` (Inline) speaks NDJSON over stdio, not a PTY -- none
        // of this policy's fields (all PTY-terminal-text-inference
        // verification: raw_pty_lifecycle, semantic_readiness, ...) describe
        // it, so there is nothing on `policy` to gate on, the same way ACP
        // below has nothing to gate on. Unlike ACP, though, the catalog fact
        // that answers "does this provider support inline" --
        // `spec.capabilities.transports.pipe.is_some()` -- is NOT enforced
        // anywhere this node can rely on: `gate4agent-kernel` re-checks the
        // analogous fact for `TransportKind::Pipe` at its own `Register`
        // time, but that crate models a different actor's session state
        // machine and this node never links against it, so a request
        // landing directly on this node's own RPC surface (`SpawnSession`,
        // managed-worktree spawn) has no other gate in front of it. The
        // caller supplies the catalog answer as `pipe_transport`, captured
        // once into `ProviderStaticCapabilities` at `ProviderRuntimeMonitor`
        // construction time.
        ProviderRuntimeRequirement::Inline => pipe_transport,
        // ACP speaks a structured JSON-RPC protocol over stdio, not a PTY --
        // none of this policy's fields (all PTY-terminal-text-inference
        // verification: raw_pty_lifecycle, semantic_readiness, ...) describe
        // it, so there is nothing on `policy` to gate on -- same reasoning
        // as `Inline` immediately above, and the same fix: `gate4agent-node`
        // never links `gate4agent-kernel`, so that crate's own `Register`-time
        // check of `spec.capabilities.transports.acp.is_some()` governs a
        // different actor's session state machine, not a request landing
        // directly on this node's own RPC surface. The caller supplies the
        // catalog answer as `acp_transport`, captured once into
        // `ProviderStaticCapabilities` at `ProviderRuntimeMonitor`
        // construction time, the same way `pipe_transport` is.
        ProviderRuntimeRequirement::Acp => acp_transport,
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

/// Computes the `ProviderRuntimePolicy` a session may actually operate
/// under, keyed on ITS OWN transport -- an explicit, exhaustive `match` with
/// no catch-all arm, so a `TransportKind` variant added later fails this
/// build instead of silently inheriting whichever arm happens to sit last.
///
/// `pty_probed_policy` is whatever `ProviderRuntimeMonitor::evaluate` (or the
/// no-monitor `admit_status` fallback) derived from the vendor terminal
/// contract table (`VERIFIED_PROFILES`) -- sound only for a session that is
/// actually going to run over a PTY, because that table encodes verified PTY
/// terminal BEHAVIOUR, not a fact about any other transport.
///
/// Pipe (NDJSON/JSONL over stdio) genuinely has no PTY either, and in
/// principle its `structured_prompt` grant should come from the Pipe
/// transport contract the catalog declares, not from this provider's
/// unrelated PTY verification status -- the same shape of fix ACP gets
/// below. It does NOT get that fix in this pass: `gate4agent-shell-native`'s
/// `validate_spawn_runtime_policy` and `gate4agent-runtime-native`'s
/// `validate_effect_runtime_policy` both still require `RawPtyLifecycle`
/// (and, since Pipe is not `Pty`, `SemanticReadiness`) before a Pipe spawn
/// effect is allowed to execute at all -- neither was changed to a
/// transport-aware equivalent this pass, so handing Pipe a `raw_pty_
/// lifecycle: false` policy here would make every currently-working Pipe
/// spawn fail at one of those two layers instead. Pipe therefore stays a
/// pass-through of the PTY probe, unchanged, exactly like Pty.
pub(crate) fn policy_for_transport(
    transport: TransportKind,
    pty_probed_policy: ProviderRuntimePolicy,
) -> ProviderRuntimePolicy {
    match transport {
        // The transport `pty_probed_policy`'s fields actually describe, and
        // (see the function doc above) Pipe, which still relies on the same
        // PTY-verification-derived value downstream -- both passed through
        // unchanged, byte for byte.
        TransportKind::Pty | TransportKind::Pipe => pty_probed_policy,
        // ACP speaks structured JSON-RPC (`session/prompt`, `session/update`)
        // as MANDATORY protocol surface -- there is genuinely no PTY, and
        // session readiness / prompt delivery are facts of the ACP protocol
        // itself, not an inference this build makes by parsing PTY terminal
        // text. Granting them outright is therefore correct, not a
        // downgrade of the PTY-terminal-verification story: that story does
        // not apply to this transport at all. `provider_session_identity` is
        // granted for the same reason: ACP's `session/new` returns a
        // `sessionId` by specification, and `AcpSession::acp_session_id`
        // merely reads it back -- it is a fact of the protocol, not
        // something inferred from a terminal, exactly like
        // `semantic_readiness`/`structured_prompt` above. `semantic_resume`/
        // `hook_semantics` stay unset -- the ACP spec gives no equivalent
        // guarantee for resuming a prior session, and the engine separately
        // refuses ACP resume outright, so granting it here would assert a
        // capability nothing downstream can act on. Both
        // `gate4agent-shell-native` and `gate4agent-runtime-native` already
        // bypass their own PTY-semantic policy check unconditionally for
        // `TransportKind::Acp`, so this shape needs no matching change there.
        TransportKind::Acp => ProviderRuntimePolicy::new(false, true, true, true, false, false)
            .expect("ACP transport policy is internally valid"),
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
    // Hook-sourced events are asserted directly by the provider CLI over an
    // authenticated route and normalized by the node's own hook adapter --
    // there is no terminal behaviour to verify, so unlike every other
    // capability above this one carries no `live_*` term at all. The vendor
    // contract table (`VERIFIED_PROFILES`) has nothing to say about a
    // channel the vendor drives itself, and gating it on that table is
    // exactly the bug this capability exists to fix: a provider with a
    // declared hook adapter but no verified terminal contract (grok, codex,
    // kimi) would otherwise never admit a single hook event.
    let hook_semantics = raw_pty_lifecycle && static_capabilities.hook_adapter;
    ProviderRuntimePolicy::new(
        raw_pty_lifecycle,
        semantic_readiness,
        structured_prompt,
        provider_session_identity,
        semantic_resume,
        hook_semantics,
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
                admit_status(&statuses, provider, false, false, ProviderRuntimeRequirement::RawPty)
                    .map(|policy| policy.raw_pty_lifecycle),
                Ok(true),
            );
        }
        for provider in [&unknown, &future] {
            assert_eq!(
                admit_status(
                    &statuses,
                    provider,
                    false,
                    false,
                    ProviderRuntimeRequirement::SemanticPrompt,
                ),
                Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
            );
        }
        assert_eq!(
            admit_status(
                &statuses,
                &verified,
                false,
                false,
                ProviderRuntimeRequirement::SemanticPrompt,
            ),
            Ok(ProviderRuntimePolicy::new(true, true, true, false, false, false).unwrap()),
        );
        assert_eq!(
            admit_status(&statuses, &unavailable, false, false, ProviderRuntimeRequirement::RawPty),
            Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
        );
        assert_eq!(
            admit_status(
                &statuses,
                &AgentId::new("missing-status").unwrap(),
                false,
                false,
                ProviderRuntimeRequirement::RawPty,
            ),
            Err(ProviderRuntimeAdmissionError::LauncherUnavailable),
        );
    }

    /// `admit_status` never sees the catalog -- its only production caller
    /// (the `fixture_raw_pty_runtime` bypass) always passes `false` for
    /// `pipe_transport`, so Inline is rejected through this path exactly as
    /// unconditionally as it is admitted through it when the caller says so.
    /// This is the fallback-path half of the coverage
    /// `ProviderRuntimeMonitor`'s own tests give the catalog-backed half.
    #[test]
    fn admit_status_honours_the_caller_supplied_pipe_transport_fact() {
        let provider = AgentId::new("claude").unwrap();
        let statuses = ProviderRuntimeStatuses::new([ProviderRuntimeStatus::raw_passthrough(
            provider.clone(),
            None,
        )])
        .unwrap();

        assert_eq!(
            admit_status(&statuses, &provider, false, false, ProviderRuntimeRequirement::Inline),
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
        );
        assert_eq!(
            admit_status(&statuses, &provider, true, false, ProviderRuntimeRequirement::Inline)
                .map(|policy| policy.raw_pty_lifecycle),
            Ok(true),
        );
    }

    /// Symmetric coverage for `acp_transport` through the same
    /// catalog-blind fallback: rejected when the caller says no, admitted
    /// when the caller says yes, independent of `pipe_transport`.
    #[test]
    fn admit_status_honours_the_caller_supplied_acp_transport_fact() {
        let provider = AgentId::new("grok").unwrap();
        let statuses = ProviderRuntimeStatuses::new([ProviderRuntimeStatus::raw_passthrough(
            provider.clone(),
            None,
        )])
        .unwrap();

        assert_eq!(
            admit_status(&statuses, &provider, false, false, ProviderRuntimeRequirement::Acp),
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
        );
        assert_eq!(
            admit_status(&statuses, &provider, false, true, ProviderRuntimeRequirement::Acp)
                .map(|policy| policy.raw_pty_lifecycle),
            Ok(true),
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

    /// Grok declares a hook adapter and no PTY-semantic adapter, and has no
    /// verified vendor terminal contract at all. It must admit hook-sourced
    /// ingestion without ever being read as having verified PTY-parsing
    /// semantics -- the pair is the whole point of splitting the two
    /// capabilities, so both halves are asserted here.
    #[test]
    fn grok_admits_hook_semantics_without_semantic_readiness() {
        let launcher = std::env::temp_dir().join(format!(
            "gate4agent-grok-hook-semantics-runtime-monitor-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX,
        ));
        std::fs::write(&launcher, b"fixture launcher identity").unwrap();
        let mut spec = builtin_registry().get_by_id("grok").unwrap().clone();
        assert!(spec.capabilities.adapters.hook.is_some());
        assert!(spec.capabilities.transports.pty_adapter.is_none());
        spec.launch.program = launcher.to_string_lossy().into_owned();
        let catalog = AgentRegistry::new([spec]).unwrap();
        let monitor = ProviderRuntimeMonitor::new(&catalog);

        let (_, admitted) = monitor.evaluate(&AgentId::new("grok").unwrap());
        let policy = admitted.unwrap();
        assert!(policy.hook_semantics);
        assert!(!policy.semantic_readiness);

        std::fs::remove_file(&launcher).unwrap();
    }

    #[test]
    fn pty_sidecar_admission_skips_version_probe_and_a_non_sidecar_provider_does_not() {
        let launcher = std::env::temp_dir().join(format!(
            "gate4agent-pty-sidecar-runtime-monitor-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX,
        ));
        std::fs::write(&launcher, b"fixture launcher identity").unwrap();
        // No fleet provider declares a PTY sidecar today, so this augments a
        // clone of `codex`'s own real, globally-registered Pipe binding into
        // a synthetic PTY sidecar, standing in for a live registry lookup.
        // `pty_sidecar_observation` is generic (keyed on the binding's
        // presence, not on any literal provider id), so which real Pipe
        // binding it is built from is not load-bearing.
        let mut sidecar_fixture = builtin_registry().get_by_id("codex").unwrap().clone();
        sidecar_fixture.id = AgentId::new("pty-sidecar-fixture").unwrap();
        sidecar_fixture.detection.command = "pty-sidecar-fixture".to_owned();
        let sidecar = sidecar_fixture.capabilities.transports.pipe.clone().unwrap().adapter;
        sidecar_fixture.capabilities.transports.pipe = None;
        sidecar_fixture.capabilities.adapters.pty_sidecar = Some(sidecar);
        // No Hook adapter -- matched here so `hook_semantics` reflects the
        // shape under test, not an artifact of the `codex` skeleton it was
        // cloned from.
        sidecar_fixture.capabilities.adapters.hook = None;
        sidecar_fixture.launch.program = launcher.to_string_lossy().into_owned();
        let mut grok = builtin_registry().get_by_id("grok").unwrap().clone();
        grok.launch.program = launcher.to_string_lossy().into_owned();
        let catalog = AgentRegistry::new([sidecar_fixture, grok]).unwrap();
        let monitor = ProviderRuntimeMonitor::new(&catalog);
        let cache_guard = monitor.probe_cache.lock().unwrap();

        let (sidecar_status, sidecar_admission) =
            monitor.evaluate(&AgentId::new("pty-sidecar-fixture").unwrap());
        assert_eq!(sidecar_status.unwrap().mode(), ProviderRuntimeMode::RawPassthrough);
        assert_eq!(
            sidecar_admission,
            Ok(ProviderRuntimePolicy::new(true, true, false, false, false, false).unwrap())
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
            hook_adapter: true,
            pipe_transport: true,
            acp_transport: true,
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
            ProviderRuntimePolicy::new(true, true, true, true, true, true).unwrap(),
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
            ProviderRuntimePolicy::new(true, true, true, true, false, true).unwrap(),
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
            ProviderRuntimePolicy::new(true, false, false, true, true, true).unwrap(),
        );

        // Grok's exact shape: no PTY-semantic adapter, but a declared hook
        // adapter. `hook_semantics` carries no `live_*` term, so it stays
        // admitted here even though `semantic_readiness` collapses to
        // false -- the split this whole capability exists for.
        let no_semantic_adapter = ProviderStaticCapabilities {
            semantic_pty_adapter: false,
            ..full_static
        };
        let hook_only = policy_from_capability_flags(
            &no_semantic_adapter,
            true,
            true,
            true,
            true,
            true,
        );
        assert_eq!(
            hook_only,
            ProviderRuntimePolicy::new(true, false, false, false, false, true).unwrap(),
        );
        assert!(hook_only.hook_semantics);
        assert!(!hook_only.semantic_readiness);

        let no_static_hook = ProviderStaticCapabilities {
            hook_adapter: false,
            ..no_semantic_adapter
        };
        assert_eq!(
            policy_from_capability_flags(
                &no_static_hook,
                true,
                true,
                true,
                true,
                true,
            ),
            ProviderRuntimePolicy::raw_pty(),
        );
    }

    /// `policy_for_transport` is the fix for the live-measured defect: a PTY
    /// transport must come back byte-for-byte identical to whatever the PTY
    /// probe computed -- no row in `VERIFIED_PROFILES` and no PTY behaviour
    /// changes because a session for a DIFFERENT transport exists.
    #[test]
    fn policy_for_transport_pty_passes_the_probed_policy_through_unchanged() {
        let probed = ProviderRuntimePolicy::new(true, true, true, true, true, true).unwrap();
        assert_eq!(policy_for_transport(TransportKind::Pty, probed), probed);

        let raw = ProviderRuntimePolicy::raw_pty();
        assert_eq!(policy_for_transport(TransportKind::Pty, raw), raw);
    }

    /// ACP grants `SemanticReadiness`/`StructuredPrompt`/
    /// `ProviderSessionIdentity` as facts of the protocol with
    /// `RawPtyLifecycle` false -- regardless of what the (irrelevant,
    /// PTY-shaped) `pty_probed_policy` argument says, since this transport
    /// has no PTY to have probed in the first place. `session/new` returns a
    /// `sessionId` by specification, the same class of protocol fact as
    /// `session/prompt`/`session/update`, so `ProviderSessionIdentity` is
    /// granted alongside them, not withheld the way `SemanticResume`/
    /// `HookSemantics` (no ACP resume guarantee, no hook wiring for this
    /// transport) still are.
    #[test]
    fn policy_for_transport_acp_grants_semantic_prompt_with_no_raw_pty() {
        let unrelated_pty_probe = ProviderRuntimePolicy::raw_pty();
        let policy = policy_for_transport(TransportKind::Acp, unrelated_pty_probe);
        assert_eq!(
            policy,
            ProviderRuntimePolicy::new(false, true, true, true, false, false).unwrap(),
        );
        assert!(!policy.raw_pty_lifecycle);
        assert!(policy.semantic_readiness);
        assert!(policy.structured_prompt);
        assert!(policy.provider_session_identity);
        assert!(!policy.semantic_resume);
        assert!(!policy.hook_semantics);

        // A verified PTY probe result must not change the ACP answer either
        // -- ACP's grant is unconditional on the transport alone.
        let verified_pty_probe = ProviderRuntimePolicy::new(true, true, true, true, true, true)
            .unwrap();
        assert_eq!(
            policy_for_transport(TransportKind::Acp, verified_pty_probe),
            policy,
        );
    }

    /// Pipe is NOT ACP: `gate4agent-shell-native`/`gate4agent-runtime-native`
    /// still require `RawPtyLifecycle` (and `SemanticReadiness`, since Pipe
    /// is not Pty) before a Pipe spawn effect is allowed to execute at all,
    /// so this stays an exact pass-through of the PTY-probed policy, just
    /// like Pty -- forcing it to the ACP shape here would make every Pipe
    /// spawn admitted by THIS crate fail one layer down instead.
    #[test]
    fn policy_for_transport_pipe_passes_the_probed_policy_through_unchanged() {
        let probed = ProviderRuntimePolicy::new(true, true, true, true, true, true).unwrap();
        assert_eq!(policy_for_transport(TransportKind::Pipe, probed), probed);

        let raw = ProviderRuntimePolicy::raw_pty();
        assert_eq!(policy_for_transport(TransportKind::Pipe, raw), raw);
    }

    #[test]
    fn raw_pty_admits_provider_native_resume_without_prompt_only() {
        let raw = ProviderRuntimePolicy::raw_pty();

        assert_eq!(
            require_policy(raw, false, false, ProviderRuntimeRequirement::Resume),
            Ok(()),
        );
        assert_eq!(
            require_policy(raw, false, false, ProviderRuntimeRequirement::ResumeWithPrompt),
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
        );
    }

    /// Regression coverage for the defect this module used to carry: `Inline`
    /// (`PipeSession`, NDJSON over stdio) was rejected unconditionally for
    /// every provider regardless of what the catalog declared. The policy
    /// argument here carries nothing relevant to Inline -- see the comment on
    /// the `Inline` match arm -- so the whole decision must come from
    /// `pipe_transport`, and both directions have to be exact.
    #[test]
    fn inline_requirement_is_admitted_only_when_pipe_transport_is_declared() {
        let admitted = ProviderRuntimePolicy::raw_pty();

        assert_eq!(
            require_policy(admitted, true, false, ProviderRuntimeRequirement::Inline),
            Ok(()),
        );
        assert_eq!(
            require_policy(admitted, false, false, ProviderRuntimeRequirement::Inline),
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
        );
    }

    /// Symmetric regression coverage for the same defect shape found in the
    /// `Acp` arm: it used to admit every provider unconditionally on the
    /// belief that `gate4agent-kernel`'s own `Register`-time check already
    /// covered it, which does not hold for this node's own RPC surface (see
    /// the comment on the `Acp` match arm). The policy argument carries
    /// nothing relevant to ACP either, so the whole decision must come from
    /// `acp_transport`, independent of `pipe_transport`.
    #[test]
    fn acp_requirement_is_admitted_only_when_acp_transport_is_declared() {
        let admitted = ProviderRuntimePolicy::raw_pty();

        assert_eq!(
            require_policy(admitted, false, true, ProviderRuntimeRequirement::Acp),
            Ok(()),
        );
        assert_eq!(
            require_policy(admitted, false, false, ProviderRuntimeRequirement::Acp),
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
        );
    }

    /// Claude declares a Pipe transport in the catalog
    /// (`transports.pipe.is_some()`); a provider observed only through a PTY
    /// sidecar does not (see `gate4agent_catalog::builtin::capabilities`).
    /// `ProviderRuntimeMonitor::supports_pipe_transport` must derive exactly
    /// that catalog fact, and `require_policy` must honour it end to end
    /// through the real monitor and the real built-in registry, not just the
    /// bare policy check above.
    ///
    /// No fleet provider declares a PTY sidecar today: the "Pipe" half is a
    /// synthetic fixture reusing `codex`'s own real, globally-registered
    /// Pipe binding as a PTY sidecar instead, standing in for a live
    /// registry lookup.
    #[test]
    fn monitor_admits_inline_for_a_provider_with_declared_pipe_and_rejects_one_without() {
        let launcher = std::env::temp_dir().join(format!(
            "gate4agent-inline-pipe-runtime-monitor-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX,
        ));
        std::fs::write(&launcher, b"fixture launcher identity").unwrap();
        let mut claude = builtin_registry().get_by_id("claude").unwrap().clone();
        assert!(claude.capabilities.transports.pipe.is_some());
        claude.launch.program = launcher.to_string_lossy().into_owned();
        let mut sidecar_fixture = builtin_registry().get_by_id("codex").unwrap().clone();
        sidecar_fixture.id = AgentId::new("pty-sidecar-fixture").unwrap();
        sidecar_fixture.detection.command = "pty-sidecar-fixture".to_owned();
        let sidecar = sidecar_fixture.capabilities.transports.pipe.clone().unwrap().adapter;
        sidecar_fixture.capabilities.transports.pipe = None;
        sidecar_fixture.capabilities.adapters.pty_sidecar = Some(sidecar);
        assert!(sidecar_fixture.capabilities.transports.pipe.is_none());
        sidecar_fixture.launch.program = launcher.to_string_lossy().into_owned();
        let catalog = AgentRegistry::new([claude, sidecar_fixture]).unwrap();
        let monitor = ProviderRuntimeMonitor::new(&catalog);
        let claude_id = AgentId::new("claude").unwrap();
        let sidecar_id = AgentId::new("pty-sidecar-fixture").unwrap();

        assert!(monitor.supports_pipe_transport(&claude_id));
        assert!(!monitor.supports_pipe_transport(&sidecar_id));

        let (_, claude_admission) = monitor.evaluate(&claude_id);
        assert_eq!(
            require_policy(
                claude_admission.unwrap(),
                monitor.supports_pipe_transport(&claude_id),
                false,
                ProviderRuntimeRequirement::Inline,
            ),
            Ok(()),
        );

        let (_, sidecar_admission) = monitor.evaluate(&sidecar_id);
        assert_eq!(
            require_policy(
                sidecar_admission.unwrap(),
                monitor.supports_pipe_transport(&sidecar_id),
                false,
                ProviderRuntimeRequirement::Inline,
            ),
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
        );

        std::fs::remove_file(&launcher).unwrap();
    }

    /// Grok declares an ACP transport in the catalog
    /// (`transports.acp.is_some()`); every fleet member now does (Claude,
    /// Codex, and Kimi Code all gained one too), so the "without" half needs
    /// a fixture that carries every other transport declaration a real
    /// provider would but omits ACP -- built from `claude` with `acp`
    /// cleared, rather than a second live fleet example, since none exists
    /// anymore. `ProviderRuntimeMonitor::supports_acp_transport` must derive
    /// exactly that catalog fact, and `require_policy` must honour it end to
    /// end through the real monitor and the real built-in registry, not just
    /// the bare policy check above -- the same shape of coverage the Inline
    /// fix above already has, for the arm this coordinator wrote.
    #[test]
    fn monitor_admits_acp_for_a_provider_with_declared_acp_and_rejects_one_without() {
        let launcher = std::env::temp_dir().join(format!(
            "gate4agent-acp-transport-runtime-monitor-{}{}",
            std::process::id(),
            std::env::consts::EXE_SUFFIX,
        ));
        std::fs::write(&launcher, b"fixture launcher identity").unwrap();
        let mut grok = builtin_registry().get_by_id("grok").unwrap().clone();
        assert!(grok.capabilities.transports.acp.is_some());
        grok.launch.program = launcher.to_string_lossy().into_owned();
        let mut claude = builtin_registry().get_by_id("claude").unwrap().clone();
        claude.capabilities.transports.acp = None;
        claude.id = AgentId::new("no-acp-fixture").unwrap();
        claude.detection.command = "no-acp-fixture".to_owned();
        assert!(claude.capabilities.transports.acp.is_none());
        claude.launch.program = launcher.to_string_lossy().into_owned();
        let catalog = AgentRegistry::new([grok, claude]).unwrap();
        let monitor = ProviderRuntimeMonitor::new(&catalog);
        let grok_id = AgentId::new("grok").unwrap();
        let claude_id = AgentId::new("no-acp-fixture").unwrap();

        assert!(monitor.supports_acp_transport(&grok_id));
        assert!(!monitor.supports_acp_transport(&claude_id));

        let (_, grok_admission) = monitor.evaluate(&grok_id);
        assert_eq!(
            require_policy(
                grok_admission.unwrap(),
                false,
                monitor.supports_acp_transport(&grok_id),
                ProviderRuntimeRequirement::Acp,
            ),
            Ok(()),
        );

        let (_, claude_admission) = monitor.evaluate(&claude_id);
        assert_eq!(
            require_policy(
                claude_admission.unwrap(),
                false,
                monitor.supports_acp_transport(&claude_id),
                ProviderRuntimeRequirement::Acp,
            ),
            Err(ProviderRuntimeAdmissionError::SemanticCapabilityUnverified),
        );

        std::fs::remove_file(&launcher).unwrap();
    }
}

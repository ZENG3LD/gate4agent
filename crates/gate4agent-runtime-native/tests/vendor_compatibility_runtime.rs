use gate4agent_runtime_native::{
    probe_installed_vendor_version, resolve_vendor_contract,
    VendorCapabilityUnavailableReason, VendorCapabilityVerdict, VendorFallbackReason,
    VendorPlatform, VendorRuntimeMode, VendorVersionProbeCache, VendorVersionProbeResult,
    VendorVersionProbeStatus, VendorVersionStatus,
    CLAUDE_WINDOWS_X86_64_2_1_223_CONTRACT_ID,
    CLAUDE_WINDOWS_X86_64_2_1_224_CONTRACT_ID,
};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const PROBE_PARENT_SENTINEL: &str = "GATE4AGENT_VENDOR_PROBE_PARENT_SENTINEL";
static PROBE_ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());
static PROBE_FIXTURE_NONCE: AtomicU64 = AtomicU64::new(1);

struct EnvironmentGuard {
    key: &'static str,
    previous: Option<OsString>,
}

impl EnvironmentGuard {
    fn set(key: &'static str, value: &'static str) -> Self {
        let previous = std::env::var_os(key);
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvironmentGuard {
    fn drop(&mut self) {
        if let Some(previous) = self.previous.take() {
            std::env::set_var(self.key, previous);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

struct FakeLauncher {
    root: PathBuf,
    path: PathBuf,
}

impl Drop for FakeLauncher {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn compiled_probe_fixture() -> &'static Path {
    static FIXTURE: OnceLock<PathBuf> = OnceLock::new();
    FIXTURE.get_or_init(|| {
        let root = std::env::temp_dir().join(format!(
            "gate4agent-version-probe-compiler-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create version probe compiler directory");
        let source = root.join("probe_fixture.rs");
        let output = root.join(format!("probe-fixture{}", std::env::consts::EXE_SUFFIX));
        fs::write(
            &source,
            r#"
use std::env;
use std::io::{self, Write};
use std::thread;
use std::time::Duration;

fn main() {
    let arguments = env::args_os().skip(1).collect::<Vec<_>>();
    if arguments.as_slice() != ["--version"] {
        std::process::exit(90);
    }
    let forbidden = [
        "GATE4AGENT_VENDOR_PROBE_PARENT_SENTINEL",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_API_KEY",
        "OPENAI_API_KEY",
        "MOONSHOT_API_KEY",
        "SSH_AUTH_SOCK",
        "SSH_AGENT_PID",
    ];
    if forbidden.iter().any(|key| env::var_os(key).is_some()) {
        std::process::exit(91);
    }
    let executable = env::current_exe().expect("fixture executable path");
    let name = executable
        .file_stem()
        .and_then(|value| value.to_str())
        .expect("fixture executable name");
    if name.contains("success-224") {
        println!("Claude Code 2.1.224");
    } else if name.contains("success") {
        println!("Claude Code 2.1.223");
    } else if name.contains("ambiguous") {
        println!("Claude Code 2.1.223 runtime 20.1.0");
    } else if name.contains("missing") {
    } else if name.contains("unparseable") {
        println!("version unavailable");
    } else if name.contains("overflow") {
        io::stdout().write_all(&vec![b'x'; 20 * 1024]).unwrap();
        println!(" 2.1.223");
    } else if name.contains("nonzero") {
        std::process::exit(7);
    } else if name.contains("timeout") {
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    } else {
        std::process::exit(92);
    }
}
"#,
        )
        .expect("write deterministic version probe fixture source");
        let compiler = std::env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc"));
        let status = Command::new(compiler)
            .arg(&source)
            .arg("-o")
            .arg(&output)
            .status()
            .expect("launch rustc for deterministic version probe fixture");
        assert!(status.success(), "compile deterministic version probe fixture");
        output
    })
}

fn fake_launcher(name: &str) -> FakeLauncher {
    let nonce = PROBE_FIXTURE_NONCE.fetch_add(1, Ordering::AcqRel);
    let root = std::env::temp_dir().join(format!(
        "gate4agent-version-probe-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&root).expect("create isolated version probe fixture directory");
    let path = root.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
    fs::copy(compiled_probe_fixture(), &path).expect("copy deterministic version probe fixture");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("make deterministic version probe fixture executable");
    }
    FakeLauncher { root, path }
}

fn assert_probe_unavailable_but_raw(
    result: &VendorVersionProbeResult,
    expected_status: VendorVersionProbeStatus,
) {
    assert_eq!(result.status(), expected_status);
    assert_eq!(result.resolution().mode(), VendorRuntimeMode::RawPassthrough);
    assert!(result.resolution().admits_raw_pty_lifecycle());
    assert!(!result
        .resolution()
        .capabilities()
        .semantic_resume
        .is_admitted());
}

fn assert_raw_lifecycle_is_admitted(
    resolution: &gate4agent_runtime_native::VendorContractResolution,
) {
    let capabilities = resolution.capabilities();
    assert!(resolution.admits_raw_pty_lifecycle());
    assert_eq!(
        [
            capabilities.raw_pty_spawn,
            capabilities.terminal_screen,
            capabilities.terminal_resize,
            capabilities.terminal_interrupt,
            capabilities.terminal_stop,
        ],
        [VendorCapabilityVerdict::RawPassthrough; 5]
    );
}

#[test]
fn unknown_future_version_degrades_semantics_without_blocking_raw_pty() {
    let resolution = resolve_vendor_contract(
        "claude",
        VendorPlatform::WindowsX86_64,
        b"Claude Code 9.0.0\n",
        b"",
    );

    assert_eq!(resolution.mode(), VendorRuntimeMode::RawPassthrough);
    assert_eq!(resolution.version_status(), VendorVersionStatus::Normalized);
    assert_eq!(resolution.normalized_version(), Some("9.0.0"));
    assert_eq!(
        resolution.fallback_reason(),
        Some(VendorFallbackReason::FutureVersion)
    );
    assert_eq!(resolution.contract_id(), None);
    assert_raw_lifecycle_is_admitted(&resolution);
    assert_eq!(
        resolution
            .capabilities()
            .semantic_resume
            .unavailable_reason(),
        Some(VendorCapabilityUnavailableReason::NoVerifiedContract(
            VendorFallbackReason::FutureVersion
        ))
    );
}

#[test]
fn legacy_version_degrades_semantics_without_blocking_raw_pty() {
    let resolution = resolve_vendor_contract(
        "claude",
        VendorPlatform::WindowsX86_64,
        b"claude 1.0.0",
        b"",
    );

    assert_eq!(resolution.mode().label(), "raw_passthrough");
    assert_eq!(resolution.normalized_version(), Some("1.0.0"));
    assert_eq!(
        resolution.fallback_reason(),
        Some(VendorFallbackReason::LegacyVersion)
    );
    assert_raw_lifecycle_is_admitted(&resolution);
    assert!(!resolution.capabilities().semantic_resume.is_admitted());
    assert!(!resolution
        .capabilities()
        .provider_session_identity
        .is_admitted());
}

#[test]
fn ambiguous_and_unparseable_outputs_fall_back_to_raw_pty() {
    let ambiguous = resolve_vendor_contract(
        "claude",
        VendorPlatform::WindowsX86_64,
        b"Claude Code 2.1.223; embedded runtime 20.1.0",
        b"",
    );
    assert_eq!(ambiguous.version_status(), VendorVersionStatus::Ambiguous);
    assert_eq!(ambiguous.normalized_version(), None);
    assert_eq!(
        ambiguous.fallback_reason(),
        Some(VendorFallbackReason::AmbiguousVersion)
    );
    assert_raw_lifecycle_is_admitted(&ambiguous);
    assert!(!ambiguous.capabilities().structured_prompt.is_admitted());

    let unparseable = resolve_vendor_contract(
        "claude",
        VendorPlatform::WindowsX86_64,
        b"Claude Code 2.1.223-beta",
        b"",
    );
    assert_eq!(
        unparseable.version_status(),
        VendorVersionStatus::Unparseable
    );
    assert_eq!(
        unparseable.fallback_reason(),
        Some(VendorFallbackReason::UnparseableVersion)
    );
    assert_raw_lifecycle_is_admitted(&unparseable);
    assert!(!unparseable.capabilities().semantic_readiness.is_admitted());
}

#[test]
fn claude_exact_profiles_select_distinct_verified_readiness_contracts() {
    let bracketed_paste = resolve_vendor_contract(
        "claude",
        VendorPlatform::WindowsX86_64,
        b"Claude Code v2.1.223",
        b"",
    );
    let win32_input = resolve_vendor_contract(
        "claude",
        VendorPlatform::WindowsX86_64,
        b"2.1.224 (Claude Code)",
        b"",
    );

    assert_eq!(
        bracketed_paste.mode(),
        VendorRuntimeMode::VerifiedSemantic
    );
    assert_eq!(win32_input.mode(), VendorRuntimeMode::VerifiedSemantic);
    assert_eq!(
        bracketed_paste.contract_id(),
        Some(CLAUDE_WINDOWS_X86_64_2_1_223_CONTRACT_ID)
    );
    assert_eq!(
        win32_input.contract_id(),
        Some(CLAUDE_WINDOWS_X86_64_2_1_224_CONTRACT_ID)
    );
    assert_ne!(
        bracketed_paste.capabilities().semantic_readiness,
        win32_input.capabilities().semantic_readiness
    );
    assert_eq!(
        bracketed_paste.capabilities().semantic_readiness,
        VendorCapabilityVerdict::Verified {
            capability_contract: "readiness.bracketed_paste"
        }
    );
    assert_eq!(
        win32_input.capabilities().semantic_readiness,
        VendorCapabilityVerdict::Verified {
            capability_contract: "readiness.win32_input_focus_cursor"
        }
    );
    assert!(bracketed_paste
        .capabilities()
        .structured_prompt
        .is_verified());
    assert!(win32_input.capabilities().structured_prompt.is_verified());
    assert_raw_lifecycle_is_admitted(&bracketed_paste);
    assert_raw_lifecycle_is_admitted(&win32_input);
}

#[test]
fn exact_profile_refuses_semantic_resume_when_resume_was_not_verified() {
    let resolution = resolve_vendor_contract(
        "claude",
        VendorPlatform::WindowsX86_64,
        b"claude 2.1.224",
        b"claude 2.1.224",
    );

    assert_eq!(resolution.mode(), VendorRuntimeMode::VerifiedSemantic);
    assert_raw_lifecycle_is_admitted(&resolution);
    assert_eq!(
        resolution
            .capabilities()
            .semantic_resume
            .unavailable_reason(),
        Some(VendorCapabilityUnavailableReason::NotVerifiedByContract)
    );
    assert_eq!(
        resolution
            .capabilities()
            .provider_session_identity
            .unavailable_reason(),
        Some(VendorCapabilityUnavailableReason::NotVerifiedByContract)
    );
}

#[test]
fn exact_version_on_unverified_platform_keeps_only_raw_passthrough() {
    let resolution = resolve_vendor_contract(
        "claude",
        VendorPlatform::MacosAarch64,
        b"claude 2.1.224",
        b"",
    );

    assert_eq!(resolution.normalized_version(), Some("2.1.224"));
    assert_eq!(resolution.mode(), VendorRuntimeMode::RawPassthrough);
    assert_eq!(
        resolution.fallback_reason(),
        Some(VendorFallbackReason::UnsupportedPlatform)
    );
    assert_raw_lifecycle_is_admitted(&resolution);
    assert!(!resolution.capabilities().semantic_resume.is_admitted());
}

#[test]
fn installed_version_probe_uses_exact_argv_and_drops_parent_credentials() {
    let _environment_lock = PROBE_ENVIRONMENT_LOCK.lock().unwrap();
    let _sentinel = EnvironmentGuard::set(PROBE_PARENT_SENTINEL, "must-not-reach-child");
    let launcher = fake_launcher("probe-success-223");
    let mut cache = VendorVersionProbeCache::default();

    let first = probe_installed_vendor_version(
        &mut cache,
        "claude",
        &launcher.path,
        Instant::now() + Duration::from_secs(5),
    );

    assert_eq!(first.status(), VendorVersionProbeStatus::Resolved);
    assert_eq!(first.resolution().normalized_version(), Some("2.1.223"));
    assert!(first.resolution().admits_raw_pty_lifecycle());
    assert!(first.launcher_identity().is_some());
    assert!(!first.was_cached());
    assert_eq!(cache.len(), 1);

    let cached = cache.probe(
        "claude",
        &launcher.path,
        Instant::now() + Duration::from_secs(5),
    );
    assert_eq!(cached.status(), VendorVersionProbeStatus::Resolved);
    assert_eq!(cached.resolution(), first.resolution());
    assert_eq!(cached.launcher_identity(), first.launcher_identity());
    assert!(cached.was_cached());
}

#[test]
fn installed_version_probe_cache_invalidates_on_launcher_content_change() {
    let launcher = fake_launcher("probe-success-224");
    let mut cache = VendorVersionProbeCache::default();
    let first = cache.probe(
        "claude",
        &launcher.path,
        Instant::now() + Duration::from_secs(5),
    );
    let cached = cache.probe(
        "claude",
        &launcher.path,
        Instant::now() + Duration::from_secs(5),
    );
    assert!(cached.was_cached());

    OpenOptions::new()
        .append(true)
        .open(&launcher.path)
        .expect("open version probe fixture for identity mutation")
        .write_all(&[0])
        .expect("mutate version probe fixture content identity");
    let after_change = cache.probe(
        "claude",
        &launcher.path,
        Instant::now() + Duration::from_secs(5),
    );

    assert_eq!(after_change.status(), VendorVersionProbeStatus::Resolved);
    assert_eq!(after_change.resolution().normalized_version(), Some("2.1.224"));
    assert!(!after_change.was_cached());
    assert_ne!(first.launcher_identity(), after_change.launcher_identity());
    assert_eq!(cache.len(), 1);
}

#[test]
fn installed_version_probe_failures_never_block_raw_pty() {
    let mut cache = VendorVersionProbeCache::default();
    let unsupported = cache.probe(
        "unlisted-provider",
        Path::new("launcher-does-not-need-to-exist"),
        Instant::now() + Duration::from_secs(1),
    );
    assert_probe_unavailable_but_raw(
        &unsupported,
        VendorVersionProbeStatus::UnsupportedVendor,
    );

    let missing_path = std::env::temp_dir().join(format!(
        "gate4agent-version-probe-missing-{}",
        std::process::id()
    ));
    let missing_launcher = cache.probe(
        "claude",
        &missing_path,
        Instant::now() + Duration::from_secs(1),
    );
    assert_probe_unavailable_but_raw(
        &missing_launcher,
        VendorVersionProbeStatus::LauncherUnavailable,
    );

    let ambiguous_launcher = fake_launcher("probe-ambiguous");
    let ambiguous = cache.probe(
        "claude",
        &ambiguous_launcher.path,
        Instant::now() + Duration::from_secs(5),
    );
    assert_probe_unavailable_but_raw(&ambiguous, VendorVersionProbeStatus::AmbiguousVersion);

    let missing_version_launcher = fake_launcher("probe-missing");
    let missing_version = cache.probe(
        "claude",
        &missing_version_launcher.path,
        Instant::now() + Duration::from_secs(5),
    );
    assert_probe_unavailable_but_raw(
        &missing_version,
        VendorVersionProbeStatus::MissingVersion,
    );

    let unparseable_launcher = fake_launcher("probe-unparseable");
    let unparseable = cache.probe(
        "claude",
        &unparseable_launcher.path,
        Instant::now() + Duration::from_secs(5),
    );
    assert_probe_unavailable_but_raw(
        &unparseable,
        VendorVersionProbeStatus::UnparseableVersion,
    );

    let nonzero_launcher = fake_launcher("probe-nonzero");
    let nonzero = cache.probe(
        "claude",
        &nonzero_launcher.path,
        Instant::now() + Duration::from_secs(5),
    );
    assert_probe_unavailable_but_raw(&nonzero, VendorVersionProbeStatus::NonzeroExit);

    let overflow_launcher = fake_launcher("probe-overflow");
    let overflow = cache.probe(
        "claude",
        &overflow_launcher.path,
        Instant::now() + Duration::from_secs(5),
    );
    assert_probe_unavailable_but_raw(&overflow, VendorVersionProbeStatus::OutputOverflow);
}

#[test]
fn installed_version_probe_obeys_absolute_deadline() {
    let launcher = fake_launcher("probe-timeout");
    let mut cache = VendorVersionProbeCache::default();
    let started = Instant::now();
    let result = cache.probe(
        "claude",
        &launcher.path,
        started + Duration::from_millis(300),
    );

    assert_probe_unavailable_but_raw(&result, VendorVersionProbeStatus::DeadlineExceeded);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(cache.is_empty());
}

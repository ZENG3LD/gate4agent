use std::collections::{BTreeSet, VecDeque};
use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, UNIX_EPOCH};

const MAX_VERSION_OUTPUT_BYTES: usize = 16 * 1024;
const MAX_LAUNCHER_BYTES: u64 = 512 * 1024 * 1024;
const VERSION_PROBE_CACHE_CAPACITY: usize = 16;
const VERSION_PROBE_POLL_INTERVAL: Duration = Duration::from_millis(5);
const VERSION_PROBE_MAX_CLEANUP_RESERVE: Duration = Duration::from_millis(100);
const CLAUDE_VERSION_ARGV: &[&str] = &["--version"];
const CODEX_VERSION_ARGV: &[&str] = &["--version"];
const KIMI_VERSION_ARGV: &[&str] = &["--version"];

pub const CLAUDE_WINDOWS_X86_64_2_1_223_CONTRACT_ID: &str =
    "claude.windows-x86_64.2.1.223";
pub const CLAUDE_WINDOWS_X86_64_2_1_224_CONTRACT_ID: &str =
    "claude.windows-x86_64.2.1.224";

const RAW_PASSTHROUGH: VendorCapabilityVerdict = VendorCapabilityVerdict::RawPassthrough;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorCliFamily {
    Claude,
    Codex,
    Kimi,
    Unknown,
}

impl VendorCliFamily {
    pub fn from_agent_id(agent_id: &str) -> Self {
        if agent_id.eq_ignore_ascii_case("claude") {
            Self::Claude
        } else if agent_id.eq_ignore_ascii_case("codex") {
            Self::Codex
        } else if agent_id.eq_ignore_ascii_case("kimi") {
            Self::Kimi
        } else {
            Self::Unknown
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
            Self::Kimi => "kimi",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorPlatform {
    WindowsX86_64,
    WindowsAarch64,
    LinuxX86_64,
    LinuxAarch64,
    MacosX86_64,
    MacosAarch64,
    Other,
}

impl VendorPlatform {
    pub fn current() -> Self {
        match (std::env::consts::OS, std::env::consts::ARCH) {
            ("windows", "x86_64") => Self::WindowsX86_64,
            ("windows", "aarch64") => Self::WindowsAarch64,
            ("linux", "x86_64") => Self::LinuxX86_64,
            ("linux", "aarch64") => Self::LinuxAarch64,
            ("macos", "x86_64") => Self::MacosX86_64,
            ("macos", "aarch64") => Self::MacosAarch64,
            _ => Self::Other,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::WindowsX86_64 => "windows-x86_64",
            Self::WindowsAarch64 => "windows-aarch64",
            Self::LinuxX86_64 => "linux-x86_64",
            Self::LinuxAarch64 => "linux-aarch64",
            Self::MacosX86_64 => "macos-x86_64",
            Self::MacosAarch64 => "macos-aarch64",
            Self::Other => "other",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorRuntimeMode {
    RawPassthrough,
    VerifiedSemantic,
}

impl VendorRuntimeMode {
    pub const fn label(self) -> &'static str {
        match self {
            Self::RawPassthrough => "raw_passthrough",
            Self::VerifiedSemantic => "verified_semantic",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorVersionStatus {
    Normalized,
    Missing,
    Ambiguous,
    Unparseable,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorFallbackReason {
    UnsupportedVendor,
    NoVerifiedProfile,
    UnsupportedPlatform,
    LegacyVersion,
    FutureVersion,
    UnlistedVersion,
    MissingVersion,
    AmbiguousVersion,
    UnparseableVersion,
    InvalidLauncher,
    LauncherUnavailable,
    LauncherChanged,
    ProbeDeadlineExceeded,
    ProbeSpawnFailed,
    ProbeNonzero,
    ProbeOutputOverflow,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorCapabilityUnavailableReason {
    NoVerifiedContract(VendorFallbackReason),
    NotVerifiedByContract,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorCapabilityVerdict {
    /// The operation is admitted through the version-independent PTY path.
    RawPassthrough,
    /// The version-specific semantic behavior has an exact verified contract.
    Verified { capability_contract: &'static str },
    /// The operation must not use semantic parsing or structured control.
    Unavailable {
        reason: VendorCapabilityUnavailableReason,
    },
}

impl VendorCapabilityVerdict {
    pub const fn is_admitted(self) -> bool {
        !matches!(self, Self::Unavailable { .. })
    }

    pub const fn is_verified(self) -> bool {
        matches!(self, Self::Verified { .. })
    }

    pub const fn unavailable_reason(self) -> Option<VendorCapabilityUnavailableReason> {
        match self {
            Self::Unavailable { reason } => Some(reason),
            Self::RawPassthrough | Self::Verified { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VendorCapabilitySet {
    pub raw_pty_spawn: VendorCapabilityVerdict,
    pub terminal_screen: VendorCapabilityVerdict,
    pub terminal_resize: VendorCapabilityVerdict,
    pub terminal_interrupt: VendorCapabilityVerdict,
    pub terminal_stop: VendorCapabilityVerdict,
    pub semantic_readiness: VendorCapabilityVerdict,
    pub provider_session_identity: VendorCapabilityVerdict,
    pub structured_prompt: VendorCapabilityVerdict,
    pub semantic_resume: VendorCapabilityVerdict,
}

impl VendorCapabilitySet {
    pub const fn admits_raw_pty_lifecycle(self) -> bool {
        self.raw_pty_spawn.is_admitted()
            && self.terminal_screen.is_admitted()
            && self.terminal_resize.is_admitted()
            && self.terminal_interrupt.is_admitted()
            && self.terminal_stop.is_admitted()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VendorContractResolution {
    vendor: VendorCliFamily,
    platform: VendorPlatform,
    mode: VendorRuntimeMode,
    version_status: VendorVersionStatus,
    normalized_version: Option<String>,
    contract_id: Option<&'static str>,
    fallback_reason: Option<VendorFallbackReason>,
    capabilities: VendorCapabilitySet,
}

impl VendorContractResolution {
    pub const fn vendor(&self) -> VendorCliFamily {
        self.vendor
    }

    pub const fn platform(&self) -> VendorPlatform {
        self.platform
    }

    pub const fn mode(&self) -> VendorRuntimeMode {
        self.mode
    }

    pub const fn version_status(&self) -> VendorVersionStatus {
        self.version_status
    }

    pub fn normalized_version(&self) -> Option<&str> {
        self.normalized_version.as_deref()
    }

    pub const fn contract_id(&self) -> Option<&'static str> {
        self.contract_id
    }

    pub const fn fallback_reason(&self) -> Option<VendorFallbackReason> {
        self.fallback_reason
    }

    pub const fn capabilities(&self) -> VendorCapabilitySet {
        self.capabilities
    }

    pub const fn admits_raw_pty_lifecycle(&self) -> bool {
        self.capabilities.admits_raw_pty_lifecycle()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct VendorLauncherIdentity {
    path_fingerprint: u64,
    bytes: u64,
    modified_unix_nanos: Option<u128>,
    content_fingerprint: u64,
}

impl VendorLauncherIdentity {
    pub const fn path_fingerprint(self) -> u64 {
        self.path_fingerprint
    }

    pub const fn bytes(self) -> u64 {
        self.bytes
    }

    pub const fn modified_unix_nanos(self) -> Option<u128> {
        self.modified_unix_nanos
    }

    pub const fn content_fingerprint(self) -> u64 {
        self.content_fingerprint
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VendorVersionProbeStatus {
    Resolved,
    UnsupportedVendor,
    InvalidLauncher,
    LauncherUnavailable,
    LauncherChanged,
    DeadlineExceeded,
    SpawnFailed,
    NonzeroExit,
    OutputOverflow,
    MissingVersion,
    AmbiguousVersion,
    UnparseableVersion,
}

impl VendorVersionProbeStatus {
    pub const fn is_resolved(self) -> bool {
        matches!(self, Self::Resolved)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VendorVersionProbeResult {
    status: VendorVersionProbeStatus,
    resolution: VendorContractResolution,
    launcher_identity: Option<VendorLauncherIdentity>,
    cache_hit: bool,
}

impl VendorVersionProbeResult {
    pub const fn status(&self) -> VendorVersionProbeStatus {
        self.status
    }

    pub const fn resolution(&self) -> &VendorContractResolution {
        &self.resolution
    }

    pub const fn launcher_identity(&self) -> Option<VendorLauncherIdentity> {
        self.launcher_identity
    }

    pub const fn was_cached(&self) -> bool {
        self.cache_hit
    }
}

struct CachedVersionProbe {
    vendor: VendorCliFamily,
    platform: VendorPlatform,
    launcher_identity: VendorLauncherIdentity,
    status: VendorVersionProbeStatus,
    resolution: VendorContractResolution,
}

/// Bounded host-local cache keyed by provider, platform, and exact launcher
/// identity. A metadata or content identity change evicts the prior entry for
/// the same canonical path fingerprint before the new result is inserted.
pub struct VendorVersionProbeCache {
    entries: VecDeque<CachedVersionProbe>,
}

impl Default for VendorVersionProbeCache {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(VERSION_PROBE_CACHE_CAPACITY),
        }
    }
}

impl VendorVersionProbeCache {
    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn probe(
        &mut self,
        agent_id: &str,
        exact_launcher: &Path,
        deadline: Instant,
    ) -> VendorVersionProbeResult {
        probe_installed_vendor_version(self, agent_id, exact_launcher, deadline)
    }

    fn lookup(
        &mut self,
        vendor: VendorCliFamily,
        platform: VendorPlatform,
        launcher_identity: VendorLauncherIdentity,
    ) -> Option<VendorVersionProbeResult> {
        let position = self.entries.iter().position(|entry| {
            entry.vendor == vendor
                && entry.platform == platform
                && entry.launcher_identity == launcher_identity
        })?;
        let entry = self
            .entries
            .remove(position)
            .expect("located vendor version cache entry must remain present");
        let result = VendorVersionProbeResult {
            status: entry.status,
            resolution: entry.resolution.clone(),
            launcher_identity: Some(entry.launcher_identity),
            cache_hit: true,
        };
        self.entries.push_back(entry);
        Some(result)
    }

    fn insert(
        &mut self,
        vendor: VendorCliFamily,
        platform: VendorPlatform,
        launcher_identity: VendorLauncherIdentity,
        status: VendorVersionProbeStatus,
        resolution: VendorContractResolution,
    ) {
        self.entries.retain(|entry| {
            entry.vendor != vendor
                || entry.platform != platform
                || entry.launcher_identity.path_fingerprint
                    != launcher_identity.path_fingerprint
        });
        if self.entries.len() == VERSION_PROBE_CACHE_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back(CachedVersionProbe {
            vendor,
            platform,
            launcher_identity,
            status,
            resolution,
        });
    }
}

#[derive(Clone, Copy)]
struct VerifiedProfile {
    contract_id: &'static str,
    vendor: VendorCliFamily,
    platform: VendorPlatform,
    version: VersionTriple,
    semantic_readiness_contract: Option<&'static str>,
    provider_session_identity_contract: Option<&'static str>,
    structured_prompt_contract: Option<&'static str>,
    semantic_resume_contract: Option<&'static str>,
}

const VERIFIED_PROFILES: &[VerifiedProfile] = &[
    VerifiedProfile {
        contract_id: CLAUDE_WINDOWS_X86_64_2_1_223_CONTRACT_ID,
        vendor: VendorCliFamily::Claude,
        platform: VendorPlatform::WindowsX86_64,
        version: VersionTriple::new(2, 1, 223),
        semantic_readiness_contract: Some("readiness.bracketed_paste"),
        provider_session_identity_contract: None,
        structured_prompt_contract: Some("pty.prompt_round_trip"),
        semantic_resume_contract: None,
    },
    VerifiedProfile {
        contract_id: CLAUDE_WINDOWS_X86_64_2_1_224_CONTRACT_ID,
        vendor: VendorCliFamily::Claude,
        platform: VendorPlatform::WindowsX86_64,
        version: VersionTriple::new(2, 1, 224),
        semantic_readiness_contract: Some("readiness.win32_input_focus_cursor"),
        provider_session_identity_contract: None,
        structured_prompt_contract: Some("pty.prompt_round_trip"),
        semantic_resume_contract: None,
    },
];

struct ResolvedLauncher {
    path: PathBuf,
    identity: VendorLauncherIdentity,
}

#[derive(Clone, Copy)]
enum LauncherResolveError {
    Invalid,
    Unavailable,
    DeadlineExceeded,
}

struct CapturedProbeOutput {
    bytes: Vec<u8>,
    overflowed: bool,
}

enum VersionChildOutcome {
    Completed {
        success: bool,
        stdout: CapturedProbeOutput,
        stderr: CapturedProbeOutput,
    },
    DeadlineExceeded,
    SpawnFailed,
}

/// Probes one exact, absolute launcher without inheriting the parent process
/// environment. The returned value contains no child output, environment
/// values, launcher path, or command line. Windows accepts an exact `.exe`
/// only; command-script wrappers degrade to raw passthrough instead of adding
/// a shell interpreter to this probe path. The deadline covers launcher
/// fingerprinting, child execution, capture, and the post-execution identity
/// check.
pub fn probe_installed_vendor_version(
    cache: &mut VendorVersionProbeCache,
    agent_id: &str,
    exact_launcher: &Path,
    deadline: Instant,
) -> VendorVersionProbeResult {
    let vendor = VendorCliFamily::from_agent_id(agent_id);
    let platform = VendorPlatform::current();
    if vendor == VendorCliFamily::Unknown {
        return unavailable_probe_result(
            vendor,
            platform,
            VendorVersionProbeStatus::UnsupportedVendor,
            VendorFallbackReason::UnsupportedVendor,
            None,
        );
    }
    if Instant::now() >= deadline {
        return unavailable_probe_result(
            vendor,
            platform,
            VendorVersionProbeStatus::DeadlineExceeded,
            VendorFallbackReason::ProbeDeadlineExceeded,
            None,
        );
    }
    let launcher = match resolve_exact_launcher(exact_launcher, deadline) {
        Ok(launcher) => launcher,
        Err(LauncherResolveError::Invalid) => {
            return unavailable_probe_result(
                vendor,
                platform,
                VendorVersionProbeStatus::InvalidLauncher,
                VendorFallbackReason::InvalidLauncher,
                None,
            );
        }
        Err(LauncherResolveError::Unavailable) => {
            return unavailable_probe_result(
                vendor,
                platform,
                VendorVersionProbeStatus::LauncherUnavailable,
                VendorFallbackReason::LauncherUnavailable,
                None,
            );
        }
        Err(LauncherResolveError::DeadlineExceeded) => {
            return unavailable_probe_result(
                vendor,
                platform,
                VendorVersionProbeStatus::DeadlineExceeded,
                VendorFallbackReason::ProbeDeadlineExceeded,
                None,
            );
        }
    };
    if let Some(cached) = cache.lookup(vendor, platform, launcher.identity) {
        return cached;
    }
    let Some(command) = provider_version_command(vendor, &launcher.path) else {
        return unavailable_probe_result(
            vendor,
            platform,
            VendorVersionProbeStatus::InvalidLauncher,
            VendorFallbackReason::InvalidLauncher,
            Some(launcher.identity),
        );
    };
    let (stdout, stderr) = match run_version_child(command, deadline) {
        VersionChildOutcome::Completed {
            success: false, ..
        } => {
            return unavailable_probe_result(
                vendor,
                platform,
                VendorVersionProbeStatus::NonzeroExit,
                VendorFallbackReason::ProbeNonzero,
                Some(launcher.identity),
            );
        }
        VersionChildOutcome::Completed {
            success: true,
            stdout,
            stderr,
        } => (stdout, stderr),
        VersionChildOutcome::DeadlineExceeded => {
            return unavailable_probe_result(
                vendor,
                platform,
                VendorVersionProbeStatus::DeadlineExceeded,
                VendorFallbackReason::ProbeDeadlineExceeded,
                Some(launcher.identity),
            );
        }
        VersionChildOutcome::SpawnFailed => {
            return unavailable_probe_result(
                vendor,
                platform,
                VendorVersionProbeStatus::SpawnFailed,
                VendorFallbackReason::ProbeSpawnFailed,
                Some(launcher.identity),
            );
        }
    };
    let combined_output_bytes = stdout
        .bytes
        .len()
        .checked_add(stderr.bytes.len())
        .and_then(|bytes| bytes.checked_add(1));
    if stdout.overflowed
        || stderr.overflowed
        || combined_output_bytes.is_none()
        || combined_output_bytes.is_some_and(|bytes| bytes > MAX_VERSION_OUTPUT_BYTES)
    {
        return unavailable_probe_result(
            vendor,
            platform,
            VendorVersionProbeStatus::OutputOverflow,
            VendorFallbackReason::ProbeOutputOverflow,
            Some(launcher.identity),
        );
    }
    let after = match resolve_exact_launcher(exact_launcher, deadline) {
        Ok(after) => after,
        Err(LauncherResolveError::DeadlineExceeded) => {
            return unavailable_probe_result(
                vendor,
                platform,
                VendorVersionProbeStatus::DeadlineExceeded,
                VendorFallbackReason::ProbeDeadlineExceeded,
                Some(launcher.identity),
            );
        }
        Err(LauncherResolveError::Invalid | LauncherResolveError::Unavailable) => {
            return unavailable_probe_result(
                vendor,
                platform,
                VendorVersionProbeStatus::LauncherChanged,
                VendorFallbackReason::LauncherChanged,
                Some(launcher.identity),
            );
        }
    };
    if after.identity != launcher.identity {
        return unavailable_probe_result(
            vendor,
            platform,
            VendorVersionProbeStatus::LauncherChanged,
            VendorFallbackReason::LauncherChanged,
            Some(after.identity),
        );
    }
    let resolution = resolve_vendor_contract(agent_id, platform, &stdout.bytes, &stderr.bytes);
    let status = probe_status_for_resolution(&resolution);
    cache.insert(
        vendor,
        platform,
        launcher.identity,
        status,
        resolution.clone(),
    );
    VendorVersionProbeResult {
        status,
        resolution,
        launcher_identity: Some(launcher.identity),
        cache_hit: false,
    }
}

fn unavailable_probe_result(
    vendor: VendorCliFamily,
    platform: VendorPlatform,
    status: VendorVersionProbeStatus,
    fallback_reason: VendorFallbackReason,
    launcher_identity: Option<VendorLauncherIdentity>,
) -> VendorVersionProbeResult {
    VendorVersionProbeResult {
        status,
        resolution: raw_resolution(
            vendor,
            platform,
            VendorVersionStatus::Unavailable,
            None,
            fallback_reason,
        ),
        launcher_identity,
        cache_hit: false,
    }
}

fn probe_status_for_resolution(
    resolution: &VendorContractResolution,
) -> VendorVersionProbeStatus {
    match resolution.version_status() {
        VendorVersionStatus::Normalized => VendorVersionProbeStatus::Resolved,
        VendorVersionStatus::Missing => VendorVersionProbeStatus::MissingVersion,
        VendorVersionStatus::Ambiguous => VendorVersionProbeStatus::AmbiguousVersion,
        VendorVersionStatus::Unparseable => VendorVersionProbeStatus::UnparseableVersion,
        VendorVersionStatus::Unavailable => VendorVersionProbeStatus::LauncherUnavailable,
    }
}

fn resolve_exact_launcher(
    exact_launcher: &Path,
    deadline: Instant,
) -> Result<ResolvedLauncher, LauncherResolveError> {
    if !exact_launcher.is_absolute() || Instant::now() >= deadline {
        return if Instant::now() >= deadline {
            Err(LauncherResolveError::DeadlineExceeded)
        } else {
            Err(LauncherResolveError::Invalid)
        };
    }
    let canonical_path = exact_launcher
        .canonicalize()
        .map_err(|_| LauncherResolveError::Unavailable)?;
    let mut launcher = OpenOptions::new()
        .read(true)
        .open(&canonical_path)
        .map_err(|_| LauncherResolveError::Unavailable)?;
    let before = launcher
        .metadata()
        .map_err(|_| LauncherResolveError::Unavailable)?;
    if !before.is_file() || before.len() > MAX_LAUNCHER_BYTES {
        return Err(LauncherResolveError::Invalid);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if before.permissions().mode() & 0o111 == 0 {
            return Err(LauncherResolveError::Invalid);
        }
    }
    let modified = before.modified().ok();
    let mut content_fingerprint = fnv_offset();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        if Instant::now() >= deadline {
            return Err(LauncherResolveError::DeadlineExceeded);
        }
        let read = launcher
            .read(&mut buffer)
            .map_err(|_| LauncherResolveError::Unavailable)?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or(LauncherResolveError::Invalid)?;
        if bytes > MAX_LAUNCHER_BYTES {
            return Err(LauncherResolveError::Invalid);
        }
        content_fingerprint = fnv_bytes(content_fingerprint, &buffer[..read]);
    }
    let after = launcher
        .metadata()
        .map_err(|_| LauncherResolveError::Unavailable)?;
    if before.len() != after.len()
        || bytes != before.len()
        || modified != after.modified().ok()
    {
        return Err(LauncherResolveError::Unavailable);
    }
    let modified_unix_nanos = modified
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_nanos());
    Ok(ResolvedLauncher {
        identity: VendorLauncherIdentity {
            path_fingerprint: fingerprint_path(&canonical_path),
            bytes,
            modified_unix_nanos,
            content_fingerprint,
        },
        path: canonical_path,
    })
}

fn provider_version_command(vendor: VendorCliFamily, launcher: &Path) -> Option<Command> {
    let argv = match vendor {
        VendorCliFamily::Claude => CLAUDE_VERSION_ARGV,
        VendorCliFamily::Codex => CODEX_VERSION_ARGV,
        VendorCliFamily::Kimi => KIMI_VERSION_ARGV,
        VendorCliFamily::Unknown => return None,
    };
    #[cfg(windows)]
    let mut command = {
        let extension = launcher
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)?;
        if extension != "exe" {
            return None;
        }
        let mut command = Command::new(launcher);
        command.args(argv);
        command.env_clear();
        command
    };
    #[cfg(not(windows))]
    let mut command = {
        let mut command = Command::new(launcher);
        command.args(argv);
        command.env_clear();
        command
    };
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    Some(command)
}

fn run_version_child(mut command: Command, deadline: Instant) -> VersionChildOutcome {
    if Instant::now() >= deadline {
        return VersionChildOutcome::DeadlineExceeded;
    }
    let Ok(mut child) = command.spawn() else {
        return VersionChildOutcome::SpawnFailed;
    };
    let stdout = child.stdout.take().map(spawn_output_reader);
    let stderr = child.stderr.take().map(spawn_output_reader);
    let execution_deadline = execution_deadline(deadline);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < execution_deadline => {
                sleep_until_poll(execution_deadline);
            }
            Ok(None) => {
                terminate_child_before(&mut child, deadline);
                return VersionChildOutcome::DeadlineExceeded;
            }
            Err(_) => {
                terminate_child_before(&mut child, deadline);
                return VersionChildOutcome::SpawnFailed;
            }
        }
    };
    let Some(stdout) = finish_output_reader(stdout, deadline) else {
        return VersionChildOutcome::DeadlineExceeded;
    };
    let Some(stderr) = finish_output_reader(stderr, deadline) else {
        return VersionChildOutcome::DeadlineExceeded;
    };
    VersionChildOutcome::Completed {
        success: status.success(),
        stdout,
        stderr,
    }
}

fn execution_deadline(deadline: Instant) -> Instant {
    let now = Instant::now();
    let remaining = deadline.saturating_duration_since(now);
    let reserve = (remaining / 4).min(VERSION_PROBE_MAX_CLEANUP_RESERVE);
    deadline.checked_sub(reserve).unwrap_or(now)
}

fn sleep_until_poll(deadline: Instant) {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if !remaining.is_zero() {
        thread::sleep(remaining.min(VERSION_PROBE_POLL_INTERVAL));
    }
}

fn terminate_child_before(child: &mut Child, deadline: Instant) {
    let _ = child.kill();
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) if Instant::now() < deadline => sleep_until_poll(deadline),
            Ok(None) => return,
        }
    }
}

fn spawn_output_reader(reader: impl Read + Send + 'static) -> thread::JoinHandle<CapturedProbeOutput> {
    thread::spawn(move || read_capped_output(reader))
}

fn read_capped_output(mut reader: impl Read) -> CapturedProbeOutput {
    let mut bytes = Vec::with_capacity(MAX_VERSION_OUTPUT_BYTES.min(4 * 1024));
    let mut overflowed = false;
    let mut buffer = [0_u8; 4 * 1024];
    loop {
        let Ok(read) = reader.read(&mut buffer) else {
            overflowed = true;
            break;
        };
        if read == 0 {
            break;
        }
        let remaining = MAX_VERSION_OUTPUT_BYTES.saturating_sub(bytes.len());
        let retained = remaining.min(read);
        bytes.extend_from_slice(&buffer[..retained]);
        overflowed |= retained != read;
    }
    CapturedProbeOutput { bytes, overflowed }
}

fn finish_output_reader(
    reader: Option<thread::JoinHandle<CapturedProbeOutput>>,
    deadline: Instant,
) -> Option<CapturedProbeOutput> {
    let Some(reader) = reader else {
        return Some(CapturedProbeOutput {
            bytes: Vec::new(),
            overflowed: false,
        });
    };
    while !reader.is_finished() && Instant::now() < deadline {
        sleep_until_poll(deadline);
    }
    if !reader.is_finished() {
        return None;
    }
    reader.join().ok()
}

const fn fnv_offset() -> u64 {
    0xcbf2_9ce4_8422_2325
}

fn fnv_bytes(mut value: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        value ^= u64::from(*byte);
        value = value.wrapping_mul(0x0000_0100_0000_01b3);
    }
    value
}

#[cfg(unix)]
fn fingerprint_path(path: &Path) -> u64 {
    use std::os::unix::ffi::OsStrExt;
    fnv_bytes(fnv_offset(), path.as_os_str().as_bytes())
}

#[cfg(windows)]
fn fingerprint_path(path: &Path) -> u64 {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .fold(fnv_offset(), |value, word| {
            fnv_bytes(value, &word.to_le_bytes())
        })
}

#[cfg(not(any(unix, windows)))]
fn fingerprint_path(path: &Path) -> u64 {
    fnv_bytes(fnv_offset(), path.to_string_lossy().as_bytes())
}

/// Resolves a pure runtime contract from already captured vendor `--version`
/// output. It never launches a process and never rejects the raw PTY lifecycle.
pub fn resolve_vendor_contract(
    agent_id: &str,
    platform: VendorPlatform,
    stdout: &[u8],
    stderr: &[u8],
) -> VendorContractResolution {
    let vendor = VendorCliFamily::from_agent_id(agent_id);
    let version = normalize_version(stdout, stderr);
    let version_status = version.status();
    let normalized_version = version.normalized();

    if let ParsedVersion::Normalized { triple, .. } = &version {
        if let Some(profile) = exact_profile(vendor, platform, *triple) {
            return verified_resolution(
                vendor,
                platform,
                version_status,
                normalized_version,
                profile,
            );
        }
    }

    let fallback_reason = match version {
        ParsedVersion::Missing => VendorFallbackReason::MissingVersion,
        ParsedVersion::Ambiguous => VendorFallbackReason::AmbiguousVersion,
        ParsedVersion::Unparseable => VendorFallbackReason::UnparseableVersion,
        ParsedVersion::Normalized { triple, .. } => {
            unverified_version_reason(vendor, platform, triple)
        }
    };
    raw_resolution(
        vendor,
        platform,
        version_status,
        normalized_version,
        fallback_reason,
    )
}

fn verified_resolution(
    vendor: VendorCliFamily,
    platform: VendorPlatform,
    version_status: VendorVersionStatus,
    normalized_version: Option<String>,
    profile: &VerifiedProfile,
) -> VendorContractResolution {
    VendorContractResolution {
        vendor,
        platform,
        mode: VendorRuntimeMode::VerifiedSemantic,
        version_status,
        normalized_version,
        contract_id: Some(profile.contract_id),
        fallback_reason: None,
        capabilities: VendorCapabilitySet {
            raw_pty_spawn: RAW_PASSTHROUGH,
            terminal_screen: RAW_PASSTHROUGH,
            terminal_resize: RAW_PASSTHROUGH,
            terminal_interrupt: RAW_PASSTHROUGH,
            terminal_stop: RAW_PASSTHROUGH,
            semantic_readiness: verified_or_unavailable(
                profile.semantic_readiness_contract,
            ),
            provider_session_identity: verified_or_unavailable(
                profile.provider_session_identity_contract,
            ),
            structured_prompt: verified_or_unavailable(profile.structured_prompt_contract),
            semantic_resume: verified_or_unavailable(profile.semantic_resume_contract),
        },
    }
}

fn raw_resolution(
    vendor: VendorCliFamily,
    platform: VendorPlatform,
    version_status: VendorVersionStatus,
    normalized_version: Option<String>,
    fallback_reason: VendorFallbackReason,
) -> VendorContractResolution {
    let semantic_unavailable = VendorCapabilityVerdict::Unavailable {
        reason: VendorCapabilityUnavailableReason::NoVerifiedContract(fallback_reason),
    };
    VendorContractResolution {
        vendor,
        platform,
        mode: VendorRuntimeMode::RawPassthrough,
        version_status,
        normalized_version,
        contract_id: None,
        fallback_reason: Some(fallback_reason),
        capabilities: VendorCapabilitySet {
            raw_pty_spawn: RAW_PASSTHROUGH,
            terminal_screen: RAW_PASSTHROUGH,
            terminal_resize: RAW_PASSTHROUGH,
            terminal_interrupt: RAW_PASSTHROUGH,
            terminal_stop: RAW_PASSTHROUGH,
            semantic_readiness: semantic_unavailable,
            provider_session_identity: semantic_unavailable,
            structured_prompt: semantic_unavailable,
            semantic_resume: semantic_unavailable,
        },
    }
}

const fn verified_or_unavailable(
    capability_contract: Option<&'static str>,
) -> VendorCapabilityVerdict {
    match capability_contract {
        Some(capability_contract) => VendorCapabilityVerdict::Verified {
            capability_contract,
        },
        None => VendorCapabilityVerdict::Unavailable {
            reason: VendorCapabilityUnavailableReason::NotVerifiedByContract,
        },
    }
}

fn exact_profile(
    vendor: VendorCliFamily,
    platform: VendorPlatform,
    version: VersionTriple,
) -> Option<&'static VerifiedProfile> {
    VERIFIED_PROFILES.iter().find(|profile| {
        profile.vendor == vendor && profile.platform == platform && profile.version == version
    })
}

fn unverified_version_reason(
    vendor: VendorCliFamily,
    platform: VendorPlatform,
    version: VersionTriple,
) -> VendorFallbackReason {
    if vendor == VendorCliFamily::Unknown {
        return VendorFallbackReason::UnsupportedVendor;
    }
    if !VERIFIED_PROFILES.iter().any(|profile| profile.vendor == vendor) {
        return VendorFallbackReason::NoVerifiedProfile;
    }
    let platform_versions = VERIFIED_PROFILES
        .iter()
        .filter(|profile| profile.vendor == vendor && profile.platform == platform)
        .map(|profile| profile.version)
        .collect::<Vec<_>>();
    let Some(oldest) = platform_versions.iter().min() else {
        return VendorFallbackReason::UnsupportedPlatform;
    };
    let newest = platform_versions
        .iter()
        .max()
        .expect("a verified platform version must have a newest member");
    if version < *oldest {
        VendorFallbackReason::LegacyVersion
    } else if version > *newest {
        VendorFallbackReason::FutureVersion
    } else {
        VendorFallbackReason::UnlistedVersion
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct VersionTriple {
    major: u64,
    minor: u64,
    patch: u64,
}

impl VersionTriple {
    const fn new(major: u64, minor: u64, patch: u64) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    fn normalized(self) -> String {
        format!("{}.{}.{}", self.major, self.minor, self.patch)
    }
}

enum ParsedVersion {
    Normalized {
        triple: VersionTriple,
        normalized: String,
    },
    Missing,
    Ambiguous,
    Unparseable,
}

impl ParsedVersion {
    const fn status(&self) -> VendorVersionStatus {
        match self {
            Self::Normalized { .. } => VendorVersionStatus::Normalized,
            Self::Missing => VendorVersionStatus::Missing,
            Self::Ambiguous => VendorVersionStatus::Ambiguous,
            Self::Unparseable => VendorVersionStatus::Unparseable,
        }
    }

    fn normalized(&self) -> Option<String> {
        match self {
            Self::Normalized { normalized, .. } => Some(normalized.clone()),
            Self::Missing | Self::Ambiguous | Self::Unparseable => None,
        }
    }
}

fn normalize_version(stdout: &[u8], stderr: &[u8]) -> ParsedVersion {
    let Some(total_bytes) = stdout
        .len()
        .checked_add(stderr.len())
        .and_then(|bytes| bytes.checked_add(1))
    else {
        return ParsedVersion::Unparseable;
    };
    if total_bytes > MAX_VERSION_OUTPUT_BYTES {
        return ParsedVersion::Unparseable;
    }
    if stdout.iter().chain(stderr).all(u8::is_ascii_whitespace) {
        return ParsedVersion::Missing;
    }
    if !stdout.is_ascii() || !stderr.is_ascii() {
        return ParsedVersion::Unparseable;
    }
    let mut output = Vec::with_capacity(total_bytes);
    output.extend_from_slice(stdout);
    output.push(b'\n');
    output.extend_from_slice(stderr);
    let Some(candidates) = version_candidates(&output) else {
        return ParsedVersion::Unparseable;
    };
    match candidates.len() {
        0 => ParsedVersion::Unparseable,
        1 => {
            let triple = *candidates
                .first()
                .expect("single version candidate must be present");
            ParsedVersion::Normalized {
                normalized: triple.normalized(),
                triple,
            }
        }
        _ => ParsedVersion::Ambiguous,
    }
}

fn version_candidates(bytes: &[u8]) -> Option<BTreeSet<VersionTriple>> {
    let mut candidates = BTreeSet::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit() || !valid_start_boundary(bytes, index) {
            index += 1;
            continue;
        }
        let start = index;
        let Some(major_end) = decimal_end(bytes, start) else {
            index += 1;
            continue;
        };
        if major_end >= bytes.len() || bytes[major_end] != b'.' {
            index = major_end;
            continue;
        }
        let minor_start = major_end + 1;
        let Some(minor_end) = decimal_end(bytes, minor_start) else {
            index = minor_start;
            continue;
        };
        if minor_end >= bytes.len() || bytes[minor_end] != b'.' {
            index = minor_end;
            continue;
        }
        let patch_start = minor_end + 1;
        let Some(end) = decimal_end(bytes, patch_start) else {
            index = patch_start;
            continue;
        };
        if !valid_end_boundary(bytes, end)
            || !valid_decimal(&bytes[start..major_end])
            || !valid_decimal(&bytes[minor_start..minor_end])
            || !valid_decimal(&bytes[patch_start..end])
        {
            return None;
        }
        let major = parse_decimal(&bytes[start..major_end])?;
        let minor = parse_decimal(&bytes[minor_start..minor_end])?;
        let patch = parse_decimal(&bytes[patch_start..end])?;
        candidates.insert(VersionTriple::new(major, minor, patch));
        index = end.max(index + 1);
    }
    Some(candidates)
}

fn valid_start_boundary(bytes: &[u8], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    let previous = bytes[index - 1];
    if matches!(previous, b'v' | b'V') {
        return index == 1 || is_version_boundary(bytes[index - 2]);
    }
    is_version_boundary(previous)
}

fn valid_end_boundary(bytes: &[u8], end: usize) -> bool {
    end == bytes.len() || is_version_boundary(bytes[end])
}

fn is_version_boundary(byte: u8) -> bool {
    !byte.is_ascii_alphanumeric() && !matches!(byte, b'.' | b'-' | b'+' | b'_')
}

fn decimal_end(bytes: &[u8], start: usize) -> Option<usize> {
    if start >= bytes.len() || !bytes[start].is_ascii_digit() {
        return None;
    }
    let mut end = start + 1;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    Some(end)
}

fn valid_decimal(bytes: &[u8]) -> bool {
    bytes.len() == 1 || bytes.first() != Some(&b'0')
}

fn parse_decimal(bytes: &[u8]) -> Option<u64> {
    bytes.iter().try_fold(0_u64, |value, byte| {
        value
            .checked_mul(10)?
            .checked_add(u64::from(byte.checked_sub(b'0')?))
    })
}

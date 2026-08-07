//! Developer-only, fail-closed vendor contract inspection and verification.
//!
//! This example deliberately has no integration surface with the runtime. It
//! only executes fixed provider `--version` probes and fixed, named Cargo
//! canaries. Reports contain normalized identifiers and bounded result enums;
//! child output, environment values, prompts, and launcher paths are never
//! written to disk.

use std::collections::BTreeMap;
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const SPEC_REVISION: &str = "vendor-contract/3";
const VERSION_TIMEOUT: Duration = Duration::from_secs(5);
const CANARY_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_CHILD_CAPTURE_BYTES: usize = 64 * 1024;
const MAX_FIXTURE_BYTES: u64 = 16 * 1024;
const MAX_LAUNCHER_BYTES: u64 = 512 * 1024 * 1024;
const MAX_REPORT_BYTES: usize = 16 * 1024;
const LIVE_PTY_CANARY_ENV: &str = "GATE4AGENT_VENDOR_PTY_CANARY";
const LIVE_PIPE_CANARY_ENV: &str = "GATE4AGENT_VENDOR_CANARY";
const LIVE_LAUNCHER_ENV: &str = "GATE4AGENT_VENDOR_CANARY_LAUNCHER";
const LIVE_LAUNCHER_AGENT_ENV: &str = "GATE4AGENT_VENDOR_CANARY_AGENT";
const PROCESS_TREE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const READER_COMPLETION_TIMEOUT: Duration = Duration::from_secs(2);

const CLAUDE_2_1_223_PTY_CAPABILITIES: &[&str] = &[
    "pty.auth_gate",
    "readiness.bracketed_paste",
    "pty.prompt_round_trip",
    "pty.resize",
    "pty.interrupt_recovery",
    "pty.teardown",
];
const CLAUDE_2_1_223_PROBE_CAPABILITIES: &[&str] = &[
    "readiness.bracketed_paste",
    "pty.prompt_round_trip",
    "pty.resize",
    "pty.interrupt_recovery",
    "pty.teardown",
];
const CLAUDE_2_1_224_PTY_CAPABILITIES: &[&str] = &[
    "pty.auth_gate",
    "readiness.win32_input",
    "readiness.focus_reporting",
    "readiness.cursor_show",
    "readiness.cursor_hide",
    "readiness.clear_screen",
    "pty.prompt_round_trip",
    "pty.resize",
    "pty.interrupt_recovery",
    "pty.teardown",
];
const CLAUDE_2_1_224_PROBE_CAPABILITIES: &[&str] = &[
    "readiness.win32_input",
    "readiness.focus_reporting",
    "readiness.cursor_show",
    "readiness.cursor_hide",
    "readiness.clear_screen",
    "pty.prompt_round_trip",
    "pty.resize",
    "pty.interrupt_recovery",
    "pty.teardown",
];
const CLAUDE_INLINE_CAPABILITIES: &[&str] = &["inline.auth_gate", "inline.fresh_resume"];
const CODEX_0_144_6_PTY_CAPABILITIES: &[&str] = &[
    "pty.auth_gate",
    "pty.authenticated_contract",
    "pty.whole_chunk_input",
];
const CODEX_0_144_6_INLINE_CAPABILITIES: &[&str] =
    &["inline.auth_gate", "inline.fresh_resume"];
const KIMI_0_31_1_PTY_CAPABILITIES: &[&str] = &[
    "pty.auth_gate",
    "readiness.no_bracketed_paste",
];
const KIMI_0_31_1_INLINE_CAPABILITIES: &[&str] = &[
    "inline.auth_gate",
    "exit.direct_propagation",
    "resume.current_session_flag",
];

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TransportScope {
    Inline,
    Pty,
}

impl TransportScope {
    fn label(self) -> &'static str {
        match self {
            Self::Inline => "inline",
            Self::Pty => "pty",
        }
    }

    fn auth_gate_id(self) -> &'static str {
        match self {
            Self::Inline => "inline.auth_gate",
            Self::Pty => "pty.auth_gate",
        }
    }
}

#[derive(Clone, Copy)]
struct TransportContract {
    transport: TransportScope,
    capabilities: &'static [&'static str],
    live_probes: &'static [LiveProbe],
    exact_launch_injected: bool,
}

#[derive(Clone, Copy)]
struct Contract {
    id: &'static str,
    agent: &'static str,
    platform: &'static str,
    version: &'static str,
    transports: &'static [TransportContract],
}

#[derive(Clone, Copy)]
struct LiveProbe {
    transport: TransportScope,
    test_target: &'static str,
    test_name: &'static str,
    env_name: &'static str,
    sentinel_prefix: &'static str,
    required_fields: &'static [(&'static str, &'static str)],
    capabilities: &'static [&'static str],
}

const CLAUDE_2_1_223_PROBES: &[LiveProbe] = &[LiveProbe {
    transport: TransportScope::Pty,
    test_target: "vendor_pty_live",
    test_name: "windows_live_claude_pty_contract",
    env_name: LIVE_PTY_CANARY_ENV,
    sentinel_prefix: "vendor_pty_canary",
    required_fields: &[
        ("agent", "claude"),
        ("initial_prompt_response", "true"),
        ("followup_response", "true"),
        ("recovery_response", "true"),
        ("complex_multiline_prompt", "true"),
        ("resize", "true"),
        ("interrupt_in_flight", "true"),
        ("active_sessions", "0"),
    ],
    capabilities: CLAUDE_2_1_223_PROBE_CAPABILITIES,
}];
const CLAUDE_2_1_224_PROBES: &[LiveProbe] = &[LiveProbe {
    transport: TransportScope::Pty,
    test_target: "vendor_pty_live",
    test_name: "windows_live_claude_pty_contract",
    env_name: LIVE_PTY_CANARY_ENV,
    sentinel_prefix: "vendor_pty_canary",
    required_fields: CLAUDE_2_1_223_PROBES[0].required_fields,
    capabilities: CLAUDE_2_1_224_PROBE_CAPABILITIES,
}];
const CLAUDE_INLINE_PROBES: &[LiveProbe] = &[LiveProbe {
    transport: TransportScope::Inline,
    test_target: "vendor_cli_live",
    test_name: "windows_live_claude_inline_contract",
    env_name: LIVE_PIPE_CANARY_ENV,
    sentinel_prefix: "vendor_canary",
    required_fields: &[
        ("agent", "claude"),
        ("transport", "pipe"),
        ("fresh_exit_code", "0"),
        ("resume_exit_code", "0"),
        ("marker_observed", "true"),
        ("same_provider_session", "true"),
        ("isolated_cwd", "true"),
        ("active_sessions", "0"),
    ],
    capabilities: &["inline.fresh_resume"],
}];
const CODEX_0_144_6_PROBES: &[LiveProbe] = &[
    LiveProbe {
        transport: TransportScope::Pty,
        test_target: "vendor_pty_live",
        test_name: "windows_live_codex_pty_contract",
        env_name: LIVE_PTY_CANARY_ENV,
        sentinel_prefix: "vendor_pty_canary",
        required_fields: &[
            ("agent", "codex"),
            ("initial_prompt_response", "true"),
            ("followup_response", "true"),
            ("recovery_response", "true"),
            ("complex_multiline_prompt", "true"),
            ("resize", "true"),
            ("interrupt_in_flight", "true"),
            ("active_sessions", "0"),
        ],
        capabilities: &["pty.authenticated_contract"],
    },
    LiveProbe {
        transport: TransportScope::Pty,
        test_target: "vendor_pty_live",
        test_name: "windows_live_codex_whole_chunk_transport_contract",
        env_name: LIVE_PTY_CANARY_ENV,
        sentinel_prefix: "vendor_pty_whole_chunk_transport",
        required_fields: &[
            ("agent", "codex"),
            ("initial_prompt", "false"),
            ("composer_stable", "true"),
            ("terminal_text_dispatches", "1"),
            ("terminal_text_completed", "1"),
            ("enter_sent", "false"),
            ("probe_visible", "true"),
            ("force_stop", "true"),
            ("pid_reaped", "true"),
            ("active_sessions", "0"),
        ],
        capabilities: &["pty.whole_chunk_input"],
    },
];
const CODEX_INLINE_PROBES: &[LiveProbe] = &[LiveProbe {
    transport: TransportScope::Inline,
    test_target: "vendor_cli_live",
    test_name: "windows_live_codex_inline_contract",
    env_name: LIVE_PIPE_CANARY_ENV,
    sentinel_prefix: "vendor_canary",
    required_fields: &[
        ("agent", "codex"),
        ("transport", "pipe"),
        ("fresh_exit_code", "0"),
        ("resume_exit_code", "0"),
        ("marker_observed", "true"),
        ("same_provider_session", "true"),
        ("isolated_cwd", "true"),
        ("active_sessions", "0"),
    ],
    capabilities: &["inline.fresh_resume"],
}];
const KIMI_0_31_1_PTY_PROBES: &[LiveProbe] = &[LiveProbe {
    transport: TransportScope::Pty,
    test_target: "vendor_pty_live",
    test_name: "windows_live_kimi_pty_contract",
    env_name: LIVE_PTY_CANARY_ENV,
    sentinel_prefix: "vendor_pty_canary",
    required_fields: &[
        ("agent", "kimi"),
        ("initial_prompt_response", "true"),
        ("followup_response", "true"),
        ("recovery_response", "true"),
        ("complex_multiline_prompt", "true"),
        ("resize", "true"),
        ("interrupt_in_flight", "true"),
        ("active_sessions", "0"),
    ],
    capabilities: &["readiness.no_bracketed_paste"],
}];
const KIMI_0_31_1_INLINE_PROBES: &[LiveProbe] = &[LiveProbe {
    transport: TransportScope::Inline,
    test_target: "vendor_cli_live",
    test_name: "windows_live_kimi_inline_contract",
    env_name: LIVE_PIPE_CANARY_ENV,
    sentinel_prefix: "vendor_canary",
    required_fields: &[
        ("agent", "kimi"),
        ("transport", "pipe"),
        ("fresh_exit_code", "0"),
        ("resume_exit_code", "0"),
        ("marker_observed", "true"),
        ("same_provider_session", "true"),
        ("isolated_cwd", "true"),
        ("active_sessions", "0"),
    ],
    capabilities: &["exit.direct_propagation", "resume.current_session_flag"],
}];

const CLAUDE_2_1_223_TRANSPORTS: &[TransportContract] = &[
    TransportContract {
        transport: TransportScope::Inline,
        capabilities: CLAUDE_INLINE_CAPABILITIES,
        live_probes: CLAUDE_INLINE_PROBES,
        exact_launch_injected: false,
    },
    TransportContract {
        transport: TransportScope::Pty,
        capabilities: CLAUDE_2_1_223_PTY_CAPABILITIES,
        live_probes: CLAUDE_2_1_223_PROBES,
        exact_launch_injected: true,
    },
];
const CLAUDE_2_1_224_TRANSPORTS: &[TransportContract] = &[
    TransportContract {
        transport: TransportScope::Inline,
        capabilities: CLAUDE_INLINE_CAPABILITIES,
        live_probes: CLAUDE_INLINE_PROBES,
        exact_launch_injected: false,
    },
    TransportContract {
        transport: TransportScope::Pty,
        capabilities: CLAUDE_2_1_224_PTY_CAPABILITIES,
        live_probes: CLAUDE_2_1_224_PROBES,
        exact_launch_injected: true,
    },
];
const CODEX_0_144_6_TRANSPORTS: &[TransportContract] = &[
    TransportContract {
        transport: TransportScope::Inline,
        capabilities: CODEX_0_144_6_INLINE_CAPABILITIES,
        live_probes: CODEX_INLINE_PROBES,
        exact_launch_injected: false,
    },
    TransportContract {
        transport: TransportScope::Pty,
        capabilities: CODEX_0_144_6_PTY_CAPABILITIES,
        live_probes: CODEX_0_144_6_PROBES,
        exact_launch_injected: true,
    },
];
const KIMI_0_31_1_TRANSPORTS: &[TransportContract] = &[
    TransportContract {
        transport: TransportScope::Inline,
        capabilities: KIMI_0_31_1_INLINE_CAPABILITIES,
        live_probes: KIMI_0_31_1_INLINE_PROBES,
        exact_launch_injected: false,
    },
    TransportContract {
        transport: TransportScope::Pty,
        capabilities: KIMI_0_31_1_PTY_CAPABILITIES,
        live_probes: KIMI_0_31_1_PTY_PROBES,
        exact_launch_injected: true,
    },
];

const CONTRACTS: &[Contract] = &[
    Contract {
        id: "claude.windows-x86_64.2.1.223",
        agent: "claude",
        platform: "windows-x86_64",
        version: "2.1.223",
        transports: CLAUDE_2_1_223_TRANSPORTS,
    },
    Contract {
        id: "claude.windows-x86_64.2.1.224",
        agent: "claude",
        platform: "windows-x86_64",
        version: "2.1.224",
        transports: CLAUDE_2_1_224_TRANSPORTS,
    },
    Contract {
        id: "codex.windows-x86_64.0.144.6",
        agent: "codex",
        platform: "windows-x86_64",
        version: "0.144.6",
        transports: CODEX_0_144_6_TRANSPORTS,
    },
    Contract {
        id: "kimi.windows-x86_64.0.31.1",
        agent: "kimi",
        platform: "windows-x86_64",
        version: "0.31.1",
        transports: KIMI_0_31_1_TRANSPORTS,
    },
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CheckResult {
    Pass,
    Mismatch,
    Missing,
    AuthGated,
    Timeout,
    ChildNonzero,
}

impl CheckResult {
    fn label(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Mismatch => "mismatch",
            Self::Missing => "missing",
            Self::AuthGated => "auth-gated",
            Self::Timeout => "timeout",
            Self::ChildNonzero => "child-nonzero",
        }
    }

    fn fixture_value(value: &str) -> Option<Self> {
        match value {
            "pass" => Some(Self::Pass),
            "mismatch" => Some(Self::Mismatch),
            "missing" => Some(Self::Missing),
            "auth-gated" => Some(Self::AuthGated),
            "timeout" => Some(Self::Timeout),
            "child-nonzero" => Some(Self::ChildNonzero),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum ChildOutcome {
    Success {
        stdout: CapturedOutput,
        stderr: CapturedOutput,
    },
    Timeout,
    Nonzero,
    SpawnFailed,
}

#[derive(Debug, Default)]
struct CapturedOutput {
    bytes: Vec<u8>,
    overflowed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LauncherSnapshot {
    canonical_path: PathBuf,
    modified: Option<std::time::SystemTime>,
    bytes: u64,
    content_fingerprint: u64,
}

struct ResolvedLauncher {
    path: PathBuf,
    snapshot: LauncherSnapshot,
}

#[derive(Debug, Eq, PartialEq)]
enum VersionOutcome {
    Exact(String),
    Missing,
    Ambiguous,
    Timeout,
    ChildNonzero,
}

struct Fixture {
    agent: String,
    platform: String,
    version: String,
    checks: BTreeMap<String, CheckResult>,
}

struct Report {
    contract: Option<&'static Contract>,
    agent: String,
    platform: String,
    version: Option<String>,
    mode: &'static str,
    checks: Vec<(String, CheckResult)>,
}

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("vendor_contract_lab: {message}");
            ExitCode::from(2)
        }
    }
}

fn run(args: Vec<String>) -> Result<bool, &'static str> {
    let Some(command) = args.first().map(String::as_str) else {
        return Err("expected inspect or verify");
    };
    match command {
        "inspect" => run_inspect(&args[1..]),
        "verify" => run_verify(&args[1..]),
        _ => Err("expected inspect or verify"),
    }
}

fn run_inspect(args: &[String]) -> Result<bool, &'static str> {
    let options = parse_options(args, &["agent"])?;
    let agent = required_option(&options, "agent")?;
    provider_program(agent).ok_or("agent is not allowlisted")?;
    let platform = current_platform();
    if !platform_is_supported(platform) {
        println!(
            "{{\"agent\":\"{}\",\"platform\":\"{}\",\"version\":null,\"contract_id\":\"unrecognized\",\"result\":\"unsupported-platform\"}}",
            escape_json(agent),
            platform,
        );
        return Ok(false);
    }
    let version = inspect_version(agent);
    let (version_value, result, contract) = match &version {
        VersionOutcome::Exact(version) => {
            let contract = contract_for(agent, platform, version);
            (
                format!("\"{}\"", escape_json(version)),
                if contract.is_some() { "known" } else { "unknown-version" },
                contract,
            )
        }
        VersionOutcome::Missing => ("null".to_owned(), "missing-version", None),
        VersionOutcome::Ambiguous => ("null".to_owned(), "ambiguous-version", None),
        VersionOutcome::Timeout => ("null".to_owned(), "timeout", None),
        VersionOutcome::ChildNonzero => ("null".to_owned(), "child-nonzero", None),
    };
    let contract_id = contract.map_or("unrecognized", |known| known.id);
    println!(
        "{{\"agent\":\"{}\",\"platform\":\"{}\",\"version\":{},\"contract_id\":\"{}\",\"result\":\"{}\"}}",
        escape_json(agent),
        platform,
        version_value,
        contract_id,
        result,
    );
    Ok(contract.is_some())
}

fn run_verify(args: &[String]) -> Result<bool, &'static str> {
    let options = parse_options(args, &["agent", "fixture", "live", "out"])?;
    let agent = required_option(&options, "agent")?;
    provider_program(agent).ok_or("agent is not allowlisted")?;
    let output = PathBuf::from(required_option(&options, "out")?);
    let fixture = options.get("fixture").and_then(|value| value.as_deref());
    let live = options.contains_key("live");
    if fixture.is_some() == live {
        return Err("verify requires exactly one of --fixture or --live");
    }

    let report = if let Some(fixture_path) = fixture {
        verify_fixture(agent, Path::new(fixture_path))?
    } else {
        verify_live(agent)
    };
    let succeeded = report.verification_succeeded();
    write_report(&output, &report)?;
    Ok(succeeded)
}

fn parse_options(
    args: &[String],
    allowed: &[&str],
) -> Result<BTreeMap<String, Option<String>>, &'static str> {
    let mut parsed = BTreeMap::new();
    let mut index = 0;
    while index < args.len() {
        let argument = args[index].as_str();
        let Some(name) = argument.strip_prefix("--") else {
            return Err("unexpected positional argument");
        };
        if !allowed.contains(&name) || parsed.contains_key(name) {
            return Err("unknown or duplicate option");
        }
        if name == "live" {
            parsed.insert(name.to_owned(), None);
            index += 1;
            continue;
        }
        let Some(value) = args.get(index + 1) else {
            return Err("option value is missing");
        };
        if value.starts_with("--") {
            return Err("option value is missing");
        }
        parsed.insert(name.to_owned(), Some(value.clone()));
        index += 2;
    }
    Ok(parsed)
}

fn required_option<'a>(
    options: &'a BTreeMap<String, Option<String>>,
    name: &str,
) -> Result<&'a str, &'static str> {
    options
        .get(name)
        .and_then(|value| value.as_deref())
        .filter(|value| !value.is_empty())
        .ok_or("required option is missing")
}

fn provider_program(agent: &str) -> Option<&'static str> {
    match agent {
        "claude" => Some("claude"),
        "codex" => Some("codex"),
        "kimi" => Some("kimi"),
        _ => None,
    }
}

fn current_platform() -> &'static str {
    match (env::consts::OS, env::consts::ARCH) {
        ("windows", "x86_64") => "windows-x86_64",
        ("windows", "aarch64") => "windows-aarch64",
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        ("macos", "aarch64") => "macos-aarch64",
        _ => "unsupported-platform",
    }
}

fn platform_is_supported(platform: &str) -> bool {
    platform == "windows-x86_64"
}

fn contract_for(agent: &str, platform: &str, version: &str) -> Option<&'static Contract> {
    CONTRACTS.iter().find(|contract| {
        contract.agent == agent && contract.platform == platform && contract.version == version
    })
}

fn contract_has_capability(contract: &Contract, capability: &str) -> bool {
    contract
        .transports
        .iter()
        .any(|transport| transport.capabilities.contains(&capability))
}

fn contract_capabilities(contract: &Contract) -> impl Iterator<Item = &'static str> + '_ {
    contract
        .transports
        .iter()
        .flat_map(|transport| transport.capabilities.iter().copied())
}

fn inspect_version(agent: &str) -> VersionOutcome {
    let Some(launcher) = resolve_provider_launcher(agent) else {
        return VersionOutcome::ChildNonzero;
    };
    inspect_resolved_version(&launcher)
}

fn inspect_resolved_version(launcher: &ResolvedLauncher) -> VersionOutcome {
    let Some(command) = provider_version_command(&launcher.path) else {
        return VersionOutcome::ChildNonzero;
    };
    match run_bounded_child(command, VERSION_TIMEOUT) {
        ChildOutcome::Success { stdout, stderr }
            if !stdout.overflowed && !stderr.overflowed =>
        {
            normalize_version(&stdout.bytes, &stderr.bytes)
        }
        ChildOutcome::Success { .. } => VersionOutcome::Ambiguous,
        ChildOutcome::Timeout => VersionOutcome::Timeout,
        ChildOutcome::Nonzero | ChildOutcome::SpawnFailed => VersionOutcome::ChildNonzero,
    }
}

fn provider_version_command(launcher: &Path) -> Option<Command> {
    #[cfg(windows)]
    {
        let extension = launcher
            .extension()
            .and_then(|value| value.to_str())
            .map(str::to_ascii_lowercase)?;
        if extension == "cmd" {
            let mut command = Command::new("cmd.exe");
            command.args(["/D", "/Q", "/C"]);
            command.arg(launcher);
            command.arg("--version");
            return Some(command);
        }
        if extension == "exe" {
            let mut command = Command::new(launcher);
            command.arg("--version");
            return Some(command);
        }
        None
    }
    #[cfg(not(windows))]
    {
        let mut command = Command::new(launcher);
        command.arg("--version");
        Some(command)
    }
}

#[cfg(windows)]
fn resolve_provider_launcher(agent: &str) -> Option<ResolvedLauncher> {
    let program = provider_program(agent)?;
    let path = env::var_os("PATH")?;
    resolve_windows_launcher_on_path(program, &path)
}

#[cfg(windows)]
fn resolve_windows_launcher_on_path(program: &str, path: &std::ffi::OsStr) -> Option<ResolvedLauncher> {
    let directories = env::split_paths(&path).collect::<Vec<_>>();
    let candidate = ["cmd", "exe"].into_iter().find_map(|extension| {
        let file_name = format!("{program}.{extension}");
        directories
            .iter()
            .map(|directory| directory.join(&file_name))
            .find(|candidate| candidate.is_file())
    })?;
    let path = windows_launch_path(candidate.canonicalize().ok()?)?;
    let snapshot = snapshot_launcher(&path).ok()?;
    Some(ResolvedLauncher { path, snapshot })
}

#[cfg(windows)]
fn windows_launch_path(path: PathBuf) -> Option<PathBuf> {
    let value = path.to_str()?;
    if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        return Some(PathBuf::from(format!(r"\\{rest}")));
    }
    Some(PathBuf::from(
        value.strip_prefix(r"\\?\").unwrap_or(value),
    ))
}

#[cfg(not(windows))]
fn resolve_provider_launcher(agent: &str) -> Option<ResolvedLauncher> {
    let path = PathBuf::from(provider_program(agent)?);
    let snapshot = snapshot_launcher(&path).ok()?;
    Some(ResolvedLauncher { path, snapshot })
}

fn snapshot_launcher(path: &Path) -> Result<LauncherSnapshot, &'static str> {
    let canonical_path = path
        .canonicalize()
        .map_err(|_| "canonical launcher path is unavailable")?;
    let mut launcher = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| "canonical launcher cannot be opened")?;
    let before = launcher
        .metadata()
        .map_err(|_| "canonical launcher metadata is unavailable")?;
    if !before.is_file() || before.len() > MAX_LAUNCHER_BYTES {
        return Err("canonical launcher is not a bounded file");
    }
    let modified = before.modified().ok();
    let mut content_fingerprint = 0xcbf29ce484222325_u64;
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = launcher
            .read(&mut buffer)
            .map_err(|_| "canonical launcher cannot be fingerprinted")?;
        if read == 0 {
            break;
        }
        bytes = bytes
            .checked_add(read as u64)
            .ok_or("canonical launcher size overflowed")?;
        if bytes > MAX_LAUNCHER_BYTES {
            return Err("canonical launcher exceeded its bound");
        }
        for byte in &buffer[..read] {
            content_fingerprint ^= u64::from(*byte);
            content_fingerprint = content_fingerprint.wrapping_mul(0x100000001b3);
        }
    }
    let after = launcher
        .metadata()
        .map_err(|_| "canonical launcher metadata changed while reading")?;
    if before.len() != after.len() || bytes != before.len() {
        return Err("canonical launcher changed while fingerprinting");
    }
    if after.modified().ok() != modified {
        return Err("canonical launcher metadata changed while fingerprinting");
    }
    Ok(LauncherSnapshot {
        canonical_path,
        modified,
        bytes,
        content_fingerprint,
    })
}

fn normalize_version(stdout: &[u8], stderr: &[u8]) -> VersionOutcome {
    let mut bytes = Vec::with_capacity(stdout.len().saturating_add(stderr.len()).saturating_add(1));
    bytes.extend_from_slice(stdout);
    bytes.push(b'\n');
    bytes.extend_from_slice(stderr);
    let Some(candidates) = semver_candidates(&bytes) else {
        return VersionOutcome::Missing;
    };
    match candidates.as_slice() {
        [] => VersionOutcome::Missing,
        [version] => VersionOutcome::Exact(version.clone()),
        _ => VersionOutcome::Ambiguous,
    }
}

fn semver_candidates(bytes: &[u8]) -> Option<Vec<String>> {
    if !bytes.is_ascii() {
        return None;
    }
    let mut candidates = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if !bytes[index].is_ascii_digit()
            || (index > 0
                && (bytes[index - 1].is_ascii_alphanumeric()
                    || matches!(bytes[index - 1], b'.' | b'-' | b'+' | b'_')))
        {
            index += 1;
            continue;
        }
        let start = index;
        let Some(first_end) = decimal_end(bytes, start) else {
            index += 1;
            continue;
        };
        if first_end >= bytes.len() || bytes[first_end] != b'.' {
            index = first_end;
            continue;
        }
        let second_start = first_end + 1;
        let Some(second_end) = decimal_end(bytes, second_start) else {
            index = second_start;
            continue;
        };
        if second_end >= bytes.len() || bytes[second_end] != b'.' {
            index = second_end;
            continue;
        }
        let third_start = second_end + 1;
        let Some(end) = decimal_end(bytes, third_start) else {
            index = third_start;
            continue;
        };
        let boundary_ok = end == bytes.len()
            || (!bytes[end].is_ascii_alphanumeric()
                && !matches!(bytes[end], b'.' | b'-' | b'+' | b'_'));
        if boundary_ok
            && valid_decimal(&bytes[start..first_end])
            && valid_decimal(&bytes[second_start..second_end])
            && valid_decimal(&bytes[third_start..end])
        {
            candidates.push(String::from_utf8(bytes[start..end].to_vec()).ok()?);
        }
        index = end.max(index + 1);
    }
    Some(candidates)
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

fn run_bounded_child(mut command: Command, timeout: Duration) -> ChildOutcome {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let Ok(mut child) = command.spawn() else {
        return ChildOutcome::SpawnFailed;
    };
    let stdout = child.stdout.take().map(|reader| {
        thread::spawn(move || read_bounded(reader, MAX_CHILD_CAPTURE_BYTES))
    });
    let stderr = child.stderr.take().map(|reader| {
        thread::spawn(move || read_bounded(reader, MAX_CHILD_CAPTURE_BYTES))
    });
    let started = Instant::now();
    let wait_outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(Some(status)),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                terminate_process_tree(&mut child);
                break Ok(None);
            }
            Err(_) => {
                terminate_process_tree(&mut child);
                break Err(());
            }
        }
    };
    let reader_deadline = Instant::now() + READER_COMPLETION_TIMEOUT;
    let stdout = finish_reader_bounded(stdout, reader_deadline);
    let stderr = finish_reader_bounded(stderr, reader_deadline);
    match wait_outcome {
        Ok(None) => ChildOutcome::Timeout,
        Ok(Some(status)) if status.success() => ChildOutcome::Success { stdout, stderr },
        Ok(Some(_)) | Err(()) => ChildOutcome::Nonzero,
    }
}

fn finish_reader_bounded(
    reader: Option<thread::JoinHandle<CapturedOutput>>,
    deadline: Instant,
) -> CapturedOutput {
    let Some(reader) = reader else {
        return CapturedOutput::default();
    };
    while !reader.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(5));
    }
    if reader.is_finished() {
        reader.join().unwrap_or(CapturedOutput {
            bytes: Vec::new(),
            overflowed: true,
        })
    } else {
        drop(reader);
        CapturedOutput {
            bytes: Vec::new(),
            overflowed: true,
        }
    }
}

fn terminate_process_tree(child: &mut std::process::Child) {
    #[cfg(windows)]
    {
        let pid = child.id().to_string();
        let mut killer = Command::new("taskkill");
        killer
            .args(["/PID", pid.as_str(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(mut killer) = killer.spawn() {
            let started = Instant::now();
            loop {
                match killer.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if started.elapsed() < PROCESS_TREE_CLEANUP_TIMEOUT => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Ok(None) | Err(_) => {
                        let _ = killer.kill();
                        break;
                    }
                }
            }
        }
    }
    let _ = child.kill();
    let started = Instant::now();
    while started.elapsed() < PROCESS_TREE_CLEANUP_TIMEOUT {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => break,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn read_bounded(mut reader: impl Read, limit: usize) -> CapturedOutput {
    let mut captured = CapturedOutput::default();
    let mut buffer = [0_u8; 4096];
    loop {
        let Ok(read) = reader.read(&mut buffer) else {
            break;
        };
        if read == 0 {
            break;
        }
        let remaining = limit.saturating_sub(captured.bytes.len());
        captured
            .bytes
            .extend_from_slice(&buffer[..read.min(remaining)]);
        captured.overflowed |= read > remaining;
    }
    captured
}

fn verify_fixture(agent: &str, path: &Path) -> Result<Report, &'static str> {
    let fixture = read_fixture(path)?;
    if fixture.agent != agent {
        return Err("fixture agent does not match --agent");
    }
    let contract = contract_for(&fixture.agent, &fixture.platform, &fixture.version);
    let Some(contract) = contract else {
        return Ok(Report {
            contract: None,
            agent: agent.to_owned(),
            platform: fixture.platform,
            version: Some(fixture.version),
            mode: "fixture",
            checks: vec![("version_exact".to_owned(), CheckResult::Mismatch)],
        });
    };
    for key in fixture.checks.keys() {
        if !contract_has_capability(contract, key) {
            return Err("fixture contains a check outside the selected contract");
        }
    }
    let mut checks = vec![("version_exact".to_owned(), CheckResult::Pass)];
    checks.extend(contract_capabilities(contract).map(|capability| {
        (
            capability.to_owned(),
            fixture
                .checks
                .get(capability)
                .copied()
                .unwrap_or(CheckResult::Missing),
        )
    }));
    Ok(Report {
        contract: Some(contract),
        agent: agent.to_owned(),
        platform: contract.platform.to_owned(),
        version: Some(contract.version.to_owned()),
        mode: "fixture",
        checks,
    })
}

fn read_fixture(path: &Path) -> Result<Fixture, &'static str> {
    let fixture = OpenOptions::new()
        .read(true)
        .open(path)
        .map_err(|_| "fixture cannot be read")?;
    let metadata = fixture
        .metadata()
        .map_err(|_| "fixture metadata cannot be read")?;
    if !metadata.is_file() || metadata.len() > MAX_FIXTURE_BYTES {
        return Err("fixture is not a bounded file");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fixture
        .take(MAX_FIXTURE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "fixture cannot be read")?;
    if bytes.len() as u64 > MAX_FIXTURE_BYTES {
        return Err("fixture is not a bounded file");
    }
    let contents = String::from_utf8(bytes).map_err(|_| "fixture must be UTF-8")?;
    let mut values = BTreeMap::new();
    for line in contents.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err("fixture line is malformed");
        };
        if !valid_fixture_key(key) || !valid_label(value) || values.contains_key(key) {
            return Err("fixture key or value is invalid");
        }
        values.insert(key.to_owned(), value.to_owned());
    }
    let agent = take_fixture_value(&mut values, "agent")?;
    let platform = take_fixture_value(&mut values, "platform")?;
    let version = take_fixture_value(&mut values, "version")?;
    if !matches!(
        semver_candidates(version.as_bytes()).as_deref(),
        Some([candidate]) if candidate == &version
    ) {
        return Err("fixture version is not strict major.minor.patch");
    }
    let mut checks = BTreeMap::new();
    for (key, value) in values {
        let Some(check) = key.strip_prefix("check.") else {
            return Err("fixture contains an unknown field");
        };
        let Some(result) = CheckResult::fixture_value(&value) else {
            return Err("fixture check result is invalid");
        };
        checks.insert(check.to_owned(), result);
    }
    Ok(Fixture {
        agent,
        platform,
        version,
        checks,
    })
}

fn take_fixture_value(
    values: &mut BTreeMap<String, String>,
    key: &str,
) -> Result<String, &'static str> {
    values.remove(key).ok_or("fixture identity field is missing")
}

fn valid_fixture_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_'))
}

fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase()
                || byte.is_ascii_digit()
                || matches!(byte, b'.' | b'_' | b'-' | b'/')
        })
}

fn verify_live(agent: &str) -> Report {
    let platform = current_platform();
    if !platform_is_supported(platform) {
        return Report {
            contract: None,
            agent: provider_program(agent).expect("allowlisted agent").to_owned(),
            platform: platform.to_owned(),
            version: None,
            mode: "live",
            checks: vec![("platform_exact".to_owned(), CheckResult::Mismatch)],
        };
    }
    let Some(launcher) = resolve_provider_launcher(agent) else {
        return Report {
            contract: None,
            agent: provider_program(agent).expect("allowlisted agent").to_owned(),
            platform: platform.to_owned(),
            version: None,
            mode: "live",
            checks: vec![("launcher.file_stable".to_owned(), CheckResult::Missing)],
        };
    };
    let version_outcome = inspect_resolved_version(&launcher);
    let VersionOutcome::Exact(version) = version_outcome else {
        let result = match version_outcome {
            VersionOutcome::Ambiguous | VersionOutcome::Missing => CheckResult::Mismatch,
            VersionOutcome::Timeout => CheckResult::Timeout,
            VersionOutcome::ChildNonzero => CheckResult::ChildNonzero,
            VersionOutcome::Exact(_) => unreachable!(),
        };
        return Report {
            contract: None,
            agent: provider_program(agent).expect("allowlisted agent").to_owned(),
            platform: platform.to_owned(),
            version: None,
            mode: "live",
            checks: vec![("version_exact".to_owned(), result)],
        };
    };
    let Some(contract) = contract_for(agent, platform, &version) else {
        return Report {
            contract: None,
            agent: provider_program(agent).expect("allowlisted agent").to_owned(),
            platform: platform.to_owned(),
            version: Some(version),
            mode: "live",
            checks: vec![("version_exact".to_owned(), CheckResult::Mismatch)],
        };
    };

    let mut results = BTreeMap::new();
    for capability in contract_capabilities(contract) {
        results.insert(capability.to_owned(), CheckResult::Missing);
    }
    for transport in contract.transports {
        let mut auth_gate = if transport.live_probes.is_empty() {
            CheckResult::Missing
        } else {
            CheckResult::Pass
        };
        for probe in transport.live_probes {
            let result = if probe.transport == transport.transport {
                run_live_probe(agent, *probe, &launcher.path)
            } else {
                CheckResult::Mismatch
            };
            for capability in probe.capabilities {
                results.insert((*capability).to_owned(), result);
            }
            if result != CheckResult::Pass && auth_gate == CheckResult::Pass {
                auth_gate = result;
            }
        }
        results.insert(transport.transport.auth_gate_id().to_owned(), auth_gate);
    }
    let launcher_stability = snapshot_launcher(&launcher.path)
        .map(|after| {
            if after == launcher.snapshot {
                CheckResult::Pass
            } else {
                CheckResult::Mismatch
            }
        })
        .unwrap_or(CheckResult::Missing);
    let launcher_chain = launcher_chain_result(&launcher.path);
    let mut checks = vec![
        ("version_exact".to_owned(), CheckResult::Pass),
        ("launcher.file_stable".to_owned(), launcher_stability),
        ("launcher.chain_pinned".to_owned(), launcher_chain),
    ];
    checks.extend(contract_capabilities(contract).map(|capability| {
        (
            capability.to_owned(),
            results
                .get(capability)
                .copied()
                .unwrap_or(CheckResult::Missing),
        )
    }));
    Report {
        contract: Some(contract),
        agent: contract.agent.to_owned(),
        platform: contract.platform.to_owned(),
        version: Some(contract.version.to_owned()),
        mode: "live",
        checks,
    }
}

fn launcher_chain_result(launcher: &Path) -> CheckResult {
    if launcher
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("exe"))
    {
        CheckResult::Pass
    } else {
        CheckResult::Missing
    }
}

fn run_live_probe(agent: &str, probe: LiveProbe, launcher: &Path) -> CheckResult {
    if !live_probe_is_allowlisted(probe) {
        return CheckResult::Mismatch;
    }
    let Some(command) = live_probe_command(agent, probe, launcher) else {
        return CheckResult::Mismatch;
    };
    match run_bounded_child(command, CANARY_TIMEOUT) {
        ChildOutcome::Success { stdout, stderr } => {
            validate_probe_sentinel(probe, &stdout, &stderr)
        }
        ChildOutcome::Timeout => CheckResult::Timeout,
        ChildOutcome::Nonzero | ChildOutcome::SpawnFailed => CheckResult::ChildNonzero,
    }
}

fn live_probe_command(agent: &str, probe: LiveProbe, launcher: &Path) -> Option<Command> {
    let cargo = resolve_cargo_launcher()?;
    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let mut command = Command::new(cargo);
    command
        .current_dir(repo_root)
        .env(probe.env_name, "1")
        .env(LIVE_LAUNCHER_AGENT_ENV, agent)
        .env(LIVE_LAUNCHER_ENV, launcher)
        .args([
            "test",
            "--locked",
            "-p",
            "gate4agent-runtime-native",
            "--test",
            probe.test_target,
            probe.test_name,
            "--",
            "--ignored",
            "--exact",
            "--nocapture",
        ]);
    Some(command)
}

fn resolve_cargo_launcher() -> Option<PathBuf> {
    let path = env::var_os("PATH")?;
    #[cfg(windows)]
    let file_name = "cargo.exe";
    #[cfg(not(windows))]
    let file_name = "cargo";
    env::split_paths(&path)
        .map(|directory| directory.join(file_name))
        .find(|candidate| candidate.is_file())?
        .canonicalize()
        .ok()
}

fn live_probe_is_allowlisted(probe: LiveProbe) -> bool {
    matches!(
        (probe.test_target, probe.test_name, probe.env_name),
        (
            "vendor_pty_live",
            "windows_live_claude_pty_contract",
            LIVE_PTY_CANARY_ENV
        ) | (
            "vendor_pty_live",
            "windows_live_codex_pty_contract",
            LIVE_PTY_CANARY_ENV
        ) | (
            "vendor_pty_live",
            "windows_live_codex_whole_chunk_transport_contract",
            LIVE_PTY_CANARY_ENV
        ) | (
            "vendor_pty_live",
            "windows_live_kimi_pty_contract",
            LIVE_PTY_CANARY_ENV
        ) | (
            "vendor_cli_live",
            "windows_live_claude_inline_contract",
            LIVE_PIPE_CANARY_ENV
        ) | (
            "vendor_cli_live",
            "windows_live_codex_inline_contract",
            LIVE_PIPE_CANARY_ENV
        ) | (
            "vendor_cli_live",
            "windows_live_kimi_inline_contract",
            LIVE_PIPE_CANARY_ENV
        )
    )
}

fn validate_probe_sentinel(
    probe: LiveProbe,
    stdout: &CapturedOutput,
    stderr: &CapturedOutput,
) -> CheckResult {
    if stdout.overflowed || stderr.overflowed {
        return CheckResult::Mismatch;
    }
    let Ok(stdout) = std::str::from_utf8(&stdout.bytes) else {
        return CheckResult::Mismatch;
    };
    let prefix = format!("{} ", probe.sentinel_prefix);
    let sentinels = stdout
        .lines()
        .filter(|line| line.starts_with(&prefix))
        .collect::<Vec<_>>();
    let [sentinel] = sentinels.as_slice() else {
        return if sentinels.is_empty() {
            CheckResult::Missing
        } else {
            CheckResult::Mismatch
        };
    };
    let mut fields = BTreeMap::new();
    for field in sentinel[prefix.len()..].split_ascii_whitespace() {
        let Some((key, value)) = field.split_once('=') else {
            return CheckResult::Mismatch;
        };
        if !valid_sentinel_component(key)
            || !valid_sentinel_component(value)
            || fields.insert(key, value).is_some()
        {
            return CheckResult::Mismatch;
        }
    }
    if probe
        .required_fields
        .iter()
        .all(|(key, expected)| fields.get(key).copied() == Some(*expected))
    {
        CheckResult::Pass
    } else {
        CheckResult::Mismatch
    }
}

fn valid_sentinel_component(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
        })
}

impl Report {
    fn check_result(&self, id: &str) -> Option<CheckResult> {
        self.checks
            .iter()
            .find_map(|(check, result)| (check == id).then_some(*result))
    }

    fn transport_checks_pass(&self, transport: &TransportContract) -> bool {
        !transport.capabilities.is_empty()
            && transport.capabilities.iter().all(|capability| {
                self.check_result(capability) == Some(CheckResult::Pass)
            })
    }

    fn transport_verdict_json(&self, scope: TransportScope) -> String {
        let Some(transport) = self.contract.and_then(|contract| {
            contract
                .transports
                .iter()
                .find(|candidate| candidate.transport == scope)
        }) else {
            return format!(
                "{{\"transport\":\"{}\",\"coverage\":\"none\",\"verdict\":\"unverified\",\"selectable\":false,\"reason\":\"unrecognized-contract\"}}",
                scope.label(),
            );
        };
        if self.mode == "fixture" {
            return format!(
                "{{\"transport\":\"{}\",\"coverage\":\"fixture\",\"verdict\":\"unverified\",\"selectable\":false,\"reason\":\"fixture-evidence-only\"}}",
                scope.label(),
            );
        }
        if transport.live_probes.is_empty() {
            return format!(
                "{{\"transport\":\"{}\",\"coverage\":\"none\",\"verdict\":\"unverified\",\"selectable\":false,\"reason\":\"no-live-canary\"}}",
                scope.label(),
            );
        }
        if self.check_result("launcher.file_stable") != Some(CheckResult::Pass) {
            return format!(
                "{{\"transport\":\"{}\",\"coverage\":\"attempted\",\"verdict\":\"unverified\",\"selectable\":false,\"reason\":\"launcher-file-not-stable\"}}",
                scope.label(),
            );
        }
        if !self.transport_checks_pass(transport) {
            return format!(
                "{{\"transport\":\"{}\",\"coverage\":\"attempted\",\"verdict\":\"unverified\",\"selectable\":false,\"reason\":\"required-check-not-pass\"}}",
                scope.label(),
            );
        }
        if self.check_result("launcher.chain_pinned") != Some(CheckResult::Pass) {
            return format!(
                "{{\"transport\":\"{}\",\"coverage\":\"observed\",\"verdict\":\"verified-behavior\",\"selectable\":false,\"reason\":\"launcher-chain-not-pinned\"}}",
                scope.label(),
            );
        }
        if !transport.exact_launch_injected {
            return format!(
                "{{\"transport\":\"{}\",\"coverage\":\"observed\",\"verdict\":\"unverified\",\"selectable\":false,\"reason\":\"exact-launch-not-injected\"}}",
                scope.label(),
            );
        }
        format!(
            "{{\"transport\":\"{}\",\"coverage\":\"verified\",\"verdict\":\"verified\",\"selectable\":true,\"reason\":\"exact-launch-live-canary-pass\"}}",
            scope.label(),
        )
    }

    fn verification_succeeded(&self) -> bool {
        let Some(contract) = self.contract else {
            return false;
        };
        if self.version.is_none()
            || self.check_result("version_exact") != Some(CheckResult::Pass)
        {
            return false;
        }
        if self.mode == "fixture" {
            return contract
                .transports
                .iter()
                .any(|transport| self.transport_checks_pass(transport));
        }
        contract.transports.iter().any(|transport| {
            transport.exact_launch_injected
                && self.check_result("launcher.file_stable") == Some(CheckResult::Pass)
                && self.check_result("launcher.chain_pinned") == Some(CheckResult::Pass)
                && self.transport_checks_pass(transport)
        })
    }

    fn to_json(&self) -> String {
        let contract_id = self.contract.map_or("unrecognized", |contract| contract.id);
        let fingerprint = self
            .contract
            .map(capability_fingerprint)
            .unwrap_or_else(|| "none".to_owned());
        let version = self.version.as_deref().map_or("null".to_owned(), |version| {
            format!("\"{}\"", escape_json(version))
        });
        let checks = self
            .checks
            .iter()
            .map(|(id, result)| {
                format!(
                    "{{\"id\":\"{}\",\"result\":\"{}\"}}",
                    escape_json(id),
                    result.label()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let transport_verdicts = [TransportScope::Inline, TransportScope::Pty]
            .into_iter()
            .map(|transport| self.transport_verdict_json(transport))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{{\"schema\":\"gate4agent.vendor-contract-verification.v3\",\"contract_id\":\"{}\",\"spec_revision\":\"{}\",\"agent\":\"{}\",\"platform\":\"{}\",\"version\":{},\"mode\":\"{}\",\"capability_fingerprint\":\"{}\",\"checks\":[{}],\"transport_verdicts\":[{}]}}\n",
            contract_id,
            SPEC_REVISION,
            escape_json(&self.agent),
            escape_json(&self.platform),
            version,
            self.mode,
            fingerprint,
            checks,
            transport_verdicts,
        )
    }
}

fn capability_fingerprint(contract: &Contract) -> String {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in SPEC_REVISION
        .bytes()
        .chain([0].into_iter())
        .chain(contract.id.bytes())
        .chain([0].into_iter())
        .chain(contract_capabilities(contract).flat_map(|capability| {
            capability.bytes().chain([0].into_iter())
        }))
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn escape_json(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            value if value.is_control() => escaped.push_str("\\uFFFD"),
            value => escaped.push(value),
        }
    }
    escaped
}

fn write_report(path: &Path, report: &Report) -> Result<(), &'static str> {
    let json = report.to_json();
    if json.len() > MAX_REPORT_BYTES {
        return Err("sanitized report exceeded its bound");
    }
    let output_path = prepare_output_path(path)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&output_path)
        .map_err(|_| "report output already exists or cannot be created")?;
    output
        .write_all(json.as_bytes())
        .and_then(|_| output.sync_all())
        .map_err(|_| "sanitized report cannot be written")
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("runtime-native crate has a repository root")
        .to_path_buf()
}

fn output_root() -> PathBuf {
    repo_root().join("target/vendor-contract-lab")
}

fn validate_output_path(path: &Path) -> Result<PathBuf, &'static str> {
    let output = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root().join(path)
    };
    if output.parent() != Some(output_root().as_path()) {
        return Err("--out must be directly under target/vendor-contract-lab");
    }
    let Some(file_name) = output.file_name().and_then(|name| name.to_str()) else {
        return Err("--out file name is invalid");
    };
    if !file_name.ends_with(".json")
        || file_name.starts_with('.')
        || !file_name.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
        })
    {
        return Err("--out file name must be a sanitized JSON name");
    }
    Ok(output)
}

fn prepare_output_path(path: &Path) -> Result<PathBuf, &'static str> {
    let output = validate_output_path(path)?;
    let target = repo_root().join("target");
    let target_metadata = fs::symlink_metadata(&target)
        .map_err(|_| "repository target directory is missing")?;
    if !target_metadata.is_dir() || target_metadata.file_type().is_symlink() {
        return Err("repository target directory is not a physical directory");
    }
    let root = output_root();
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => return Err("report directory is not a physical directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&root).map_err(|_| "report directory cannot be created")?;
        }
        Err(_) => return Err("report directory cannot be inspected"),
    }
    let canonical_root = root
        .canonicalize()
        .map_err(|_| "report directory cannot be resolved")?;
    let canonical_parent = output
        .parent()
        .and_then(|parent| parent.canonicalize().ok())
        .ok_or("report parent cannot be resolved")?;
    if canonical_parent != canonical_root {
        return Err("report path escaped its bounded directory");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_path(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/vendor_contract_lab")
            .join(name)
    }

    #[test]
    fn vendor_contract_lab_normalizes_one_strict_version_and_rejects_ambiguity() {
        assert_eq!(
            normalize_version(b"2.1.224 (Claude Code)\n", b""),
            VersionOutcome::Exact("2.1.224".to_owned())
        );
        assert_eq!(
            normalize_version(b"codex-cli 0.144.6\n", b"release 0.144.5\n"),
            VersionOutcome::Ambiguous
        );
        assert_eq!(
            normalize_version(b"kimi 0.31.1-beta\n", b""),
            VersionOutcome::Missing
        );
        assert_eq!(
            normalize_version(b"claude 02.1.224\n", b""),
            VersionOutcome::Missing
        );
        assert_eq!(
            normalize_version(b"x2.1.224\n", b""),
            VersionOutcome::Missing
        );
    }

    #[cfg(windows)]
    #[test]
    fn vendor_contract_lab_windows_launcher_resolution_and_nested_cargo_are_exact() {
        fs::create_dir_all(output_root()).expect("create bounded resolver test root");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = output_root().join(format!(
            "resolver-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&directory).expect("create resolver fixture directory");
        let cmd = directory.join("audit-agent.cmd");
        let exe = directory.join("audit-agent.exe");
        fs::write(&exe, b"native-executable").expect("write executable fixture");
        fs::write(&cmd, b"@echo off\r\n").expect("write command shim fixture");
        let search_path = env::join_paths([directory.as_path()]).expect("fixture PATH");
        let resolved = resolve_windows_launcher_on_path("audit-agent", &search_path)
            .expect("resolve exact command shim");
        assert_eq!(
            resolved.path.canonicalize().expect("resolved path"),
            cmd.canonicalize().expect("command shim path")
        );
        assert_eq!(launcher_chain_result(&resolved.path), CheckResult::Missing);
        assert_eq!(launcher_chain_result(&exe), CheckResult::Pass);
        let command = provider_version_command(&resolved.path).expect("version command");
        assert_eq!(command.get_program(), "cmd.exe");
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("/D"),
                std::ffi::OsStr::new("/Q"),
                std::ffi::OsStr::new("/C"),
                resolved.path.as_os_str(),
                std::ffi::OsStr::new("--version"),
            ]
        );
        let nested = live_probe_command(
            "kimi",
            KIMI_0_31_1_INLINE_PROBES[0],
            &resolved.path,
        )
        .expect("build nested Cargo command");
        assert!(Path::new(nested.get_program()).is_absolute());
        assert!(nested
            .get_args()
            .any(|argument| argument == std::ffi::OsStr::new("--locked")));
        assert!(!nested.get_envs().any(|(key, _)| {
            key.to_str()
                .is_some_and(|key| key.eq_ignore_ascii_case("PATH"))
        }));
        let before = resolved.snapshot;
        assert_eq!(
            before.canonical_path,
            cmd.canonicalize().expect("canonical command shim path")
        );
        fs::write(&cmd, b"@echo changed\r\n").expect("mutate command shim fixture");
        let after = snapshot_launcher(&cmd).expect("snapshot mutated command shim");
        assert_ne!(before.content_fingerprint, after.content_fingerprint);
        assert_ne!(before, after);
        fs::remove_file(&cmd).expect("remove command shim fixture");
        fs::remove_file(&exe).expect("remove executable fixture");
        fs::remove_dir(&directory).expect("remove resolver fixture directory");
    }

    #[test]
    fn vendor_contract_lab_fixture_pass_is_valid_but_never_selectable() {
        let report = verify_fixture(
            "claude",
            &fixture_path("claude-2.1.224-pass.fixture"),
        )
        .expect("valid Claude fixture");
        assert!(report.verification_succeeded());
        let json = report.to_json();
        assert!(json.contains("\"contract_id\":\"claude.windows-x86_64.2.1.224\""));
        assert_eq!(json.matches("\"selectable\":false").count(), 2);
        assert!(!json.contains("\"selectable\":true"));
        assert_eq!(json.matches("\"reason\":\"fixture-evidence-only\"").count(), 2);
        assert!(!json.contains("stdout"));
        assert!(!json.contains("stderr"));
        assert!(!json.contains("ANTHROPIC_AUTH_TOKEN"));
        assert!(!json.contains("Reply with one string only"));
        assert!(!json.contains(env!("CARGO_MANIFEST_DIR")));
        assert!(json.len() <= MAX_REPORT_BYTES);
    }

    #[test]
    fn vendor_contract_lab_auth_gate_and_missing_check_fail_closed() {
        let codex = verify_fixture(
            "codex",
            &fixture_path("codex-0.144.6-auth-gated.fixture"),
        )
        .expect("valid auth-gated Codex fixture");
        assert!(codex.to_json().contains("\"result\":\"auth-gated\""));
        assert!(!codex.to_json().contains("\"selectable\":true"));

        let kimi = verify_fixture(
            "kimi",
            &fixture_path("kimi-0.31.1-unverified.fixture"),
        )
        .expect("valid unverified Kimi fixture");
        assert!(kimi.to_json().contains("\"result\":\"missing\""));
        assert!(!kimi.to_json().contains("\"selectable\":true"));
    }

    #[test]
    fn vendor_contract_lab_manifest_selection_is_exact() {
        assert!(platform_is_supported("windows-x86_64"));
        assert!(!platform_is_supported("windows-aarch64"));
        assert!(!platform_is_supported("linux-x86_64"));
        assert!(!platform_is_supported("macos-aarch64"));
        assert!(contract_for("claude", "windows-x86_64", "2.1.223").is_some());
        assert!(contract_for("claude", "windows-x86_64", "2.1.224").is_some());
        assert!(contract_for("claude", "windows-x86_64", "2.1.225").is_none());
        assert!(contract_for("claude", "linux-x86_64", "2.1.224").is_none());
        assert!(contract_for("codex", "windows-x86_64", "0.144.6").is_some());
        assert!(contract_for("kimi", "windows-x86_64", "0.31.1").is_some());
    }

    #[test]
    fn vendor_contract_lab_transport_verdicts_are_scoped_and_fail_closed() {
        let contract = contract_for("codex", "windows-x86_64", "0.144.6")
            .expect("known Codex contract");
        let passing_checks = std::iter::once(("version_exact".to_owned(), CheckResult::Pass))
            .chain(std::iter::once((
                "launcher.file_stable".to_owned(),
                CheckResult::Pass,
            )))
            .chain(std::iter::once((
                "launcher.chain_pinned".to_owned(),
                CheckResult::Pass,
            )))
            .chain(contract_capabilities(contract).map(|capability| {
                (capability.to_owned(), CheckResult::Pass)
            }))
            .collect::<Vec<_>>();
        let live = Report {
            contract: Some(contract),
            agent: contract.agent.to_owned(),
            platform: contract.platform.to_owned(),
            version: Some(contract.version.to_owned()),
            mode: "live",
            checks: passing_checks.clone(),
        };
        let live_json = live.to_json();
        assert!(live_json.contains("\"transport\":\"pty\",\"coverage\":\"verified\",\"verdict\":\"verified\",\"selectable\":true"));
        assert!(live_json.contains("\"transport\":\"inline\",\"coverage\":\"observed\",\"verdict\":\"unverified\",\"selectable\":false,\"reason\":\"exact-launch-not-injected\""));
        let mut shim_checks = passing_checks.clone();
        shim_checks
            .iter_mut()
            .find(|(id, _)| id == "launcher.chain_pinned")
            .expect("launcher-chain check")
            .1 = CheckResult::Missing;
        let shim = Report {
            contract: Some(contract),
            agent: contract.agent.to_owned(),
            platform: contract.platform.to_owned(),
            version: Some(contract.version.to_owned()),
            mode: "live",
            checks: shim_checks,
        };
        let shim_json = shim.to_json();
        assert_eq!(shim_json.matches("\"verdict\":\"verified-behavior\"").count(), 2);
        assert_eq!(shim_json.matches("\"reason\":\"launcher-chain-not-pinned\"").count(), 2);
        assert!(!shim_json.contains("\"selectable\":true"));
        for result in [
            CheckResult::Mismatch,
            CheckResult::Missing,
            CheckResult::AuthGated,
            CheckResult::Timeout,
            CheckResult::ChildNonzero,
        ] {
            let mut checks = passing_checks.clone();
            let pty_auth = checks
                .iter_mut()
                .find(|(id, _)| id == "pty.auth_gate")
                .expect("PTY auth check");
            pty_auth.1 = result;
            let report = Report {
                contract: Some(contract),
                agent: contract.agent.to_owned(),
                platform: contract.platform.to_owned(),
                version: Some(contract.version.to_owned()),
                mode: "live",
                checks,
            };
            let json = report.to_json();
            assert!(json.contains(&format!("\"result\":\"{}\"", result.label())));
            assert!(json.contains("\"transport\":\"pty\",\"coverage\":\"attempted\",\"verdict\":\"unverified\",\"selectable\":false"));
        }

        let unknown = Report {
            contract: None,
            agent: "codex".to_owned(),
            platform: "windows-x86_64".to_owned(),
            version: Some("0.144.7".to_owned()),
            mode: "fixture",
            checks: vec![("version_exact".to_owned(), CheckResult::Mismatch)],
        };
        let unknown_json = unknown.to_json();
        assert!(unknown_json.contains("\"contract_id\":\"unrecognized\""));
        assert_eq!(unknown_json.matches("\"selectable\":false").count(), 2);
    }

    #[test]
    fn vendor_contract_lab_live_probe_requires_one_exact_sentinel() {
        let probe = KIMI_0_31_1_INLINE_PROBES[0];
        assert_eq!(probe.env_name, LIVE_PIPE_CANARY_ENV);
        assert!(live_probe_is_allowlisted(probe));
        let mut typo = probe;
        typo.test_name = "windows_live_kimi_inline_contract_typo";
        assert!(!live_probe_is_allowlisted(typo));
        let mut wrong_env = probe;
        wrong_env.env_name = LIVE_PTY_CANARY_ENV;
        assert!(!live_probe_is_allowlisted(wrong_env));
        let line = b"vendor_canary agent=kimi transport=pipe fresh_exit_code=0 resume_exit_code=0 completed_turns=2 marker_observed=true same_provider_session=true generation=1 isolated_cwd=true active_sessions=0\n";
        let captured = CapturedOutput {
            bytes: line.to_vec(),
            overflowed: false,
        };
        assert_eq!(
            validate_probe_sentinel(probe, &captured, &CapturedOutput::default()),
            CheckResult::Pass
        );

        let missing = CapturedOutput {
            bytes: b"test result: ok\n".to_vec(),
            overflowed: false,
        };
        assert_eq!(
            validate_probe_sentinel(probe, &missing, &CapturedOutput::default()),
            CheckResult::Missing
        );
        let duplicate = CapturedOutput {
            bytes: [line.as_slice(), line.as_slice()].concat(),
            overflowed: false,
        };
        assert_eq!(
            validate_probe_sentinel(probe, &duplicate, &CapturedOutput::default()),
            CheckResult::Mismatch
        );
        let mismatch = CapturedOutput {
            bytes: line
                .windows("resume_exit_code=0".len())
                .position(|window| window == b"resume_exit_code=0")
                .map(|index| {
                    let mut changed = line.to_vec();
                    changed[index + "resume_exit_code=".len()] = b'1';
                    changed
                })
                .expect("sentinel field"),
            overflowed: false,
        };
        assert_eq!(
            validate_probe_sentinel(probe, &mismatch, &CapturedOutput::default()),
            CheckResult::Mismatch
        );
        let overflow = CapturedOutput {
            bytes: line.to_vec(),
            overflowed: true,
        };
        assert_eq!(
            validate_probe_sentinel(probe, &overflow, &CapturedOutput::default()),
            CheckResult::Mismatch
        );
        assert!(read_bounded(std::io::Cursor::new(vec![b'x'; 9]), 8).overflowed);
    }

    #[test]
    fn vendor_contract_lab_output_path_is_bounded_to_report_directory() {
        assert_eq!(
            validate_output_path(Path::new(
                "target/vendor-contract-lab/claude-verification.json"
            ))
            .expect("bounded report path"),
            output_root().join("claude-verification.json")
        );
        assert!(validate_output_path(Path::new("target/outside.json")).is_err());
        assert!(validate_output_path(Path::new(
            "target/vendor-contract-lab/../escaped.json"
        ))
        .is_err());
        assert!(validate_output_path(Path::new(
            "target/vendor-contract-lab/nested/report.json"
        ))
        .is_err());
        assert!(validate_output_path(Path::new(
            "target/vendor-contract-lab/report.txt"
        ))
        .is_err());
    }

    #[test]
    fn vendor_contract_lab_report_create_new_never_overwrites_existing_output() {
        let report = verify_fixture(
            "claude",
            &fixture_path("claude-2.1.224-pass.fixture"),
        )
        .expect("valid Claude fixture");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let relative = PathBuf::from(format!(
            "target/vendor-contract-lab/exclusive-{}-{nonce}.json",
            std::process::id()
        ));
        let absolute = repo_root().join(&relative);
        write_report(&relative, &report).expect("first exclusive report write");
        let original = fs::read(&absolute).expect("read exclusive report");
        assert!(write_report(&relative, &report).is_err());
        assert_eq!(
            fs::read(&absolute).expect("read report after rejected overwrite"),
            original
        );
        fs::remove_file(&absolute).expect("remove exclusive report fixture");
    }

    #[cfg(windows)]
    #[test]
    fn vendor_contract_lab_timeout_kills_descendant_holding_capture_pipe() {
        fs::create_dir_all(output_root()).expect("create bounded test output directory");
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let marker = output_root().join(format!(
            "descendant-{}-{nonce}.marker",
            std::process::id()
        ));
        let parent_script = r#"$childScript = 'Start-Sleep -Seconds 3; [IO.File]::WriteAllText($env:GATE4AGENT_VENDOR_CONTRACT_READER_MARKER, ''survived'')'; & powershell.exe -NoLogo -NoProfile -NonInteractive -Command $childScript"#;
        let mut command = Command::new("powershell.exe");
        command
            .args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Command", parent_script])
            .env("GATE4AGENT_VENDOR_CONTRACT_READER_MARKER", &marker);
        let started = Instant::now();
        assert!(matches!(
            run_bounded_child(command, Duration::from_millis(100)),
            ChildOutcome::Timeout
        ));
        assert!(started.elapsed() < Duration::from_secs(8));
        thread::sleep(Duration::from_millis(3_500));
        let descendant_survived = marker.exists();
        if descendant_survived {
            let _ = fs::remove_file(&marker);
        }
        assert!(!descendant_survived, "timed-out descendant survived tree cleanup");
    }
}

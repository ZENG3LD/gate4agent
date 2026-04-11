//! CLI tool probe and capability discovery with caching.
//!
//! `probe_all()` checks which CLI tools are installed on PATH, reads their
//! config files, and returns a `ProbeResult` with per-CLI capabilities.
//! Results are cached to `~/.gate4agent/probe-cache.json` for 1 hour.

use serde::{Deserialize, Serialize};
use crate::core::types::CliTool;
use crate::core::capabilities::CliCapabilities;

/// One CLI tool's probe result.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliProbe {
    pub tool: CliTool,
    /// Binary found on PATH at probe time.
    pub installed: bool,
    /// Capability descriptor (enriched from config files when installed).
    pub capabilities: CliCapabilities,
}

/// Result of probing all four CLI tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProbeResult {
    /// One entry per CLI tool, in stable order.
    pub probes: Vec<CliProbe>,
    /// UTC timestamp of this probe (Unix seconds).
    pub probed_at: i64,
}

impl ProbeResult {
    /// Find the probe for a specific tool.
    pub fn for_tool(&self, tool: CliTool) -> Option<&CliProbe> {
        self.probes.iter().find(|p| p.tool == tool)
    }

    /// All installed CLIs.
    pub fn installed(&self) -> Vec<&CliProbe> {
        self.probes.iter().filter(|p| p.installed).collect()
    }

    /// Whether at least one CLI is installed.
    pub fn any_installed(&self) -> bool {
        self.probes.iter().any(|p| p.installed)
    }
}

const DEFAULT_MAX_AGE_SECS: i64 = 3600;

/// Probe all four CLIs. Uses cache if fresh (< 1 hour).
pub fn probe_all() -> ProbeResult {
    probe_with_max_age(DEFAULT_MAX_AGE_SECS)
}

/// Probe all four CLIs, ignoring cache.
pub fn probe_force() -> ProbeResult {
    let result = do_probe();
    let _ = write_cache(&result);
    result
}

/// Load cached probe without re-probing. Returns None if no cache or parse error.
pub fn load_cached_probe() -> Option<ProbeResult> {
    let path = cache_path()?;
    let content = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&content).ok()
}

fn probe_with_max_age(max_age_secs: i64) -> ProbeResult {
    if let Some(cached) = load_cached_probe() {
        let now = chrono::Utc::now().timestamp();
        if now - cached.probed_at < max_age_secs {
            return cached;
        }
    }
    let result = do_probe();
    let _ = write_cache(&result);
    result
}

fn do_probe() -> ProbeResult {
    let tools = [
        CliTool::ClaudeCode,
        CliTool::Codex,
        CliTool::Gemini,
        CliTool::OpenCode,
    ];

    let probes = tools.iter().map(|&tool| {
        let binary = tool.capabilities().binary.clone();
        let installed = binary_on_path(&binary);
        let capabilities = if installed {
            tool.discover_capabilities()
        } else {
            tool.capabilities()
        };
        CliProbe { tool, installed, capabilities }
    }).collect();

    ProbeResult {
        probes,
        probed_at: chrono::Utc::now().timestamp(),
    }
}

fn cache_path() -> Option<std::path::PathBuf> {
    let home = crate::utils::home_dir()?;
    Some(home.join(".gate4agent").join("probe-cache.json"))
}

fn write_cache(result: &ProbeResult) -> std::io::Result<()> {
    let path = cache_path().ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "no home dir")
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(result)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json.as_bytes())?;
    std::fs::rename(tmp, path)?;
    Ok(())
}

/// Check if a binary exists on PATH without executing it.
fn binary_on_path(name: &str) -> bool {
    let path_var = std::env::var_os("PATH").unwrap_or_default();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() { return true; }
        // Windows: check .exe and .cmd suffixes
        if cfg!(windows) {
            for ext in &["exe", "cmd", "bat"] {
                let with_ext = dir.join(format!("{name}.{ext}"));
                if with_ext.is_file() { return true; }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_result_for_tool_finds_entry() {
        let result = ProbeResult {
            probes: vec![
                CliProbe { tool: CliTool::ClaudeCode, installed: true, capabilities: CliTool::ClaudeCode.capabilities() },
                CliProbe { tool: CliTool::Codex, installed: false, capabilities: CliTool::Codex.capabilities() },
            ],
            probed_at: 0,
        };
        assert!(result.for_tool(CliTool::ClaudeCode).is_some());
        assert!(result.for_tool(CliTool::Codex).is_some());
        assert!(result.for_tool(CliTool::Gemini).is_none());
    }

    #[test]
    fn installed_filters_correctly() {
        let result = ProbeResult {
            probes: vec![
                CliProbe { tool: CliTool::ClaudeCode, installed: true, capabilities: CliTool::ClaudeCode.capabilities() },
                CliProbe { tool: CliTool::Codex, installed: false, capabilities: CliTool::Codex.capabilities() },
                CliProbe { tool: CliTool::Gemini, installed: true, capabilities: CliTool::Gemini.capabilities() },
            ],
            probed_at: 0,
        };
        assert_eq!(result.installed().len(), 2);
        assert!(result.any_installed());
    }

    #[test]
    fn binary_on_path_returns_false_for_nonexistent() {
        assert!(!binary_on_path("this_binary_definitely_does_not_exist_xyz_12345"));
    }

    #[test]
    fn cache_roundtrip() {
        let result = ProbeResult {
            probes: vec![
                CliProbe { tool: CliTool::ClaudeCode, installed: true, capabilities: CliTool::ClaudeCode.capabilities() },
            ],
            probed_at: 1234567890,
        };
        let json = serde_json::to_string_pretty(&result).unwrap();
        let parsed: ProbeResult = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.probes.len(), 1);
        assert_eq!(parsed.probed_at, 1234567890);
        assert!(parsed.probes[0].installed);
    }

    #[test]
    fn do_probe_returns_four_entries() {
        let result = do_probe();
        assert_eq!(result.probes.len(), 4);
        assert!(result.probed_at > 0);
    }
}

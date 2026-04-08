//! Rate limit detection from terminal output.

use chrono::Utc;
use regex::Regex;

use crate::types::{CliTool, RateLimitInfo, RateLimitType};

/// Detects rate limits from CLI output.
pub struct RateLimitDetector {
    /// Active patterns for this detector instance.
    patterns: Vec<RateLimitPattern>,
}

struct RateLimitPattern {
    regex: Regex,
    limit_type: RateLimitType,
}

impl RateLimitDetector {
    /// Create a detector that runs all patterns for all tools.
    pub fn new() -> Self {
        let mut patterns = Self::build_claude_patterns();
        patterns.extend(Self::build_codex_patterns());
        Self { patterns }
    }

    /// Create a detector scoped to a single tool, avoiding false positives
    /// from other tools' patterns firing on unrelated output.
    pub fn new_for_tool(tool: CliTool) -> Self {
        let patterns = match tool {
            CliTool::ClaudeCode => Self::build_claude_patterns(),
            CliTool::Codex => Self::build_codex_patterns(),
            CliTool::Gemini => Self::build_gemini_patterns(),
            // Phase 3: Cursor, OpenCode, OpenClaw will get their own pattern builders
            // once real CLI output has been captured and rate-limit message formats confirmed.
            // For now use empty pattern sets — no false positives, no detections.
            CliTool::Cursor | CliTool::OpenCode | CliTool::OpenClaw => vec![],
        };
        Self { patterns }
    }

    fn build_claude_patterns() -> Vec<RateLimitPattern> {
        vec![
            RateLimitPattern {
                regex: Regex::new(r"(?i)rate\s*limit|usage\s*limit|too\s*many\s*requests")
                    .expect("valid regex"),
                limit_type: RateLimitType::Unknown,
            },
            RateLimitPattern {
                regex: Regex::new(r"(?i)session\s*limit|hourly\s*limit|5[- ]?hour")
                    .expect("valid regex"),
                limit_type: RateLimitType::Session,
            },
            RateLimitPattern {
                regex: Regex::new(r"(?i)daily\s*limit|24[- ]?hour").expect("valid regex"),
                limit_type: RateLimitType::Daily,
            },
            RateLimitPattern {
                regex: Regex::new(r"(?i)weekly\s*limit|7[- ]?day").expect("valid regex"),
                limit_type: RateLimitType::Weekly,
            },
        ]
    }

    fn build_codex_patterns() -> Vec<RateLimitPattern> {
        vec![RateLimitPattern {
            regex: Regex::new(r"(?i)rate\s*limit|quota|exceeded").expect("valid regex"),
            limit_type: RateLimitType::Unknown,
        }]
    }

    fn build_gemini_patterns() -> Vec<RateLimitPattern> {
        vec![RateLimitPattern {
            regex: Regex::new(r"(?i)rate\s*limit|quota\s*exceeded|resource\s*exhausted")
                .expect("valid regex"),
            limit_type: RateLimitType::Unknown,
        }]
    }

    /// Detect rate limit from an output line.
    pub fn detect(&self, line: &str) -> Option<RateLimitInfo> {
        for pattern in &self.patterns {
            if pattern.regex.is_match(line) {
                return Some(RateLimitInfo {
                    limit_type: pattern.limit_type,
                    resets_at: None,
                    usage_percent: None,
                    raw_message: line.to_string(),
                    detected_at: Utc::now(),
                });
            }
        }
        None
    }

    /// Detect from multiple lines.
    pub fn detect_all(&self, lines: &[String]) -> Vec<RateLimitInfo> {
        lines.iter().filter_map(|line| self.detect(line)).collect()
    }
}

impl Default for RateLimitDetector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_rate_limit() {
        let detector = RateLimitDetector::new();
        let result = detector.detect("Error: rate limit exceeded. Please wait.");
        assert!(result.is_some());
    }

    #[test]
    fn no_false_positive_on_clean_output() {
        let detector = RateLimitDetector::new();
        let result = detector.detect("Building artifacts for user...");
        assert!(result.is_none());
    }

    #[test]
    fn tool_scoped_detector_does_not_mix() {
        let claude_detector = RateLimitDetector::new_for_tool(CliTool::ClaudeCode);
        assert!(claude_detector.detect("session limit reached").is_some());
    }

    #[test]
    fn detects_session_limit_type() {
        let detector = RateLimitDetector::new_for_tool(CliTool::ClaudeCode);
        let info = detector.detect("You have hit your session limit for today").unwrap();
        assert_eq!(info.limit_type, RateLimitType::Session);
    }

    #[test]
    fn detects_daily_limit_type() {
        let detector = RateLimitDetector::new_for_tool(CliTool::ClaudeCode);
        let info = detector.detect("daily limit exceeded").unwrap();
        assert_eq!(info.limit_type, RateLimitType::Daily);
    }
}

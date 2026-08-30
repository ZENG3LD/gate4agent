//! Rate limit detection from terminal output.
//!
//! Two kinds of signal are recognized:
//!
//! - **Failure** — the provider has already refused a request (`"rate
//!   limit exceeded"` and similar). This fires at the moment of refusal
//!   and carries no quota numbers, because the refusal message itself
//!   never contains any.
//! - **Quota state** — a live budget reading the provider prints on its
//!   own initiative (currently only codex's `/status` screen: `"5h
//!   limit: [...] N% left (resets HH:MM)"`). This is OPPORTUNISTIC: the
//!   line only appears in the frame right after `/status` was requested,
//!   never on a steady cadence, so seeing it is a matter of catching a
//!   frame that happens to carry it — not something this detector can
//!   solicit on its own.

use chrono::{DateTime, Datelike, Local, LocalResult, NaiveDate, TimeZone, Utc};
use regex::Regex;

use crate::core::types::{CliTool, RateLimitInfo, RateLimitType};

/// Detects rate limits from CLI output.
pub struct RateLimitDetector {
    /// Active patterns for this detector instance.
    patterns: Vec<RateLimitPattern>,
}

enum RateLimitPattern {
    /// Fires the instant a provider's refusal text is seen. Only the
    /// limit's coarse type is known; no quota numbers are available.
    Failure { regex: Regex, limit_type: RateLimitType },
    /// Fires on a live quota-state line and extracts limit name, percent
    /// remaining, and reset time from it.
    QuotaState { regex: Regex },
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
            CliTool::KimiCode => Self::build_kimi_patterns(),
            // Unreachable for Grok, but NOT because Grok lacks a PTY: it
            // runs over one every day, and its catalog entry declares
            // `pty: true`. What it lacks is a `pty_adapter`, so it is
            // spawned through the catalog path and never through the legacy
            // `CliTool`-keyed one this detector belongs to. Its rate-limit
            // patterns still need capturing -- Grok prints remaining quota
            // in its status bar on frames the node already collects.
            CliTool::Grok => vec![],
        };
        Self { patterns }
    }

    fn build_claude_patterns() -> Vec<RateLimitPattern> {
        vec![
            RateLimitPattern::Failure {
                regex: Regex::new(r"(?i)rate\s*limit|usage\s*limit|too\s*many\s*requests")
                    .expect("valid regex"),
                limit_type: RateLimitType::Unknown,
            },
            RateLimitPattern::Failure {
                regex: Regex::new(r"(?i)session\s*limit|hourly\s*limit|5[- ]?hour")
                    .expect("valid regex"),
                limit_type: RateLimitType::Session,
            },
            RateLimitPattern::Failure {
                regex: Regex::new(r"(?i)daily\s*limit|24[- ]?hour").expect("valid regex"),
                limit_type: RateLimitType::Daily,
            },
            RateLimitPattern::Failure {
                regex: Regex::new(r"(?i)weekly\s*limit|7[- ]?day").expect("valid regex"),
                limit_type: RateLimitType::Weekly,
            },
        ]
    }

    fn build_codex_patterns() -> Vec<RateLimitPattern> {
        vec![
            RateLimitPattern::Failure {
                regex: Regex::new(r"(?i)rate\s*limit|quota|exceeded").expect("valid regex"),
                limit_type: RateLimitType::Unknown,
            },
            RateLimitPattern::QuotaState {
                regex: codex_quota_state_regex(),
            },
        ]
    }

    fn build_kimi_patterns() -> Vec<RateLimitPattern> {
        vec![RateLimitPattern::Failure {
            regex: Regex::new(
                r"(?i)rate\s*limit|too\s*many\s*requests|quota|overload|retry-after",
            )
            .expect("valid regex"),
            limit_type: RateLimitType::Unknown,
        }]
    }

    /// Detect rate limit from an output line (or chunk — callers do not
    /// guarantee single-line granularity; PTY reads are chunked
    /// arbitrarily). If a chunk carries more than one quota-state line
    /// (e.g. codex's `/status` prints both `5h` and `Weekly` at once),
    /// only the first is surfaced — same single-result contract this
    /// method has always had.
    pub fn detect(&self, line: &str) -> Option<RateLimitInfo> {
        self.detect_at(line, Utc::now())
    }

    fn detect_at(&self, line: &str, now: DateTime<Utc>) -> Option<RateLimitInfo> {
        // Quota-state lines are checked in a first pass, ahead of every
        // generic failure pattern, regardless of where they sit in
        // `self.patterns`. They are far more specific and strictly more
        // informative than the failure heuristics below, so when both
        // happen to match the same chunk -- codex's `/status` banner says
        // "...information on rate limits and credits" in plain prose
        // right above the actual state lines, which the codex failure
        // pattern's `rate\s*limit` alternative also matches -- the richer
        // fact wins instead of being shadowed by the generic one.
        for pattern in &self.patterns {
            if let RateLimitPattern::QuotaState { regex } = pattern {
                if let Some(info) = parse_codex_quota_state(regex, line, now) {
                    return Some(info);
                }
            }
        }
        for pattern in &self.patterns {
            if let RateLimitPattern::Failure { regex, limit_type } = pattern {
                if regex.is_match(line) {
                    return Some(RateLimitInfo {
                        limit_type: *limit_type,
                        resets_at: None,
                        resets_at_text: None,
                        usage_percent: None,
                        raw_message: line.to_string(),
                        detected_at: now,
                    });
                }
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

/// Matches a codex `/status` quota-state line, e.g.:
///
/// ```text
///  5h limit:             [████████████████████] 100% left (resets 23:40)
///  Weekly limit:         [████████████████████] 100% left (resets 18:40 on 6 Sep)
/// ```
///
/// Capture groups: 1 = limit name (`5h` / `Weekly`), 2 = percent
/// remaining, 3 = the whole reset-time text (verbatim, for
/// `resets_at_text`), 4 = hour, 5 = minute, 6 = optional day-of-month,
/// 7 = optional month name. The progress-bar interior is matched with
/// `[^\]]*` rather than a specific fill glyph, since only `100%`-filled
/// bars have been observed live and partial fills may render with a
/// different glyph than the filled block character.
fn codex_quota_state_regex() -> Regex {
    Regex::new(
        r"(?i)(5h|weekly)\s*limit:\s*\[[^\]]*\]\s*(\d{1,3}(?:\.\d+)?)%\s*left\s*\(resets\s+((\d{1,2}):(\d{2})(?:\s+on\s+(\d{1,2})\s+([A-Za-z]{3,9}))?)\)",
    )
    .expect("valid regex")
}

/// Parse one match of [`codex_quota_state_regex`] into a [`RateLimitInfo`],
/// or `None` if the line does not carry a recognized limit name.
fn parse_codex_quota_state(regex: &Regex, line: &str, now: DateTime<Utc>) -> Option<RateLimitInfo> {
    let caps = regex.captures(line)?;
    let name = caps.get(1)?.as_str().to_ascii_lowercase();
    let limit_type = match name.as_str() {
        "5h" => RateLimitType::Session,
        "weekly" => RateLimitType::Weekly,
        _ => return None,
    };
    let percent_left: f64 = caps.get(2)?.as_str().parse().ok()?;
    // The provider reports quota REMAINING ("N% left"); `usage_percent`
    // documents itself as consumption, so it is stored as the complement.
    let usage_percent = Some((100.0 - percent_left).clamp(0.0, 100.0));
    let resets_at_text = caps.get(3).map(|m| m.as_str().to_owned());
    let hour: u32 = caps.get(4)?.as_str().parse().ok()?;
    let minute: u32 = caps.get(5)?.as_str().parse().ok()?;
    let month_day = match (caps.get(6), caps.get(7)) {
        (Some(day), Some(month_name)) => {
            let day: u32 = day.as_str().parse().ok()?;
            let month = month_number(month_name.as_str())?;
            Some((day, month))
        }
        _ => None,
    };
    let resets_at = resolve_codex_local_reset(hour, minute, month_day, now);
    Some(RateLimitInfo {
        limit_type,
        resets_at,
        resets_at_text,
        usage_percent,
        raw_message: line.to_string(),
        detected_at: now,
    })
}

/// Resolve a codex-printed local reset time into an absolute UTC instant.
///
/// See [`RateLimitInfo::resets_at`] for the interpretation this
/// implements: nearest FUTURE occurrence of the printed wall-clock time
/// (rolling a bare `HH:MM` to tomorrow, or a `D Mon` date to next year,
/// if it has already passed), resolved against this process's own local
/// zone. Returns `None` when the resulting local wall-clock time does not
/// exist on this host (a spring-forward DST gap); an ambiguous one
/// (a fall-back DST repeat) resolves to the earlier of the two instants.
fn resolve_codex_local_reset(
    hour: u32,
    minute: u32,
    month_day: Option<(u32, u32)>,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let now_local_naive = now.with_timezone(&Local).naive_local();
    let candidate_naive = match month_day {
        Some((day, month)) => {
            let this_year = now_local_naive.year();
            let this_year_naive =
                NaiveDate::from_ymd_opt(this_year, month, day)?.and_hms_opt(hour, minute, 0)?;
            if this_year_naive > now_local_naive {
                this_year_naive
            } else {
                NaiveDate::from_ymd_opt(this_year + 1, month, day)?.and_hms_opt(hour, minute, 0)?
            }
        }
        None => {
            let today = now_local_naive.date();
            let today_naive = today.and_hms_opt(hour, minute, 0)?;
            if today_naive > now_local_naive {
                today_naive
            } else {
                today.succ_opt()?.and_hms_opt(hour, minute, 0)?
            }
        }
    };
    match Local.from_local_datetime(&candidate_naive) {
        LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
        LocalResult::Ambiguous(earlier, _later) => Some(earlier.with_timezone(&Utc)),
        LocalResult::None => None,
    }
}

/// Three-letter (or longer) English month abbreviation to month number.
fn month_number(name: &str) -> Option<u32> {
    let key = name.get(..3)?.to_ascii_lowercase();
    match key.as_str() {
        "jan" => Some(1),
        "feb" => Some(2),
        "mar" => Some(3),
        "apr" => Some(4),
        "may" => Some(5),
        "jun" => Some(6),
        "jul" => Some(7),
        "aug" => Some(8),
        "sep" => Some(9),
        "oct" => Some(10),
        "nov" => Some(11),
        "dec" => Some(12),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveTime;

    /// Build the UTC instant corresponding to a given LOCAL wall-clock
    /// date/time on this host. Panics only on a fixture date this test
    /// controls (never on provider input), and only if that fixture date
    /// happens to sit inside a DST gap on the machine running the test —
    /// noon in June avoids that on every real-world zone in use here.
    fn local_instant(y: i32, mo: u32, d: u32, h: u32, mi: u32, sec: u32) -> DateTime<Utc> {
        let naive = NaiveDate::from_ymd_opt(y, mo, d)
            .expect("fixture date must be valid")
            .and_time(NaiveTime::from_hms_opt(h, mi, sec).expect("fixture time must be valid"));
        match Local.from_local_datetime(&naive) {
            LocalResult::Single(dt) => dt.with_timezone(&Utc),
            LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc),
            LocalResult::None => panic!("fixture local time must exist on the test host"),
        }
    }

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

    #[test]
    fn kimi_detector_recognizes_transient_provider_limits() {
        let detector = RateLimitDetector::new_for_tool(CliTool::KimiCode);
        assert!(detector.detect("provider overloaded; Retry-After: 10").is_some());
    }

    // --- Codex quota-state line: literal samples captured from a live
    // `/status` screen (see task record for the full 40x120 frame). ---

    const CODEX_STATUS_5H_LINE: &str =
        "│  5h limit:             [████████████████████] 100% left (resets 23:40)          │";
    const CODEX_STATUS_WEEKLY_LINE: &str =
        "│  Weekly limit:         [████████████████████] 100% left (resets 18:40 on 6 Sep) │";

    #[test]
    fn codex_5h_quota_state_line_extracts_full_remaining_quota() {
        let now = local_instant(2026, 6, 15, 12, 0, 0);
        let detector = RateLimitDetector::new_for_tool(CliTool::Codex);
        let info = detector.detect_at(CODEX_STATUS_5H_LINE, now).unwrap();
        assert_eq!(info.limit_type, RateLimitType::Session);
        assert_eq!(info.usage_percent, Some(0.0));
        assert_eq!(info.resets_at_text.as_deref(), Some("23:40"));
        // now is local noon; 23:40 local is still later today.
        assert_eq!(info.resets_at, Some(local_instant(2026, 6, 15, 23, 40, 0)));
        assert_eq!(info.raw_message, CODEX_STATUS_5H_LINE);
    }

    #[test]
    fn codex_weekly_quota_state_line_extracts_dated_reset() {
        let now = local_instant(2026, 6, 15, 12, 0, 0);
        let detector = RateLimitDetector::new_for_tool(CliTool::Codex);
        let info = detector.detect_at(CODEX_STATUS_WEEKLY_LINE, now).unwrap();
        assert_eq!(info.limit_type, RateLimitType::Weekly);
        assert_eq!(info.usage_percent, Some(0.0));
        assert_eq!(info.resets_at_text.as_deref(), Some("18:40 on 6 Sep"));
        // "6 Sep" has not happened yet relative to 15 June of the same year.
        assert_eq!(info.resets_at, Some(local_instant(2026, 9, 6, 18, 40, 0)));
    }

    #[test]
    fn codex_quota_state_line_with_partial_percent_and_dated_reset() {
        let line =
            " Weekly limit:         [████████░░░░░░░░░░░░] 38% left (resets 09:15 on 12 Dec)";
        let now = local_instant(2026, 6, 15, 12, 0, 0);
        let detector = RateLimitDetector::new_for_tool(CliTool::Codex);
        let info = detector.detect_at(line, now).unwrap();
        assert_eq!(info.limit_type, RateLimitType::Weekly);
        assert_eq!(info.usage_percent, Some(62.0));
        assert_eq!(info.resets_at_text.as_deref(), Some("09:15 on 12 Dec"));
        assert_eq!(info.resets_at, Some(local_instant(2026, 12, 12, 9, 15, 0)));
    }

    #[test]
    fn codex_quota_state_beats_the_generic_failure_pattern_in_the_same_chunk() {
        // The `/status` banner's own help text mentions "rate limits" in
        // plain prose right above the state lines -- exactly what the
        // generic codex failure pattern matches. The specific quota-state
        // fact must win, not the generic "something failed" guess.
        let chunk = format!(
            "│ Visit https://chatgpt.com/codex/settings/usage for up-to-date                   │\n\
             │ information on rate limits and credits                                          │\n\
             {CODEX_STATUS_5H_LINE}\n\
             {CODEX_STATUS_WEEKLY_LINE}"
        );
        let now = local_instant(2026, 6, 15, 12, 0, 0);
        let detector = RateLimitDetector::new_for_tool(CliTool::Codex);
        let info = detector.detect_at(&chunk, now).unwrap();
        assert_eq!(info.limit_type, RateLimitType::Session);
        assert_eq!(info.usage_percent, Some(0.0));
    }

    #[test]
    fn codex_no_false_positive_on_ordinary_limit_mention() {
        let detector = RateLimitDetector::new_for_tool(CliTool::Codex);
        let result = detector.detect("Setting output limit to 4096 tokens.");
        assert!(result.is_none());
    }

    // --- resolve_codex_local_reset: date-rolling and DST edge cases ---

    #[test]
    fn bare_time_already_past_today_rolls_to_tomorrow() {
        let now = local_instant(2026, 6, 15, 12, 0, 0);
        let resolved = resolve_codex_local_reset(11, 0, None, now).unwrap();
        assert_eq!(resolved, local_instant(2026, 6, 16, 11, 0, 0));
    }

    #[test]
    fn bare_time_still_ahead_today_stays_today() {
        let now = local_instant(2026, 6, 15, 12, 0, 0);
        let resolved = resolve_codex_local_reset(13, 0, None, now).unwrap();
        assert_eq!(resolved, local_instant(2026, 6, 15, 13, 0, 0));
    }

    #[test]
    fn dated_reset_already_past_this_year_rolls_to_next_year() {
        let now = local_instant(2026, 6, 15, 12, 0, 0);
        let resolved = resolve_codex_local_reset(9, 0, Some((1, 1)), now).unwrap();
        assert_eq!(resolved, local_instant(2027, 1, 1, 9, 0, 0));
    }

    #[test]
    fn dated_reset_still_ahead_this_year_stays_this_year() {
        let now = local_instant(2026, 6, 15, 12, 0, 0);
        let resolved = resolve_codex_local_reset(9, 0, Some((25, 12)), now).unwrap();
        assert_eq!(resolved, local_instant(2026, 12, 25, 9, 0, 0));
    }

    /// Scan for a real DST spring-forward gap in this host's own local
    /// zone, rather than hardcoding one specific zone's transition rule
    /// (which would only exercise the `LocalResult::None` branch on a
    /// machine actually configured for that zone). Returns the first
    /// `(date, hour)` whose `hour:30` local wall-clock time does not
    /// exist, searching a three-year window at half-hour granularity.
    fn find_local_dst_gap() -> Option<(NaiveDate, u32)> {
        let start = NaiveDate::from_ymd_opt(2020, 1, 2)?;
        for day_offset in 0..(366 * 3) {
            let date = start.checked_add_signed(chrono::Duration::days(day_offset))?;
            for hour in 0..24u32 {
                let naive = date.and_hms_opt(hour, 30, 0)?;
                if matches!(Local.from_local_datetime(&naive), LocalResult::None) {
                    return Some((date, hour));
                }
            }
        }
        None
    }

    #[test]
    fn nonexistent_local_wall_clock_resolves_to_none() {
        let Some((gap_date, gap_hour)) = find_local_dst_gap() else {
            // This host's local zone has no DST gap in the scanned window
            // (e.g. a fixed-offset zone) -- nothing to exercise here, but
            // the branch this guards (`LocalResult::None` -> `None`,
            // never `.unwrap()`) still runs on every host that has one.
            return;
        };
        let year_start = NaiveDate::from_ymd_opt(gap_date.year(), 1, 1)
            .expect("Jan 1 is always a valid date")
            .and_hms_opt(0, 0, 0)
            .expect("midnight is always a valid time");
        let now = match Local.from_local_datetime(&year_start) {
            LocalResult::Single(dt) => dt.with_timezone(&Utc),
            LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc),
            // Exceedingly unlikely (midnight of Jan 1st inside a DST gap);
            // skip rather than flake the whole test on it.
            LocalResult::None => return,
        };
        let resolved = resolve_codex_local_reset(
            gap_hour,
            30,
            Some((gap_date.day(), gap_date.month())),
            now,
        );
        assert!(resolved.is_none());
    }
}

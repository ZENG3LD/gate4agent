//! Per-session token usage tracking and context window computation.
//!
//! `ContextTracker` accumulates token counts from `TurnComplete` events and
//! computes remaining context window capacity.

use serde::{Deserialize, Serialize};
use crate::core::capabilities::ModelInfo;

/// Data extracted from a TurnComplete event for feeding into ContextTracker.
#[derive(Debug, Clone, Copy, Default)]
pub struct TurnCompleteData {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    /// Set only by Codex (from model_context_window pipe field).
    pub context_window_hint: Option<u64>,
    /// When true, this represents cumulative session totals (Codex event_msg).
    /// Consumer should SET counters, not ADD to them.
    pub is_cumulative: bool,
}

/// Per-session token usage accumulator.
///
/// Consumer creates one per session and feeds each TurnComplete into `update()`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ContextTracker {
    /// Known context window size (tokens).
    /// `None` = unknown; UI should show raw counts only.
    pub context_window: Option<u64>,
    /// Cumulative input tokens across all turns.
    pub cumulative_input: u64,
    /// Cumulative output tokens across all turns.
    pub cumulative_output: u64,
    /// Cumulative cache-read tokens.
    pub cumulative_cache_read: u64,
    /// Cumulative cache-write tokens.
    pub cumulative_cache_write: u64,
    /// Cumulative reasoning tokens.
    pub cumulative_reasoning: u64,
}

impl ContextTracker {
    /// Create with a known context window.
    pub fn with_window(context_window: u64) -> Self {
        Self {
            context_window: Some(context_window),
            ..Default::default()
        }
    }

    /// Create with unknown context window.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// Initialize from a ModelInfo's context_window field.
    pub fn from_model_info(info: &ModelInfo) -> Self {
        match info.context_window {
            Some(w) => Self::with_window(w),
            None => Self::unknown(),
        }
    }

    /// Feed one TurnComplete event into the tracker.
    pub fn update(&mut self, ev: &TurnCompleteData) {
        if ev.is_cumulative {
            // Cumulative data: replace counters.
            self.cumulative_input = ev.input_tokens;
            self.cumulative_output = ev.output_tokens;
            self.cumulative_cache_read = ev.cache_read_tokens;
            self.cumulative_cache_write = ev.cache_write_tokens;
            self.cumulative_reasoning = ev.reasoning_tokens;
        } else {
            // Per-turn delta: accumulate output (generated tokens grow the
            // context), but REPLACE cache & input counters.
            //
            // Why replace?  Claude/Gemini report per-request cache stats:
            //   cache_read  = tokens served from prompt cache THIS turn
            //   cache_write = tokens written to cache THIS turn
            //   input       = tokens NOT in cache THIS turn
            // These are a snapshot of the current context composition, not
            // cumulative deltas.  Summing them across 1 000+ turns would
            // wildly overcount.  The last turn's values reflect the actual
            // context window occupancy.
            self.cumulative_input = ev.input_tokens;
            self.cumulative_output += ev.output_tokens;
            self.cumulative_cache_read = ev.cache_read_tokens;
            self.cumulative_cache_write = ev.cache_write_tokens;
            self.cumulative_reasoning = ev.reasoning_tokens;
        }
        if let Some(w) = ev.context_window_hint {
            self.set_window_from_pipe(w);
        }
    }

    /// Total tokens occupying the context window.
    ///
    /// For providers with prompt caching (Claude, Gemini), the real context
    /// size is `cache_read + cache_write + uncached_input + output`.
    /// `input_tokens` from the API is only the uncached portion, so we must
    /// add cache counters to get the true window occupancy.
    pub fn used_tokens(&self) -> u64 {
        self.cumulative_input
            + self.cumulative_output
            + self.cumulative_cache_read
            + self.cumulative_cache_write
    }

    /// Tokens remaining before context window is full.
    pub fn remaining_tokens(&self) -> Option<u64> {
        self.context_window.map(|w| w.saturating_sub(self.used_tokens()))
    }

    /// Fraction of context window consumed (0.0–1.0).
    pub fn usage_fraction(&self) -> Option<f64> {
        self.context_window.map(|w| {
            if w == 0 { return 0.0; }
            (self.used_tokens() as f64) / (w as f64)
        })
    }

    /// Percentage of context window consumed (0.0–100.0).
    pub fn usage_percent(&self) -> Option<f64> {
        self.usage_fraction().map(|f| f * 100.0)
    }

    /// Set context window from a pipe signal (e.g. Codex model_context_window).
    pub fn set_window_from_pipe(&mut self, window: u64) {
        self.context_window = Some(window);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_per_turn_replaces_input_accumulates_output() {
        let mut t = ContextTracker::with_window(200_000);
        t.update(&TurnCompleteData {
            input_tokens: 100,
            output_tokens: 50,
            cache_read_tokens: 5000,
            ..Default::default()
        });
        t.update(&TurnCompleteData {
            input_tokens: 200,
            output_tokens: 80,
            cache_read_tokens: 5200,
            ..Default::default()
        });
        // input/cache REPLACED by last turn values.
        assert_eq!(t.cumulative_input, 200);
        assert_eq!(t.cumulative_cache_read, 5200);
        // output ACCUMULATED across turns.
        assert_eq!(t.cumulative_output, 130);
        // used = input + output + cache_read + cache_write
        assert_eq!(t.used_tokens(), 200 + 130 + 5200);
        assert_eq!(t.remaining_tokens(), Some(200_000 - (200 + 130 + 5200)));
    }

    #[test]
    fn update_sets_when_cumulative() {
        let mut t = ContextTracker::with_window(200_000);
        // First: per-turn
        t.update(&TurnCompleteData {
            input_tokens: 100,
            output_tokens: 50,
            ..Default::default()
        });
        assert_eq!(t.used_tokens(), 100 + 50); // input + output, no cache
        // Then: cumulative replaces ALL counters
        t.update(&TurnCompleteData {
            input_tokens: 1240,
            output_tokens: 88,
            cache_read_tokens: 900,
            reasoning_tokens: 12,
            is_cumulative: true,
            ..Default::default()
        });
        assert_eq!(t.cumulative_input, 1240);
        assert_eq!(t.cumulative_output, 88);
        assert_eq!(t.cumulative_cache_read, 900);
        assert_eq!(t.cumulative_reasoning, 12);
        // used = input + output + cache_read + cache_write
        assert_eq!(t.used_tokens(), 1240 + 88 + 900);
    }

    #[test]
    fn remaining_saturates_at_zero() {
        let mut t = ContextTracker::with_window(100);
        t.update(&TurnCompleteData {
            input_tokens: 200,
            ..Default::default()
        });
        assert_eq!(t.remaining_tokens(), Some(0));
    }

    #[test]
    fn usage_percent_none_when_unknown() {
        let t = ContextTracker::unknown();
        assert!(t.usage_percent().is_none());
        assert!(t.remaining_tokens().is_none());
    }

    #[test]
    fn usage_percent_correct() {
        let mut t = ContextTracker::with_window(1_000_000);
        // Simulate a Claude turn: 3 uncached input, 43K cached, 200 output.
        t.update(&TurnCompleteData {
            input_tokens: 3,
            output_tokens: 200,
            cache_read_tokens: 43_000,
            ..Default::default()
        });
        // used = 3 + 200 + 43_000 = 43_203
        let pct = t.usage_percent().unwrap();
        let expected = 43_203.0 / 1_000_000.0 * 100.0;
        assert!((pct - expected).abs() < 0.01, "got {pct}, expected {expected}");
    }

    #[test]
    fn set_window_from_pipe_overwrites() {
        let mut t = ContextTracker::with_window(100_000);
        assert_eq!(t.context_window, Some(100_000));
        t.set_window_from_pipe(258_400);
        assert_eq!(t.context_window, Some(258_400));
    }

    #[test]
    fn context_window_hint_applied_via_update() {
        let mut t = ContextTracker::unknown();
        assert!(t.context_window.is_none());
        t.update(&TurnCompleteData {
            input_tokens: 100,
            output_tokens: 50,
            context_window_hint: Some(258_400),
            is_cumulative: true,
            ..Default::default()
        });
        assert_eq!(t.context_window, Some(258_400));
        assert!(t.remaining_tokens().is_some());
    }

    #[test]
    fn from_model_info_with_window() {
        let info = ModelInfo {
            id: "test".to_string(),
            display_name: "Test".to_string(),
            is_default: true,
            is_free_tier: false,
            context_window: Some(200_000),
        };
        let t = ContextTracker::from_model_info(&info);
        assert_eq!(t.context_window, Some(200_000));
    }

    #[test]
    fn from_model_info_without_window() {
        let info = ModelInfo {
            id: "test".to_string(),
            display_name: "Test".to_string(),
            is_default: true,
            is_free_tier: false,
            context_window: None,
        };
        let t = ContextTracker::from_model_info(&info);
        assert!(t.context_window.is_none());
    }

    /// Realistic Claude session: 3 turns, cache grows, output accumulates.
    /// Based on real session data: input_tokens=1-6, cache_read=27K→43K.
    #[test]
    fn realistic_claude_session() {
        let mut t = ContextTracker::with_window(1_000_000);

        // Turn 1: initial prompt
        t.update(&TurnCompleteData {
            input_tokens: 6,
            output_tokens: 139,
            cache_read_tokens: 27_385,
            cache_write_tokens: 21_482,
            ..Default::default()
        });
        // input=6, output=139, cache_read=27385, cache_write=21482
        assert_eq!(t.used_tokens(), 6 + 139 + 27_385 + 21_482);

        // Turn 2: context grows, most is cached
        t.update(&TurnCompleteData {
            input_tokens: 3,
            output_tokens: 280,
            cache_read_tokens: 35_000,
            cache_write_tokens: 500,
            ..Default::default()
        });
        // input REPLACED=3, output ACCUMULATED=139+280=419,
        // cache_read REPLACED=35000, cache_write REPLACED=500
        assert_eq!(t.cumulative_input, 3);
        assert_eq!(t.cumulative_output, 419);
        assert_eq!(t.cumulative_cache_read, 35_000);
        assert_eq!(t.cumulative_cache_write, 500);
        assert_eq!(t.used_tokens(), 3 + 419 + 35_000 + 500);

        // Turn 3: near end of session
        t.update(&TurnCompleteData {
            input_tokens: 3,
            output_tokens: 150,
            cache_read_tokens: 43_429,
            cache_write_tokens: 0,
            ..Default::default()
        });
        assert_eq!(t.cumulative_output, 569); // 419 + 150
        assert_eq!(t.used_tokens(), 3 + 569 + 43_429 + 0);
        // ~4.4% of 1M window — matches real session data
        let pct = t.usage_percent().unwrap();
        assert!(pct < 5.0, "usage should be ~4.4%, got {pct}%");
        assert!(pct > 3.0, "usage should be ~4.4%, got {pct}%");
    }
}

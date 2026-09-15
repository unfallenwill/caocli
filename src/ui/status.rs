//! The status line: the session summary, in one row.
//!
//! Shared by both front ends so the line reads the same either way, and so
//! the progressive-disclosure rule lives in one place.
//!
//! Segment order is the priority order: model + effort are first because they
//! describe *what is about to think for me*, and a narrow terminal that loses
//! everything else still tells me which model will run the next turn. Cache
//! stats come second because they accumulate over the session: every dropped
//! segment is one the user can recover by widening the terminal.

use crate::types::{CacheTokens, Usage};
use crate::ui::glyphs;

/// Session-level cache statistics. Accumulates the hit/miss of every sub-request
/// within this process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hit: u64,
    pub miss: u64,
}

impl CacheStats {
    /// Fold one sub-request's cache tokens into the running totals.
    pub fn record(&mut self, c: CacheTokens) {
        self.hit += c.hit;
        self.miss += c.miss;
    }

    /// Hit rate as a percentage; zero until any request has reported caching,
    /// which is the default a new session opens with.
    pub fn hit_rate(&self) -> f64 {
        let total = self.hit + self.miss;
        if total == 0 {
            0.0
        } else {
            self.hit as f64 * 100.0 / total as f64
        }
    }

    /// Status segments, most significant first: the hit rate, then the raw
    /// counts. A session that has reported nothing shows the same segments with
    /// zero in them, so the line is the same line from the first row onwards.
    fn segments(&self) -> Vec<String> {
        vec![
            format!("cache {:.1}%", self.hit_rate()),
            format!("{}/{}", self.hit, self.miss),
        ]
    }
}

/// The status line's content: the session's model id, its effort tier, and
/// the cumulative cache statistics. The bar shows what is *current*: a reader
/// looking at the bottom line before typing wants to know which model will
/// run the next turn, not which one ran the last one.
#[derive(Debug, Default)]
pub struct Status {
    stats: CacheStats,
    /// The model id in effect. Set at session start and on `/model`. Drives
    /// the first segment of the bar.
    model: Option<String>,
    /// The reasoning effort tier in effect. Set at session start and on
    /// `/effort`. Joins the model on the bar.
    effort: Option<String>,
}

impl Status {
    /// Adopt a new model id. The bar redraws through the caller; this only
    /// stores the value. An empty / whitespace-only string is treated as
    /// "not set" so a partial state never pushes an empty segment into the
    /// line.
    pub fn set_model(&mut self, model: &str) {
        self.model = non_empty(model);
    }

    /// Adopt a new effort tier. Same rule as [`Self::set_model`]: empty is
    /// "not set".
    pub fn set_effort(&mut self, effort: &str) {
        self.effort = non_empty(effort);
    }

    /// Fold one sub-request's usage into the cache statistics.
    pub fn record(&mut self, usage: &Usage) {
        // Normalization: DeepSeek's flat fields and GLM's nested details both
        // converge in Usage::cache(). A provider that does not report caching
        // records nothing, so the line keeps the defaults it opened with.
        if let Some(c) = usage.cache() {
            self.stats.record(c);
        }
    }

    /// Clear the cache statistics (when switching sessions).
    pub fn reset_stats(&mut self) {
        self.stats = CacheStats::default();
    }

    /// Statistics accumulated so far. Read by tests and by `/debug`.
    #[allow(dead_code)]
    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// The status line at the width the caller has.
    ///
    /// Segments are tried from the longest prefix down: a narrow bar loses
    /// detail by whole segments rather than by clipping, and only an
    /// over-long shortest segment is clipped below.
    pub fn line(&self, width: usize) -> String {
        let parts = self.parts();
        let mut label = String::new();
        for n in (1..=parts.len()).rev() {
            label = parts[..n].join(glyphs::sep());
            if super::text::width(&label) <= width {
                break;
            }
        }
        label
    }

    fn parts(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(m) = &self.model {
            out.push(m.clone());
        }
        if let Some(e) = &self.effort {
            out.push(format!("effort {e}"));
        }
        out.extend(self.stats.segments());
        out
    }

    /// Every segment joined: what a line wide enough for everything displays.
    #[allow(dead_code)]
    pub fn full_line(&self) -> String {
        self.parts().join(glyphs::sep())
    }
}

/// An empty / whitespace-only string becomes `None` so the segment is skipped.
fn non_empty(s: &str) -> Option<String> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Default Status: no model, no effort, no usage yet. The bar shows
    /// just the cache segments with zeroes, which is what a fresh session
    /// looks like until anything has been recorded.
    #[test]
    fn default_status_full_line_is_cache_only() {
        let s = Status::default();
        assert_eq!(s.full_line(), "cache 0.0% · 0/0");
    }

    /// Model id lands at the head of the segment list, ahead of cache stats,
    /// because that is the segment a reader wants to see even when the
    /// terminal is too narrow for the rest.
    #[test]
    fn model_prepends_to_segments() {
        let mut s = Status::default();
        s.set_model("deepseek-v4-pro");
        assert_eq!(s.full_line(), "deepseek-v4-pro · cache 0.0% · 0/0");
    }

    /// Effort tier joins the model with a "effort X" label, ordered after
    /// the model id and ahead of cache stats.
    #[test]
    fn effort_joins_model_in_order() {
        let mut s = Status::default();
        s.set_model("m-1");
        s.set_effort("high");
        assert_eq!(s.full_line(), "m-1 · effort high · cache 0.0% · 0/0");
    }

    /// A blank or whitespace-only string clears the field rather than
    /// leaving an empty segment that would render as ` · cache ...`.
    #[test]
    fn blank_string_clears_the_field() {
        let mut s = Status::default();
        s.set_model("m-1");
        s.set_effort("high");
        s.set_model("");
        s.set_effort("   ");
        assert_eq!(s.full_line(), "cache 0.0% · 0/0");
    }

    /// Progressive disclosure drops whole segments from the end, starting
    /// with the cache counts, then the rate, then the effort, keeping the
    /// model id to the bitter end.
    #[test]
    fn line_drops_segments_in_reverse_priority_order() {
        let mut s = Status::default();
        s.set_model("deepseek-v4-pro");
        s.set_effort("high");
        // Force a known hit rate so the cache segments have predictable width.
        s.record(&cache_usage(6, 4)); // 60.0%

        // Plenty of room: everything fits. The full label is 49 columns.
        assert_eq!(
            s.line(60),
            "deepseek-v4-pro · effort high · cache 60.0% · 6/4"
        );
        // Drop the counts first. The `model · effort · rate` label is 43 columns.
        assert_eq!(s.line(45), "deepseek-v4-pro · effort high · cache 60.0%");
        // Drop the rate. The `model · effort` label is 29 columns.
        assert_eq!(s.line(32), "deepseek-v4-pro · effort high");
        // Drop the effort. The bare model id is 15 columns.
        assert_eq!(s.line(20), "deepseek-v4-pro");
        // Below the model's own width: the caller clips it; line() still
        // returns the unclipped model id so the caller's truncate() has
        // something to work with.
        assert_eq!(s.line(8), "deepseek-v4-pro");
    }

    fn cache_usage(hit: u64, miss: u64) -> Usage {
        Usage {
            prompt_tokens: hit + miss,
            total_tokens: hit + miss,
            completion_tokens: 0,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
            ..Default::default()
        }
    }
}

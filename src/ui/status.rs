//! The status line: the session summary, in one row.
//!
//! Shared by both front ends so the line reads the same either way, and so the
//! progressive-disclosure rule lives in one place.

use crate::types::Usage;

/// Session-level cache statistics. Accumulates the hit/miss of every sub-request
/// within this process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hit: u64,
    pub miss: u64,
}

impl CacheStats {
    fn record(&mut self, u: &Usage) {
        // Normalization: DeepSeek's flat fields and GLM's nested details both
        // converge in Usage::cache(). A provider that does not report caching
        // records nothing, so the line keeps the defaults it opened with.
        if let Some(c) = u.cache() {
            self.hit += c.hit;
            self.miss += c.miss;
        }
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
            format!("hit {} · miss {}", self.hit, self.miss),
        ]
    }
}

/// The status line's content: the model id, the reasoning effort tier, and the
/// session's cache statistics.
#[derive(Debug, Default)]
pub struct Status {
    /// Model id (updated when a session is created or switched).
    model: Option<String>,
    /// Reasoning effort tier in effect (updated when a session is created or
    /// switched, and when `/effort` changes it).
    effort: Option<String>,
    stats: CacheStats,
}

impl Status {
    /// Set the model id shown in the first segment.
    pub fn set_model(&mut self, model: &str) {
        self.model = Some(model.to_owned());
    }

    /// Set the reasoning effort tier shown right after the model.
    pub fn set_effort(&mut self, effort: &str) {
        self.effort = Some(effort.to_owned());
    }

    /// Fold one sub-request's usage into the cache statistics.
    pub fn record(&mut self, u: &Usage) {
        self.stats.record(u);
    }

    /// Clear the cache statistics (when switching sessions). The model id stays:
    /// it follows the session meta and is set separately.
    pub fn reset_stats(&mut self) {
        self.stats = CacheStats::default();
    }

    /// Statistics accumulated so far. Read by tests only.
    #[cfg(test)]
    pub fn stats(&self) -> CacheStats {
        self.stats
    }

    /// Status segments, most significant first: the model, the effort tier,
    /// then the cache statistics. This is the order a narrow line drops them in.
    pub fn parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(m) = &self.model
            && !m.is_empty()
        {
            parts.push(m.clone());
        }
        if let Some(e) = &self.effort
            && !e.is_empty()
        {
            parts.push(format!("effort {e}"));
        }
        parts.extend(self.stats.segments());
        parts
    }

    /// The longest segment combination that fits `width` display columns.
    ///
    /// Detail is dropped by whole segments rather than by clipping, so a narrow
    /// terminal never shows half a number. If not even the shortest combination
    /// fits, it is returned anyway and the caller clips it -- an over-long line
    /// reads better than an empty one.
    pub fn line(&self, width: usize) -> String {
        let parts = self.parts();
        let mut label = String::new();
        for n in (1..=parts.len()).rev() {
            label = parts[..n].join(" · ");
            if super::text::width(&label) <= width {
                break;
            }
        }
        label
    }

    /// Every segment joined: what a line wide enough for everything displays.
    #[cfg(test)]
    pub fn full_line(&self) -> String {
        self.parts().join(" · ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(hit: u64, miss: u64) -> Usage {
        Usage {
            prompt_tokens: hit + miss,
            total_tokens: hit + miss,
            completion_tokens: 0,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
            prompt_tokens_details: None,
        }
    }

    #[test]
    fn an_empty_status_shows_the_zero_defaults() {
        let s = Status::default();
        assert_eq!(s.full_line(), "cache 0.0% · hit 0 · miss 0");
        assert_eq!(s.stats().hit_rate(), 0.0);
    }

    #[test]
    fn model_comes_first_and_survives_a_stats_reset() {
        let mut s = Status::default();
        s.set_model("deepseek-v4-flash");
        s.record(&usage(6, 4));
        assert_eq!(
            s.full_line(),
            "deepseek-v4-flash · cache 60.0% · hit 6 · miss 4"
        );
        s.reset_stats();
        assert_eq!(
            s.full_line(),
            "deepseek-v4-flash · cache 0.0% · hit 0 · miss 0"
        );
    }

    #[test]
    fn an_empty_model_is_not_a_segment() {
        let mut s = Status::default();
        s.set_model("");
        assert_eq!(s.full_line(), "cache 0.0% · hit 0 · miss 0");
    }

    #[test]
    fn effort_sits_between_the_model_and_the_cache() {
        let mut s = Status::default();
        s.set_model("deepseek/deepseek-flash");
        s.set_effort("high");
        s.record(&usage(6, 4));
        assert_eq!(
            s.full_line(),
            "deepseek/deepseek-flash · effort high · cache 60.0% · hit 6 · miss 4"
        );
        // It follows the session, not the statistics.
        s.reset_stats();
        assert_eq!(
            s.full_line(),
            "deepseek/deepseek-flash · effort high · cache 0.0% · hit 0 · miss 0"
        );
    }

    #[test]
    fn an_empty_effort_is_not_a_segment() {
        let mut s = Status::default();
        s.set_model("m-1");
        s.set_effort("");
        assert_eq!(s.full_line(), "m-1 · cache 0.0% · hit 0 · miss 0");
    }

    #[test]
    fn line_drops_the_effort_before_the_model() {
        let mut s = Status::default();
        s.set_model("m-1");
        s.set_effort("max");
        assert_eq!(
            s.line(29),
            "m-1 · effort max · cache 0.0%",
            "the counts go first"
        );
        assert_eq!(s.line(16), "m-1 · effort max");
        assert_eq!(s.line(15), "m-1", "the model is the last thing dropped");
    }

    #[test]
    fn line_drops_whole_segments_to_fit() {
        let mut s = Status::default();
        s.set_model("deepseek-v4-flash");
        s.record(&usage(6, 4));
        // 79 columns: everything fits
        assert_eq!(
            s.line(79),
            "deepseek-v4-flash · cache 60.0% · hit 6 · miss 4"
        );
        // 34 columns: the counts go, the model and rate stay
        assert_eq!(s.line(34), "deepseek-v4-flash · cache 60.0%");
        // 19 columns: only the model is left
        assert_eq!(s.line(19), "deepseek-v4-flash");
        // 9 columns: nothing fits, so the shortest segment comes back to be clipped
        assert_eq!(s.line(9), "deepseek-v4-flash");
    }

    #[test]
    fn line_measures_wide_characters_by_column() {
        let mut s = Status::default();
        // four ideographs: 8 columns, 4 chars
        s.set_model("\u{6df1}\u{5ea6}\u{6c42}\u{7d22}");
        // the model plus " · cache 0.0%" is 21 columns, and the counts are 14
        // more: 21 fits, 20 does not, and the whole line is 38.
        assert_eq!(
            s.line(38),
            "\u{6df1}\u{5ea6}\u{6c42}\u{7d22} · cache 0.0% · hit 0 · miss 0"
        );
        assert_eq!(s.line(21), "\u{6df1}\u{5ea6}\u{6c42}\u{7d22} · cache 0.0%");
        assert_eq!(s.line(20), "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}");
    }

    #[test]
    fn stats_accumulate_across_requests() {
        let mut s = Status::default();
        s.record(&usage(6, 4));
        s.record(&usage(0, 10));
        assert_eq!(s.stats().hit, 6);
        assert_eq!(s.stats().miss, 14);
        assert_eq!(s.stats().hit_rate(), 30.0);
    }

    #[test]
    fn a_provider_without_cache_reporting_keeps_the_defaults() {
        let mut s = Status::default();
        s.record(&Usage {
            prompt_tokens: 20,
            total_tokens: 28,
            completion_tokens: 8,
            ..Usage::default()
        });
        assert_eq!(s.full_line(), "cache 0.0% · hit 0 · miss 0");
    }

    #[test]
    fn glm_nested_details_derive_the_miss_count() {
        // GLM shape: only prompt_tokens_details.cached_tokens is given, so the
        // miss count has to be derived from prompt_tokens.
        let mut s = Status::default();
        s.record(&Usage {
            prompt_tokens: 1200,
            completion_tokens: 300,
            total_tokens: 1500,
            prompt_tokens_details: Some(crate::types::PromptTokensDetails { cached_tokens: 800 }),
            ..Usage::default()
        });
        assert_eq!(
            s.full_line(),
            "cache 66.7% · hit 800 · miss 400",
            "1200 prompt tokens with 800 cached leaves 400 miss"
        );
    }

    #[test]
    fn a_long_run_accumulates_a_rate_to_one_decimal() {
        let mut s = Status::default();
        s.record(&usage(6, 4));
        s.record(&usage(32378, 457));
        assert_eq!((s.stats().hit, s.stats().miss), (32384, 461));
        assert_eq!(s.full_line(), "cache 98.6% · hit 32384 · miss 461");
    }
}

//! The status line: what the session is doing, in one row.
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
        // converge in Usage::cache(). When a provider does not report caching,
        // nothing is recorded, so "unknown" is never displayed as 0% hit.
        if let Some(c) = u.cache() {
            self.hit += c.hit;
            self.miss += c.miss;
        }
    }

    /// Hit rate as a percentage; None while there is no data yet.
    pub fn hit_rate(&self) -> Option<f64> {
        let total = self.hit + self.miss;
        (total > 0).then(|| self.hit as f64 * 100.0 / total as f64)
    }

    /// Status segments, most significant first: the hit rate, then the raw
    /// counts. A provider that reports no cache usage yields a single
    /// "unknown" segment, so it is never displayed as 0% hit.
    fn segments(&self) -> Vec<String> {
        match self.hit_rate() {
            Some(rate) => vec![
                format!("cache {rate:.1}%"),
                format!("hit {} · miss {}", self.hit, self.miss),
            ],
            None => vec!["cache —".to_string()],
        }
    }
}

/// The status line's content: the model id and the session's cache statistics.
#[derive(Debug, Default)]
pub struct Status {
    /// Model id (updated when a session is created or switched).
    model: Option<String>,
    stats: CacheStats,
}

impl Status {
    /// Set the model id shown in the first segment.
    pub fn set_model(&mut self, model: &str) {
        self.model = Some(model.to_owned());
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

    /// Status segments, most significant first: the model, then the cache
    /// statistics. This is the order a narrow line drops them in.
    pub fn parts(&self) -> Vec<String> {
        let mut parts = Vec::new();
        if let Some(m) = &self.model
            && !m.is_empty()
        {
            parts.push(m.clone());
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
    fn empty_status_reports_an_unknown_rate() {
        let s = Status::default();
        assert_eq!(s.full_line(), "cache —");
        assert_eq!(s.stats().hit_rate(), None);
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
        assert_eq!(s.full_line(), "deepseek-v4-flash · cache —");
    }

    #[test]
    fn an_empty_model_is_not_a_segment() {
        let mut s = Status::default();
        s.set_model("");
        assert_eq!(s.full_line(), "cache —");
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
        // the model plus " · cache —" is 18 columns, so 20 fits and 17 does not
        assert_eq!(s.line(20), "\u{6df1}\u{5ea6}\u{6c42}\u{7d22} · cache —");
        assert_eq!(s.line(17), "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}");
    }

    #[test]
    fn stats_accumulate_across_requests() {
        let mut s = Status::default();
        s.record(&usage(6, 4));
        s.record(&usage(0, 10));
        assert_eq!(s.stats().hit, 6);
        assert_eq!(s.stats().miss, 14);
        assert_eq!(s.stats().hit_rate(), Some(30.0));
    }

    #[test]
    fn a_provider_without_cache_reporting_stays_unknown() {
        let mut s = Status::default();
        s.record(&Usage {
            prompt_tokens: 20,
            total_tokens: 28,
            completion_tokens: 8,
            ..Usage::default()
        });
        assert_eq!(s.full_line(), "cache —");
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

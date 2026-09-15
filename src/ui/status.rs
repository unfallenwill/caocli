//! The status line: the session summary, in one row.
//!
//! Slimmed to cache statistics only. The model id and reasoning effort
//! tier used to live here too, but they describe a *turn*, not a session:
//! the user can `/model` and `/effort` mid-session, so a model name on
//! the pinned row is a snapshot the user did not ask for. They moved to
//! the per-prompt metadata row above each User cell. Cache stats, by
//! contrast, are session-level and cumulative, so the bottom row is
//! still the right place for them.
//!
//! Shared by both front ends so the line reads the same either way, and so
//! the progressive-disclosure rule lives in one place.

use crate::types::Usage;
use crate::ui::glyphs;

/// Session-level cache statistics. Accumulates the hit/miss of every sub-request
/// within this process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct CacheStats {
    pub hit: u64,
    pub miss: u64,
}

impl CacheStats {
    pub fn record(&mut self, usage: &Usage) {
        // Normalization: DeepSeek's flat fields and GLM's nested details both
        // converge in Usage::cache(). A provider that does not report caching
        // records nothing, so the line keeps the defaults it opened with.
        if let Some(c) = usage.cache() {
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
            format!("{}/{}", self.hit, self.miss),
        ]
    }
}

/// The status line's content: the session's cache statistics. The model
/// id and effort tier moved to the per-prompt metadata row above each
/// User cell, so this struct now carries only what is truly session-level.
#[derive(Debug, Default)]
pub struct Status {
    stats: CacheStats,
}

impl Status {
    /// Fold one sub-request's usage into the cache statistics.
    pub fn record(&mut self, usage: &Usage) {
        self.stats.record(usage);
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

    /// The status line: `cache X% · hit/miss` at the width the caller has.
    /// Cache stats are the only thing the bottom row carries after the
    /// metadata row took the model and effort.
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
        self.stats.segments()
    }

    /// Every segment joined: what a line wide enough for everything displays.
    #[allow(dead_code)]
    pub fn full_line(&self) -> String {
        self.parts().join(glyphs::sep())
    }
}

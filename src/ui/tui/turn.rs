//! The turn half of the session: the queue, the running turn's clock, and the
//! characters the live speed estimate counts.
//!
//! Kept apart from [`super::View`] (what the screen shows) and the other
//! halves because the turn has its own lifetime: a queue outlives a keypress,
//! the running flag tracks a session-level concern (is a model call in
//! flight?), and the chars-per-token counter measures this provider's
//! accounting. None of them touch the cells on the screen.

use std::collections::VecDeque;
use std::time::Instant;

/// The characters-per-token ratio the estimate starts on, before the first
/// usage notice of the session has measured the real one. English prose runs
/// about four characters to the token; CJK lands near one. Both are
/// approximations, which is why the estimate carries a tilde.
pub(super) const BLIND_CHARS_PER_TOKEN: f64 = 4.0;

/// What the running turn carries: the queue behind it, its clock, and the
/// counters the live speed estimate watches.
pub(crate) struct Turn {
    /// Lines submitted while a turn was running, oldest first. They are not
    /// part of the session until they run, so they are held here rather than
    /// in the transcript: the head runs as soon as the turn in flight ends,
    /// interrupted or not.
    pub(crate) queued: VecDeque<String>,
    /// Whether a turn is running: what the box's placeholder and the queue's
    /// behaviour both hang on.
    pub(crate) running: bool,
    /// When the running turn began. None between turns; what the border's
    /// working indicator counts up from.
    pub(crate) started: Option<Instant>,
    /// Characters the model produced this turn -- streamed reasoning and
    /// content, plus the arguments of every tool call it declared: the
    /// numerator of the live tokens-per-second estimate, and the same set of
    /// text the backend's completion tokens bill for.
    pub(crate) streamed_chars: usize,
    /// Characters produced since the last usage notice, counted the same way
    /// [`Turn::streamed_chars`] is. With that notice's completion tokens it
    /// measures the characters-per-token ratio this provider and model
    /// actually produce, which is what keeps the estimate honest after the
    /// first sub-request.
    pub(crate) chars_since_usage: usize,
    /// Characters per token, as last measured, or [`BLIND_CHARS_PER_TOKEN`]
    /// before the first measurement. Session-level: it survives the turn that
    /// taught it.
    pub(crate) chars_per_token: f64,
    /// The indicator title the last tick saw, so a tick bumps the revision only
    /// when the border would show something new.
    pub(crate) ticked_activity: Option<String>,
}

impl Default for Turn {
    fn default() -> Self {
        Self {
            queued: VecDeque::new(),
            running: false,
            started: None,
            streamed_chars: 0,
            chars_since_usage: 0,
            chars_per_token: BLIND_CHARS_PER_TOKEN,
            ticked_activity: None,
        }
    }
}

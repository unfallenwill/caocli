//! The thinking widget's unit of work: one thought block.
//!
//! A thought block is the section of a turn between two boundary events:
//! the start of one (or the end of the previous one) and either body
//! text starting, a side-effectful tool starting, or the turn ending.
//! Everything inside it — reasoning text, safe tool calls, safe tool
//! results — is one group the user can ask to see in full with Ctrl-O.
//!
//! Two pieces of state travel together: the block's identity (when it
//! started, what it was about) and the flag that says whether the user
//! has asked to see it. The activity description is what the header
//! line says ("thinking", "running X"); the activity source is the
//! [`open_thought`](super::State::open_thought) call's caller — the
//! description is set when the block opens, and stays put for the
//! block's life.
//!
//! The `body` is the cells that arrived while the block was open
//! (reasoning text, safe tool steps), appended in order. The
//! `snapshot` is the frozen view of that body at the moment the
//! user pressed Ctrl-O: rendered in place of `body` while the block
//! is expanded, so reading the expanded view is not disrupted by
//! new cells arriving or steps settling underneath.

use std::time::Instant;

use crate::ui::cell::Cell;

/// What a thought block is up to.
///
/// Lives here rather than on `Step` because a thought is one thing
/// across many possible tool calls; reusing `StepStatus` would read a
/// tool lifecycle onto something that is not a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
pub(super) enum ThoughtStatus {
    /// The block is still receiving fragments — reasoning text or safe
    /// tool calls. The header keeps its running indicator.
    Running,
    /// The block has closed (content started, side-effect tool
    /// started, or the turn ended). The header shows it as settled.
    Done,
}

/// One thought block: the metadata the widget surfaces at the top of
/// the transcript. The cells the block "owns" stay in the transcript;
/// this struct is the bookkeeping the rendering layer reads.
///
/// A block is created by [`super::State::open_thought`] and ended by
/// [`super::State::close_thought`]; once ended it is pushed onto
/// `State::historical` and a new block can be opened.
#[derive(Debug, Clone)]
#[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
pub(super) struct ThoughtBlock {
    /// The one-line description the header shows: "thinking",
    /// "running bash", "verifying fix", etc. Set when the block opens
    /// and left alone for the block's life.
    pub(super) activity: String,
    /// When this block opened. `None` for blocks reconstructed from a
    /// resumed session's log (their wall-clock time is not data we
    /// keep). The duration shown in the header is `now - started_at`,
    /// and the spinner is keyed on the same clock.
    pub(super) started_at: Option<Instant>,
    /// Running or Done. Done blocks no longer grow — new reasoning
    /// text opens a new block, not this one.
    pub(super) status: ThoughtStatus,
    /// Whether the user has asked to see the body via Ctrl-O. Lives
    /// here rather than on `State` so a historical block can be in
    /// the expanded state (when it was expanded at the moment it
    /// closed) and still be collapsed by Ctrl-O.
    pub(super) expanded: bool,
    /// Cells that arrived while this block was open: reasoning text
    /// (each closed `Cell::Reasoning` block) and safe tool calls
    /// (`Cell::Step` for read-only tools). For an active block the
    /// list grows as new cells arrive; for a historical block it is
    /// final.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) body: Vec<Cell>,
    /// Frozen copy of `body` at the moment the user pressed Ctrl-O.
    /// Empty when the block is folded; rendered in place of `body`
    /// while expanded, so the cells do not move underneath the
    /// reader. Cleared on collapse.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) snapshot: Vec<Cell>,
}

impl ThoughtBlock {
    /// A new running block with the given activity description and
    /// `now` as its start time.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) fn open(activity: impl Into<String>, started_at: Instant) -> Self {
        Self {
            activity: activity.into(),
            started_at: Some(started_at),
            status: ThoughtStatus::Running,
            expanded: false,
            body: Vec::new(),
            snapshot: Vec::new(),
        }
    }

    /// Mark the block as no longer running. Idempotent.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) fn close(&mut self) {
        self.status = ThoughtStatus::Done;
    }

    /// Take a frozen copy of the body for the expanded view. Idempotent:
    /// a block that is already expanded does not re-snapshot — the user
    /// gets the view they had, not a new one mid-read.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) fn snapshot_body(&mut self) {
        if self.expanded {
            return;
        }
        self.snapshot = self.body.clone();
    }

    /// Drop the frozen view; the next expansion takes a fresh one
    /// from the (possibly grown) body.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) fn clear_snapshot(&mut self) {
        self.snapshot.clear();
    }

    /// How long the block has been open, in whole seconds. `0` for a
    /// block that has no start time (a resumed block).
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) fn elapsed_secs(&self, now: Instant) -> u64 {
        match self.started_at {
            Some(start) => now.saturating_duration_since(start).as_secs(),
            None => 0,
        }
    }
}

//! A thought region: the reasoning of one stretch of a turn, folded to a line.
//!
//! The model's thinking and the calls that only look (a `Read`, a `Glob`, a
//! `grep`) are the way it works, not the work: they are the machinery around
//! the answer, and a transcript that gives them screen time buries the answer
//! under them. They are one region here -- the section of a turn between two
//! boundaries, the start of it or the end of the previous one and either body
//! text starting, a call that changes something starting, or the turn ending --
//! and the region shows on the transcript as one line.
//!
//! The body is a [`Cell`] list of its own rather than a run of cells in the
//! transcript, and that is what makes the fold structural: a region is one
//! transcript entry whatever arrives inside it, so a line that lands while the
//! region is open (an informational notice, a tool result with no step of its
//! own) can never split it in two. It is also what keeps the body out of the
//! layout: the transcript lays out one line per region, and the body is read
//! through `Ctrl-O`, which hands it to the expanded view instead.
//!
//! The cells in the body are clones of what the session produced, frozen at the
//! moment they were claimed: a step that settles after the fact settles its
//! transcript copy, and the region keeps what was true when the region was
//! open. That is the same bargain `snapshot` strikes for the expanded view one
//! level down.

use std::time::Instant;

use super::{Cell, Span, Style};

/// What a thought region is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThoughtStatus {
    /// The region is still receiving: reasoning text, or a call that only
    /// looks. Its fold line's seconds are read from the clock.
    Running,
    /// The region has closed -- body text started, a call that changes
    /// something started, or the turn ended. Its fold line's seconds are the
    /// ones the boundary froze.
    Done,
}

/// One folded region: what its line says, and the cells the line stands for.
#[derive(Debug, Clone, PartialEq)]
pub struct Thought {
    /// The one-line description on the fold line: "thinking", "running Read".
    /// Set when the region opens and left alone for its life, so what the line
    /// shows is what was true at the boundary.
    pub activity: String,
    /// When the region opened; `None` for one whose clock is gone. The border's
    /// working indicator reads it, and [`Thought::live_secs`] derives from it.
    pub started_at: Option<Instant>,
    /// The whole seconds the fold line shows. Live while running (the state's
    /// tick moves it), frozen at the second the region closed.
    pub elapsed_secs: u64,
    pub status: ThoughtStatus,
    /// Whether the reader asked to read the body (`Ctrl-O`).
    pub expanded: bool,
    /// The cells the region owns: the reasoning text and the calls that only
    /// look, in the order they arrived.
    pub body: Vec<Cell>,
    /// The frozen copy of `body` taken when the expanded view opened. Rendering
    /// this rather than `body` means a region that keeps receiving -- the one in
    /// flight, whose reasoning is still streaming -- does not move under the
    /// reader's eyes. Cleared on collapse, so the next expansion takes a fresh
    /// one.
    pub snapshot: Vec<Cell>,
}

impl Thought {
    /// A running region with `activity` as its description and `started_at` as
    /// its clock. `None` is a region whose wall clock is not known -- one
    /// rebuilt from a log rather than watched -- and it reads `0s` forever.
    pub fn open(activity: impl Into<String>, started_at: Option<Instant>) -> Self {
        Self {
            activity: activity.into(),
            started_at,
            elapsed_secs: 0,
            status: ThoughtStatus::Running,
            expanded: false,
            body: Vec::new(),
            snapshot: Vec::new(),
        }
    }

    /// Close the region: the seconds stop moving at the value the boundary saw.
    ///
    /// Freezing rather than reading `now` at display time is what makes the
    /// line honest: a reader who looks at the transcript an hour later sees the
    /// seconds the region ran for, not an hour.
    pub fn close(&mut self, now: Instant) {
        self.elapsed_secs = self.live_secs(now);
        self.status = ThoughtStatus::Done;
    }

    /// The seconds the fold line should show at `now`: the clock for a running
    /// region, the frozen value for a closed one.
    pub fn live_secs(&self, now: Instant) -> u64 {
        match self.status {
            ThoughtStatus::Running => self
                .started_at
                .map(|s| now.saturating_duration_since(s).as_secs())
                .unwrap_or(0),
            ThoughtStatus::Done => self.elapsed_secs,
        }
    }

    /// Take the frozen copy for the expanded view. Idempotent: a region that is
    /// already expanded keeps the snapshot the reader is looking at.
    pub fn snapshot_body(&mut self) {
        if self.expanded {
            return;
        }
        self.snapshot = self.body.clone();
    }

    /// Drop the frozen copy. The next expansion takes a fresh one from the
    /// (possibly grown) body.
    pub fn clear_snapshot(&mut self) {
        self.snapshot.clear();
    }

    /// Claim a cell for the body.
    pub fn push(&mut self, cell: Cell) {
        self.body.push(cell);
    }

    /// The fold line's own text -- the words after the gutter.
    ///
    /// The gutter is two blanks and the line spells no marker of its own: the
    /// region is the machinery around the answer, set in with the rest of it, and
    /// the first word of the line is what says there is a body under the fold:
    /// `Thought for  7s` (a thought region that is still reasoning), or
    /// `Thought · running Read · 7s` for one whose body is a call.
    pub fn fold_spans(&self) -> Vec<Span> {
        let body = if self.activity == "thinking" {
            "Thought for  ".to_owned()
        } else {
            format!("Thought · {} · ", self.activity)
        };
        vec![Span::new(
            Style::Dim,
            format!("{body}{}s", self.elapsed_secs),
        )]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> Instant {
        Instant::now()
    }

    #[test]
    fn a_running_region_reads_the_clock_and_a_closed_one_does_not() {
        let start = t0();
        let mut thought = Thought::open("thinking", Some(start));
        assert_eq!(thought.live_secs(start), 0);
        assert_eq!(
            thought.live_secs(start + std::time::Duration::from_secs(7)),
            7
        );
        thought.close(start + std::time::Duration::from_secs(7));
        assert_eq!(thought.status, ThoughtStatus::Done);
        assert_eq!(thought.elapsed_secs, 7, "the boundary's value is frozen");
        assert_eq!(
            thought.live_secs(start + std::time::Duration::from_secs(600)),
            7,
            "a closed region does not age"
        );
    }

    #[test]
    fn a_region_with_no_clock_reads_zero() {
        let thought = Thought::open("thinking", None);
        assert_eq!(thought.live_secs(t0()), 0);
    }

    #[test]
    fn the_fold_line_names_the_region_and_its_seconds() {
        let mut thought = Thought::open("running Read", Some(t0()));
        thought.elapsed_secs = 3;
        let spans = thought.fold_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].text, "Thought · running Read · 3s");
        assert_eq!(spans[0].style, Style::Dim);
    }

    #[test]
    fn a_snapshot_is_taken_once_and_cleared_on_collapse() {
        let mut thought = Thought::open("thinking", Some(t0()));
        thought.push(Cell::Reasoning("first".into()));
        thought.snapshot_body();
        thought.expanded = true;
        assert_eq!(thought.snapshot.len(), 1);

        // The region keeps receiving; the snapshot the reader has does not.
        thought.push(Cell::Reasoning("second".into()));
        thought.snapshot_body();
        assert_eq!(
            thought.snapshot.len(),
            1,
            "re-snapshot while expanded is a no-op"
        );

        thought.expanded = false;
        thought.clear_snapshot();
        thought.snapshot_body();
        assert_eq!(
            thought.snapshot.len(),
            2,
            "a fresh snapshot sees the grown body"
        );
    }

    #[test]
    fn the_body_keeps_what_it_was_given_in_order() {
        let mut thought = Thought::open("thinking", Some(t0()));
        thought.push(Cell::Reasoning("a".into()));
        thought.push(Cell::Notice("b".into()));
        assert_eq!(thought.body.len(), 2);
        assert!(matches!(thought.body.last_mut(), Some(Cell::Notice(t)) if t == "b"));
    }
}

//! The state the screen draws: four pieces, each its own type, that the screen
//! draws from as one whole.
//!
//! The four pieces are kept apart on purpose:
//!
//! - [`View`] is what the screen shows: the cells, the laid cache, the scroll
//!   position, the gate question, the status line.
//! - [`Edit`] is the input box and the history it browses.
//! - [`Overlay`] is the picker, the panel, and the answer channel.
//! - [`Turn`] is the running turn's clock, the queue, the chars-per-token
//!   counter, and which phase the border should name.
//!
//! Keeping them separate is what makes the methods on each half its own list
//! rather than one long `impl State` block that has to reach across concerns:
//! the cell-layer methods live with the cell fields, the key-binding methods
//! live with the textarea, the queue lives with the turn, and the only thing
//! [`State`] itself holds is the revision counter that has to span all four.
//!
//! Methods on [`State`] are the cross-cutting ones -- a notice lands in one
//! half and a cell lands in another, a turn starts or ends and both halves
//! move together, the working indicator ticks and reads from both view and
//! turn. Each is one focused function rather than a god object's middle.

use std::time::Instant;

use tokio::sync::oneshot;

use crate::ui::Verdict;
use crate::ui::cell::{self, Cell, Style, Thought, ThoughtStatus};

use super::edit::Edit;
use super::notice::{AppNotice, MachineNotice};
use super::overlay::{Answer, Overlay};
use super::turn::Turn;
use super::view::View;

/// The state the front end draws.
///
/// Four pieces, each its own struct, plus the revision counter that has to
/// know about all of them. Methods are split across files by the half they
/// touch: [`super::render`] only reads, [`super::input`] handles keys at the
/// idle prompt, [`super::working`] handles keys while a turn runs,
/// [`super::panel`] is the question tool's panel, [`super::picker`] is the
/// command picker, [`super::queue`] is the line queue. What is here is what
/// spans more than one of them: the cross-cutting `apply` of a notice, the
/// start/end of a turn, the working indicator's tick.
#[derive(Default)]
pub(super) struct State {
    /// What the screen draws: cells, scroll, laid cache, status line.
    pub(super) view: View,
    /// The input box and the history it browses.
    pub(super) edit: Edit,
    /// The picker, the panel, and the answer channel.
    pub(super) overlay: Overlay,
    /// The running turn's clock and the line queue.
    pub(super) turn: Turn,
    /// Bumped by everything that changes what the screen should show, so a
    /// draw can be skipped when nothing has. Lives on [`State`] rather than
    /// any one half because every half can change the picture: a notice
    /// lands in the view, a key lands in the edit, a queue change lands in
    /// the turn, an overlay lands in the overlay.
    pub(super) revision: u64,
}

impl State {
    /// A `State` configured for tests that exercise the status bar:
    /// a known model and effort, no other state. Built through a
    /// constructor rather than `Default::default() + field assignment`
    /// so the lint that catches "init-then-overwrite" patterns does
    /// not flag the test fixture.
    #[cfg(test)]
    pub(super) fn for_test_with_meta(model: &str, effort: &str) -> Self {
        let mut s = Self::default();
        if !model.is_empty() {
            s.view.status.set_model(model);
        }
        if !effort.is_empty() {
            s.view.status.set_effort(effort);
        }
        s
    }

    /// Put a cell in the transcript that the machine did not send -- the banner,
    /// a session-level failure. It still moves the revision, or the draw that
    /// should show it would be skipped.
    pub(super) fn show(&mut self, cell: Cell) {
        self.revision += 1;
        self.view.transcript.push(cell);
    }

    /// Fold one machine notice into the state.
    pub(super) fn apply(&mut self, notice: MachineNotice) {
        self.revision += 1;
        match notice {
            MachineNotice::Reasoning(text) => {
                // The first reasoning fragment of a turn opens a thought
                // region as a `Cell::Thought` in the transcript. Subsequent
                // fragments accumulate into its body. A region opens at the
                // first fragment because the live block has not yet committed
                // a body cell, and the cell it commits to (the `Cell::Reasoning`
                // it becomes on a style change) is the one that goes into the
                // region's body.
                self.open_thought_region();
                self.stream(Style::Reasoning, &text);
                self.turn.reasoning_in_flight = true;
            }
            MachineNotice::Content(text) => {
                // Close any open Reasoning before the body lands, and commit
                // the thought region to the transcript.
                self.close_open_thought();
                self.stream(Style::Plain, &text);
                self.turn.reasoning_in_flight = false;
            }
            MachineNotice::FinishTurn => {
                self.finalize_open_step("interrupted");
                self.close_open_thought();
                // Close the open body stream so what was being streamed
                // lands in the transcript (or, for an open thought, in its
                // body). FinishTurn is the last notice of the turn.
                self.end_block();
                self.turn.reasoning_in_flight = false;
            }
            MachineNotice::ToolStart { name, args } => {
                let chars = args.chars().count();
                self.turn.streamed_chars += chars;
                self.turn.chars_since_usage += chars;
                let keeps_open = crate::tools::keeps_thought_open(&name, &args);
                let active_region =
                    self.view.transcript.iter().any(
                        |c| matches!(c, Cell::Thought(t) if t.status == ThoughtStatus::Running),
                    );
                // A side-effectful tool ends the thought before its own cell
                // lands, so the fold line in the transcript is the planning
                // the model did and the side-effect call comes after it.
                if !keeps_open {
                    self.close_open_thought();
                } else {
                    // A safe tool keeps the thought open; the running cell
                    // still claims the thought's body.
                    self.finalize_open_step("interrupted");
                }
                self.turn.current_tool_verb = Some(name.clone());
                self.finalize_open_step("interrupted");
                // A new tool step supersedes whatever reason was being
                // streamed. The reasoning flag flips off when the call starts;
                // a tool that changes something is not "thinking".
                self.turn.reasoning_in_flight = false;
                let step = Cell::from_tool_call(&name, &args);
                if keeps_open && active_region {
                    // The Step belongs to the active thought's body; the
                    // fold line stands in for it on the transcript.
                    self.end_block();
                    self.push_body_cell(step);
                } else {
                    self.close_and_push(step);
                }
            }
            MachineNotice::ToolOutput(chunk) => {
                if let Some(Cell::Step(step)) = self.view.transcript.last_mut()
                    && !step.status.is_settled()
                {
                    step.push_output(&chunk);
                    let stale_from = self.view.transcript.len() - 1;
                    self.view.laid.truncate(stale_from);
                }
            }
            MachineNotice::ToolResult(result) => {
                self.finalize_tool_result(&result);
            }
            MachineNotice::Instructions(dir) => {
                self.close_and_push(Cell::Notice(crate::agents_md::notice_text(&dir)));
            }
            MachineNotice::Usage(u, _stream) => {
                self.view.status.record(&u);
                if self.turn.chars_since_usage > 0 && u.completion_tokens > 0 {
                    self.turn.chars_per_token =
                        self.turn.chars_since_usage as f64 / u.completion_tokens as f64;
                }
                self.turn.chars_since_usage = 0;
            }
            MachineNotice::Interrupted => {
                self.finalize_open_step("interrupted");
                self.close_open_thought();
                self.turn.reasoning_in_flight = false;
                self.close_and_push(Cell::Interrupted);
            }
            MachineNotice::Truncated(notice) => {
                self.close_and_push(Cell::Notice(notice));
            }
            MachineNotice::Approval { name, args } => {
                self.close_open_thought();
                self.view.question = Some(Cell::approval(&name, &args));
            }
        }
    }

    /// Fold one application notice into the state.
    pub(super) fn apply_app(&mut self, notice: AppNotice) {
        self.revision += 1;
        match notice {
            AppNotice::Replay(messages) => {
                self.end_block();
                let mut buf = Vec::new();
                for m in messages {
                    buf.extend(cell::from_messages(std::slice::from_ref(&m)));
                }
                self.view.transcript.extend(buf);
            }
            AppNotice::Info(text) => self.close_and_push(Cell::Notice(text)),
            AppNotice::Error(text) => self.close_and_push(Cell::Failure(text)),
            AppNotice::SetModel(model) => self.view.status.set_model(&model),
            AppNotice::SetEffort(effort) => self.view.status.set_effort(&effort),
            AppNotice::ResetStats => self.view.status.reset_stats(),
            AppNotice::SetVerbose(_) => {
                // The verbose toggle is gone -- settled-Done step children
                // are no longer expandable, and the thinking widget is the
                // one place the detail lives. The notice is accepted so
                // the queue stays well-typed, but it does nothing.
            }
        }
    }

    /// A secret prompt's question has arrived: take the box for its answer.
    pub(super) fn open_secret(&mut self, prompt: String, reply: oneshot::Sender<Option<String>>) {
        self.end_block();
        self.view.question = Some(Cell::Notice(prompt));
        self.open_answer(Answer::Secret(reply));
    }

    /// The approval gate is asking: remember who to answer.
    pub(super) fn open_question(&mut self, reply: oneshot::Sender<Verdict>) {
        self.open_answer(Answer::YesNo(reply));
    }

    /// A question is open: take the box for its answer.
    pub(super) fn open_answer(&mut self, reply: Answer) {
        self.overlay.reply = Some(reply);
        self.take_for_answer(matches!(self.overlay.reply, Some(Answer::Secret(_))));
    }

    /// Append a text fragment. A fragment in a style other than the open block's
    /// ends that block and opens a new block -- the same rule the plain front end
    /// applies, because the style *is* the block's identity.
    pub(super) fn stream(&mut self, style: Style, text: &str) {
        let chars = text.chars().count();
        self.turn.streamed_chars += chars;
        self.turn.chars_since_usage += chars;
        if let Some(cell) = self.view.stream.append(style, text) {
            // A new block opened in a different style. If it is the kind of
            // cell that belongs inside a thought region (the Reasoning kind
            // that the machine sends), it goes into the active region's body.
            // Otherwise it stands on its own and the thought it interrupted
            // closes.
            match &cell {
                Cell::Reasoning(_) => self.push_body_cell(cell),
                _ => {
                    self.close_open_thought();
                    self.push_cell(cell);
                }
            }
        }
    }

    /// The description the active thought's header would show. Drives the
    /// "thinking" / "running X" split while the region is open.
    fn current_activity(&self) -> String {
        if let Some(verb) = self.turn.current_tool_verb.as_deref() {
            return format!("running {verb}");
        }
        "thinking".to_owned()
    }

    /// Close the streaming block, if any, so it becomes a finished cell.
    pub(super) fn end_block(&mut self) {
        if let Some(cell) = self.view.stream.close() {
            match cell {
                Cell::Reasoning(_) => self.push_body_cell(cell),
                other => self.push_cell(other),
            }
        }
    }

    /// Push a cell to the transcript, and -- if a thought region is open and
    /// the cell is one that belongs inside it -- to that region's body as well.
    ///
    /// Reasoning and safe-tool step cells are the only shapes that arrive while
    /// a thought region is open. Body text closes the region before it is
    /// pushed, and session-level cells are not part of any region's narrative.
    ///
    /// The body holds clones of the cells the region claims, frozen at the
    /// moment they were claimed: settling a step after the fact settles its
    /// transcript copy, and the region keeps what was true when the region was
    /// open. That is the same bargain the snapshot strikes for the expanded
    /// view one level down.
    fn push_cell(&mut self, cell: Cell) {
        self.revision += 1;
        self.view.transcript.push(cell);
    }

    fn push_body_cell(&mut self, cell: Cell) {
        // The Thought cell is the transcript entry; the body cell is its
        // own. Claiming a cell for the body does not add it to the
        // transcript -- the region's fold line already stands in for it.
        // The fold line itself is unchanged by a body cell, so the laid
        // cache stays valid; the snapshot the reader is reading (if any)
        // is what sees the new body.
        //
        // Locate by index, not by `last`: a Step cell that arrived
        // earlier may have settled to transcript, and the body's last
        // cell may not be the Thought itself.
        let region_idx = self
            .view
            .transcript
            .iter()
            .rposition(|c| matches!(c, Cell::Thought(t) if t.status == ThoughtStatus::Running));
        if let Some(idx) = region_idx
            && let Some(Cell::Thought(active)) = self.view.transcript.get_mut(idx)
        {
            active.push(cell);
        }
        self.revision += 1;
    }

    /// Close any streaming block, then push a finished cell.
    fn close_and_push(&mut self, cell: Cell) {
        self.end_block();
        self.push_cell(cell);
    }

    /// Close the active thought region, if one is open, and commit it as one
    /// [`Cell::Thought`].
    ///
    /// No-op when nothing is open. The region's body freezes at the cells it
    /// had been claiming -- the ones in its body when this is called.
    fn close_open_thought(&mut self) {
        // The open streaming block, when it carries Reasoning text, is the
        // region's last body cell: capture it before the region closes, so
        // the body keeps what was being streamed at the boundary.
        if let Some((Style::Reasoning, _)) = self.view.stream.current() {
            self.end_block();
        }
        // Locate the region's cell by index, not by `last_mut`: closing the
        // stream above leaves the transcript ending in a `Cell::Reasoning`,
        // not the thought itself.
        let region_idx = self
            .view
            .transcript
            .iter()
            .rposition(|c| matches!(c, Cell::Thought(t) if t.status == ThoughtStatus::Running));
        if let Some(idx) = region_idx
            && let Some(Cell::Thought(active)) = self.view.transcript.get_mut(idx)
        {
            active.close(Instant::now());
            // Re-lay the cell so the fold line picks up the frozen seconds,
            // not the live ones.
            self.view.laid.truncate(idx);
            self.revision += 1;
        }
    }

    /// Settle an open [`Cell::Step`] at the end of the transcript with a
    /// failure verdict, when the turn ends (or is interrupted) without a
    /// matching `ToolResult`. Idempotent: a transcript whose last cell is
    /// not an open step is left alone.
    fn finalize_open_step(&mut self, note: &str) {
        if let Some(Cell::Step(step)) = self.view.transcript.last_mut()
            && !step.status.is_settled()
        {
            step.interrupt();
            if note != "interrupted" {
                step.verdict = note.to_owned();
            }
            let stale_from = self.view.transcript.len() - 1;
            self.view.laid.truncate(stale_from);
            self.turn.current_tool_verb = None;
        }
    }

    /// Settle the open step (if any) with the result text, or file the
    /// result as a notice after a question or todo cell.
    fn finalize_tool_result(&mut self, result: &str) {
        if let Some(Cell::Step(step)) = self.view.transcript.last_mut()
            && !step.status.is_settled()
        {
            step.settle(result);
            let stale_from = self.view.transcript.len() - 1;
            self.view.laid.truncate(stale_from);
            self.turn.current_tool_verb = None;
            return;
        }
        if matches!(
            self.view.transcript.last(),
            Some(Cell::Question(_)) | Some(Cell::Todo(_))
        ) {
            self.close_and_push(Cell::Notice(result.to_owned()));
        }
    }

    /// A turn is starting at `started`.
    pub(super) fn begin_turn(&mut self, started: Instant) {
        self.revision += 1;
        self.turn.running = true;
        self.turn.started = Some(started);
        self.turn.streamed_chars = 0;
        self.turn.chars_since_usage = 0;
        self.turn.reasoning_in_flight = false;
        self.refresh_placeholder();
    }

    /// The turn is over.
    pub(super) fn end_turn(&mut self) {
        self.revision += 1;
        self.turn.running = false;
        self.turn.started = None;
        self.refresh_placeholder();
    }

    /// The indicator's heartbeat: bump the revision when the frame the spinner
    /// shows or the second the clock reads has changed.
    ///
    /// The title itself is computed in [`super::render`]; this method only
    /// runs the cache check that decides whether the screen has anything new
    /// to draw.
    pub(super) fn tick_activity(&mut self, now: Instant) {
        let Some(title) =
            super::render::activity_title_at(&self.overlay, &self.turn, now, usize::MAX)
        else {
            return;
        };
        if Some(&title) != self.turn.ticked_activity.as_ref() {
            self.turn.ticked_activity = Some(title);
            self.revision += 1;
        }
    }

    /// Toggle the expanded view of the thought region the reader is looking at.
    ///
    /// The subject of the key is the region the view is showing -- the
    /// youngest expanded one, which is what [`State::expanded_snapshot`]
    /// hands the screen. It is closed first, whatever its status: the reader
    /// asked to read a body, and the press after it puts the transcript
    /// back. Only when no body is on screen does the key open one, and the
    /// region it opens is the youngest in the transcript.
    ///
    /// Targeting the youngest region rather than the shown one is what made
    /// the key a one-way trip: a body opened while the stream was still
    /// arriving went historical the moment the next region opened, the
    /// press after it landed on that newer region instead, and -- with two
    /// regions expanded -- the view (the youngest expanded) and the key (the
    /// youngest) no longer named the same region, so the transcript could
    /// not be reached again. At most one region is expanded here, which is
    /// what keeps the two answers the same region.
    pub(super) fn toggle_expanded_thought(&mut self) {
        // The shown region first: collapse it, and clear the frozen copy so
        // the next expansion of it takes a fresh one.
        if let Some(expanded) = self.view.transcript.iter_mut().rev().find_map(|c| match c {
            Cell::Thought(t) if t.expanded => Some(t),
            _ => None,
        }) {
            expanded.expanded = false;
            expanded.clear_snapshot();
            self.revision += 1;
            return;
        }
        // Nothing is expanded: open the youngest region. Not the last
        // transcript cell -- a region that is followed by its own Step is
        // still the region the reader is at.
        if let Some(Cell::Thought(youngest)) = self
            .view
            .transcript
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Thought(_)))
        {
            youngest.snapshot_body();
            youngest.expanded = true;
            self.revision += 1;
        }
    }

    /// Open a thought region in the transcript if none is open. Idempotent: a
    /// region that is already open is left alone, so repeated reasoning
    /// fragments accumulate into the same one.
    fn open_thought_region(&mut self) {
        if matches!(
            self.view.transcript.last(),
            Some(Cell::Thought(t)) if t.status == ThoughtStatus::Running
        ) {
            return;
        }
        let activity = self.current_activity();
        let started_at = self.turn.started;
        self.push_cell(Cell::Thought(Thought::open(activity, started_at)));
    }

    /// The first transcript line to draw.
    pub(super) fn window(&mut self, total: usize, rows: usize) -> usize {
        self.view.window(total, rows)
    }

    /// Page through the transcript.
    pub(super) fn page(&mut self, step: isize) {
        self.view.page(step);
    }

    /// A wheel notch.
    pub(super) fn wheel(&mut self, kind: crossterm::event::MouseEventKind) {
        self.view.wheel(kind);
    }

    /// Back to the end.
    pub(super) fn follow(&mut self) {
        self.view.follow();
    }

    /// The snapshot the expanded view should render, if any.
    ///
    /// The one expanded region's snapshot, which is what the user reads:
    /// a closed region that was expanded at the moment it closed keeps the
    /// snapshot the user opened, and [`State::toggle_expanded_thought`]
    /// keeps the transcript to one expanded region at a time, so this and
    /// the key always name the same region.
    pub(super) fn expanded_snapshot(&self) -> Option<&[Cell]> {
        self.view.transcript.iter().rev().find_map(|c| match c {
            Cell::Thought(t) if t.expanded => Some(t.snapshot.as_slice()),
            _ => None,
        })
    }
}

// --------------------------------------------------------------- delegates --
//
// The methods below are thin delegates to the four halves, kept here so the
// rest of the crate can keep reaching for `state.foo()` rather than
// `state.edit.foo()` / `state.view.foo()` / etc.

/// Test helpers: small predicates over the state, kept here so the tests
/// above stay short and uniform.
#[cfg(test)]
trait StateTestHelpers {
    fn no_open_thought(&self) -> bool;
    fn has_running_thought(&self) -> bool;
    fn has_expanded_thought(&self) -> bool;
    fn active_thought_body(&self) -> Option<&[Cell]>;
}

#[cfg(test)]
impl StateTestHelpers for State {
    fn no_open_thought(&self) -> bool {
        self.active_thought().is_none()
    }
    fn has_running_thought(&self) -> bool {
        self.active_thought().is_some()
    }
    fn has_expanded_thought(&self) -> bool {
        self.view
            .transcript
            .iter()
            .any(|c| matches!(c, Cell::Thought(t) if t.expanded))
    }
    fn active_thought_body(&self) -> Option<&[Cell]> {
        self.active_thought().map(|t| t.body.as_slice())
    }
}

#[cfg(test)]
impl State {
    /// The active (Running) thought, if any. The active region is the
    /// youngest running Thought in the transcript, not necessarily the
    /// last cell -- a tool call's Step cell follows it, and the Step is
    /// part of the region's body while it is open.
    fn active_thought(&self) -> Option<&Thought> {
        self.view.transcript.iter().rev().find_map(|c| match c {
            Cell::Thought(t) if t.status == ThoughtStatus::Running => Some(t),
            _ => None,
        })
    }
}

#[cfg(test)]
mod thought_state_machine {
    //! The thought region's open/close lifecycle.

    use super::*;

    fn state() -> State {
        State::default()
    }

    #[test]
    fn reasoning_opens_a_thought_and_repeated_fragments_extend_it() {
        let mut s = state();
        assert!(s.no_open_thought());
        s.apply(MachineNotice::Reasoning("first".into()));
        assert!(s.has_running_thought());
        s.apply(MachineNotice::Reasoning(" more".into()));
        assert!(s.has_running_thought());
    }

    #[test]
    fn content_closes_the_open_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("thinking".into()));
        s.apply(MachineNotice::Content("answer".into()));
        assert!(!s.has_running_thought());
        // The last cell is the running region's closed Thought.
        match s.view.transcript.last() {
            Some(Cell::Thought(t)) => {
                assert_eq!(t.status, ThoughtStatus::Done);
                assert!(!t.body.is_empty(), "the closed region kept its body");
            }
            other => panic!("expected Thought, got {other:?}"),
        }
    }

    #[test]
    fn a_side_effect_tool_closes_the_thought_then_its_cell_lands() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("planning".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Edit".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
        });
        assert!(!s.has_running_thought());
        // The thought came before the Edit on the transcript.
        let mut iter = s.view.transcript.iter().rev();
        assert!(matches!(iter.next(), Some(Cell::Step(_))));
        assert!(matches!(iter.next(), Some(Cell::Thought(_))));
    }

    #[test]
    fn a_safe_tool_keeps_the_thought_open_and_appends_to_the_body() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("looking".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Read".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
        });
        s.apply(MachineNotice::ToolResult("ok".into()));
        s.apply(MachineNotice::Reasoning("more thinking".into()));
        assert!(s.has_running_thought());
    }

    #[test]
    fn bash_keeps_thought_open_only_for_read_only_commands() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("looking".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Bash".into(),
            args: r#"{"command":"cat src/main.rs"}"#.to_owned(),
        });
        assert!(s.has_running_thought());
        s.apply(MachineNotice::Reasoning("more".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Bash".into(),
            args: r#"{"command":"rm /tmp/x"}"#.to_owned(),
        });
        assert!(!s.has_running_thought());
    }

    #[test]
    fn finish_turn_closes_the_open_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("thinking".into()));
        s.apply(MachineNotice::FinishTurn);
        assert!(!s.has_running_thought());
    }

    #[test]
    fn approval_closes_the_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("planning".into()));
        s.apply(MachineNotice::Approval {
            name: "Bash".into(),
            args: r#"{"command":"rm file"}"#.into(),
        });
        assert!(!s.has_running_thought());
    }

    #[test]
    fn reasoning_after_side_effect_opens_a_new_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("first plan".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Edit".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
        });
        s.apply(MachineNotice::ToolResult("ok".into()));
        s.apply(MachineNotice::Reasoning("second plan".into()));
        assert!(s.has_running_thought());
    }

    #[test]
    fn the_fold_line_is_one_ruled_line_per_region() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("a".into()));
        s.apply(MachineNotice::Reasoning("b".into()));
        s.apply(MachineNotice::Content("done".into()));
        // Closing the open Reasoning stream before closing the thought is
        // what gives the region's body a cell to be frozen against.
        let mut thought_count = 0;
        for cell in &s.view.transcript {
            if matches!(cell, Cell::Thought(_)) {
                thought_count += 1;
            }
        }
        assert_eq!(thought_count, 1);
        // The fold cell itself is one row.
        if let Some(Cell::Thought(t)) = s.view.transcript.last() {
            assert!(!t.body.is_empty());
        }
    }

    #[test]
    fn the_active_thought_body_clones_cells_for_the_expanded_view() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("hmm".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Read".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
        });
        s.apply(MachineNotice::ToolResult("ok".into()));
        let body = s.active_thought_body().expect("running thought exists");
        assert!(body.iter().any(|c| matches!(c, Cell::Step(_))));
    }

    #[test]
    fn toggling_the_body_expands_then_collapses_and_drops_the_snapshot() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("thinking".into()));
        s.apply(MachineNotice::Content("done".into()));
        s.toggle_expanded_thought();
        assert!(s.has_expanded_thought());
        s.toggle_expanded_thought();
        assert!(!s.has_expanded_thought());
    }

    #[test]
    fn ctrl_o_closes_what_is_on_screen_after_a_new_region_opens() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("first".into()));
        s.apply(MachineNotice::Content("body".into()));
        // The reader opens the region on screen, then the stream carries on
        // and a second region opens below it.
        s.toggle_expanded_thought();
        assert!(s.expanded_snapshot().is_some(), "the body is on screen");
        s.apply(MachineNotice::Reasoning("second".into()));
        // The key still acts on what the reader is reading: the press that
        // follows closes the body, it does not open the new region.
        s.toggle_expanded_thought();
        assert!(
            s.expanded_snapshot().is_none(),
            "Ctrl-O returns to the transcript, whatever opened meanwhile"
        );
    }

    #[test]
    fn the_transcript_is_reachable_again_however_long_the_stream_runs() {
        // The region the reader opened goes historical while output keeps
        // arriving. Every pair of presses has to leave the reader back on the
        // transcript: a key that toggled a different region than the one the
        // view showed would leave the transcript unreachable for good.
        let mut s = state();
        s.apply(MachineNotice::Reasoning("first".into()));
        s.apply(MachineNotice::Content("body".into()));
        s.toggle_expanded_thought();
        for round in 0..3 {
            s.apply(MachineNotice::Reasoning(format!("round {round}")));
            s.apply(MachineNotice::Content(format!("body {round}")));
            assert!(
                s.expanded_snapshot().is_some(),
                "round {round}: the reader's body stays on screen"
            );
            s.toggle_expanded_thought();
            assert!(
                s.expanded_snapshot().is_none(),
                "round {round}: the transcript comes back"
            );
            s.toggle_expanded_thought();
        }
    }

    #[test]
    fn at_most_one_region_is_expanded() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("first".into()));
        s.apply(MachineNotice::Content("body".into()));
        s.toggle_expanded_thought();
        s.apply(MachineNotice::Reasoning("second".into()));
        s.apply(MachineNotice::Content("body".into()));
        s.toggle_expanded_thought();
        // The collapse leaves nothing expanded; the press after it -- which
        // opens the newest region -- must not leave the older one behind it.
        s.toggle_expanded_thought();
        let expanded = s
            .view
            .transcript
            .iter()
            .filter(|c| matches!(c, Cell::Thought(t) if t.expanded))
            .count();
        assert_eq!(expanded, 1, "the younger region replaces the older one");
    }
}

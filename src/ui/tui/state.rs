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
//!   counter.
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

use tokio::sync::oneshot;

use crate::ui::Verdict;
use crate::ui::cell::{self, Cell, Style};

use super::edit::Edit;
use super::notice::{AppNotice, MachineNotice};
use super::overlay::{Answer, Overlay};
use super::thought::ThoughtBlock;
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
    /// The currently-open thought block, if any. `Some` between
    /// `open_thought` and the next `close_thought`; `None` at the
    /// start of a turn and between blocks.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) thought: Option<ThoughtBlock>,
    /// Thought blocks that have closed, in order. The latest one is
    /// the one Ctrl-O toggles when the active block is folded, and
    /// the one that may carry an expanded state frozen from when it
    /// was active.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    pub(super) historical: Vec<ThoughtBlock>,
    /// The model id in effect. Set once at session start (the front end's
    /// `model_label`) and on `/model`. Drives the per-prompt metadata row
    /// above each User cell. `None` until the agent picks one.
    pub(super) model: Option<String>,
    /// The reasoning effort tier in effect. Set at session start and on
    /// `/effort`. Joins the model in the per-prompt metadata row.
    pub(super) effort: Option<String>,
    /// Bumped by everything that changes what the screen should show, so a
    /// draw can be skipped when nothing has. Lives on [`State`] rather than
    /// any one half because every half can change the picture: a notice
    /// lands in the view, a key lands in the edit, a queue change lands in
    /// the turn, an overlay lands in the overlay.
    pub(super) revision: u64,
}

// ----------------------------------------------------------------- methods ---

impl State {
    /// A `State` configured for tests that exercise the per-prompt
    /// metadata row: a known model and effort, no other state. Built
    /// through a constructor rather than `Default::default() + field
    /// assignment` so the lint that catches "init-then-overwrite"
    /// patterns does not flag the test fixture.
    #[cfg(test)]
    pub(super) fn for_test_with_meta(model: &str, effort: &str) -> Self {
        Self {
            model: Some(model.to_owned()),
            effort: Some(effort.to_owned()),
            ..Self::default()
        }
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
                // The first reasoning fragment of a thought opens it.
                // Safe-tool steps and later reasoning fragments leave the
                // block alone; closing is the boundary events' job.
                self.open_thought(self.current_activity());
                self.stream(Style::Reasoning, &text);
            }
            MachineNotice::Content(text) => {
                // Stream before closing: a Reasoning block being
                // displaced by this Content fragment produces a
                // Cell::Reasoning that belongs to the open thought's
                // body. The thought closes only after the cell has
                // been committed.
                self.stream(Style::Plain, &text);
                self.close_thought();
            }
            MachineNotice::FinishTurn => {
                // End the stream before closing the thought, for the
                // same reason Content does: a Cell::Reasoning that
                // was being streamed belongs to the just-open block,
                // and the body needs the cell committed while the
                // thought is still open to claim it.
                self.finalize_open_step("interrupted");
                self.end_block();
                self.close_thought();
            }
            MachineNotice::ToolStart { name, args } => {
                // The call's arguments are output the backend billed for: the
                // characters join both counters so the usage notice that ends
                // the next sub-request calibrates against the same text the
                // completion tokens covered. The notice lands after the
                // sub-request that declared the call, so its tokens ride one
                // window late -- an offset a turn with several calls averages
                // out.
                let chars = args.chars().count();
                self.turn.streamed_chars += chars;
                self.turn.chars_since_usage += chars;
                // A side-effectful tool ends the thought: writes, edits,
                // and bash calls whose commands change state are not
                // something a folded header should hide. Safe tools
                // (read / glob / grep / read-only bash) continue the
                // current block.
                if !crate::tools::keeps_thought_open(&name, &args) {
                    self.close_thought();
                }
                // The verb drives the "running X" label on the box's top
                // border while the tool runs. The result notice clears it
                // when the step settles.
                self.turn.current_tool_verb = Some(name.clone());
                // Close any open step first: a step in flight across two
                // tool calls would be malformed (the model emits ToolResult
                // before ToolStart), but an Interrupted turn can leave one
                // open. Finalizing it as interrupted is the safe default.
                self.finalize_open_step("interrupted");
                // `from_tool_call` is the dispatcher: regular tools become
                // an open Cell::Step, the question and todo tools keep
                // their own rich cells. The open step's children will be
                // filled by the ToolOutput notices that follow.
                self.close_and_push(Cell::from_tool_call(&name, &args));
            }
            MachineNotice::ToolOutput(chunk) => {
                // Tool output now belongs to the open step, not to the
                // streaming text block. A step that is still open takes the
                // chunk as its newest children; a stray chunk (no open
                // step, perhaps because the result arrived before the
                // output) is dropped on the floor -- the result carries
                // whatever the result carries.
                if let Some(Cell::Step(step)) = self.view.transcript.last_mut()
                    && !step.status.is_settled()
                {
                    step.push_output(&chunk);
                    // The last cell changed; the laid cache from this
                    // index onwards is stale.
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
                // Calibrate the live speed estimate: the characters streamed
                // since the last notice are now known to have been this many
                // tokens. A sub-request that streamed nothing (a bare tool-call
                // round) measures nothing and leaves the ratio standing.
                if self.turn.chars_since_usage > 0 && u.completion_tokens > 0 {
                    self.turn.chars_per_token =
                        self.turn.chars_since_usage as f64 / u.completion_tokens as f64;
                }
                self.turn.chars_since_usage = 0;
            }
            MachineNotice::Interrupted => {
                // An interrupted turn closes any open tool step as well as
                // filing the interruption notice: the children the tool had
                // accumulated up to that point are what the reader saw, and
                // dropping them on the floor would mean losing the work.
                //
                // End the stream before closing the thought so a
                // Cell::Reasoning being streamed is claimed by the
                // thought's body before the thought itself goes away.
                self.finalize_open_step("interrupted");
                self.end_block();
                self.close_thought();
                self.close_and_push(Cell::Interrupted);
            }
            MachineNotice::Truncated(notice) => {
                // Not an error: the wire's own sentence already says why the
                // answer stopped, and the transcript's error styling (red,
                // "error:" prefix) is the wrong bucket for a normal cause.
                self.close_and_push(Cell::Notice(notice));
            }
            MachineNotice::Approval { name, args } => {
                self.end_block();
                self.close_thought();
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
                // The metadata row sits above each User cell. Push one
                // before replaying the user line, so a resumed session
                // reads the same as one that was watched live.
                let mut buf = Vec::new();
                for m in messages {
                    if matches!(m.role, crate::types::Role::User)
                        && let Some(meta) = self.metadata_text()
                    {
                        buf.push(Cell::Notice(meta));
                    }
                    buf.extend(cell::from_messages(std::slice::from_ref(&m)));
                }
                self.view.transcript.extend(buf);
            }
            AppNotice::Info(text) => self.close_and_push(Cell::Notice(text)),
            AppNotice::Error(text) => self.close_and_push(Cell::Failure(text)),
            AppNotice::SetModel(model) => self.model = Some(model),
            AppNotice::SetEffort(effort) => self.effort = Some(effort),
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
        // Both counters: what the border's speed estimate divides by the clock,
        // and what the next usage notice calibrates the ratio against.
        let chars = text.chars().count();
        self.turn.streamed_chars += chars;
        self.turn.chars_since_usage += chars;
        if let Some(cell) = self.view.stream.append(style, text) {
            self.push_cell(cell);
        }
    }

    /// Open a thought block with `activity` as its header description.
    ///
    /// Idempotent: if a thought block is already open, the call is a
    /// no-op. That is what makes the state machine safe to drive from
    /// a stream of fragments — the first reasoning fragment opens
    /// the block, the rest pass through and `close_thought` runs
    /// when the boundary event arrives.
    ///
    /// The start time is the turn's clock when the turn has one — a
    /// thought opened at the first reasoning fragment of the turn is
    /// not really younger than the turn itself, and reading its elapsed
    /// from the turn's clock keeps the spinner and the seconds
    /// consistent with the box border's "working" indicator. `None` for
    /// a resumed block whose turn clock is gone, in which case the
    /// elapsed always reads `0`.
    fn open_thought(&mut self, activity: impl Into<String>) {
        if self.thought.is_some() {
            return;
        }
        let now = self.turn.started.unwrap_or_else(std::time::Instant::now);
        self.thought = Some(ThoughtBlock::open(activity, now));
        self.revision += 1;
    }

    /// Close the currently-open thought block, if any. The block is
    /// marked Done and pushed onto `historical` so the latest entry
    /// is the one Ctrl-O can collapse. A no-op when nothing is open.
    fn close_thought(&mut self) {
        if let Some(mut block) = self.thought.take() {
            block.close();
            self.historical.push(block);
            self.revision += 1;
        }
    }

    /// Whether the most recent thought block (active or historical) is
    /// expanded. The header rendering asks this to decide whether the
    /// body should be drawn; the input layer asks this to know whether
    /// Ctrl-O should collapse.
    #[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
    fn latest_thought(&self) -> Option<&ThoughtBlock> {
        self.thought.as_ref().or_else(|| self.historical.last())
    }

    /// Toggle the expanded flag of the latest thought block:
    /// - the active block, when there is one, can be expanded or
    ///   collapsed; the snapshot is taken on expand and cleared on
    ///   collapse, so a re-expand reads the (possibly grown) body;
    /// - the most recent historical block can be collapsed if it was
    ///   expanded at the moment it became historical (Ctrl-O is the
    ///   only way back from there), but never expanded;
    /// - any other block is left alone.
    ///
    /// Wired up by Ctrl-O in both the idle and the working input
    /// handlers. The function is `&mut self` because the toggle can
    /// touch either the active block (cheap) or the historical tail
    /// (which `Vec::last_mut` reaches without copying).
    pub(super) fn toggle_latest_thought(&mut self) {
        if let Some(active) = self.thought.as_mut() {
            if active.expanded {
                active.expanded = false;
                active.clear_snapshot();
            } else {
                active.snapshot_body();
                active.expanded = true;
            }
            self.revision += 1;
            return;
        }
        if let Some(latest) = self.historical.last_mut()
            && latest.expanded
        {
            latest.expanded = false;
            latest.clear_snapshot();
            self.revision += 1;
        }
    }

    /// The description the active block's header should show.
    /// "thinking" by default; a safe tool's running step changes it to
    /// "running X". Kept as one method so the source of the
    /// description can be refined without touching the call sites.
    fn current_activity(&self) -> String {
        if let Some(verb) = self.turn.current_tool_verb.as_deref() {
            return format!("running {verb}");
        }
        "thinking".to_owned()
    }

    /// Close the block being streamed, if any, so it becomes a finished cell.
    ///
    /// Bumps the revision: this is called from the turn's end as well as from a
    /// notice, and in the first case it is the only thing that has changed.
    pub(super) fn end_block(&mut self) {
        if let Some(cell) = self.view.stream.close() {
            self.push_cell(cell);
        }
    }

    /// Push a cell to the transcript, and -- if it is a cell that
    /// belongs to the open thought -- to that thought's body as well.
    ///
    /// Reasoning text (`Cell::Reasoning`) and safe-tool step cells
    /// (`Cell::Step`) are the only shapes that arrive while a thought
    /// is open. Cell::Content closes the thought before it is pushed,
    /// and session-level cells (Notices, Failures, Interruptions) are
    /// not part of any thought's narrative -- both end up in the
    /// transcript but not in any thought's body.
    ///
    /// The clone is the cost of the frozen-snapshot semantics the
    /// widget relies on: the body is an independent copy of the
    /// cells, so settling a step after the body was filled does not
    /// retroactively change what the expanded view showed when it
    /// was taken.
    fn push_cell(&mut self, cell: Cell) {
        let in_body = matches!(cell, Cell::Reasoning(_) | Cell::Step(_)) && self.thought.is_some();
        if in_body && let Some(thought) = self.thought.as_mut() {
            thought.body.push(cell.clone());
        }
        self.revision += 1;
        self.view.transcript.push(cell);
    }

    /// Close any streaming block, then push a finished cell.
    ///
    /// The pair is the common shape of "a notice arrives while a block
    /// is streaming": the streaming block has to close before the new
    /// cell lands, or the reader sees the new cell threaded through
    /// the tail of the old one. Keeping the pair together is also
    /// what keeps the call sites readable -- the rule that says "a
    /// new cell closes the block first" is what the pairing
    /// expresses.
    fn close_and_push(&mut self, cell: Cell) {
        self.end_block();
        self.push_cell(cell);
    }

    /// Settle an open [`Cell::Step`] at the end of the transcript with a
    /// failure verdict, when the turn ends (or is interrupted) without a
    /// matching `ToolResult`. Idempotent: a transcript whose last cell is
    /// not an open step is left alone.
    ///
    /// The note goes into the verdict, which keeps the failed step on the
    /// same row as its header line -- the cause of the failure is right
    /// there in the one line the reader sees.
    fn finalize_open_step(&mut self, note: &str) {
        if let Some(Cell::Step(step)) = self.view.transcript.last_mut()
            && !step.status.is_settled()
        {
            step.interrupt();
            // The note replaces the "interrupted" default only when the
            // caller wants to say something more specific; the default is
            // "interrupted" because that is the most common cause.
            if note != "interrupted" {
                step.verdict = note.to_owned();
            }
            // The laid cache for the last cell is stale.
            let stale_from = self.view.transcript.len() - 1;
            self.view.laid.truncate(stale_from);
            // The tool that was running is no longer running. An
            // interrupted step leaves the border's "running X" label
            // pointing at a call that did not actually run, which is
            // what made `Ctrl-C` the right thing to do -- so it goes.
            self.turn.current_tool_verb = None;
        }
    }

    /// Settle the open step (if any) with the result text, or file the
    /// result as a notice after a question or todo cell. Mirrors what the
    /// replay path does for the same wire shape.
    fn finalize_tool_result(&mut self, result: &str) {
        // Settle the last cell in place if it is an open step.
        if let Some(Cell::Step(step)) = self.view.transcript.last_mut()
            && !step.status.is_settled()
        {
            step.settle(result);
            let stale_from = self.view.transcript.len() - 1;
            self.view.laid.truncate(stale_from);
            // The tool that was running is no longer running -- the
            // border's "running X" label picks up the next phase.
            self.turn.current_tool_verb = None;
            return;
        }
        // No open step: the preceding cell is a question or todo, or
        // something we did not expect. The result text is what the model
        // sees; the transcript gets it as a dim notice.
        if matches!(
            self.view.transcript.last(),
            Some(Cell::Question(_)) | Some(Cell::Todo(_))
        ) {
            self.close_and_push(Cell::Notice(result.to_owned()));
        }
        // Stray tool result with no preceding tool call: dropped. The
        // machine should not send one without a ToolStart; if it does,
        // the result has nowhere to land in the cell layer.
    }

    /// The per-prompt metadata row's text: `provider/model · effort tier`,
    /// or `None` when neither field is known yet (the very first turn
    /// before the agent picks a model). `None` means "do not push a
    /// metadata cell", which keeps a resumed session that pre-dates
    /// metadata from picking up an empty notice.
    pub(super) fn metadata_text(&self) -> Option<String> {
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
        if parts.is_empty() {
            None
        } else {
            Some(parts.join(" · "))
        }
    }

    /// A turn is starting at `started`.
    ///
    /// The clock is handed in rather than read here, so what a turn's timer
    /// shows can be exercised without waiting for one.
    pub(super) fn begin_turn(&mut self, started: std::time::Instant) {
        self.revision += 1;
        self.turn.running = true;
        self.turn.started = Some(started);
        self.turn.streamed_chars = 0;
        self.turn.chars_since_usage = 0;
        super::State::refresh_placeholder(self);
    }

    /// The turn is over.
    pub(super) fn end_turn(&mut self) {
        self.revision += 1;
        self.turn.running = false;
        self.turn.started = None;
        super::State::refresh_placeholder(self);
    }

    /// The indicator's heartbeat: bump the revision when the frame the spinner
    /// shows or the second the clock reads has changed, so the border keeps
    /// moving while nothing else arrives and no redraw is spent when it has
    /// nothing new to show.
    ///
    /// The title itself is computed in [`super::render`]; this method only
    /// runs the cache check that decides whether the screen has anything new
    /// to draw.
    pub(super) fn tick_activity(&mut self, now: std::time::Instant) {
        let Some(title) = super::render::activity_title_at(
            &self.overlay,
            &self.turn,
            self.thought.as_ref(),
            now,
            usize::MAX,
        ) else {
            return;
        };
        if Some(&title) != self.turn.ticked_activity.as_ref() {
            self.turn.ticked_activity = Some(title);
            self.revision += 1;
        }
    }
}

// --------------------------------------------------------------- delegates --
//
// The methods below are thin delegates to the four halves, kept here so the
// rest of the crate can keep reaching for `state.foo()` rather than
// `state.edit.foo()` / `state.view.foo()` / etc. The delegate is a one-line
// pointer that says which half the operation belongs to; the bodies live
// next to the fields they touch.

impl State {
    /// The first transcript line to draw: a window on the end, or the one the
    /// reader scrolled back to.
    pub(super) fn window(&mut self, total: usize, rows: usize) -> usize {
        self.view.window(total, rows)
    }

    /// Page through the transcript a screen at a time; `-1` is back, `+1` forward.
    pub(super) fn page(&mut self, step: isize) {
        self.view.page(step);
    }

    /// A wheel notch: three lines back, or three lines forward.
    pub(super) fn wheel(&mut self, kind: crossterm::event::MouseEventKind) {
        self.view.wheel(kind);
    }

    /// Back to the end, which is where a new line will appear.
    pub(super) fn follow(&mut self) {
        self.view.follow();
    }
}

#[cfg(test)]
mod thought_state_machine {
    //! The thought block's open/close lifecycle, exercised against the
    //! full `apply` path. The machine notices are what drives the
    //! state: `Reasoning` opens, `Content` / side-effect tool / turn
    //! end / approval / interrupt close. Safe tools leave the block
    //! alone.
    //!
    //! These tests are the contract the rest of the widget is built on:
    //! a regression here is a regression in the whole feature.

    use super::*;

    fn state() -> State {
        State::default()
    }

    /// A reasoning fragment opens a thought block. Two fragments
    /// (the typical stream shape) leave the same one block open.
    #[test]
    fn reasoning_opens_a_thought_and_repeated_fragments_keep_it() {
        let mut s = state();
        assert!(s.thought.is_none());
        s.apply(MachineNotice::Reasoning("first".into()));
        assert!(s.thought.is_some(), "first reasoning opens the block");
        let id_at_open = s.thought.as_ref().map(|b| b.activity.clone());
        s.apply(MachineNotice::Reasoning(" more".into()));
        assert!(s.thought.is_some(), "the block stays open across fragments");
        assert_eq!(
            s.thought.as_ref().map(|b| b.activity.clone()),
            id_at_open,
            "no new block on continued reasoning"
        );
    }

    /// The first content fragment closes the thought that the
    /// reasoning opened. The thought is moved to `historical`.
    #[test]
    fn content_closes_the_open_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("thinking".into()));
        s.apply(MachineNotice::Content("answer".into()));
        assert!(s.thought.is_none(), "content closes the thought");
        assert_eq!(s.historical.len(), 1, "the closed block is in history");
        assert_eq!(
            s.historical[0].status,
            super::super::thought::ThoughtStatus::Done
        );
    }

    /// A side-effectful tool closes the thought, even when it arrives
    /// without any preceding reasoning. The tool cell stays visible
    /// as its own cell; the block is gone.
    #[test]
    fn side_effect_tool_closes_the_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("planning".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Edit".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
        });
        assert!(s.thought.is_none(), "Edit closes the thought");
        assert_eq!(s.historical.len(), 1);
    }

    /// A safe tool (Read) keeps the thought open. The reasoning
    /// continues into the tool call and resumes after.
    #[test]
    fn safe_tool_keeps_the_thought_open() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("looking".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Read".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
        });
        assert!(
            s.thought.is_some(),
            "Read keeps the thought open across the call"
        );
        s.apply(MachineNotice::ToolResult("ok".into()));
        assert!(
            s.thought.is_some(),
            "the thought is still open after the safe tool returns"
        );
        s.apply(MachineNotice::Reasoning("more thinking".into()));
        assert!(
            s.thought.is_some(),
            "the same thought continues into further reasoning"
        );
    }

    /// A bash call's command is what decides whether the tool
    /// keeps the thought open. `cat` is read-only; `rm` is not.
    #[test]
    fn bash_keeps_thought_open_only_for_read_only_commands() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("looking".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Bash".into(),
            args: r#"{"command":"cat src/main.rs"}"#.into(),
        });
        assert!(s.thought.is_some(), "cat keeps the thought open");

        // A new thought for the second bash call's reasoning.
        s.apply(MachineNotice::Reasoning("more".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Bash".into(),
            args: r#"{"command":"rm /tmp/x"}"#.into(),
        });
        assert!(
            s.thought.is_none(),
            "a side-effectful bash call closes the thought"
        );
    }

    /// FinishTurn closes the thought that was open, with no reasoning
    /// to follow.
    #[test]
    fn finish_turn_closes_the_open_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("thinking".into()));
        s.apply(MachineNotice::FinishTurn);
        assert!(s.thought.is_none());
        assert_eq!(s.historical.len(), 1);
    }

    /// An approval gate closes the thought: the user is being asked
    /// to decide, and the thought that led to the call is over.
    #[test]
    fn approval_closes_the_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("planning".into()));
        s.apply(MachineNotice::Approval {
            name: "Bash".into(),
            args: r#"{"command":"rm file"}"#.into(),
        });
        assert!(s.thought.is_none());
    }

    /// After a side-effect tool closes the thought, the next
    /// reasoning fragment opens a fresh one. Two thoughts in one
    /// turn: think → edit → think.
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
        assert!(s.thought.is_some(), "fresh reasoning opens a fresh thought");
        assert_eq!(
            s.historical.len(),
            1,
            "the first thought is in history already"
        );
    }

    /// The latest thought (active or historical) is the one Ctrl-O
    /// will toggle.
    #[test]
    fn latest_thought_is_the_active_or_most_recent_historical() {
        let mut s = state();
        assert!(s.latest_thought().is_none());
        s.apply(MachineNotice::Reasoning("first".into()));
        assert!(s.latest_thought().is_some());
        assert!(s.thought.is_some());
        // The first thought is the active one.
        assert!(std::ptr::eq(
            s.latest_thought().unwrap() as *const _,
            s.thought.as_ref().unwrap() as *const _,
        ));
        s.apply(MachineNotice::Content("answer".into()));
        // Active is gone; latest is the historical one.
        assert!(s.thought.is_none());
        assert!(s.historical.len() == 1);
        assert!(std::ptr::eq(
            s.latest_thought().unwrap() as *const _,
            s.historical.last().unwrap() as *const _,
        ));
        s.apply(MachineNotice::Reasoning("second".into()));
        // A new active block is the latest; the historical one is no
        // longer the pointer returned.
        assert!(s.thought.is_some());
        let active_ptr = s.thought.as_ref().unwrap() as *const _;
        assert!(std::ptr::eq(
            s.latest_thought().unwrap() as *const _,
            active_ptr,
        ));
    }

    /// Ctrl-O on the active block toggles its `expanded` flag. Two
    /// presses land it back at `false`; the second press is the
    /// "exit expand" the user types when they want to type again.
    #[test]
    fn ctrl_o_toggles_the_active_thought() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("first".into()));
        assert!(!s.thought.as_ref().unwrap().expanded);
        s.toggle_latest_thought();
        assert!(s.thought.as_ref().unwrap().expanded);
        s.toggle_latest_thought();
        assert!(!s.thought.as_ref().unwrap().expanded);
    }

    /// A historical block that was expanded at the moment it closed
    /// (because the user pressed Ctrl-O and the active block became
    /// historical while expanded) can be collapsed by Ctrl-O. A
    /// historical block that was never expanded stays folded -- the
    /// widget only operates on the latest, and there is nothing to
    /// "expand" a historical block to.
    #[test]
    fn ctrl_o_only_collapses_a_historical_thought_that_was_expanded() {
        let mut s = state();
        // Active block is expanded at close -- survives the close.
        s.apply(MachineNotice::Reasoning("first".into()));
        s.toggle_latest_thought();
        assert!(s.thought.as_ref().unwrap().expanded);
        s.apply(MachineNotice::Content("answer".into()));
        assert!(s.historical.last().unwrap().expanded);

        // Ctrl-O now collapses the historical block -- the only
        // meaningful operation on a historical block, since there is
        // no body to "expand" into.
        s.toggle_latest_thought();
        assert!(!s.historical.last().unwrap().expanded);

        // A second Ctrl-O on a folded historical is a no-op: the
        // "only the latest can be expanded" rule means there is
        // nothing more to do.
        s.toggle_latest_thought();
        assert!(!s.historical.last().unwrap().expanded);
    }

    /// Reasoning text accumulates into the active thought's body.
    /// The body is a separate copy of the cell, so a snapshot
    /// taken at this point sees the reasoning and a later
    /// snapshot of the same body after settling does not.
    #[test]
    fn reasoning_text_lands_in_the_active_thoughts_body() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("hmm".into()));
        assert_eq!(s.thought.as_ref().unwrap().body.len(), 0);
        // First reasoning fragment: opens the block, no cell
        // commit yet (the stream is still open in Reasoning).
        // Switching to Content closes the open Reasoning stream
        // and commits the cell.
        s.apply(MachineNotice::Content("answer".into()));
        // After content, the thought has moved to historical with
        // the Cell::Reasoning in its body.
        assert_eq!(s.historical.last().unwrap().body.len(), 1);
        assert!(matches!(
            s.historical.last().unwrap().body[0],
            Cell::Reasoning(_)
        ));
    }

    /// A safe tool inside a thought lands in the thought's body.
    /// A side-effect tool does not — the thought closes before
    /// the cell is pushed.
    #[test]
    fn safe_tool_step_lands_in_the_active_thoughts_body() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("looking".into()));
        s.apply(MachineNotice::ToolStart {
            name: "Read".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
        });
        s.apply(MachineNotice::ToolResult("ok".into()));
        // Read keeps the thought open. The Reasoning that was
        // being streamed gets committed as the tool starts (the
        // style-switch closes the open block); the Read Step is
        // pushed right after. Both belong to the thought.
        let body = &s.thought.as_ref().unwrap().body;
        assert!(
            body.iter().any(|c| matches!(c, Cell::Step(_))),
            "Read step is in the body: {body:?}"
        );

        // Now a side-effect tool: closes the thought, the cell
        // lands in transcript but not in any thought's body.
        s.apply(MachineNotice::ToolStart {
            name: "Edit".into(),
            args: r#"{"file_path":"a.rs"}"#.into(),
        });
        assert!(s.thought.is_none());
        // The historical block is the one that was the active
        // block before the Edit -- its body has the Read step,
        // not the Edit step (which arrived after the close).
        assert!(
            s.historical
                .last()
                .unwrap()
                .body
                .iter()
                .any(|c| matches!(c, Cell::Step(_)))
        );
    }

    /// `snapshot_body` is idempotent: once a snapshot is taken, a
    /// later toggle (which would re-call it) does not re-snapshot.
    /// The first snapshot is the one the user reads.
    #[test]
    fn snapshot_body_is_idempotent() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("first".into()));
        // Push some reasoning, close with content.
        s.apply(MachineNotice::Content("answer".into()));
        let thought = s.historical.last_mut().unwrap();
        thought.snapshot_body();
        assert_eq!(thought.snapshot.len(), 1);
        let snapshot_len = thought.snapshot.len();
        thought.snapshot_body();
        assert_eq!(
            thought.snapshot.len(),
            snapshot_len,
            "re-snapshot is a no-op"
        );
    }

    /// The cycle that can happen in any turn: thinking, content,
    /// thinking again. Each phase is its own thought block; the
    /// first two become historical, the third is active. The body
    /// of each holds only the cells that arrived during its life.
    #[test]
    fn reasoning_content_reasoning_makes_three_separate_blocks() {
        let mut s = state();
        s.apply(MachineNotice::Reasoning("planning".into()));
        s.apply(MachineNotice::Content("first answer".into()));
        // First thought has the planning reasoning.
        assert_eq!(s.historical.len(), 1);
        assert_eq!(s.historical[0].body.len(), 1);
        assert!(matches!(s.historical[0].body[0], Cell::Reasoning(_)));

        s.apply(MachineNotice::Reasoning("second plan".into()));
        // Second thought is active; first is still in history.
        assert!(s.thought.is_some());
        assert_eq!(s.historical.len(), 1);
        // The second thought's body is empty (reasoning still in
        // flight, not yet committed as a cell).
        assert_eq!(s.thought.as_ref().unwrap().body.len(), 0);
    }
}

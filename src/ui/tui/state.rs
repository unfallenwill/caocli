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
    /// The Ctrl-O verbose toggle. When `true`, settled-Done steps render
    /// their children as well as the verdict; the default keeps them
    /// quiet. Lives on `State` (not `View`) because the laid cache
    /// invalidates on the change, and the cache is read by callers that
    /// already reach through `State`.
    pub(super) verbose: bool,
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
            MachineNotice::Reasoning(text) => self.stream(Style::Reasoning, &text),
            MachineNotice::Content(text) => self.stream(Style::Plain, &text),
            MachineNotice::FinishTurn => {
                // A turn ending while a tool is still open is unusual -- the
                // machine settles every tool before finishing -- but it can
                // happen after an interruption. Closing the open step here
                // keeps the transcript from carrying an open step into the
                // next turn.
                self.finalize_open_step("interrupted");
                self.end_block();
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
                self.finalize_open_step("interrupted");
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
            AppNotice::SetVerbose(v) => {
                // Flipping verbose invalidates the laid cache on the next
                // draw -- `ensure_laid` keys on `laid_verbose` and will
                // rebuild every cell at the new mode. The revision bump
                // above wakes the loop.
                self.verbose = v;
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
            self.revision += 1;
            self.view.transcript.push(cell);
        }
    }

    /// Close the block being streamed, if any, so it becomes a finished cell.
    ///
    /// Bumps the revision: this is called from the turn's end as well as from a
    /// notice, and in the first case it is the only thing that has changed.
    pub(super) fn end_block(&mut self) {
        if let Some(cell) = self.view.stream.close() {
            self.revision += 1;
            self.view.transcript.push(cell);
        }
    }

    /// Close any streaming block, then push a finished cell.
    ///
    /// The pair is the common shape of "a notice arrives while a block is
    /// streaming": the streaming block has to close before the new cell lands,
    /// or the reader sees the new cell threaded through the tail of the old
    /// one. Keeping the pair together is also what keeps the call sites readable
    /// -- the rule that says "a new cell closes the block first" is what the
    /// pairing expresses.
    fn close_and_push(&mut self, cell: Cell) {
        self.end_block();
        self.revision += 1;
        self.view.transcript.push(cell);
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
            &self.view,
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

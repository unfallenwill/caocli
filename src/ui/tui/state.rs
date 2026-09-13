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
    /// Bumped by everything that changes what the screen should show, so a
    /// draw can be skipped when nothing has. Lives on [`State`] rather than
    /// any one half because every half can change the picture: a notice
    /// lands in the view, a key lands in the edit, a queue change lands in
    /// the turn, an overlay lands in the overlay.
    pub(super) revision: u64,
}

// ----------------------------------------------------------------- methods ---

impl State {
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
            MachineNotice::FinishTurn => self.end_block(),
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
                self.close_and_push(Cell::from_tool_call(&name, &args));
            }
            MachineNotice::ToolOutput(chunk) => self.stream_output(&chunk),
            MachineNotice::ToolResult(result) => self.close_and_push(Cell::ToolResult(result)),
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
            MachineNotice::Interrupted => self.close_and_push(Cell::Interrupted),
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
                self.view.transcript.extend(cell::from_messages(&messages));
            }
            AppNotice::Info(text) => self.close_and_push(Cell::Notice(text)),
            AppNotice::Error(text) => self.close_and_push(Cell::Failure(text)),
            AppNotice::SetModel(model) => self.view.status.set_model(&model),
            AppNotice::SetEffort(effort) => self.view.status.set_effort(&effort),
            AppNotice::ResetStats => self.view.status.reset_stats(),
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

    /// Append a running command's own output.
    ///
    /// Deliberately not [`State::stream`]: the counters behind the speed estimate
    /// measure the model's output against the tokens it was billed for, and a
    /// compiler's chatter is neither.
    pub(super) fn stream_output(&mut self, text: &str) {
        if let Some(cell) = self.view.stream.append(Style::Dim, text) {
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
        let Some(title) = super::render::activity_title_at(self, now, usize::MAX) else {
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

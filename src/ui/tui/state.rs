//! The state the screen draws: the transcript, the window over it, the turn
//! in flight, and what the machine's notices do to all three.
//!
//! Kept apart from the terminal it is drawn on, so that what a notice or a key
//! does to it can be tested without one. Nothing here touches the terminal or
//! the clock the loop runs on: methods that need the time are handed an
//! `Instant`, and the only I/O is the channel a caller drains.
//!
//! State and rendering are separate concerns. The methods that turn this state
//! into the lines the screen draws live in [`super::render`]; what is here is
//! what a notice or a key changes. The laid cache (`laid`, `laid_width`,
//! `laid_cells`) is a field on `State` because it is part of the layout, but
//! it is read-only to anything outside the render module.

use std::collections::VecDeque;
use std::time::Instant;

use crossterm::event::MouseEventKind;
use ratatui::text::Line;
use ratatui_textarea::TextArea;

use crate::ui::cell::{self, Cell, Style};
use crate::ui::status::Status;
use crate::ui::tui::notice::Notice;

use super::input::Answer;
use super::input::input_box;
use super::panel::Panel;
use super::picker::Picker;
use super::render;

/// The lines one wheel notch moves the window over the transcript: the step a
/// terminal's own scrollback takes, so a notch here reads like a notch anywhere
/// else. Not a page -- a notebook's worth of lines per flick of a wheel is a way
/// of losing the place rather than of reading.
pub(super) const WHEEL_LINES: isize = 3;

/// The characters-per-token ratio the estimate starts on, before the first
/// usage notice of the session has measured the real one. English prose runs
/// about four characters to the token; CJK lands near one. Both are approximations,
/// which is why the estimate carries a tilde.
pub(super) const BLIND_CHARS_PER_TOKEN: f64 = 4.0;

/// The state the front end draws.
///
/// Kept apart from the terminal it is drawn on, so that what a notice or a key
/// does to it can be tested without one.
pub(super) struct State {
    pub(super) status: Status,
    /// Everything the session has produced, in draw order: replayed history, the
    /// turn in flight, and the lines the user submitted. The whole session, not
    /// an increment of it -- with the alternate screen there is no scrollback to
    /// hand finished lines to, and the terminal keeps no copy of its own.
    pub(super) transcript: Vec<Cell>,
    /// The transcript's cells laid out at [`State::laid_width`], one entry per
    /// cell and in the same order. A cell is never laid out twice for one width:
    /// the cells are the session's and do not change once they are pushed, so
    /// what a draw has to wrap is only what has arrived since the last one.
    /// Everything else -- how long the transcript is, which lines a window shows
    /// -- is derived from this rather than from the cells again.
    ///
    /// `laid.len()` is how many cells are laid and the transcript may have more
    /// waiting: a prefix, never a different list.
    pub(super) laid: Vec<Vec<Line<'static>>>,
    /// The width `laid` was laid at, or `None` when nothing is laid out. Every
    /// line is wrapped to a width, so a different one invalidates all of it.
    pub(super) laid_width: Option<usize>,
    /// How many cells have been laid out here, over the life of this state.
    ///
    /// Nothing on the screen can show this: a draw that re-wrapped the whole
    /// session would draw the same picture. It is what a test has to read to pin
    /// the layout to being done once per cell rather than once per draw, and it
    /// is per state because the tests run side by side.
    #[cfg(test)]
    pub(super) laid_cells: usize,
    /// Which part of the transcript is on screen.
    pub(super) scroll: Scroll,
    /// What the last draw saw: how long the transcript was, and how many rows of
    /// it there were room for. Paging and staying put while lines arrive both
    /// need them, and both are the terminal's to say rather than the state's.
    pub(super) drawn_lines: usize,
    pub(super) drawn_rows: usize,
    /// How many lines the box held at the last draw, so that a box that has just
    /// lost lines can be told from one that has not.
    pub(super) drawn_draft: usize,
    /// The text block being streamed, with the style it is drawn in. The style is
    /// the block's identity, so a fragment in the other style opens a new block.
    pub(super) live: Option<(Style, String)>,
    /// The question standing over the box, while one is open.
    pub(super) question: Option<Cell>,
    /// The question tool's panel, while the tool is waiting on an answer.
    pub(super) panel: Option<Panel>,
    /// Where the answer to that question goes, and what kind of answer it is.
    pub(super) reply: Option<Answer>,
    /// The answer being typed.
    pub(super) textarea: TextArea<'static>,
    /// Lines submitted while a turn was running, oldest first. They are not part
    /// of the session until they run, so they are held here rather than in the
    /// transcript: the head runs as soon as the turn in flight ends, interrupted
    /// or not.
    pub(super) queued: VecDeque<String>,
    /// What was in the box when the approval gate opened. The answer is typed
    /// there, and a line being composed is not an answer.
    pub(super) held_draft: Option<String>,
    /// The command picker, while what is in the box is a command still being
    /// named.
    pub(super) picker: Option<Picker>,
    /// Submitted lines, oldest first.
    pub(super) history: Vec<String>,
    /// Where the user is browsing the history from, if they are.
    pub(super) browsing: Option<usize>,
    /// What was in the box before browsing started, so that stepping past the
    /// newest entry gives it back.
    pub(super) draft: String,
    /// Whether a turn is running: what the box's placeholder and the queue's
    /// behaviour both hang on.
    pub(super) turn_running: bool,
    /// When the running turn began. None between turns; what the border's
    /// working indicator counts up from.
    pub(super) turn_started: Option<Instant>,
    /// Characters the model produced this turn — streamed reasoning and
    /// content, plus the arguments of every tool call it declared: the
    /// numerator of the live tokens-per-second estimate, and the same set of
    /// text the backend's completion tokens bill for.
    pub(super) streamed_chars: usize,
    /// Characters produced since the last usage notice, counted the same way
    /// [`State::streamed_chars`] is. With that notice's completion tokens it
    /// measures the characters-per-token ratio this provider and model
    /// actually produce, which is what keeps the estimate honest after the
    /// first sub-request.
    pub(super) chars_since_usage: usize,
    /// Characters per token, as last measured, or [`BLIND_CHARS_PER_TOKEN`]
    /// before the first measurement. Session-level: it survives the turn that
    /// taught it.
    pub(super) chars_per_token: f64,
    /// The indicator title the last tick saw, so a tick bumps the revision only
    /// when the border would show something new.
    pub(super) ticked_activity: Option<String>,
    /// Bumped by everything that changes what the screen should show, so a draw
    /// can be skipped when nothing has.
    pub(super) revision: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: Status::default(),
            transcript: Vec::new(),
            laid: Vec::new(),
            laid_width: None,
            #[cfg(test)]
            laid_cells: 0,
            scroll: Scroll::default(),
            drawn_lines: 0,
            drawn_rows: 0,
            drawn_draft: 0,
            live: None,
            question: None,
            panel: None,
            reply: None,
            textarea: input_box(),
            queued: VecDeque::new(),
            held_draft: None,
            picker: None,
            history: Vec::new(),
            browsing: None,
            draft: String::new(),
            turn_running: false,
            turn_started: None,
            streamed_chars: 0,
            chars_since_usage: 0,
            chars_per_token: BLIND_CHARS_PER_TOKEN,
            ticked_activity: None,
            revision: 0,
        }
    }
}

/// Lay the transcript out at `width`, keeping what is already laid.
///
/// Only the cells that are not laid yet are wrapped, which for a running turn
/// is the one block that has just closed. The alternative is wrapping the
/// whole session for every fragment that arrives, and a session is as long as
/// the conversation has been.
///
/// These methods describe state transitions; the rendering they enable is in
/// [`super::render`], which takes `&State` (or `&mut State` to maintain the
/// laid cache) and produces the ratatui types the screen hands to its
/// backend.
impl State {
    /// The first transcript line to draw: a window on the end, or the one the
    /// reader scrolled back to.
    ///
    /// Lines that arrive between two draws take the window with them rather than
    /// sliding under it, so a reader who scrolled back stays on the line they were
    /// reading instead of being carried to the end. Everything is clamped, because
    /// a resize re-wraps the transcript and the line count is not the one this was
    /// scrolled against.
    pub(super) fn window(&mut self, total: usize, rows: usize) -> usize {
        if self.scroll.back > 0 {
            let grew = total.saturating_sub(self.drawn_lines);
            self.scroll.by(-(grew as isize), total, rows);
        }
        self.drawn_lines = total;
        self.drawn_rows = rows;
        self.scroll.first(total, rows)
    }

    /// Put a cell in the transcript that the machine did not send -- the banner,
    /// a session-level failure. It still moves the revision, or the draw that
    /// should show it would be skipped.
    pub(super) fn show(&mut self, cell: Cell) {
        self.revision += 1;
        self.transcript.push(cell);
    }

    /// Move the window over the transcript by `lines`, positive being towards the
    /// newest line. The bounds come from the last draw, which is the only thing
    /// that knows how long the transcript is and how many rows of it there are
    /// room for.
    pub(super) fn scroll_by(&mut self, lines: isize) {
        let (total, rows) = (self.drawn_lines, self.drawn_rows);
        self.scroll.by(lines, total, rows);
    }

    /// Page through the transcript a screen at a time; `-1` is back, `+1` forward.
    pub(super) fn page(&mut self, step: isize) {
        self.scroll_by(step * self.drawn_rows as isize);
    }

    /// A wheel notch: three lines back, or three lines forward.
    ///
    /// The wheel has to be answered here rather than left to the terminal, which
    /// cannot do it: the transcript is the application's, so the alternate screen
    /// it is drawn on has no scrollback for the terminal to scroll. What a
    /// terminal does with an unclaimed wheel is send Up and Down -- the only way it
    /// has to say "scroll" to a program it believes cannot hear it -- and this box
    /// reads those as the history of what was typed, so the wheel recalled lines
    /// instead of reading them. Asked for the mouse, the terminal sends the notch
    /// itself, and it moves the one thing scrolling back can mean here.
    ///
    /// A notch is not a click: a press, a drag, a shift of the wheel sideways --
    /// everything else a mouse can say is ignored. A click that does nothing is
    /// less surprising than one that moved a cursor nobody aimed.
    pub(super) fn wheel(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.scroll_by(-WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_LINES),
            _ => {}
        }
    }

    /// Back to the end, which is where a new line will appear.
    pub(super) fn follow(&mut self) {
        self.scroll.bottom();
    }

    /// A turn is starting at `started`.
    ///
    /// The clock is handed in rather than read here, so what a turn's timer
    /// shows can be exercised without waiting for one.
    pub(super) fn begin_turn(&mut self, started: Instant) {
        self.revision += 1;
        self.turn_running = true;
        self.turn_started = Some(started);
        self.streamed_chars = 0;
        self.chars_since_usage = 0;
        self.refresh_placeholder();
    }

    /// The turn is over.
    pub(super) fn end_turn(&mut self) {
        self.revision += 1;
        self.turn_running = false;
        self.turn_started = None;
        self.refresh_placeholder();
    }

    /// The indicator's heartbeat: bump the revision when the frame the spinner
    /// shows or the second the clock reads has changed, so the border keeps
    /// moving while nothing else arrives and no redraw is spent when it has
    /// nothing new to show.
    ///
    /// The title itself is computed in [`super::render`]; this method only
    /// runs the cache check that decides whether the screen has anything new
    /// to draw.
    pub(super) fn tick_activity(&mut self, now: Instant) {
        render::tick_activity(self, now);
    }

    /// Fold one notice into the state.
    pub(super) fn apply(&mut self, notice: Notice) {
        self.revision += 1;
        match notice {
            Notice::Reasoning(text) => self.stream(Style::Reasoning, &text),
            Notice::Content(text) => self.stream(Style::Plain, &text),
            Notice::FinishTurn => self.end_block(),
            Notice::ToolStart { name, args } => {
                // The call's arguments are output the backend billed for: the
                // characters join both counters so the usage notice that ends
                // the next sub-request calibrates against the same text the
                // completion tokens covered. The notice lands after the
                // sub-request that declared the call, so its tokens ride one
                // window late — an offset a turn with several calls averages
                // out.
                let chars = args.chars().count();
                self.streamed_chars += chars;
                self.chars_since_usage += chars;
                self.end_block();
                self.transcript.push(Cell::tool_call(&name, &args));
            }
            Notice::ToolOutput(chunk) => self.stream_output(&chunk),
            Notice::ToolResult(result) => {
                self.end_block();
                self.transcript.push(Cell::ToolResult(result));
            }
            Notice::Instructions(dir) => {
                self.end_block();
                self.transcript
                    .push(Cell::Notice(crate::agents_md::notice_text(&dir)));
            }
            Notice::Usage(u, _stream) => {
                self.status.record(&u);
                // Calibrate the live speed estimate: the characters streamed
                // since the last notice are now known to have been this many
                // tokens. A sub-request that streamed nothing (a bare tool-call
                // round) measures nothing and leaves the ratio standing.
                if self.chars_since_usage > 0 && u.completion_tokens > 0 {
                    self.chars_per_token =
                        self.chars_since_usage as f64 / u.completion_tokens as f64;
                }
                self.chars_since_usage = 0;
            }
            Notice::Interrupted => {
                self.end_block();
                self.transcript.push(Cell::Interrupted);
            }
            Notice::Approval { name, args } => {
                self.end_block();
                self.question = Some(Cell::approval(&name, &args));
            }
            Notice::Secret { prompt, reply } => {
                self.end_block();
                self.question = Some(Cell::Notice(prompt));
                self.open_answer(Answer::Secret(reply));
            }
            Notice::Replay(messages) => {
                self.end_block();
                self.transcript.extend(cell::from_messages(&messages));
            }
            Notice::Info(text) => {
                self.end_block();
                self.transcript.push(Cell::Notice(text));
            }
            Notice::Error(text) => {
                self.end_block();
                self.transcript.push(Cell::Failure(text));
            }
            Notice::SetModel(model) => self.status.set_model(&model),
            Notice::SetEffort(effort) => self.status.set_effort(&effort),
            Notice::ResetStats => self.status.reset_stats(),
        }
    }

    /// Append a text fragment. A fragment in a style other than the open block's
    /// ends that block and opens a new block -- the same rule the plain front end
    /// applies, because the style *is* the block's identity.
    pub(super) fn stream(&mut self, style: Style, text: &str) {
        // Both counters: what the border's speed estimate divides by the clock,
        // and what the next usage notice calibrates the ratio against.
        let chars = text.chars().count();
        self.streamed_chars += chars;
        self.chars_since_usage += chars;
        self.block(style).push_str(text);
    }

    /// Append a running command's own output.
    ///
    /// Deliberately not [`State::stream`]: the counters behind the speed estimate
    /// measure the model's output against the tokens it was billed for, and a
    /// compiler's chatter is neither.
    pub(super) fn stream_output(&mut self, text: &str) {
        self.block(Style::Dim).push_str(text);
    }

    /// The block a fragment in `style` belongs to, opening one if the block being
    /// streamed is another style's: the style is the block's identity, which is what
    /// an arriving fragment in a different one means.
    fn block(&mut self, style: Style) -> &mut String {
        if self.live.as_ref().is_none_or(|(open, _)| *open != style) {
            self.end_block();
            self.live = Some((style, String::new()));
        }
        &mut self.live.as_mut().expect("the block was just opened").1
    }

    /// Close the block being streamed, if any, so it becomes a finished cell.
    ///
    /// Bumps the revision: this is called from the turn's end as well as from a
    /// notice, and in the first case it is the only thing that has changed.
    pub(super) fn end_block(&mut self) {
        if let Some((style, text)) = self.live.take() {
            let cell = crate::ui::tui::paint::live_cell(style, &text);
            self.revision += 1;
            self.transcript.push(cell);
        }
    }
}

/// Where the window over the transcript sits.
///
/// Held as lines back from the newest one, because that is where it opens: the end
/// is what the reader is watching.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Scroll {
    pub(super) back: usize,
}

impl Scroll {
    /// Scroll by `step` lines, positive being towards the newest line, as far as
    /// there is anything to see.
    ///
    /// `total` and `height` are the transcript's length and the rows available for
    /// it; together they say how far back the start is.
    pub(super) fn by(&mut self, step: isize, total: usize, height: usize) {
        let most = total.saturating_sub(height) as isize;
        self.back = (self.back as isize - step).clamp(0, most) as usize;
    }

    /// Go back to the end, which is where it starts.
    pub(super) fn bottom(&mut self) {
        self.back = 0;
    }

    /// The first line to draw.
    pub(super) fn first(&self, total: usize, height: usize) -> usize {
        total.saturating_sub(height).saturating_sub(self.back)
    }
}

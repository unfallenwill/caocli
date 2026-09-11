//! The state the screen draws: the transcript, the window over it, the turn
//! in flight, and what the machine's notices do to all three.
//!
//! Kept apart from the terminal it is drawn on, so that what a notice or a key
//! does to it can be tested without one. Nothing here touches the terminal or
//! the clock the loop runs on: methods that need the time are handed an
//! `Instant`, and the only I/O is the channel a caller drains.

use std::collections::VecDeque;
use std::time::Instant;

use crossterm::event::MouseEventKind;
use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::Line;
use ratatui::widgets::Block;
use ratatui_textarea::TextArea;

use crate::tools::todo::{self, Todo};
use crate::ui::cell::{self, Cell, Style};
use crate::ui::status::Status;
use crate::ui::tui::layout::{self, BOX_BORDERS};
use crate::ui::tui::notice::Notice;
use crate::ui::tui::paint::{cell_lines, live_cell, measure, more_line, style_of, wrapped_under};

use super::input::Answer;
use super::input::input_box;
use super::panel::Panel;
use super::picker::Picker;

/// The lines one wheel notch moves the window over the transcript: the step a
/// terminal's own scrollback takes, so a notch here reads like a notch anywhere
/// else. Not a page -- a notebook's worth of lines per flick of a wheel is a way
/// of losing the place rather than of reading.
pub(super) const WHEEL_LINES: isize = 3;

/// The frames the working spinner cycles through while a turn runs, one per
/// [`SPINNER_MS`]. Braille raising-dots: the convention terminal spinners have
/// settled on, and narrow enough to sit inside a border without crowding it.
pub(super) const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// How long one spinner frame holds, in milliseconds. Twelve-ish frames a
/// second: fast enough to read as motion, slow enough that a frame is one
/// redraw and no more.
pub(super) const SPINNER_MS: u128 = 80;

/// The live speed estimate appears once the turn has run this many seconds:
/// before that the cumulative average swings too much to be worth reading.
pub(super) const SPEED_AFTER_SECS: u64 = 3;

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
    /// Characters streamed this turn, reasoning and content both: the numerator
    /// of the live tokens-per-second estimate.
    pub(super) streamed_chars: usize,
    /// Characters streamed since the last usage notice. With that notice's
    /// completion tokens it measures the characters-per-token ratio this
    /// provider and model actually produce, which is what keeps the estimate
    /// honest after the first sub-request.
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

/// The heading of the standing task list: what the block is, and how far it has
/// got.
///
/// Dim, and marked with the same `·` a note carries: the block is pinned under
/// the transcript for the whole of a turn, and its one job when nothing has
/// changed is to not be read. The tasks below it carry the attention.
///
/// The count is [`todo::summary`], the same string the cell's head and the model's
/// result carry, so the three cannot say different things about one list.
pub(super) fn todo_title(todos: &[Todo]) -> Line<'static> {
    Line::styled(
        format!("· todos · {}", todo::summary(todos)),
        style_of(Style::Dim),
    )
}

/// Lay the transcript out at `width`, keeping what is already laid.
///
/// Only the cells that are not laid yet are wrapped, which for a running turn
/// is the one block that has just closed. The alternative is wrapping the
/// whole session for every fragment that arrives, and a session is as long as
/// the conversation has been.
impl State {
    pub(super) fn ensure_laid(&mut self, width: usize) {
        // A different width is a different wrapping of every line there is.
        if self.laid_width != Some(width) {
            self.laid.clear();
            self.laid_width = Some(width);
        }
        // Never more cells than the transcript has. Only a test can take one away,
        // and lines of a cell that is gone are worse than laying one out twice.
        self.laid.truncate(self.transcript.len());
        let from = self.laid.len();
        for cell in &self.transcript[from..] {
            #[cfg(test)]
            {
                self.laid_cells += 1;
            }
            self.laid.push(cell_lines(cell, width));
        }
    }

    /// The rows the laid cells take: what a window over the transcript is measured
    /// in. A count rather than a copy of the lines, so the part of a long session
    /// that is off the top costs a draw nothing.
    pub(super) fn laid_rows(&self) -> usize {
        self.laid.iter().map(Vec::len).sum()
    }

    /// The rows `[first, last)`, at the width the cells were laid at.
    ///
    /// Only the cells that overlap the window are touched, and of those only the
    /// lines inside it. `live` and `question` come after the cells, in that order:
    /// they are the two blocks that are not laid out with them, because they
    /// change with every fragment and every keystroke.
    pub(super) fn window_lines(
        &self,
        first: usize,
        last: usize,
        live: &[Line<'static>],
        question: &[Line<'static>],
    ) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(last.saturating_sub(first));
        let mut at = 0;
        for segment in self.laid.iter().map(Vec::as_slice).chain([live, question]) {
            if at >= last {
                break;
            }
            let end = at + segment.len();
            if end > first {
                let lo = first.saturating_sub(at);
                let hi = (last - at).min(segment.len());
                out.extend_from_slice(&segment[lo..hi]);
            }
            at = end;
        }
        out
    }

    /// The block still being streamed, laid out, or nothing when no turn is writing
    /// one.
    ///
    /// Laid out through the same [`cell_lines`] a filed cell goes through, and not
    /// merely because the two look alike: a live block that took no gutter would
    /// jump two columns left the moment the block closed, which is the one reading
    /// position a reader is sitting on when the model stops typing.
    pub(super) fn live_lines(&self, width: usize) -> Vec<Line<'static>> {
        match &self.live {
            Some((style, text)) => cell_lines(&live_cell(*style, text), width),
            None => Vec::new(),
        }
    }

    /// The question standing over the box, laid out, or nothing when none is open.
    ///
    /// The question is what the answer is about, and the call in it can be a long
    /// command: one that is clipped gives the user nothing to decide with. It is a
    /// cell like any other, so it carries the marker its kind carries, and the
    /// answer typed into the box below it starts in the column its own text does.
    pub(super) fn question_lines(&self, width: usize) -> Vec<Line<'static>> {
        match &self.question {
            Some(question) => cell_lines(question, width),
            None => Vec::new(),
        }
    }

    /// The standing task list, laid out, or nothing when no call has written one
    /// or the last one cleared it.
    ///
    /// A fold over the transcript and not a copy of it: what is standing is the
    /// last list a call wrote, which is a cell like any other, so a resumed session
    /// keeps exactly the list in view that the session watched live had. There is
    /// nothing here to keep in step with anything, which is the whole reason the
    /// tool writes the list into the log instead of holding it somewhere.
    ///
    /// Each task is wrapped on its own: the block has a fixed number of rows to
    /// give, and a task long enough to wrap must cost its own rows rather than
    /// push the tasks behind it out of the block. The tasks drawn are the window
    /// [`layout::todo_window`] picks, so the task in hand is one of them.
    pub(super) fn todo_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(todos) = cell::standing_todos(&self.transcript) else {
            return Vec::new();
        };
        let width = measure(width);
        let room = layout::TODO_ROWS.saturating_sub(layout::TODO_HEADS);
        let active = todos
            .iter()
            .position(|todo| todo.status == todo::Status::InProgress);
        let window = layout::todo_window(todos.len(), active, room);
        // The blank row first, so that the block cannot be read as the tail of the
        // transcript above it, then the title.
        let mut lines = vec![Line::default(), todo_title(todos)];
        if window.above > 0 {
            lines.push(more_line("  ", window.above));
        }
        for todo in &todos[window.first..window.last] {
            // Through the cell's own wrapping, with the task's own gutter: a task
            // is one row until its words run out of columns, and the rows it then
            // takes are its own -- the ones behind it keep theirs.
            lines.extend(wrapped_under(
                &cell::todo_line_spans(todo),
                width,
                cell::todo_gutter(todo),
            ));
        }
        if window.below > 0 {
            lines.push(more_line("  ", window.below));
        }
        lines
    }

    /// The whole transcript as lines, in draw order: the session's finished cells,
    /// then the block still streaming, then a pending question.
    ///
    /// A draw wants the window, not the whole of it, but a test that asks what the
    /// transcript says has to be able to read all of it -- and it reads it through
    /// the same pieces the draw uses, which is what keeps the two from being two
    /// renderers.
    #[cfg(test)]
    pub(super) fn lines(&mut self, width: usize) -> Vec<Line<'static>> {
        self.ensure_laid(width);
        let live = self.live_lines(width);
        let question = self.question_lines(width);
        let total = self.laid_rows() + live.len() + question.len();
        self.window_lines(0, total, &live, &question)
    }

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

    /// The pinned status line: the session summary, always. What a turn is doing
    /// is the transcript's to say -- the cells it produces -- and the summary is
    /// what you read when you are about to type rather than while you wait.
    ///
    /// One column is left free so the write cannot trigger autowrap.
    pub(super) fn status_line(&self, width: usize) -> Line<'static> {
        Line::from(self.status.line(width.saturating_sub(1)))
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

    /// The working indicator the input box's top border carries while a turn
    /// runs: a spinner, the seconds it has run, and — once the turn has lasted
    /// long enough for the average to settle — an estimated tokens-per-second.
    ///
    /// None when there is nothing to say: no turn, or a question standing over
    /// the box (the gate's own words own that border's neighbourhood), or a
    /// border too narrow even for the spinner and the count. A narrow border
    /// drops the estimate whole first and hides the indicator entirely second —
    /// never a clipped number, the same rule the status line keeps to.
    pub(super) fn activity_title(&self, width: usize) -> Option<String> {
        self.activity_title_at(Instant::now(), width)
    }

    /// The indicator's words at `now`, without reading the clock.
    fn activity_title_at(&self, now: Instant, width: usize) -> Option<String> {
        if self.reply.is_some() {
            return None;
        }
        let started = self.turn_started?;
        let elapsed = now - started;
        let frame = SPINNER[(elapsed.as_millis() / SPINNER_MS) as usize % SPINNER.len()];
        let count = format!("{frame} {}s", elapsed.as_secs());
        let mut title = count.clone();
        if elapsed.as_secs() >= SPEED_AFTER_SECS && self.streamed_chars > 0 {
            // The measured ratio turns characters into tokens; the tilde keeps
            // the estimate honest about being one.
            let per_second =
                self.streamed_chars as f64 / self.chars_per_token / elapsed.as_secs_f64();
            let per_second = per_second.round().max(1.0) as u64;
            title = format!("{count} · ~{per_second} token/s");
        }
        if crate::ui::text::width(&title) <= width {
            return Some(title);
        }
        (crate::ui::text::width(&count) <= width).then_some(count)
    }

    /// The box's rules: the two lines that close it in, and the working indicator
    /// the top of them carries while a turn runs.
    ///
    /// The rules are the front end's to draw rather than the editor's, and that is
    /// what buys the draft its column: a block on the editor is rendered into the
    /// area the editor is rendered into, so insetting the editor to clear the
    /// marker would pull both rules in with it and leave the box two columns short
    /// of the edges the transcript is read against.
    ///
    /// Built per draw, which costs one small struct: the title is a clock and a
    /// spinner, so there is nothing here worth remembering, and nothing that can go
    /// stale when the editor is replaced whole by a submitted or a cleared line.
    pub(super) fn box_rule(&self, width: usize) -> Block<'static> {
        let mut block = Block::default()
            .borders(BOX_BORDERS)
            .border_style(RStyle::new().add_modifier(Modifier::DIM));
        if let Some(title) = self.activity_title(width.saturating_sub(2)) {
            // Not dim, unlike the rule it sits on: it is the one thing on this box
            // that moves, and the only sign that a turn is still running when the
            // model has gone quiet. A dim indicator on a dim border is the signal
            // painted out of sight.
            //
            // The dim is taken *off* rather than left unset: a title is written on
            // the border's own cells, and the border's style is patched into them
            // before the title is, so a title that says nothing about weight is a
            // dim title. Only a style that removes the modifier can undo that.
            let lit = RStyle::new().remove_modifier(Modifier::DIM);
            block = block.title_top(Line::styled(title, lit).right_aligned());
        }
        block
    }

    /// The indicator's heartbeat: bump the revision when the frame the spinner
    /// shows or the second the clock reads has changed, so the border keeps
    /// moving while nothing else arrives and no redraw is spent when it has
    /// nothing new to show.
    pub(super) fn tick_activity(&mut self, now: Instant) {
        let Some(title) = self.activity_title_at(now, usize::MAX) else {
            return;
        };
        if Some(&title) != self.ticked_activity.as_ref() {
            self.ticked_activity = Some(title);
            self.revision += 1;
        }
    }

    /// Fold one notice into the state.
    pub(super) fn apply(&mut self, notice: Notice) {
        self.revision += 1;
        match notice {
            Notice::Reasoning(text) => self.stream(Style::Reasoning, &text),
            Notice::Content(text) => self.stream(Style::Plain, &text),
            Notice::FinishTurn => self.end_block(),
            Notice::ToolStart { name, args } => {
                self.end_block();
                self.transcript.push(Cell::tool_call(&name, &args));
            }
            Notice::ToolOutput(chunk) => self.stream_output(&chunk),
            Notice::ToolResult(result) => {
                self.end_block();
                self.transcript.push(Cell::ToolResult(result));
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
            let cell = live_cell(style, &text);
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

//! The interactive front end: the whole screen, with the session held in the
//! application rather than in the terminal.
//!
//! The alternate screen is what makes the layout the application's to decide, and
//! it is also what costs the terminal's own scrollback: nothing drawn here reaches
//! it, so the transcript is kept as cells and read back by moving a window over
//! them. The session log is the durable copy either way.
//!
//! It owns the terminal, so it also owns the two channels the machine needs: raw
//! mode clears `ISIG`, which means Ctrl-C arrives as a key event and there is no
//! SIGINT left to listen for, and the approval answer comes from the input box
//! rather than from stdin.
//!
//! A turn therefore runs *concurrently* with this event loop rather than inside
//! it. The machine's notifications become [`Notice`]s on a channel, the loop
//! applies them and redraws, and both channels above are answered from the same
//! loop. Nothing on the machine's side ever touches the terminal.

use std::collections::VecDeque;
use std::future::Future;
use std::io::{self, Stdout};
use std::path::Path;
use std::pin::Pin;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
#[cfg(test)]
use ratatui::style::Color;
use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::{Line, Span as RSpan, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use ratatui_textarea::{CursorMove, ScreenCursor, TextArea};
use tokio::sync::{mpsc, oneshot, watch};

use crate::agent::{Agent, Approve, Interrupt};
use crate::config;
use crate::history;
use crate::repl;
use crate::session;
use crate::types::{Message, ToolCall, Usage};
use crate::ui::Front;

use super::Ui;
use super::cell::{self, Cell, Span, Style};
use super::status::Status;

mod paint;

#[cfg(test)]
use paint::{MEASURE, THINKING_LINES};
use paint::{cell_lines, live_cell, measure, more_line, style_of, wrapped_lines};

/// Rows the pinned region needs besides the input box: the status line below it.
const PINNED_ROWS: u16 = 1;

/// The rows the input box takes when it holds nothing: a border, the empty line,
/// a border. An empty box is still a box.
const BOX_ROWS: u16 = 3;

/// The input box's borders: top and bottom only.
///
/// The box is a band the width of the screen, so a pair of vertical edges would
/// be two columns of frame with nothing between them. Without them the draft
/// starts in the column the transcript's own user lines start in.
const BOX_BORDERS: Borders = Borders::TOP.union(Borders::BOTTOM);

/// The columns the box's marker takes, which is the same two the transcript's user
/// lines take: the draft is written where the same line will be written once it is
/// submitted, so submitting moves nothing on the screen.
const BOX_GUTTER: u16 = cell::MARKER_COLUMNS as u16;

/// The rows the input box takes when it holds `lines` lines of text: one row each
/// -- a line added with Ctrl-J is a line the box has to show -- plus the two
/// borders.
///
/// Capped at what a terminal `height` rows tall can spare once the pinned line
/// and a row of transcript are accounted for: a draft taller than the
/// screen scrolls inside the box rather than leaving the screen with nothing but
/// a box on it.
///
/// The floor gives way with the cap. The pinned region and the transcript cannot
/// both have all of what they ask for on a short terminal, and the draft is the
/// one of the three that can be bounded without losing the session -- so the box
/// shrinks first, down to nothing at all. Asking for a three-row box on a
/// two-row terminal is asking the layout for rows it does not have, and the rows
/// it then takes come out of some other region's answer.
fn box_rows(lines: usize, height: u16) -> u16 {
    let wanted = u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(BOX_ROWS - 1);
    let most = height.saturating_sub(PINNED_ROWS + 1);
    wanted.clamp(BOX_ROWS.min(most), most)
}

/// The screen's rows, from the area the frame is drawn into: the transcript, the
/// queue waiting to run, the input box, and the status line under it.
///
/// The transcript's height is the layout's to decide and is read back from it
/// rather than worked out a second time here: two copies of the arithmetic are
/// two answers, and only one of them is the area the frame was laid out with. A
/// terminal too short for the pinned region has no transcript row to give, and
/// the window has to hear that from the layout -- it is the difference between a
/// window over the end of the transcript and one over a row that was never
/// drawn.
fn screen_rows(area: Rect, input: u16, queued: u16) -> Rc<[Rect]> {
    Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(queued),
        Constraint::Length(input),
        Constraint::Length(1),
    ])
    .split(area)
}

/// A window over a list of rows: the run of them it draws, and how many it leaves
/// behind at each end -- which is also what says whether that end carries a count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Window {
    /// The run drawn, as the first row and the one after the last.
    first: usize,
    last: usize,
    /// Rows left out above the run, and below it, as the count drawn at that end
    /// says. Both are zero when there was no room for the counts at all -- the one
    /// case a cut window is drawn without them.
    above: usize,
    below: usize,
}

/// The window a list of `total` rows is seen through, for a selection at
/// `selected` and `room` rows to draw it in.
///
/// The window follows the selection, so the row being chosen is always one of the
/// rows drawn: a menu scrolled past its own highlight is a menu that cannot be
/// answered, because the one row `Enter` is about is the one row the reader cannot
/// see. The selection sits at the end of the window it last moved into, so the
/// window moves when the selection leaves it and not before.
///
/// A window that is cut says so, which costs a row at each end it is cut at: what
/// is drawn is the longest run that fits in `room` along with the counts it needs.
/// A list with room to spare is never counted, and is never cut.
fn picker_window(total: usize, selected: usize, room: usize) -> Window {
    let room = room.max(1);
    let total = total.max(1);
    let selected = selected.min(total - 1);
    for shown in (1..=room.min(total)).rev() {
        // Where the run would sit with the selection at its end, and the furthest
        // down it can start while still holding the selection. Between them: the
        // first position whose counts fit is the one drawn, so that a count the
        // room does not have is given up for a row of the list, which is what the
        // row would have been spent on anyway.
        let last = total - shown;
        for first in selected.saturating_sub(shown - 1)..=last.min(selected) {
            let end = first + shown;
            let cut = usize::from(first > 0) + usize::from(end < total);
            if shown + cut <= room {
                return Window {
                    first,
                    last: end,
                    above: first,
                    below: total - end,
                };
            }
        }
    }
    // A room too small for a count at each end of a single row -- two rows, with
    // the selection in the middle of the list. The counts are what give way: the
    // row `Enter` is about is the one row that cannot.
    Window {
        first: selected,
        last: selected + 1,
        above: 0,
        below: 0,
    }
}

/// The lines one wheel notch moves the window over the transcript: the step a
/// terminal's own scrollback takes, so a notch here reads like a notch anywhere
/// else. Not a page -- a notebook's worth of lines per flick of a wheel is a way
/// of losing the place rather than of reading.
const WHEEL_LINES: isize = 3;

/// How many rows of the queue are drawn above the input box. The queue is what
/// was asked for while a turn ran, and a long one costs the transcript rows it is
/// capped at: what is worth seeing is that the line arrived.
const QUEUE_ROWS: usize = 3;

/// What the box says while a turn runs: the line being typed is not this turn's
/// message, it is the one to run when this turn ends -- and the turn itself can
/// be stopped, which is worth saying, since a key that stops work is no good to
/// a reader who cannot find it.
const QUEUE_PLACEHOLDER: &str = "the turn is running · Enter queues · Ctrl-C stops";

/// What the box says while nothing runs.
///
/// Neither it nor the queue line carries the box's marker: the marker is the
/// box's own, drawn in the column before this text whatever the text says, so a
/// placeholder cannot displace it and typing cannot take it away.
const IDLE_PLACEHOLDER: &str = "type a message · /help for commands";

/// What the box says while the approval gate is open. The box is where the answer
/// goes, so it says so rather than inviting the next message.
const ANSWER_PLACEHOLDER: &str = "y to allow · anything else denies";

/// What the box says while a secret is being asked for. The text is not shown --
/// the box masks it -- so the line has to say what is expected of it.
const SECRET_PLACEHOLDER: &str = "type or paste it · Enter saves · empty cancels";

/// What a secret's characters are drawn as. A character and not a blank: the
/// length of a key is not the key, but a box that shows nothing at all looks
/// like a box that is not taking anything.
const SECRET_MASK: char = '•';

/// How many command rows the picker shows at once. It draws over the bottom of the
/// transcript, so it has to leave the transcript somewhere to live.
const PICKER_ROWS: usize = 6;

// ------------------------------------------------------------- activity ---

/// The frames the working spinner cycles through while a turn runs, one per
/// [`SPINNER_MS`]. Braille raising-dots: the convention terminal spinners have
/// settled on, and narrow enough to sit inside a border without crowding it.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// How long one spinner frame holds, in milliseconds. Twelve-ish frames a
/// second: fast enough to read as motion, slow enough that a frame is one
/// redraw and no more.
const SPINNER_MS: u128 = 80;

/// The live speed estimate appears once the turn has run this many seconds:
/// before that the cumulative average swings too much to be worth reading.
const SPEED_AFTER_SECS: u64 = 3;

/// The characters-per-token ratio the estimate starts on, before the first
/// usage notice of the session has measured the real one. English prose runs
/// about four characters to the token; CJK lands near one. Both are approximations,
/// which is why the estimate carries a tilde.
const BLIND_CHARS_PER_TOKEN: f64 = 4.0;

// ---------------------------------------------------------------- notices ---

/// One thing the machine or the application tells the front end.
///
/// Everything that the plain front end would draw on the spot becomes one of
/// these, so that the only code touching the terminal is the event loop.
///
/// It is not `Clone`: a notice carries text that is moved into the state.
#[derive(Debug)]
enum Notice {
    Reasoning(String),
    Content(String),
    FinishTurn,
    ToolStart {
        name: String,
        args: String,
    },
    ToolResult(String),
    Usage(Usage, Duration),
    Interrupted,
    Approval {
        name: String,
        args: String,
    },
    /// A question whose answer must not be shown by the box, let alone kept:
    /// the reply handle travels with the prompt, because there is nothing to
    /// pair it with here -- the approval gate's answer arrives on a channel of
    /// its own only because the machine asks for it, and this is asked for by
    /// the line being handled.
    Secret {
        prompt: String,
        reply: oneshot::Sender<Option<String>>,
    },
    Replay(Vec<Message>),
    Info(String),
    Error(String),
    SetModel(String),
    ResetStats,
}

/// The handle both the machine and the application hold while a turn runs. It
/// implements [`Ui`] and [`Front`] by turning every call into a notice, because
/// the object that draws them owns the terminal on the other side of the channel.
struct Notifier {
    tx: mpsc::UnboundedSender<Notice>,
}

impl Notifier {
    fn send(&self, notice: Notice) {
        // A closed channel means the front end is gone; the turn still running
        // will be dropped with it.
        let _ = self.tx.send(notice);
    }
}

impl Ui for Notifier {
    fn reasoning_delta(&mut self, s: &str) {
        self.send(Notice::Reasoning(s.to_owned()));
    }
    fn content_delta(&mut self, s: &str) {
        self.send(Notice::Content(s.to_owned()));
    }
    fn finish_turn(&mut self) {
        self.send(Notice::FinishTurn);
    }
    fn tool_start(&mut self, name: &str, args: &str) {
        self.send(Notice::ToolStart {
            name: name.to_owned(),
            args: args.to_owned(),
        });
    }
    fn tool_result(&mut self, result: &str) {
        self.send(Notice::ToolResult(result.to_owned()));
    }
    fn usage(&mut self, u: &Usage, stream: Duration) {
        self.send(Notice::Usage(u.clone(), stream));
    }
    fn interrupted(&mut self) {
        self.send(Notice::Interrupted);
    }
    fn approval_requested(&mut self, name: &str, args: &str) {
        self.send(Notice::Approval {
            name: name.to_owned(),
            args: args.to_owned(),
        });
    }
}

impl Front for Notifier {
    fn replay(&mut self, messages: &[Message]) {
        self.send(Notice::Replay(messages.to_vec()));
    }
    fn info(&mut self, s: &str) {
        self.send(Notice::Info(s.to_owned()));
    }
    fn error(&mut self, s: &str) {
        self.send(Notice::Error(s.to_owned()));
    }
    fn set_model(&mut self, model: &str) {
        self.send(Notice::SetModel(model.to_owned()));
    }
    fn reset_stats(&mut self) {
        self.send(Notice::ResetStats);
    }
    fn ask_secret(&mut self, prompt: &str) -> Pin<Box<dyn Future<Output = Option<String>> + '_>> {
        let (reply, answer) = oneshot::channel();
        self.send(Notice::Secret {
            prompt: prompt.to_owned(),
            reply,
        });
        Box::pin(async move {
            // A front end that has gone away takes the question with it: an
            // answer that will never come is a cancellation.
            answer.await.unwrap_or(None)
        })
    }
}

// --------------------------------------------------------------- channels ---

/// Cancellation as the event loop delivers it: Ctrl-C sets the watch and every
/// await point in the turn races against [`CtrlC::wait`].
///
/// This is why the cancel channel is a parameter of `Agent::turn`: in raw mode
/// there is no SIGINT to subscribe to, so a key event is the only source there is.
struct CtrlC(watch::Receiver<bool>);

impl Interrupt for CtrlC {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        let rx = &mut self.0;
        Box::pin(async move {
            // `borrow_and_update` first: a cancel that arrived before this wait
            // was created must still be seen, which a bare `changed()` would miss.
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    // The front end is gone; never fire again.
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

/// The approval gate, answered from the input box.
///
/// The question goes to the event loop as a reply handle; the next line the user
/// submits is the answer. Denial is the default, including when the front end has
/// gone away mid-ask.
struct Ask {
    tx: mpsc::UnboundedSender<oneshot::Sender<bool>>,
}

impl Approve for Ask {
    fn ask(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = bool> + '_>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            if self.tx.send(reply).is_err() {
                return false;
            }
            answer.await.unwrap_or(false)
        })
    }
}

/// How long the loop will wait for a key before looking at everything else.
///
/// Short enough to feel immediate, long enough not to spin.
const TICK: Duration = Duration::from_millis(8);

/// Read a key if one is waiting, without blocking for longer than a tick.
///
/// This is deliberately *not* a reader thread, and that is the whole reason the
/// loop is shaped the way it is: a reader parked in `event::read` would own the
/// handle, and a turn running in this one would then be waiting on a thread that
/// nothing else can wake. A read that never returns is a front end that never
/// redraws.
fn poll_key(timeout: Duration) -> io::Result<Option<Event>> {
    if crossterm::event::poll(timeout)? {
        Ok(Some(crossterm::event::read()?))
    } else {
        Ok(None)
    }
}

/// A menu as the picker draws it: the rows are the application's, the drawing is
/// this front end's.
#[cfg(test)]
fn named_rows(rows: Vec<(String, String)>) -> Vec<Choice> {
    rows.into_iter()
        .map(|(label, detail)| Choice {
            argument: label.clone(),
            label,
            detail,
        })
        .collect()
}

/// A menu as the picker draws it: the rows are the application's, the drawing is
/// this front end's.
fn choice_rows(rows: Vec<crate::config::Choice>) -> Vec<Choice> {
    rows.into_iter()
        .map(|row| Choice {
            label: row.label,
            argument: row.argument,
            detail: row.detail,
        })
        .collect()
}

/// Everything that has arrived on a channel, without waiting for more.
fn drain<T>(rx: &mut mpsc::UnboundedReceiver<T>) -> Vec<T> {
    let mut drained = Vec::new();
    while let Ok(item) = rx.try_recv() {
        drained.push(item);
    }
    drained
}

// ----------------------------------------------------------------- screen ---

/// The state the front end draws.
///
/// Kept apart from the terminal it is drawn on, so that what a notice or a key
/// does to it can be tested without one.
struct State {
    status: Status,
    /// Everything the session has produced, in draw order: replayed history, the
    /// turn in flight, and the lines the user submitted. The whole session, not
    /// an increment of it -- with the alternate screen there is no scrollback to
    /// hand finished lines to, and the terminal keeps no copy of its own.
    transcript: Vec<Cell>,
    /// The transcript's cells laid out at [`State::laid_width`], one entry per
    /// cell and in the same order. A cell is never laid out twice for one width:
    /// the cells are the session's and do not change once they are pushed, so
    /// what a draw has to wrap is only what has arrived since the last one.
    /// Everything else -- how long the transcript is, which lines a window shows
    /// -- is derived from this rather than from the cells again.
    ///
    /// `laid.len()` is how many cells are laid and the transcript may have more
    /// waiting: a prefix, never a different list.
    laid: Vec<Vec<Line<'static>>>,
    /// The width `laid` was laid at, or `None` when nothing is laid out. Every
    /// line is wrapped to a width, so a different one invalidates all of it.
    laid_width: Option<usize>,
    /// How many cells have been laid out here, over the life of this state.
    ///
    /// Nothing on the screen can show this: a draw that re-wrapped the whole
    /// session would draw the same picture. It is what a test has to read to pin
    /// the layout to being done once per cell rather than once per draw, and it
    /// is per state because the tests run side by side.
    #[cfg(test)]
    laid_cells: usize,
    /// Which part of the transcript is on screen.
    scroll: Scroll,
    /// What the last draw saw: how long the transcript was, and how many rows of
    /// it there were room for. Paging and staying put while lines arrive both
    /// need them, and both are the terminal's to say rather than the state's.
    drawn_lines: usize,
    drawn_rows: usize,
    /// How many lines the box held at the last draw, so that a box that has just
    /// lost lines can be told from one that has not.
    drawn_draft: usize,
    /// The text block being streamed, with the style it is drawn in. The style is
    /// the block's identity, so a fragment in the other style opens a new block.
    live: Option<(Style, String)>,
    /// The question standing over the box, while one is open.
    question: Option<Cell>,
    /// Where the answer to that question goes, and what kind of answer it is.
    reply: Option<Answer>,
    /// The answer being typed.
    textarea: TextArea<'static>,
    /// Lines submitted while a turn was running, oldest first. They are not part
    /// of the session until they run, so they are held here rather than in the
    /// transcript: the head runs as soon as the turn in flight ends, interrupted
    /// or not.
    queued: VecDeque<String>,
    /// What was in the box when the approval gate opened. The answer is typed
    /// there, and a line being composed is not an answer.
    held_draft: Option<String>,
    /// The command picker, while what is in the box is a command still being
    /// named.
    picker: Option<Picker>,
    /// Submitted lines, oldest first.
    history: Vec<String>,
    /// Where the user is browsing the history from, if they are.
    browsing: Option<usize>,
    /// What was in the box before browsing started, so that stepping past the
    /// newest entry gives it back.
    draft: String,
    /// Whether a turn is running: what the box's placeholder and the queue's
    /// behaviour both hang on.
    turn_running: bool,
    /// When the running turn began. None between turns; what the border's
    /// working indicator counts up from.
    turn_started: Option<Instant>,
    /// Characters streamed this turn, reasoning and content both: the numerator
    /// of the live tokens-per-second estimate.
    streamed_chars: usize,
    /// Characters streamed since the last usage notice. With that notice's
    /// completion tokens it measures the characters-per-token ratio this
    /// provider and model actually produce, which is what keeps the estimate
    /// honest after the first sub-request.
    chars_since_usage: usize,
    /// Characters per token, as last measured, or [`BLIND_CHARS_PER_TOKEN`]
    /// before the first measurement. Session-level: it survives the turn that
    /// taught it.
    chars_per_token: f64,
    /// The indicator title the last tick saw, so a tick bumps the revision only
    /// when the border would show something new.
    ticked_activity: Option<String>,
    /// Bumped by everything that changes what the screen should show, so a draw
    /// can be skipped when nothing has.
    revision: u64,
}

/// A question the box is waiting on, and who to give the answer to.
///
/// The box is the one place an answer is typed, so the two kinds share it and
/// are told apart by what they do with what was typed.
enum Answer {
    /// The approval gate: a line starting with `y` allows and anything else
    /// denies, which is the rule the plain front end applies to a line of stdin.
    YesNo(oneshot::Sender<bool>),
    /// A secret (an API key): whatever was typed, with the text hidden while it
    /// is typed, and an empty line for a cancellation. Nothing of it is echoed
    /// into the transcript, and nothing of it is remembered.
    Secret(oneshot::Sender<Option<String>>),
}

/// The picker: what it is offering, and which row is highlighted.
struct Picker {
    kind: Choosing,
    choices: Vec<Choice>,
    selected: usize,
}

/// What the picker is for, which is what choosing a row means.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Choosing {
    /// A command name still being typed. What matches is recomputed on every
    /// keystroke, Tab completes, and Enter runs what is in the box -- an argument
    /// may still be wanted.
    Command,
    /// A session to switch to. The list is the whole message and nothing is
    /// half-typed, so Enter takes the highlighted row.
    Session,
    /// A provider to store a key for: `/login`'s menu, which the plain front end
    /// can only print.
    Provider,
    /// A model to switch to: `/model`'s menu.
    Model,
}

impl Choosing {
    /// The command a chosen row is submitted as: the row becomes the line the
    /// plain prompt would have been given, so that choosing has one
    /// implementation rather than one per front end.
    fn command(self) -> &'static str {
        match self {
            Choosing::Command => "",
            Choosing::Session => "/resume",
            Choosing::Provider => "/login",
            Choosing::Model => "/model",
        }
    }

    /// Whether the list is the whole message -- a list of things to do rather
    /// than a name being typed -- and so whether Enter takes the highlighted row
    /// instead of submitting what is in the box.
    fn takes_a_row(self) -> bool {
        self != Choosing::Command
    }
}

/// One row of the picker: what it says, what it is called, and what choosing it
/// means.
struct Choice {
    /// What the row shows.
    label: String,
    /// What choosing it submits as the argument of the command the picker is
    /// offering: the same string as the label, except for a provider, which is
    /// listed by name and addressed by id.
    argument: String,
    /// The dim second column.
    detail: String,
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

impl State {
    /// Lay the transcript out at `width`, keeping what is already laid.
    ///
    /// Only the cells that are not laid yet are wrapped, which for a running turn
    /// is the one block that has just closed. The alternative is wrapping the
    /// whole session for every fragment that arrives, and a session is as long as
    /// the conversation has been.
    fn ensure_laid(&mut self, width: usize) {
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
    fn laid_rows(&self) -> usize {
        self.laid.iter().map(Vec::len).sum()
    }

    /// The rows `[first, last)`, at the width the cells were laid at.
    ///
    /// Only the cells that overlap the window are touched, and of those only the
    /// lines inside it. `live` and `question` come after the cells, in that order:
    /// they are the two blocks that are not laid out with them, because they
    /// change with every fragment and every keystroke.
    fn window_lines(
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
    fn live_lines(&self, width: usize) -> Vec<Line<'static>> {
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
    fn question_lines(&self, width: usize) -> Vec<Line<'static>> {
        match &self.question {
            Some(question) => cell_lines(question, width),
            None => Vec::new(),
        }
    }

    /// The whole transcript as lines, in draw order: the session's finished cells,
    /// then the block still streaming, then a pending question.
    ///
    /// A draw wants the window, not the whole of it, but a test that asks what the
    /// transcript says has to be able to read all of it -- and it reads it through
    /// the same pieces the draw uses, which is what keeps the two from being two
    /// renderers.
    #[cfg(test)]
    fn lines(&mut self, width: usize) -> Vec<Line<'static>> {
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
    fn window(&mut self, total: usize, rows: usize) -> usize {
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
    fn show(&mut self, cell: Cell) {
        self.revision += 1;
        self.transcript.push(cell);
    }

    /// Move the window over the transcript by `lines`, positive being towards the
    /// newest line. The bounds come from the last draw, which is the only thing
    /// that knows how long the transcript is and how many rows of it there are
    /// room for.
    fn scroll_by(&mut self, lines: isize) {
        let (total, rows) = (self.drawn_lines, self.drawn_rows);
        self.scroll.by(lines, total, rows);
    }

    /// Page through the transcript a screen at a time; `-1` is back, `+1` forward.
    fn page(&mut self, step: isize) {
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
    fn wheel(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.scroll_by(-WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_LINES),
            _ => {}
        }
    }

    /// Back to the end, which is where a new line will appear.
    fn follow(&mut self) {
        self.scroll.bottom();
    }

    /// The pinned status line: the session summary, always. What a turn is doing
    /// is the transcript's to say -- the cells it produces -- and the summary is
    /// what you read when you are about to type rather than while you wait.
    ///
    /// One column is left free so the write cannot trigger autowrap.
    fn status_line(&self, width: usize) -> Line<'static> {
        Line::from(self.status.line(width.saturating_sub(1)))
    }

    /// A turn is starting.
    fn begin_turn(&mut self) {
        self.revision += 1;
        self.turn_running = true;
        self.turn_started = Some(Instant::now());
        self.streamed_chars = 0;
        self.chars_since_usage = 0;
        self.refresh_placeholder();
    }

    /// The turn is over.
    fn end_turn(&mut self) {
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
    fn activity_title(&self, width: usize) -> Option<String> {
        if self.reply.is_some() {
            return None;
        }
        let elapsed = self.turn_started?.elapsed();
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
        if super::text::width(&title) <= width {
            return Some(title);
        }
        (super::text::width(&count) <= width).then_some(count)
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
    fn box_rule(&self, width: usize) -> Block<'static> {
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
    fn tick_activity(&mut self) {
        let Some(title) = self.activity_title(usize::MAX) else {
            return;
        };
        if Some(&title) != self.ticked_activity.as_ref() {
            self.ticked_activity = Some(title);
            self.revision += 1;
        }
    }

    /// Fold one notice into the state.
    fn apply(&mut self, notice: Notice) {
        self.revision += 1;
        match notice {
            Notice::Reasoning(text) => self.stream(Style::Reasoning, &text),
            Notice::Content(text) => self.stream(Style::Plain, &text),
            Notice::FinishTurn => self.end_block(),
            Notice::ToolStart { name, args } => {
                self.end_block();
                self.transcript.push(Cell::tool_call(&name, &args));
            }
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
            Notice::ResetStats => self.status.reset_stats(),
        }
    }

    /// Append a text fragment. A fragment in a style other than the open block's
    /// ends that block and opens a new block -- the same rule the plain front end
    /// applies, because the style *is* the block's identity.
    fn stream(&mut self, style: Style, text: &str) {
        // Both counters: what the border's speed estimate divides by the clock,
        // and what the next usage notice calibrates the ratio against.
        let chars = text.chars().count();
        self.streamed_chars += chars;
        self.chars_since_usage += chars;
        match &mut self.live {
            Some((open, buffer)) if *open == style => buffer.push_str(text),
            _ => {
                self.end_block();
                self.live = Some((style, text.to_owned()));
            }
        }
    }

    /// Close the block being streamed, if any, so it becomes a finished cell.
    ///
    /// Bumps the revision: this is called from the turn's end as well as from a
    /// notice, and in the first case it is the only thing that has changed.
    fn end_block(&mut self) {
        if let Some((style, text)) = self.live.take() {
            let cell = live_cell(style, &text);
            self.revision += 1;
            self.transcript.push(cell);
        }
    }

    /// Handle a key at the prompt.
    fn key(&mut self, event: Event) -> Submitted {
        self.revision += 1;
        let key = match event {
            Event::Key(key) => key,
            // The wheel is the reader's, not the box's: nothing that is typed
            // here is what a notch moves.
            Event::Mouse(mouse) => {
                self.wheel(mouse.kind);
                return Submitted::Nothing;
            }
            // A paste goes to the box; the caller redraws either way.
            Event::Paste(text) => {
                self.textarea.insert_str(text);
                return Submitted::Nothing;
            }
            _ => return Submitted::Nothing,
        };
        if key.kind != KeyEventKind::Press {
            return Submitted::Nothing;
        }
        match key {
            // Enter submits. Ctrl-J is deliberately left to the box, which
            // inserts a newline, so multi-line input works as it does at the
            // plain prompt.
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                // A list of sessions, providers or models is a list of things to
                // do rather than a name being typed, so Enter takes the
                // highlighted row and submits the line it stands for.
                if self.choose() {
                    return Submitted::Line;
                }
                if self.textarea.is_empty() {
                    Submitted::Nothing
                } else {
                    Submitted::Line
                }
            }
            // At the prompt Ctrl-C clears the line, as it does in the plain front
            // end: there is no turn to cancel here.
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.textarea = input_box();
                Submitted::Nothing
            }
            // Ctrl-J is the plain prompt's newline key, and Shift-Enter is what
            // everyone tries first. Both are taken here because the box binds
            // Ctrl-J to delete-to-line-start, which is not what this front end
            // promises.
            KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::SHIFT,
                ..
            } => {
                self.textarea.insert_newline();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } if self.textarea.is_empty() => Submitted::Exit,
            // Paging through the transcript. The box scrolls itself with the same
            // two keys, so a draft with more lines than it has rows keeps them --
            // which is the only case where the box has anything to page.
            KeyEvent {
                code: KeyCode::PageUp,
                modifiers: KeyModifiers::NONE,
                ..
            } if !self.text().contains('\n') => {
                self.page(-1);
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::PageDown,
                modifiers: KeyModifiers::NONE,
                ..
            } if !self.text().contains('\n') => {
                self.page(1);
                Submitted::Nothing
            }
            // Up and Down mean the picker while it is open and the history
            // otherwise: the picker is only open while a command is being named,
            // so the two never compete for the same keystroke.
            KeyEvent {
                code: KeyCode::Up, ..
            } => {
                self.up();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => {
                self.down();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            } => {
                // Completing a command leaves it in the box; choosing a row is
                // the whole action, so it submits.
                if self.choose() {
                    return Submitted::Line;
                }
                self.complete();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                // Dismissing the command picker takes its line with it: the line
                // is the query the list was filtered by, and a lone `/` left
                // behind glues itself to the next word, which then goes out as
                // an unknown command instead of as a message. A row list was
                // asked for in full by a submitted command, so what the box
                // holds is whatever has been typed since it opened -- a draft,
                // which dismissing has no claim on.
                if self
                    .picker
                    .take()
                    .is_some_and(|p| p.kind == Choosing::Command)
                {
                    self.textarea = input_box();
                    self.refresh_placeholder();
                }
                Submitted::Nothing
            }
            _ => {
                self.textarea.input(Event::Key(key));
                self.refresh_picker();
                Submitted::Nothing
            }
        }
    }

    /// What is in the box.
    fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// How many rows the box needs on a terminal `height` rows tall: one per line
    /// of the draft, so that Ctrl-J -- the key that makes the draft multi-line --
    /// makes room for the line it adds.
    fn input_rows(&self, height: u16) -> u16 {
        box_rows(self.textarea.lines().len(), height)
    }

    /// Put the box's window back where the draft it now holds wants it, for a box
    /// `rows` rows tall -- what [`input_rows`] says it is.
    ///
    /// A box tall enough for every line has nothing to hide, but the editor
    /// remembers where it was scrolled to while the box was shorter and only moves
    /// that window when the cursor leaves it: a draft that has just lost lines, or
    /// a terminal that has just grown, would be drawn from the remembered row --
    /// the lines above it missing, blank rows where they were. Scrolling the window
    /// to the top is what clears that, and the editor then puts it where the cursor
    /// needs it; the cursor is put back afterwards, since a scroll takes it along.
    ///
    /// Called before the box is drawn, which is the only time the window matters.
    fn reset_box_scroll(&mut self, rows: u16) {
        let lines = self.textarea.lines().len();
        let shrank = lines < self.drawn_draft;
        self.drawn_draft = lines;
        // A draft too tall for the box is the editor's to page -- the window is the
        // point there, and taking it over would undo what the box's own keys did.
        // One that has just lost lines is the exception, whatever its length: it
        // was paged against a longer draft than it is now.
        if !shrank && lines + 2 > usize::from(rows) {
            return;
        }
        let cursor = self.textarea.cursor();
        // Scrolling further than there is to scroll is how the window is sent to
        // the top from wherever it was: the editor has no "go to the top".
        self.textarea.scroll((-i16::MAX, 0));
        if let (Ok(row), Ok(col)) = (u16::try_from(cursor.0), u16::try_from(cursor.1)) {
            self.textarea.move_cursor(CursorMove::Jump(row, col));
        }
    }

    /// Replace what is in the box, leaving the cursor after it.
    fn set_text(&mut self, text: &str) {
        let mut box_ = input_box();
        if !text.is_empty() {
            box_.insert_str(text);
        }
        self.textarea = box_;
        // A box built from scratch carries the idle invitation, and the box this
        // replaces may have been saying something else: what it says is the
        // state's, not the constructor's.
        self.refresh_placeholder();
    }

    /// Recompute what the picker offers for what is in the box, keeping the
    /// highlight on the same command while it is still among the matches.
    fn refresh_picker(&mut self) {
        // An offered list is not a completion: it was asked for in full, and it
        // stays until it is answered or dismissed, whatever is typed next.
        if self.picker.as_ref().is_some_and(|p| p.kind.takes_a_row()) {
            return;
        }
        let matches = repl::completions(&self.text());
        if matches.is_empty() {
            self.picker = None;
            return;
        }
        let previous = self
            .picker
            .as_ref()
            .and_then(|p| p.choices.get(p.selected))
            .map(|c| c.argument.clone());
        let selected = previous
            .and_then(|name| matches.iter().position(|c| c.name == name))
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: Choosing::Command,
            choices: matches
                .into_iter()
                .map(|c| Choice {
                    label: c.name.to_owned(),
                    argument: c.name.to_owned(),
                    detail: c.description.to_owned(),
                })
                .collect(),
            selected,
        });
    }

    /// Offer the sessions in `list` to switch to, and say whether there was
    /// anything to offer.
    ///
    /// The sessions are passed in rather than read here: the terminal is not where
    /// the filesystem is read, and this is the state half of the front end.
    fn open_sessions(&mut self, list: &[session::SessionInfo]) -> bool {
        self.open_choices(
            Choosing::Session,
            list.iter()
                .map(|s| Choice {
                    label: s.id.clone(),
                    argument: s.id.clone(),
                    detail: format!("{} messages · {}", s.message_count, s.preview),
                })
                .collect(),
        )
    }

    /// Offer `rows` to choose from, and say whether there was anything to offer.
    /// The rows are built from [`crate::config`]'s tables -- what exists is the
    /// application's to know, and how a row is drawn is this front end's.
    fn open_choices(&mut self, kind: Choosing, rows: Vec<Choice>) -> bool {
        self.revision += 1;
        if rows.is_empty() {
            self.picker = None;
            return false;
        }
        self.picker = Some(Picker {
            kind,
            choices: rows,
            selected: 0,
        });
        true
    }

    /// Take the highlighted row. It becomes the line the plain prompt would have
    /// been given, so choosing has one implementation rather than one per front
    /// end.
    fn choose(&mut self) -> bool {
        let Some(picker) = &self.picker else {
            return false;
        };
        if !picker.kind.takes_a_row() {
            return false;
        }
        let Some(argument) = picker
            .choices
            .get(picker.selected)
            .map(|c| c.argument.clone())
        else {
            return false;
        };
        let line = format!("{} {argument}", picker.kind.command());
        self.picker = None;
        self.set_text(&line);
        true
    }

    /// Up: the previous command, or the previous line typed.
    fn up(&mut self) {
        if let Some(picker) = &mut self.picker {
            let len = picker.choices.len().max(1);
            picker.selected = if picker.selected == 0 {
                len - 1
            } else {
                picker.selected - 1
            };
            return;
        }
        self.browse(-1);
    }

    /// Down: the next command, or the next line typed.
    fn down(&mut self) {
        if let Some(picker) = &mut self.picker {
            let len = picker.choices.len().max(1);
            picker.selected = (picker.selected + 1) % len;
            return;
        }
        self.browse(1);
    }

    /// Step through the history. `-1` is older and `+1` newer; stepping past the
    /// newest entry returns what was being typed before browsing started.
    ///
    /// Browsing closes the picker: a recalled line may well be a command, and
    /// letting the picker open would take the very keys being used to browse.
    fn browse(&mut self, step: isize) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.browsing {
            None if step < 0 => {
                self.draft = self.text();
                Some(self.history.len() - 1)
            }
            // Already showing the draft, and there is nothing newer to show.
            None => None,
            Some(0) if step < 0 => Some(0),
            Some(at) if step > 0 && at + 1 >= self.history.len() => None,
            Some(at) => Some(at.checked_add_signed(step).unwrap_or(at)),
        };
        self.browsing = next;
        let text = match next {
            Some(at) => self.history[at].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.picker = None;
        self.set_text(&text);
    }

    /// Tab: put the highlighted command in the box without running it, since an
    /// argument may still be wanted. Only a command reaches here: the other lists
    /// are menus, and `Tab` submits a menu's row the way `Enter` does.
    fn complete(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        if let Some(choice) = picker.choices.get(picker.selected) {
            self.set_text(&choice.argument);
        }
    }

    /// Record a submitted line. Repeating the previous one is not recorded
    /// again: it is noise when stepping back through the history.
    fn remember(&mut self, line: &str) {
        if line.is_empty() || self.history.last().is_some_and(|last| last == line) {
            return;
        }
        self.history.push(line.to_owned());
        let excess = self.history.len().saturating_sub(history::MAX_ENTRIES);
        if excess > 0 {
            self.history.drain(..excess);
        }
    }

    /// Take a submitted line as part of the session: it goes into the transcript
    /// the way replay would put it there, and into the history for recall.
    ///
    /// A command is not part of the session: the handler answers it without the
    /// model ever seeing it, so a session replayed later does not have it either,
    /// and showing one live would make the same session read two ways depending on
    /// when it was looked at.
    fn submit(&mut self, line: &str) {
        self.remember(line);
        if !line.starts_with('/') {
            self.revision += 1;
            self.transcript.push(Cell::User(line.to_owned()));
        }
        // What was just asked is what the user wants to watch, so the transcript
        // goes back to its end whether or not that line becomes a cell.
        self.follow();
    }

    /// The picker as it is drawn: the rows its window holds, the highlighted one
    /// reversed, and a count at each end the list is cut off at.
    ///
    /// The window follows the selection ([`picker_window`]), so however long the
    /// menu is, the row `Enter` is about is one of the rows on the screen. It used
    /// not to be: the rows were drawn from the first choice down, so a menu longer
    /// than [`PICKER_ROWS`] could be scrolled -- by a key that wraps around, no
    /// less -- past its own end, and what `Enter` would choose was then not on the
    /// screen at all.
    fn picker_lines(&self) -> Vec<Line<'static>> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        // As wide as the widest name in this list, so its rows line up without
        // every menu paying for the widest one there is: a menu of command names
        // is narrow, one of `<provider id>/<modelid>` is not.
        let width = picker
            .choices
            .iter()
            .map(|c| super::text::width(&c.label))
            .max()
            .unwrap_or(0);
        let row = |i: usize| {
            let choice = &picker.choices[i];
            let selected = i == picker.selected;
            let name = if selected {
                RStyle::new().add_modifier(Modifier::REVERSED)
            } else {
                RStyle::new()
            };
            let detail = if selected {
                RStyle::new().add_modifier(Modifier::REVERSED)
            } else {
                RStyle::new().add_modifier(Modifier::DIM)
            };
            Line::from(vec![
                RSpan::styled(
                    format!(" {}", super::text::padded(&choice.label, width)),
                    name,
                ),
                RSpan::styled(format!(" {}", choice.detail), detail),
            ])
        };
        let window = picker_window(picker.choices.len(), picker.selected, PICKER_ROWS);
        let mut lines: Vec<Line<'static>> = Vec::new();
        if window.above > 0 {
            lines.push(more_line(" ", window.above));
        }
        lines.extend((window.first..window.last).map(row));
        if window.below > 0 {
            lines.push(more_line(" ", window.below));
        }
        lines
    }

    /// Handle a key while a turn is running.
    ///
    /// Three things take typing here: the cancel key, always; the answer to an
    /// approval question, while the gate is waiting for one; and otherwise the
    /// next line, which Enter puts in the queue rather than running now. A turn is
    /// not the place to *start* anything -- the one in flight is what the user is
    /// watching -- but it is exactly the place to say what should follow it, which
    /// is what the queue is for: it runs from its head when the turn ends, whether
    /// it ended by finishing or by being interrupted.
    fn key_while_working(&mut self, event: Event, cancel: &watch::Sender<bool>) {
        // A resize arrives here too, and it changes the layout, so anything
        // arriving at all is reason enough to redraw -- and a draw that was not
        // needed costs one comparison.
        self.revision += 1;
        // The wheel is the one thing a turn does not have to be told about:
        // reading back is what there is to do while the model writes, and a notch
        // is not a line being composed. Answered before the gate, which is shut for
        // all but the moment it asks its question.
        if let Event::Mouse(mouse) = &event {
            self.wheel(mouse.kind);
            return;
        }
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let _ = cancel.send(true);
            return;
        }
        // No question is open, so the box is free for the next line. Enter queues
        // it, which is the whole point of typing here. One key is still dropped --
        // Ctrl-D, which leaves the session -- because a turn in flight is not the
        // place to leave from either.
        if self.reply.is_none() {
            if let Submitted::Line = self.key(event) {
                let line = self.take_line();
                self.enqueue(line);
            }
            return;
        }
        // A question is open: the box is where the answer goes and nowhere else,
        // so what is typed is the answer and Enter gives it. Enter submits it
        // whether or not anything was typed: a blank line denies, or cancels a
        // secret, which is the rule the plain front end reads from stdin.
        let answering = matches!(
            &event,
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.code == KeyCode::Enter
                    && key.modifiers == KeyModifiers::NONE
        );
        if answering {
            self.close_question();
        } else {
            // Backspace, a paste, a letter: all of it is the answer being typed.
            self.key(event);
        }
    }

    /// Put a line after the running turn. It is the whole of what Enter does
    /// during a turn: a session written as if this turn had ended would record two
    /// answers at once.
    fn enqueue(&mut self, line: String) {
        self.revision += 1;
        self.queued.push_back(line);
    }

    /// Take the head of the queue: the line to run next, if there is one.
    fn dequeue(&mut self) -> Option<String> {
        let next = self.queued.pop_front();
        if next.is_some() {
            self.revision += 1;
        }
        next
    }

    /// The queue as it is drawn: one dim line per queued line, the end of it last,
    /// like the transcript's own window. Capped at [`QUEUE_ROWS`] rows, so that a
    /// queue longer than that -- more lines, or longer ones -- costs the transcript
    /// those rows and no more. What the end of the window keeps is the newest line,
    /// which is the one just typed and the one being waited for; what it is not
    /// showing is counted rather than dropped, the rule the picker keeps to as
    /// well, so that a queue running past the cap does not read as a queue of
    /// three. The count is drawn on one of the rows the cap allows rather than on
    /// a row of its own: the cap is what the transcript is paying.
    fn queue_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = measure(width);
        let mut lines = Vec::new();
        for line in &self.queued {
            lines.extend(wrapped_lines(
                &[Span::new(Style::Dim, format!("› {line}"))],
                width,
            ));
        }
        if lines.len() <= QUEUE_ROWS {
            return lines;
        }
        let hidden = lines.len() - (QUEUE_ROWS - 1);
        let mut window = vec![more_line("  ", hidden)];
        window.extend(lines.split_off(hidden));
        window
    }

    /// The placeholder for what the box is for right now: the answer while a
    /// question is open, the queue while a turn runs, the next message
    /// otherwise.
    fn placeholder(&self) -> &'static str {
        match self.reply {
            Some(Answer::YesNo(_)) => ANSWER_PLACEHOLDER,
            Some(Answer::Secret(_)) => SECRET_PLACEHOLDER,
            None if self.turn_running => QUEUE_PLACEHOLDER,
            None => IDLE_PLACEHOLDER,
        }
    }

    /// Put that placeholder on the box, which is a thing of the box's own rather
    /// than of the screen's.
    fn refresh_placeholder(&mut self) {
        self.textarea.set_placeholder_text(self.placeholder());
    }

    /// Take the submitted line out of the box, leaving it empty for the next one.
    fn take_line(&mut self) -> String {
        let line = self.text();
        self.textarea = input_box();
        self.refresh_placeholder();
        self.picker = None;
        self.browsing = None;
        self.draft.clear();
        line
    }

    /// The approval gate is asking: remember who to answer.
    fn open_question(&mut self, reply: oneshot::Sender<bool>) {
        self.open_answer(Answer::YesNo(reply));
    }

    /// A question is open: take the box for its answer.
    fn open_answer(&mut self, reply: Answer) {
        self.revision += 1;
        // A secret is typed on a screen other people can see, so the box shows
        // dots instead of what is in it. The answer is still what was typed:
        // this is the drawing, not the text.
        let secret = matches!(reply, Answer::Secret(_));
        self.reply = Some(reply);
        // A line being composed when the question arrives is held aside: the
        // answer to "run it?" is a `y`, and a sentence that happened to be in the
        // box is not one. It comes back when the question closes.
        self.held_draft = Some(self.text());
        self.textarea = input_box();
        if secret {
            self.textarea.set_mask_char(SECRET_MASK);
        }
        self.refresh_placeholder();
    }

    /// Answer the open question from what is in the box, if anything. What the
    /// answer means is the question's: `y` allows a tool call, an empty line
    /// cancels a secret.
    fn close_question(&mut self) {
        self.revision += 1;
        if let Some(reply) = self.reply.take() {
            let answer = self.textarea.lines().join("\n").trim().to_owned();
            match reply {
                Answer::YesNo(reply) => {
                    let _ = reply.send(answer.to_lowercase().starts_with('y'));
                }
                Answer::Secret(reply) => {
                    let _ = reply.send((!answer.is_empty()).then_some(answer));
                }
            }
            let held = self.held_draft.take().unwrap_or_default();
            self.set_text(&held);
        }
        self.question = None;
        self.refresh_placeholder();
    }
}

// ------------------------------------------------------------ scroll view ---

/// Where the window over the transcript sits.
///
/// Held as lines back from the newest one, because that is where it opens: the end
/// is what the reader is watching.
#[derive(Debug, Default, Clone, Copy)]
struct Scroll {
    back: usize,
}

impl Scroll {
    /// Scroll by `step` lines, positive being towards the newest line, as far as
    /// there is anything to see.
    ///
    /// `total` and `height` are the transcript's length and the rows available for
    /// it; together they say how far back the start is.
    fn by(&mut self, step: isize, total: usize, height: usize) {
        let most = total.saturating_sub(height) as isize;
        self.back = (self.back as isize - step).clamp(0, most) as usize;
    }

    /// Go back to the end, which is where it starts.
    fn bottom(&mut self) {
        self.back = 0;
    }

    /// The first line to draw.
    fn first(&self, total: usize, height: usize) -> usize {
        total.saturating_sub(height).saturating_sub(self.back)
    }
}

/// The screen: a terminal, and the state it shows.
///
/// Generic over the backend so that what it draws can be asserted on. The
/// terminal-touching half of this front end is exactly the half nothing else can
/// check, and ratatui ships a backend that keeps the screen in memory -- which
/// makes the drawing testable instead of merely inspected by hand.
struct Screen<B: Backend> {
    terminal: Terminal<B>,
    state: State,
    /// The terminal this screen took, for as long as it holds it. `None` in a test,
    /// which draws into memory and has nothing to give back.
    tty: Option<Tty>,
    /// The view key of the last draw, or `None` when the screen has to be
    /// repainted whatever the key says: nothing has been drawn yet.
    drawn: Option<ViewKey>,
}

/// The mouse, as far as this front end wants it: the wheel, and nothing else.
///
/// `?1000` is what reports a button press, and a wheel notch is one -- buttons 4
/// and 5 -- so asking for it is asking for the wheel without the movement reports
/// `?1003` adds: those arrive for every cell the pointer crosses, which over a
/// whole screen is a stream of events and a repaint for each of them. `?1006` is
/// the encoding that spells a notch out as a sequence of its own, rather than
/// folding its coordinates into the bytes that name the button.
///
/// Written out rather than taken from crossterm's own `EnableMouseCapture`, which
/// asks for `?1003` as well. What it also costs is the terminal's own selection:
/// a terminal that has handed the mouse over keeps it, and gives it back under
/// `Shift`.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1000l\x1b[?1006l";

/// The terminal, held by the screen that took it.
///
/// A type of its own because a `Drop` impl cannot be written for a single
/// instantiation of a generic one: the screen is generic over its backend so that a
/// test can draw into memory, and the terminal that has to be given back is not
/// generic at all.
struct Tty;

impl Tty {
    /// Take the terminal over: raw mode, then the alternate screen, then the wheel.
    ///
    /// Written so that a failure at any step undoes the steps before it: the value
    /// exists before the first escape sequence is written, so an error drops it and
    /// the drop is what gives the terminal back.
    fn take() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let taken = Self;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::style::Print(MOUSE_ON)
        )?;
        Ok(taken)
    }
}

/// Give the terminal back: the mouse, the alternate screen, so what the user had
/// on it reappears, and the cursor where a shell prompt expects to find it.
fn restore() -> io::Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::style::Print(MOUSE_OFF),
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )
}

/// Restoring the terminal does not depend on the success path running. Raw mode is
/// process-wide and would wreck the shell if it survived an unwind, the alternate
/// screen would hide everything the user had on it -- so both are given back by
/// dropping what took them, which an unwind does too. The mouse is the same
/// bargain: a terminal left reporting it hands nothing to the shell that follows.
impl Drop for Tty {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = restore();
    }
}

/// What the screen would show, as a value that can be compared.
///
/// Equal keys mean a redraw cannot change a pixel, so it is skipped. The
/// revision covers everything the state knows about itself -- nothing on the
/// screen moves without the state changing, so it is the whole of what a draw
/// could differ in besides the size, which is there because the rows are
/// laid out from it. Reading the size is an
/// `ioctl`, not a round trip to the terminal, so it is cheap enough to be part
/// of a check that runs on every tick of the loop.
#[derive(PartialEq, Eq, Clone, Copy)]
struct ViewKey {
    revision: u64,
    width: u16,
    height: u16,
}

/// A terminal that draws over the whole screen.
///
/// Nothing here has to ask the terminal where its cursor is -- that question is
/// what an inline viewport lives by -- so the only way this fails is a terminal
/// that cannot be put into raw mode at all.
fn fullscreen<B: Backend>(backend: B) -> Result<Terminal<B>, B::Error> {
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
}

impl Screen<CrosstermBackend<Stdout>> {
    /// Enter the screen.
    ///
    /// Fallible on purpose: raw mode is process-wide and the alternate screen hides
    /// everything the user had on it, so an attempt that cannot be finished has to
    /// leave the terminal as it was found. An error here is the caller's signal to
    /// fall back to the plain front end rather than to fail to start.
    fn enter() -> io::Result<Self> {
        Ok(Self {
            tty: Some(Tty::take()?),
            terminal: fullscreen(CrosstermBackend::new(std::io::stdout()))?,
            state: State::default(),
            drawn: None,
        })
    }

    /// Leave the screen: giving the terminal back is the whole of it.
    fn leave(&mut self) {
        drop(self.tty.take());
    }
}

impl<B: Backend> Screen<B> {
    /// What the screen would show right now.
    fn view_key(&self) -> Result<ViewKey, B::Error> {
        let size = self.terminal.size()?;
        Ok(ViewKey {
            revision: self.state.revision,
            width: size.width,
            height: size.height,
        })
    }

    /// Repaint, unless the screen already shows this.
    ///
    /// The loop wakes on a tick to stay responsive to the keyboard, and most of
    /// those ticks have nothing new to show: a turn spends its time waiting to be
    /// told something. The framework compares each frame with the last and writes
    /// only the difference, so an unchanged frame costs no write -- what it costs
    /// is being built, and this check is what keeps a tick from paying for that.
    /// It is a coarser question than the framework's (one frame against another)
    /// and it is asked on purpose: what it is sparing is the building, not the
    /// writing.
    fn draw_if_changed(&mut self) -> Result<(), B::Error> {
        let key = self.view_key()?;
        if self.drawn == Some(key) {
            return Ok(());
        }
        let area = self.draw_at()?;
        // Stamped from the frame that was drawn rather than from the size the
        // check above was made against, and only after the draw succeeded: a
        // failed draw leaves the screen showing something else, and a resize that
        // lands between the check and the draw leaves a key that describes an area
        // the screen is not showing -- either way the next call has to try again.
        self.drawn = Some(ViewKey {
            revision: self.state.revision,
            width: area.width,
            height: area.height,
        });
        Ok(())
    }

    /// Draw the screen as of now, for tests that do not care about the clock.
    #[cfg(test)]
    fn draw(&mut self) -> Result<(), B::Error> {
        self.draw_at().map(|_| ())
    }

    /// Draw the screen: the window on the transcript, the input box, and the
    /// status line under it. The box is sized for what is in it, so the transcript
    /// gives up rows to a multi-line draft and takes them back when the line is
    /// submitted.
    ///
    /// Everything that depends on the size is asked of the frame inside the
    /// callback: the width the text is wrapped at and the rows each region gets
    /// come from the same [`Rect`], so the layout and the text laid out for it
    /// cannot disagree. The area drawn is returned, so the caller stamps its
    /// change key with what was drawn rather than with what it expected.
    fn draw_at(&mut self) -> Result<Rect, B::Error> {
        let drawn = self.terminal.draw(|frame| {
            let area = frame.area();
            let width = area.width as usize;
            let input = self.state.input_rows(area.height);
            // What is waiting to run, drawn at the bottom of the transcript: the
            // session, then what comes next, then the box, and under the box the
            // session summary. Asked for before the layout, because how many rows
            // it takes is what the transcript gives up.
            let queue = Text::from(self.state.queue_lines(width));
            let queued = queue.height() as u16;
            let rows = screen_rows(area, input, queued);
            self.state.reset_box_scroll(input);
            // What the transcript has to show, in the three pieces it is made of:
            // the cells that are laid out and kept, then the block still being
            // written, then a question if one is open.
            self.state.ensure_laid(width);
            let live = self.state.live_lines(width);
            let question = self.state.question_lines(width);
            let total = self.state.laid_rows() + live.len() + question.len();
            // The rows the transcript really has: the layout's answer, not a copy
            // of its arithmetic.
            let room = rows[0].height as usize;
            // The window over the transcript: its end unless the reader scrolled
            // back. The picker belongs to the line being typed, so it takes the
            // box's end of the transcript with it.
            let picker = self.state.picker_lines();
            let first = if picker.is_empty() {
                self.state.window(total, room)
            } else {
                self.state.follow();
                total.saturating_sub(room)
            };
            let last = (first + room).min(total);
            let transcript = Text::from(self.state.window_lines(first, last, &live, &question));
            let status = self.state.status_line(width);
            let cursor = self.state.textarea.screen_cursor();
            // The box's rules, its marker, and the columns the draft is written in
            // between them. Its marker is the box's own rather than a character of
            // the draft or of the placeholder, so it is drawn whether the box holds
            // a line, a hint or nothing at all -- and typing cannot take it away,
            // which is what the marker inside the placeholder did.
            let field = box_field(rows[2]);

            frame.render_widget(Paragraph::new(transcript), rows[0]);
            if !picker.is_empty() {
                let height = picker.len().min(PICKER_ROWS) as u16;
                let over = Rect {
                    x: rows[0].x,
                    y: rows[0].bottom().saturating_sub(height),
                    width: rows[0].width,
                    height,
                };
                // Cleared first: a shorter list must not leave the tail of a
                // longer one behind it.
                frame.render_widget(Clear, over);
                frame.render_widget(Paragraph::new(Text::from(picker)), over);
            }
            frame.render_widget(Paragraph::new(queue), rows[1]);
            frame.render_widget(self.state.box_rule(rows[2].width as usize), rows[2]);
            frame.render_widget(
                Paragraph::new(Line::styled(cell::USER_MARKER, style_of(Style::Dim))),
                box_marker(rows[2]),
            );
            frame.render_widget(&self.state.textarea, field);
            place_cursor(frame, field, cursor);
            frame.render_widget(Paragraph::new(status), rows[3]);
        })?;
        Ok(drawn.area)
    }

    /// A turn is over: close the block that was still being streamed.
    ///
    /// Nothing leaves the screen. The transcript is the state's, and a turn is part
    /// of it from the moment it arrives; this only closes the block, so that the
    /// next turn's first fragment opens one of its own.
    fn commit(&mut self) {
        self.state.end_block();
    }
}

/// The input box's editor. Enter submits and Ctrl-J inserts a newline, matching
/// the plain prompt's keys.
///
/// It carries no block of its own: the rules are drawn by the front end, so that
/// the draft can start past the marker while the rules still run the width of the
/// screen.
///
/// Its cursor cell is left plain. The widget draws one of its own by reversing
/// whatever is under it, and the terminal's own cursor is put on that same cell by
/// `place_cursor` -- two carets on one cell, and the visible one is the terminal's:
/// it is the one that blinks, that a bar-shaped cursor can be told apart in, and
/// that says where a typed character will land. So the cell is drawn as what it
/// holds and the cursor is left to the terminal.
///
/// A placeholder is drawn one column right of where the draft starts, and that is the
/// editor's and not this: an empty box has no cursor cell of its own to draw, so the
/// widget puts one at the head of the placeholder's first line, and the hint follows
/// it. The hint is not the draft -- nothing is being typed while it is up -- so the
/// column it starts in is not one anything can be compared against.
fn input_box() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text(IDLE_PLACEHOLDER);
    textarea.set_cursor_line_style(RStyle::new());
    textarea.set_cursor_style(RStyle::new());
    textarea
}

/// The columns inside the box's rules that the draft is written in: the whole
/// width less the marker's columns.
///
/// This is the area the editor is rendered into, and with no block of its own the
/// area it is given is also the area its cursor is reported against -- so the two
/// have to be worked out the same way, which is what this being one function is
/// for.
fn box_field(area: Rect) -> Rect {
    let inner = Block::default().borders(BOX_BORDERS).inner(area);
    Rect {
        x: inner.x + BOX_GUTTER,
        width: inner.width.saturating_sub(BOX_GUTTER),
        ..inner
    }
}

/// The box's marker: the same one a user line carries in the transcript, in the
/// same column, so that what is being typed and what was said line up.
///
/// One row tall and over the first row the draft has, rather than centred or
/// repeated: the transcript sets a user line's marker against its first line, and
/// a box that moved its marker as the draft grew would be a box that moved under
/// the reader's eye.
fn box_marker(area: Rect) -> Rect {
    let inner = Block::default().borders(BOX_BORDERS).inner(area);
    Rect {
        width: BOX_GUTTER.min(inner.width),
        height: inner.height.min(1),
        ..inner
    }
}

/// Translate the input box's own cursor into a position on the screen, so the
/// terminal's caret sits where the next character will go.
///
/// The box reports its cursor relative to the area it is rendered into, which --
/// with no block of its own -- is the field [`box_field`] cuts out. So this is
/// handed the rect the editor was rendered into rather than working out an area of
/// its own: a second attempt at the same arithmetic is a caret in a column the
/// character after it will not be in.
fn place_cursor(frame: &mut Frame, field: Rect, cursor: ScreenCursor) {
    let x = field.x + cursor.col as u16;
    let y = field.y + cursor.row as u16;
    if x < field.right() && y < field.bottom() {
        frame.set_cursor_position((x, y));
    }
}

// ------------------------------------------------------------ event loop ---

/// What a key did while the input box had focus.
enum Submitted {
    /// The box holds a line to run.
    Line,
    /// The user asked to leave.
    Exit,
    /// Nothing to run; keep waiting.
    Nothing,
}

/// Run the interactive front end until the user leaves.
///
/// `banner` and `history` are what the plain front end prints before its first
/// prompt; here they are the first thing in the transcript, so a resumed session
/// reads the same way either way.
/// Returns `Ok(false)` when the terminal will not take raw mode and the alternate
/// screen, which is the caller's signal to fall back to the plain front end rather
/// than to fail. `Ok(true)` means the front end ran and the session is over.
pub async fn run(
    agent: &mut Agent,
    sdir: &Path,
    banner: &str,
    history: &[Message],
) -> anyhow::Result<bool> {
    let mut screen = match Screen::enter() {
        Ok(screen) => screen,
        Err(_) => return Ok(false),
    };
    let history_path = crate::config::history_file()?;
    screen.state.history = history::load(&history_path);
    screen.state.status.set_model(&agent.model_label());
    screen.state.show(Cell::Notice(banner.to_owned()));
    screen.state.transcript.extend(cell::from_messages(history));

    let (tx, mut notices) = mpsc::unbounded_channel();
    let (ask_tx, mut asked) = mpsc::unbounded_channel();
    let mut handle = Notifier { tx };

    let result: anyhow::Result<()> = 'session: loop {
        // Idle: draw, then wait for something to submit.
        if let Err(e) = screen.draw_if_changed() {
            break 'session Err(e.into());
        }
        let mut line = loop {
            for notice in drain(&mut notices) {
                screen.state.apply(notice);
            }
            screen.draw_if_changed()?;
            match poll_key(TICK)? {
                Some(event) => match screen.state.key(event) {
                    Submitted::Line => break screen.state.take_line(),
                    Submitted::Exit => break String::new(),
                    Submitted::Nothing => {}
                },
                None => continue,
            }
        };
        // Everything from here runs without returning to the keyboard in between,
        // and the next line is the head of the queue: what was asked for while the
        // last one ran is what comes after it. That is the whole point of the
        // queue -- an interruption ends the turn, not the sequence.
        loop {
            if line.is_empty() {
                break 'session Ok(());
            }
            screen.state.submit(&line);
            // `/resume` with nothing to resume is a request for the list rather
            // than a command to run: the plain front end can only say so, and this
            // one can offer it. The chosen row is submitted as `/resume <id>`,
            // which is the line the plain prompt would have been given, so the
            // switching itself is unchanged. An empty directory falls through to
            // that same reply.
            // The choice is typed, so the queue waits behind it: a picker answered
            // by a line that is still in the queue would answer itself.
            if line.trim() == "/resume" && screen.state.open_sessions(&session::list(sdir)?) {
                break;
            }
            // The other two menus work the same way: the command still needs an
            // argument, and the front end that can offer the possibilities does.
            if line.trim() == "/login"
                && screen
                    .state
                    .open_choices(Choosing::Provider, choice_rows(config::provider_choices()))
            {
                break;
            }
            if line.trim() == "/model"
                && screen.state.open_choices(
                    Choosing::Model,
                    choice_rows(config::model_menu(&agent.model_label())),
                )
            {
                break;
            }

            // A turn: it races against the keyboard, so Ctrl-C can reach it.
            // The block is what bounds the borrow of `line`: it ends with the
            // turn, and the queue then hands the same variable the next line.
            let outcome = {
                let (cancel_tx, cancel_rx) = watch::channel(false);
                let mut interrupt = CtrlC(cancel_rx);
                let mut approve = Ask { tx: ask_tx.clone() };
                let turn = repl::handle(
                    agent,
                    &mut handle,
                    sdir,
                    &line,
                    &mut interrupt,
                    &mut approve,
                );
                tokio::pin!(turn);
                screen.state.begin_turn();
                let outcome = loop {
                    // Keys are read here, never on another thread: see `poll_key`.
                    if let Some(event) = poll_key(TICK)? {
                        screen.state.key_while_working(event, &cancel_tx);
                    }
                    for notice in drain(&mut notices) {
                        screen.state.apply(notice);
                    }
                    for reply in drain(&mut asked) {
                        screen.state.open_question(reply);
                    }
                    screen.state.tick_activity();
                    screen.draw_if_changed()?;
                    tokio::select! {
                        outcome = &mut turn => break outcome,
                        // Nothing else to wait on: the tick above paces the loop,
                        // and this branch only gives the turn a real waker so that
                        // it is driven by readiness rather than by the tick.
                        _ = tokio::time::sleep(TICK) => {}
                    }
                };
                // Drain once more before closing the turn. The turn is polled
                // *inside* the select above, so its last notifications are sent
                // after the loop's last drain and are still queued when it
                // resolves; closing the block without them would leave the tail of
                // the turn to open one of its own.
                for notice in drain(&mut notices) {
                    screen.state.apply(notice);
                }
                for reply in drain(&mut asked) {
                    screen.state.open_question(reply);
                }
                outcome
            };
            screen.state.end_turn();
            screen.state.close_question();
            screen.commit();
            match outcome {
                Ok(repl::Outcome::Exit) => break 'session Ok(()),
                Ok(repl::Outcome::Continue) => {}
                Err(e) => {
                    // `repl::handle` reports turn failures itself; anything
                    // escaping it is a session-level problem worth showing.
                    screen.state.show(Cell::Failure(format!("{e:#}")));
                }
            }
            // Next: the head of the queue, or back to the keyboard.
            match screen.state.dequeue() {
                Some(next) => line = next,
                None => break,
            }
        }
    };

    screen.leave();
    // Written on the way out rather than per line: the file is small, and a
    // rewrite per keystroke would be work for nothing.
    if let Err(e) = history::save(&history_path, &screen.state.history) {
        screen
            .state
            .transcript
            .push(Cell::Failure(format!("{e:#}")));
    }
    result?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::Renderer;
    use crossterm::event::{MouseButton, MouseEvent};

    #[test]
    fn every_notice_becomes_a_cell_or_a_status_change() {
        // The mapping is the front end's whole job; a notice with no effect would
        // silently swallow the machine's output.
        let mut screen = State::default();
        screen.apply(Notice::Reasoning("think".into()));
        screen.apply(Notice::Content("answer".into()));
        screen.apply(Notice::FinishTurn);
        screen.apply(Notice::ToolStart {
            name: "Bash".into(),
            args: r#"{"command":"ls"}"#.into(),
        });
        screen.apply(Notice::ToolResult("exit_code: 0\nbody".into()));
        screen.apply(Notice::Info("note".into()));
        screen.apply(Notice::Error("boom".into()));
        screen.apply(Notice::Interrupted);
        assert_eq!(
            screen.transcript,
            vec![
                Cell::Reasoning("think".into()),
                Cell::Content("answer".into()),
                Cell::tool_call("Bash", r#"{"command":"ls"}"#),
                Cell::ToolResult("exit_code: 0\nbody".into()),
                Cell::Notice("note".into()),
                Cell::Failure("boom".into()),
                Cell::Interrupted,
            ]
        );
    }

    #[test]
    fn a_fragment_in_the_other_style_opens_a_new_block() {
        let mut screen = State::default();
        screen.apply(Notice::Reasoning("a".into()));
        screen.apply(Notice::Reasoning("b".into()));
        assert!(screen.live.is_some(), "still one open block");
        screen.apply(Notice::Content("x".into()));
        assert_eq!(
            screen.transcript,
            vec![Cell::Reasoning("ab".into())],
            "the reasoning block closed when the style changed"
        );
        screen.apply(Notice::FinishTurn);
        assert_eq!(screen.transcript[1], Cell::Content("x".into()));
        assert!(screen.live.is_none());
    }

    #[test]
    fn status_notices_reach_the_status_line() {
        let mut screen = State::default();
        screen.apply(Notice::SetModel("m-1".into()));
        screen.apply(Notice::Usage(
            Usage {
                prompt_tokens: 6,
                total_tokens: 10,
                completion_tokens: 4,
                prompt_cache_hit_tokens: 6,
                prompt_cache_miss_tokens: 4,
                prompt_tokens_details: None,
            },
            Duration::ZERO,
        ));
        assert_eq!(
            screen.status.full_line(),
            "m-1 · cache 60.0% · hit 6 · miss 4"
        );
        screen.apply(Notice::ResetStats);
        assert_eq!(
            screen.status.full_line(),
            "m-1 · cache 0.0% · hit 0 · miss 0"
        );
    }

    #[test]
    fn replay_and_the_live_stream_produce_the_same_cells() {
        // The invariant the plain front end is held to, held here too: a resumed
        // session must look like the one that was watched live.
        let mut screen = State::default();
        screen.apply(Notice::Replay(vec![
            Message {
                role: crate::types::Role::Assistant,
                content: Some("running it".into()),
                reasoning_content: Some("let me think".into()),
                tool_calls: None,
                tool_call_id: None,
            },
            Message::tool("call_1", "exit_code: 0\n--- stdout ---\nbody"),
        ]));
        assert_eq!(
            screen.transcript,
            vec![
                Cell::Reasoning("let me think".into()),
                Cell::Content("running it".into()),
                Cell::ToolResult("exit_code: 0\n--- stdout ---\nbody".into()),
            ]
        );
    }

    #[test]
    fn an_open_question_is_drawn_after_the_transcript() {
        let mut screen = State::default();
        screen.apply(Notice::Content("before".into()));
        screen.apply(Notice::Approval {
            name: "Bash".into(),
            args: r#"{"command":"rm -rf /"}"#.into(),
        });
        let lines = screen.lines(80);
        assert_eq!(lines.len(), 2, "answer, then the question");
        assert!(format!("{:?}", lines[1]).contains("run it?"));
    }

    /// The text of a rendered line, with whatever width it occupies.
    fn rendered(lines: &[Line<'static>]) -> Vec<(String, usize)> {
        lines
            .iter()
            .map(|l| {
                let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                let width = super::super::text::width(&text);
                (text, width)
            })
            .collect()
    }

    fn wrap(text: &str, width: usize) -> Vec<(String, usize)> {
        rendered(&wrapped_lines(&[Span::new(Style::Plain, text)], width))
    }

    #[test]
    fn a_long_line_is_wrapped_not_clipped() {
        // Nothing may be lost: the plain front end leaves this to the terminal's
        // soft wrapping, but `insert_before` renders into a fixed-width buffer,
        // where an over-long line is silently cut off.
        let text: String = "abcdefghij".repeat(3); // 30 columns
        let lines = wrap(&text, 8);
        assert_eq!(
            lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["abcdefgh", "ijabcdef", "ghijabcd", "efghij"],
            "every column survives, in order"
        );
        assert!(lines.iter().all(|(_, w)| *w <= 8), "and none overflows");
    }

    #[test]
    fn a_line_breaks_at_a_space_and_not_inside_a_word() {
        // The whole point of the break: a wrapped line reads as two lines instead of
        // as two halves of a word.
        let wrapped: Vec<String> = wrap("aaaa bbbb cccc", 10)
            .into_iter()
            .map(|(text, _)| text)
            .collect();
        assert_eq!(wrapped, vec!["aaaa bbbb", "cccc"]);
        // The space it broke at is the break and not a column of the text: no line
        // begins or ends with one.
        for width in 1..=20 {
            for (text, _) in wrap("alpha beta gamma", width) {
                assert!(!text.starts_with(' '), "{width}: {text:?}");
                assert!(!text.ends_with(' '), "{width}: {text:?}");
            }
        }
    }

    #[test]
    fn a_word_wider_than_the_line_is_cut_where_it_falls() {
        // A word that fits no line at all is cut -- and cut on the line it started
        // on: moving it down would leave the columns before it empty without saving
        // the cut.
        let text = format!("aaaa {}", "b".repeat(14));
        let wrapped: Vec<String> = wrap(&text, 8).into_iter().map(|(t, _)| t).collect();
        assert_eq!(wrapped, vec!["aaaa bbb", "bbbbbbbb", "bbb"]);
        assert_eq!(wrapped.concat().replace(' ', ""), text.replace(' ', ""));
    }

    #[test]
    fn wrapping_loses_no_column_of_what_is_not_a_break() {
        // Every width: no line over it, and every character that is not a space the
        // break was taken at still on the screen, in order.
        let text = "the quick brown fox jumps over the lazy dog";
        let letters: String = text.chars().filter(|c| *c != ' ').collect();
        for width in 1..=24 {
            let lines = wrap(text, width);
            assert!(lines.iter().all(|(_, w)| *w <= width), "{width}: {lines:?}");
            let joined: String = lines.iter().map(|(t, _)| t.as_str()).collect();
            let kept: String = joined.chars().filter(|c| *c != ' ').collect();
            assert_eq!(kept, letters, "{width}: {lines:?}");
        }
    }

    #[test]
    fn a_wide_terminal_keeps_a_line_of_text_to_the_measure() {
        // The region can be wider than a line of text should be. What is laid out is
        // laid out to the measure, so the far columns of a wide terminal stay empty
        // and the eye does not have to run the whole way back.
        let mut screen = screen_for_test(200, 30);
        screen
            .state
            .transcript
            .push(Cell::Content("word ".repeat(60).trim_end().to_owned()));
        screen.draw().unwrap();
        let rows = all_rows(&screen);
        let transcript = &rows[..transcript_rows(30, BOX_ROWS, 0) as usize];
        let widest = transcript
            .iter()
            .map(|r| super::super::text::width(r.trim_end()))
            .max()
            .unwrap();
        assert!(
            widest <= MEASURE,
            "laid out to the measure: {widest} columns"
        );
        assert!(widest > MEASURE - 10, "and the measure is used: {widest}");
        assert!(
            transcript[0].starts_with("word word"),
            "the left edge is kept"
        );
    }

    #[test]
    fn a_wide_character_is_never_split_across_lines() {
        // Ten ideographs are twenty columns; at eight, each line holds four.
        let text = "\u{6df1}".repeat(10);
        let lines = wrap(&text, 8);
        assert_eq!(lines.len(), 3, "8 + 8 + 4 columns");
        assert_eq!(lines[2].1, 4);
        assert_eq!(super::super::text::width(&text), 20, "nothing was dropped");
        assert!(
            lines
                .iter()
                .all(|(t, _)| t.chars().count() % 4 == 0 || t.chars().count() == 2)
        );
    }

    #[test]
    fn a_character_wider_than_the_field_still_makes_progress() {
        // Degenerate, but a loop that cannot make progress would hang the front
        // end rather than look wrong for one frame.
        let lines = wrap("\u{6df1}\u{6df1}", 1);
        assert_eq!(lines.len(), 2, "one glyph per line, clipped");
        assert_eq!(lines[0].0, "\u{6df1}");
    }

    #[test]
    fn explicit_line_breaks_survive_wrapping() {
        let lines = wrap("one\ntwo", 40);
        assert_eq!(
            lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
    }

    #[test]
    fn a_blank_line_in_the_text_stays_blank() {
        let lines = wrap("a\n\nb", 40);
        assert_eq!(
            lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["a", "", "b"]
        );
    }

    #[test]
    fn breaking_exactly_at_the_width_does_not_add_a_blank_line() {
        // The wrap already ended the line; the explicit newline that follows must
        // not look like a blank one.
        let lines = wrap("ab\ncd", 2);
        assert_eq!(
            lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            vec!["ab", "cd"]
        );
    }

    #[test]
    fn wrapping_keeps_each_span_in_its_own_style() {
        let spans = [Span::new(Style::Dim, "ab"), Span::new(Style::Plain, "cd")];
        let lines = wrapped_lines(&spans, 4);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].spans.len(), 2, "two styles on one line");
        assert_eq!(lines[0].spans[0].style.fg, None, "dim is a modifier");
    }

    /// A front end drawing into memory instead of a terminal.
    fn screen_for_test(width: u16, height: u16) -> Screen<ratatui::backend::TestBackend> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        Screen {
            terminal: fullscreen(backend).unwrap(),
            state: State::default(),
            tty: None,
            drawn: None,
        }
    }

    /// How many rows a test's transcript has on a terminal `height` rows tall,
    /// with a box `input` rows tall and `queued` rows for the queue: the layout's
    /// answer, asked the same way the screen asks it, so that a test that fills
    /// the transcript fills the rows that are really there.
    fn transcript_rows(height: u16, input: u16, queued: u16) -> u16 {
        screen_rows(Rect::new(0, 0, 40, height), input, queued)[0].height
    }

    #[test]
    fn paging_to_either_end_stops_there() {
        let mut view = Scroll::default();
        let (total, height) = (100, 10);
        view.by(-1000, total, height);
        assert_eq!(view.first(total, height), 0, "the beginning of it");
        view.bottom();
        assert_eq!(view.first(total, height), 90, "and its end");
    }

    /// One row of what was drawn, without the padding.
    fn row(screen: &Screen<ratatui::backend::TestBackend>, y: u16) -> String {
        let buf = screen.terminal.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// A transcript row with its gutter taken off, for the tests that ask which
    /// line is where rather than which columns it is set in.
    ///
    /// The gutter itself is what the tests about the left edge are for: a test about
    /// scrolling that had to name the marker would fail the day the marker changed,
    /// and would have failed for a reason that has nothing to do with scrolling.
    fn body(screen: &Screen<ratatui::backend::TestBackend>, y: u16) -> String {
        unset(&row(screen, y))
    }

    /// The same, for a test that already holds the drawn rows.
    fn unset(drawn: &str) -> String {
        // Skipped by character and not by column: every marker is one column, which
        // is the whole reason the gutters are one width.
        drawn.chars().skip(cell::MARKER_COLUMNS).collect()
    }

    /// Where the drawn area is. A full screen starts at the origin, and a test
    /// asks rather than assumes: the frame is what says where the rows are.
    fn origin(screen: &mut Screen<ratatui::backend::TestBackend>) -> Rect {
        screen.terminal.get_frame().area()
    }

    /// Every row of the screen, so a test can ask whether something was drawn
    /// without pinning down which row it landed on.
    fn all_rows(screen: &Screen<ratatui::backend::TestBackend>) -> Vec<String> {
        let height = screen.terminal.backend().buffer().area.height;
        (0..height).map(|y| row(screen, y)).collect()
    }

    #[test]
    fn the_pinned_rows_take_the_bottom_of_the_screen() {
        // The shape the pinned region has to keep, whatever the transcript does:
        // the box, then the status line, on the last rows of the terminal.
        let mut screen = screen_for_test(60, 20);
        screen.state.status.set_model("m-1");
        screen.draw().unwrap();
        let last = screen.terminal.backend().buffer().area.height - 1;
        assert!(row(&screen, last - 3).starts_with('─'), "the box's top");
        assert!(
            // The editor puts a space in front of the placeholder, which is where
            // its cursor would stand.
            row(&screen, last - 2).ends_with(IDLE_PLACEHOLDER),
            "its text: {:?}",
            row(&screen, last - 2)
        );
        assert!(row(&screen, last - 1).starts_with('─'), "its bottom");
        assert_eq!(row(&screen, last), "m-1 · cache 0.0% · hit 0 · miss 0");
    }

    #[test]
    fn the_queue_is_drawn_above_the_box_until_it_is_run() {
        // What was typed during a turn has to be visible somewhere, or the only
        // proof it arrived is that something happens later. It is drawn as the
        // user line it is about to become, dimmed to say it has not run, at the
        // foot of the transcript -- and it takes rows from the transcript rather
        // than covering it.
        let mut screen = screen_for_test(40, 20);
        screen.state.status.set_model("m-1");
        let last = screen.terminal.backend().buffer().area.height - 1;
        screen.state.enqueue("first".into());
        screen.state.enqueue("second".into());
        screen.draw().unwrap();
        assert_eq!(row(&screen, last - 5), "› first");
        assert_eq!(row(&screen, last - 4), "› second");
        assert!(
            screen.terminal.backend().buffer()[(0, last - 5)]
                .style()
                .add_modifier
                .contains(Modifier::DIM),
            "dimmed: it is waiting, not part of the session"
        );
        // ... and the box and the status line are where they always are: the
        // queue is inserted, not drawn over anything.
        assert!(row(&screen, last - 3).starts_with('─'), "the box's top");
        assert_eq!(row(&screen, last), "m-1 · cache 0.0% · hit 0 · miss 0");

        // Run one: the queue gives a row back, and what ran is drawn as the
        // transcript's own line -- the same line the queue was showing, in the
        // place the session keeps it, and no longer dimmed.
        screen.state.submit("first");
        assert_eq!(screen.state.dequeue().as_deref(), Some("first"));
        screen.draw().unwrap();
        assert_eq!(row(&screen, 0), "› first", "the transcript has it now");
        assert_eq!(row(&screen, last - 5), "", "the row it gave back");
        assert_eq!(
            row(&screen, last - 4),
            "› second",
            "only what is still waiting is in the queue"
        );
        assert_eq!(row(&screen, last), "m-1 · cache 0.0% · hit 0 · miss 0");
        let buf = screen.terminal.backend().buffer();
        assert!(
            !buf[(2, 0)].style().add_modifier.contains(Modifier::DIM),
            "the line that ran reads as the session's, not as something waiting"
        );
        assert!(
            buf[(2, last - 4)]
                .style()
                .add_modifier
                .contains(Modifier::DIM),
            "and the one behind it still waits"
        );
    }

    #[test]
    fn the_queue_is_capped_and_keeps_its_end() {
        // A queue longer than its rows is drawn from its end, like the transcript:
        // the line that was just typed is the one being looked for, and the rest
        // are waiting behind it either way. What it is not drawing is counted on
        // one of the rows the cap allows, so that three rows of a longer queue do
        // not read as the whole queue.
        let mut state = State::default();
        for i in 0..(QUEUE_ROWS + 2) {
            state.enqueue(format!("line {i}"));
        }
        assert_eq!(state.queue_lines(40).len(), QUEUE_ROWS, "capped, in rows");
        assert_eq!(
            state
                .queue_lines(40)
                .iter()
                .map(|l| l.to_string())
                .collect::<Vec<_>>(),
            vec!["  … 3 more", "› line 3", "› line 4"]
        );
    }

    #[test]
    fn a_queued_line_wider_than_the_screen_is_wrapped_not_clipped() {
        // Queued lines are wrapped like everything else that is committed to the
        // screen: a fixed-width path would cut the end off the command that is
        // about to be run.
        let mut state = State::default();
        let typed = "x".repeat(50);
        state.enqueue(typed.clone());
        let lines: Vec<String> = state
            .queue_lines(20)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(lines.len(), 3, "52 columns at 20 columns a row");
        assert!(lines.iter().all(|l| super::super::text::width(l) <= 20));
        assert_eq!(lines.concat(), format!("› {typed}"));
    }

    #[test]
    fn ctrl_j_makes_the_box_a_line_taller() {
        // The box is the draft's shape: the line Ctrl-J adds has a row to be typed
        // on, and the transcript gives one up for it.
        let mut screen = screen_for_test(40, 20);
        type_in(&mut screen.state, "first");
        screen.draw().unwrap();
        let last = screen.terminal.backend().buffer().area.height - 1;
        assert!(
            row(&screen, last - 3).starts_with('─'),
            "one line, three rows"
        );

        screen.state.key(ctrl_j());
        type_in(&mut screen.state, "second");
        screen.draw().unwrap();
        assert!(
            row(&screen, last - 4).starts_with('─'),
            "two lines, four rows"
        );
        assert!(row(&screen, last - 3).contains("first"), "the first line");
        assert!(row(&screen, last - 2).contains("second"), "and the second");
        assert!(
            row(&screen, last - 1).starts_with('─'),
            "the bottom stays put"
        );
    }

    #[test]
    fn a_submitted_line_gives_its_rows_back_to_the_transcript() {
        // The box is as tall as the draft and no taller: what a multi-line line
        // borrowed goes back when the line is sent.
        let mut screen = screen_for_test(40, 20);
        let last = screen.terminal.backend().buffer().area.height - 1;
        type_in(&mut screen.state, "first");
        screen.state.key(ctrl_j());
        type_in(&mut screen.state, "second");
        screen.draw().unwrap();
        assert!(
            row(&screen, last - 4).starts_with('─'),
            "two lines, four rows"
        );

        screen.state.take_line();
        screen.draw().unwrap();
        assert!(row(&screen, last - 3).starts_with('─'), "empty, three rows");
    }

    #[test]
    fn a_draft_that_has_lost_lines_is_drawn_from_its_first_one() {
        // A draft taller than the box scrolls inside it. When lines are deleted
        // the box gets shorter with them, and the rows it was scrolled to must not
        // hide the top of what is left -- the editor would otherwise draw from
        // the row it remembered and leave the rest of the box blank.
        let height = 12;
        let shows = usize::from(box_rows(usize::MAX, height)) - 2;
        let (draft, kept) = (shows + 4, shows - 2);
        let letters: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().take(draft).collect();
        let mut screen = screen_for_test(40, height);
        for (i, letter) in letters.iter().enumerate() {
            if i > 0 {
                screen.state.key(ctrl_j());
            }
            type_in(&mut screen.state, &letter.to_string());
        }
        screen.draw().unwrap();

        // Each line is a letter and a newline, so this leaves the first `kept` of
        // them and the cursor on the last.
        for _ in 0..(2 * (draft - kept)) {
            press(&mut screen.state, KeyCode::Backspace);
        }
        screen.draw().unwrap();

        // The box is ruled off above and below, so its lines are what lies between
        // the rules: the first line is the row after the top one.
        let top = 1 + all_rows(&screen)
            .iter()
            .position(|r| r.starts_with('─'))
            .expect("the box is drawn");
        for (i, letter) in letters[..kept].iter().enumerate() {
            let drawn = body(&screen, (top + i) as u16);
            assert!(drawn.starts_with(*letter), "{drawn:?}");
        }
        assert!(
            row(&screen, (top + kept) as u16).starts_with('─'),
            "and nothing below the last line"
        );
    }

    #[test]
    fn the_status_line_is_the_summary_whether_or_not_a_turn_runs() {
        // The line as drawn, not as formatted: while a turn runs the row is the
        // same session summary it is at rest, and the turn's own doings are the
        // transcript's cells, not the status line's.
        let mut screen = screen_for_test(60, 20);
        screen.state.status.set_model("m-1");
        screen.state.apply(Notice::Usage(
            Usage {
                prompt_cache_hit_tokens: 6,
                prompt_cache_miss_tokens: 4,
                ..Usage::default()
            },
            Duration::ZERO,
        ));
        screen.draw().unwrap();
        let last = screen.terminal.backend().buffer().area.height - 1;
        assert_eq!(row(&screen, last), "m-1 · cache 60.0% · hit 6 · miss 4");
        screen.state.begin_turn();
        screen.state.apply(Notice::ToolStart {
            name: "read_file".into(),
            args: "{}".into(),
        });
        screen.state.apply(Notice::Content("here it is".into()));
        screen.draw().unwrap();
        assert_eq!(
            row(&screen, last),
            "m-1 · cache 60.0% · hit 6 · miss 4",
            "a running turn does not take the row over"
        );
    }

    #[test]
    fn a_tool_call_that_changes_a_file_is_drawn_across_its_lines() {
        let mut screen = screen_for_test(40, 20);
        screen.state.transcript.push(Cell::tool_call(
            "Edit",
            r#"{"file_path":"a.rs","old_string":"one\ntwo","new_string":"three"}"#,
        ));
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        assert_eq!(row(&screen, top), "▸ Edit a.rs");
        assert_eq!(row(&screen, top + 1), "  - one");
        assert_eq!(row(&screen, top + 2), "  - two");
        assert_eq!(row(&screen, top + 3), "  + three");
    }

    #[test]
    fn a_long_line_of_a_change_is_wrapped_like_any_other() {
        // It is drawn into a fixed-width region: the terminal cannot be left to
        // wrap it, or the tail of the line is lost.
        let mut screen = screen_for_test(20, 20);
        let long = "x".repeat(30);
        screen.state.transcript.push(Cell::tool_call(
            "Write",
            &format!(r#"{{"file_path":"a.txt","content":"{long}"}}"#),
        ));
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        assert_eq!(row(&screen, top + 1), format!("  + {}", "x".repeat(16)));
        assert_eq!(row(&screen, top + 2), format!("  {}", "x".repeat(14)));
    }

    #[test]
    fn thinking_is_set_in_behind_a_rule_of_its_own() {
        // The thinking is the machinery around an answer rather than the answer, so
        // it is set in two columns -- faintly, behind a rule -- and no longer on a
        // ground of its own. The rule is the whole of what says "this is thinking",
        // which is what makes it a difference a terminal cannot lose: a ground is in
        // the colors and SGR 2 is not honored everywhere, while the columns are in
        // the layout.
        let mut screen = screen_for_test(40, 20);
        screen.state.transcript.push(Cell::Reasoning("hmm".into()));
        screen.state.transcript.push(Cell::Content("answer".into()));
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        let buf = screen.terminal.backend().buffer();
        assert_eq!(row(&screen, top), "┆ hmm");
        assert_eq!(
            buf[(0, top)].bg,
            Color::Reset,
            "no ground to reach the edge with"
        );
        assert_eq!(buf[(39, top)].bg, Color::Reset);
        assert_eq!(
            row(&screen, top + 1),
            "answer",
            "and the answer keeps the edge"
        );
        assert_eq!(buf[(0, top + 1)].bg, Color::Reset);
    }

    #[test]
    fn a_wrapped_think_keeps_the_rule_on_every_line() {
        // A continuation line that came back to the left edge would be a line that
        // reads as an answer, in the middle of a block that is not one.
        let mut screen = screen_for_test(12, 20);
        screen
            .state
            .transcript
            .push(Cell::Reasoning("aaaa bbbb cccc".into()));
        screen.state.transcript.push(Cell::Content("answer".into()));
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        assert_eq!(row(&screen, top), "┆ aaaa bbbb");
        assert_eq!(row(&screen, top + 1), "┆ cccc");
        assert_eq!(row(&screen, top + 2), "answer");
    }

    #[test]
    fn a_long_think_folds_to_its_head_and_a_count() {
        // The think is the one block that grows without bound, and the window
        // pins to the newest line: kept whole, a hundred lines of faint text
        // would be exactly the thing standing between the reader and the answer
        // the turn was for.
        let mut screen = screen_for_test(40, 30);
        let think = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.state.transcript.push(Cell::Reasoning(think));
        screen.state.transcript.push(Cell::Content("answer".into()));
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        for i in 0..THINKING_LINES {
            assert_eq!(row(&screen, top + i as u16), format!("┆ line {i}"));
        }
        assert_eq!(
            row(&screen, top + THINKING_LINES as u16),
            format!("┆ … {} more line(s)", 30 - THINKING_LINES),
            "the count wears the block's own rule and says what is behind it"
        );
        assert_eq!(
            row(&screen, top + THINKING_LINES as u16 + 1),
            "answer",
            "and the answer is still on the screen the think was folded for"
        );
    }

    #[test]
    fn a_think_at_the_cap_is_not_folded() {
        // The count exists to say that something was left out; a block that
        // gave up nothing would be paying a row to say nothing.
        let mut screen = screen_for_test(40, 30);
        let think = (0..THINKING_LINES)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.state.transcript.push(Cell::Reasoning(think));
        screen.draw().unwrap();
        let drawn = all_rows(&screen).join("\n");
        assert!(
            !drawn.contains("more line(s)"),
            "nothing is hidden, so nothing is counted: {drawn}"
        );
    }

    #[test]
    fn only_a_think_folds() {
        // The fold is about dim machinery evicting the answer. The answer
        // itself is the thing the transcript is here for, and it is never
        // counted away.
        let mut screen = screen_for_test(40, 30);
        let long = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.state.transcript.push(Cell::Content(long));
        screen.draw().unwrap();
        let drawn = all_rows(&screen).join("\n");
        assert!(drawn.contains("line 29"), "kept whole: {drawn}");
        assert!(!drawn.contains("more line(s)"), "{drawn}");
    }

    #[test]
    fn a_folded_think_does_not_jump_open_when_it_closes() {
        // The live block and the cell it becomes go through the same layout, so
        // the frame that files the block away is allowed to change nothing: the
        // think the reader watched is the think the transcript keeps.
        let mut screen = screen_for_test(40, 30);
        let think = (0..30)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        screen.state.apply(Notice::Reasoning(think));
        screen.draw().unwrap();
        let live = screen.state.lines(40);
        screen.state.apply(Notice::FinishTurn);
        screen.draw().unwrap();
        assert_eq!(screen.state.lines(40), live, "the same lines either way");
    }

    #[test]
    fn the_gate_is_drawn_whole_however_long_the_call_is() {
        // The question is what the answer is about: a command clipped by the
        // width leaves nothing to decide with.
        let mut screen = screen_for_test(20, 20);
        screen.state.question = Some(Cell::approval(
            "Bash",
            r#"{"command":"rm -rf /tmp/aaaaaaaaaaaaaaaaaaaa"}"#,
        ));
        screen.draw().unwrap();
        let drawn = all_rows(&screen).join("");
        assert!(drawn.contains("aaaa"), "the call is there: {drawn}");
        assert!(
            drawn.contains("run it? [y/N]"),
            "and so is the question: {drawn}"
        );
    }

    #[test]
    fn the_transcript_fills_the_screen_and_shows_its_end() {
        // The whole screen is the transcript, less the pinned rows -- and what
        // does not fit is off the top, because the end is what was just written.
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
        for i in 0..(rows + 5) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        assert_eq!(body(&screen, 0), "line 5", "the first five are off the top");
        assert_eq!(body(&screen, rows as u16 - 1), format!("line {}", rows + 4));
    }

    #[test]
    fn paging_back_moves_the_window_and_paging_forward_returns_it() {
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
        for i in 0..(rows * 3) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        let last = rows * 3;
        // A screen back: the window is the one above the end.
        press(&mut screen.state, KeyCode::PageUp);
        screen.draw().unwrap();
        assert_eq!(body(&screen, 0), format!("line {}", last - rows * 2));
        assert_eq!(
            body(&screen, rows as u16 - 1),
            format!("line {}", last - rows - 1)
        );
        // Forward again, one page at a time.
        press(&mut screen.state, KeyCode::PageDown);
        screen.draw().unwrap();
        assert_eq!(body(&screen, rows as u16 - 1), format!("line {}", last - 1));
        // And no further: there is nothing past the end.
        press(&mut screen.state, KeyCode::PageDown);
        screen.draw().unwrap();
        assert_eq!(body(&screen, rows as u16 - 1), format!("line {}", last - 1));
    }

    #[test]
    fn a_wheel_notch_moves_the_window_three_lines() {
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
        for i in 0..(rows * 3) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        let last = rows * 3;
        // Up: the window moves a notch, not a page.
        assert!(matches!(
            screen.state.key(mouse(MouseEventKind::ScrollUp)),
            Submitted::Nothing
        ));
        screen.draw().unwrap();
        assert_eq!(
            body(&screen, rows as u16 - 1),
            format!("line {}", last - 1 - WHEEL_LINES as usize)
        );
        // Down: back to where the writing ends, and no further, since there is
        // nothing past the end for the window to show.
        for _ in 0..2 {
            screen.state.key(mouse(MouseEventKind::ScrollDown));
        }
        screen.draw().unwrap();
        assert_eq!(body(&screen, rows as u16 - 1), format!("line {}", last - 1));
        assert_eq!(screen.state.scroll.back, 0, "following the end again");
    }

    #[test]
    fn the_wheel_does_not_browse_the_history_the_box_holds() {
        // What this is here for: a terminal that has not been asked for the mouse
        // sends Up and Down in place of a notch, and both of those recall a line
        // -- which is a wheel that reads back through what was typed instead of
        // through what was said.
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
        for i in 0..(rows * 3) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        screen.state.history = vec!["look at src/main.rs".into()];
        screen.state.key(mouse(MouseEventKind::ScrollUp));
        assert!(
            screen.state.textarea.is_empty(),
            "the box was not the thing a notch moved"
        );
        assert!(screen.state.scroll.back > 0, "the transcript was");
    }

    #[test]
    fn a_click_is_not_a_notch_and_moves_nothing() {
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
        for i in 0..(rows * 3) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::Moved,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            assert!(matches!(screen.state.key(mouse(kind)), Submitted::Nothing));
        }
        assert_eq!(screen.state.scroll.back, 0, "the window did not move");
        assert!(screen.state.textarea.is_empty());
    }

    #[test]
    fn the_wheel_reads_back_while_a_turn_runs() {
        // A turn is when there is most to read: the window is the one thing a key
        // pressed during one is allowed to move.
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
        for i in 0..(rows * 3) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        let (cancel, _cancelled) = watch::channel(false);
        screen
            .state
            .key_while_working(mouse(MouseEventKind::ScrollUp), &cancel);
        assert!(screen.state.scroll.back > 0, "the window moved");
        assert!(
            screen.state.textarea.is_empty(),
            "and the box stayed out of it"
        );
    }

    #[test]
    fn lines_arriving_do_not_move_a_reader_who_scrolled_back() {
        // A turn keeps writing while the user reads what came before: the window
        // has to stay on the line they were on instead of sliding to the end
        // under them.
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
        for i in 0..(rows * 3) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        press(&mut screen.state, KeyCode::PageUp);
        screen.draw().unwrap();
        let was = row(&screen, 0);
        for i in 0..5 {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("more {i}")));
        }
        screen.draw().unwrap();
        assert_eq!(row(&screen, 0), was, "still looking at the same line");
    }

    #[test]
    fn submitting_a_line_returns_to_the_end_of_the_transcript() {
        let mut screen = screen_for_test(40, 20);
        for i in 0..(transcript_rows(20, BOX_ROWS, 0) as usize * 2) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        press(&mut screen.state, KeyCode::PageUp);
        assert!(screen.state.scroll.back > 0, "scrolled back");
        screen.state.submit("look at this");
        assert_eq!(screen.state.scroll.back, 0, "back to where it is written");
    }

    #[test]
    fn a_long_line_is_wrapped_into_the_window_not_clipped() {
        // The transcript is drawn into a region of a fixed width, where an
        // over-long line would otherwise be cut off at the edge.
        let mut screen = screen_for_test(10, 30);
        let long = "abcdefghijklmnopqrstuvwxyz"; // 26 columns at width 10
        screen.state.transcript.push(Cell::Notice(long.into()));
        screen.draw().unwrap();
        let rows = all_rows(&screen);
        let joined: String = rows[..transcript_rows(30, BOX_ROWS, 0) as usize]
            .iter()
            .map(|r| unset(r))
            .collect();
        assert_eq!(joined, long, "every column survived, in order");
    }

    #[test]
    fn a_turn_stays_on_screen_when_it_ends() {
        // Nothing is handed to the terminal's scrollback any more, so what is
        // drawn has to outlive the turn that produced it.
        let mut screen = screen_for_test(40, 20);
        screen.state.transcript.push(Cell::Notice("first".into()));
        screen.state.transcript.push(Cell::Notice("second".into()));
        screen.commit();
        screen.draw().unwrap();
        let rows: Vec<String> = all_rows(&screen).iter().map(|r| unset(r)).collect();
        let first = rows.iter().position(|r| r == "first").expect("still there");
        assert_eq!(rows[first + 1], "second", "in order, on the next row");
    }

    #[test]
    fn an_empty_turn_commits_nothing() {
        let mut screen = screen_for_test(40, 20);
        screen.draw().unwrap();
        let before = all_rows(&screen);
        screen.commit();
        screen.draw().unwrap();
        assert_eq!(all_rows(&screen), before);
    }

    #[test]
    fn committing_closes_the_block_that_was_still_streaming() {
        // `commit` ends the turn, so the block open at that moment becomes a
        // finished cell: the next turn's first fragment must open one of its own.
        let mut screen = screen_for_test(40, 20);
        screen.state.stream(Style::Plain, "half a line");
        screen.commit();
        assert!(screen.state.live.is_none(), "nothing left open");
        screen.state.stream(Style::Plain, " and the rest");
        screen.commit();
        assert_eq!(
            screen.state.transcript,
            vec![
                Cell::Content("half a line".into()),
                Cell::Content(" and the rest".into()),
            ],
            "one cell per turn, not one for the two of them"
        );
    }

    // ------------------------------------------------------------ activity ---

    /// A state with a turn running, its clock and its stream pre-loaded.
    fn working(elapsed: Duration, chars: usize, chars_per_token: f64) -> State {
        State {
            turn_running: true,
            turn_started: Some(Instant::now() - elapsed),
            streamed_chars: chars,
            chars_since_usage: chars,
            chars_per_token,
            ..State::default()
        }
    }

    #[test]
    fn the_working_border_spins_counts_and_estimates() {
        // 12 345 ms in: frame 154 % 10 = 4, the fifth glyph; 4 000 characters at
        // a measured four to the token over 12.3 s rounds to 81 a second.
        let s = working(Duration::from_millis(12_345), 4000, 4.0);
        assert_eq!(s.activity_title(60), Some("⠼ 12s · ~81 token/s".to_owned()));
    }

    #[test]
    fn the_estimate_waits_for_the_average_to_settle() {
        // 2 040 ms: frame 25, off a frame boundary so the clock's second read
        // cannot tip it
        let s = working(Duration::from_millis(2_040), 4000, 4.0);
        assert_eq!(s.activity_title(60), Some("⠴ 2s".to_owned()));
    }

    #[test]
    fn a_silent_turn_estimates_nothing() {
        let s = working(Duration::from_millis(30_040), 0, 4.0);
        assert_eq!(s.activity_title(60), Some("⠴ 30s".to_owned()));
    }

    #[test]
    fn a_narrow_border_drops_the_estimate_then_hides_the_indicator() {
        let s = working(Duration::from_millis(12_345), 4000, 4.0);
        let full = s.activity_title(usize::MAX).unwrap();
        let count = "⠼ 12s".to_owned();
        // one column short of the whole thing, the estimate goes whole
        assert_eq!(
            s.activity_title(crate::ui::text::width(&full) - 1),
            Some(count.clone())
        );
        // one column short of the count, nothing at all: a clipped spinner is
        // not an indicator
        assert_eq!(s.activity_title(crate::ui::text::width(&count) - 1), None);
    }

    #[test]
    fn the_working_indicator_is_not_dim_on_a_dim_rule() {
        // While the model is quiet, the spinner on the box's rule is the only thing
        // on the screen that moves: it is chrome that has to be seen, so it is the
        // one thing on the box painted at full strength. A dim indicator on a dim
        // rule is the signal painted out of sight.
        let mut screen = screen_for_test(60, 20);
        screen.state = working(Duration::from_secs(3), 0, 4.0);
        screen.draw().unwrap();
        let rule = screen.terminal.backend().buffer().area.height - 1 - 3;
        let buf = screen.terminal.backend().buffer();
        let at = (0..buf.area.width)
            .find(|&x| SPINNER.contains(&buf[(x, rule)].symbol().chars().next().unwrap_or(' ')))
            .expect("the indicator is drawn on the box's top rule");
        assert!(
            !buf[(at, rule)].style().add_modifier.contains(Modifier::DIM),
            "the indicator is lit"
        );
        assert!(
            buf[(0, rule)].style().add_modifier.contains(Modifier::DIM),
            "and the rule it sits on is still chrome"
        );
    }

    #[test]
    fn calibration_learns_from_a_usage_notice() {
        let mut s = State {
            turn_running: true,
            turn_started: Some(Instant::now()),
            ..State::default()
        };
        s.stream(Style::Reasoning, &"x".repeat(800));
        s.apply(Notice::Usage(
            Usage {
                completion_tokens: 200,
                ..Usage::default()
            },
            Duration::ZERO,
        ));
        assert_eq!(s.chars_per_token, 4.0);
        // the counter is spent on the measurement: the next ratio starts clean
        assert_eq!(s.chars_since_usage, 0);
    }

    #[test]
    fn a_question_takes_the_border_title_back() {
        let mut s = working(Duration::from_secs(12), 4000, 4.0);
        let (tx, _rx) = oneshot::channel();
        s.open_question(tx);
        assert_eq!(s.activity_title(60), None);
    }

    #[test]
    fn the_border_shows_the_working_turn_and_then_does_not() {
        let mut screen = screen_for_test(60, 20);
        screen.state.begin_turn();
        screen.state.turn_started = Some(Instant::now() - Duration::from_secs(12));
        screen.state.streamed_chars = 4000;
        screen.state.chars_per_token = 4.0;
        screen.draw().unwrap();
        // the box's top border: the status line's row, the box's three, and no
        // queue above it
        let top = 20 - 1 - BOX_ROWS;
        let line = row(&screen, top);
        assert!(line.contains("token/s"), "{line:?}");
        screen.state.end_turn();
        screen.draw().unwrap();
        assert!(!row(&screen, top).contains("token/s"));
    }

    fn press(state: &mut State, code: KeyCode) -> Submitted {
        state.key(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    /// A mouse event of a given kind. Where the pointer is does not matter: this
    /// front end reads the kind and nothing else.
    fn mouse(kind: MouseEventKind) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column: 1,
            row: 1,
            modifiers: KeyModifiers::NONE,
        })
    }

    fn type_in(state: &mut State, text: &str) {
        for c in text.chars() {
            press(state, KeyCode::Char(c));
        }
    }

    /// Ctrl-J: the newline key, and so the one the box has to make room for.
    fn ctrl_j() -> Event {
        Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
    }

    #[test]
    fn a_slash_opens_the_picker_and_a_space_closes_it() {
        let mut screen = State::default();
        assert!(screen.picker.is_none(), "nothing typed, nothing to offer");
        type_in(&mut screen, "/res");
        let picker = screen.picker.as_ref().expect("a command is being named");
        assert_eq!(
            picker
                .choices
                .iter()
                .map(|c| c.label.as_str())
                .collect::<Vec<_>>(),
            vec!["/resume"]
        );
        // A space means the rest is an argument, not part of the name.
        type_in(&mut screen, " 2026");
        assert!(screen.picker.is_none());
    }

    #[test]
    fn the_highlight_wraps_in_both_directions() {
        let mut screen = State::default();
        type_in(&mut screen, "/");
        let count = screen.picker.as_ref().unwrap().choices.len();
        assert!(count > 1, "the bare slash offers every command");
        assert_eq!(screen.picker.as_ref().unwrap().selected, 0);
        press(&mut screen, KeyCode::Up);
        assert_eq!(
            screen.picker.as_ref().unwrap().selected,
            count - 1,
            "up from the first reaches the last"
        );
        press(&mut screen, KeyCode::Down);
        assert_eq!(screen.picker.as_ref().unwrap().selected, 0);
    }

    #[test]
    fn tab_puts_the_highlighted_command_in_the_box_without_running_it() {
        let mut screen = State::default();
        type_in(&mut screen, "/res");
        assert!(matches!(
            press(&mut screen, KeyCode::Tab),
            Submitted::Nothing
        ));
        assert_eq!(screen.text(), "/resume");
        assert!(screen.picker.is_none(), "it has been chosen");
        // And it is not submitted: an argument may still be wanted.
        assert!(!screen.history.iter().any(|h| h == "/resume"));
    }

    #[test]
    fn escape_dismisses_the_command_picker_and_the_line_it_was_filtering() {
        let mut screen = State::default();
        type_in(&mut screen, "/s");
        press(&mut screen, KeyCode::Esc);
        assert!(screen.picker.is_none());
        assert_eq!(
            screen.text(),
            "",
            "the half-typed command goes with the list: a lone `/` left behind \
             would glue itself to the next word"
        );
        assert_eq!(
            screen.textarea.placeholder_text(),
            IDLE_PLACEHOLDER,
            "the box is back to inviting the next message"
        );
    }

    #[test]
    fn escape_while_a_turn_runs_gives_the_queue_line_back() {
        let mut screen = State::default();
        screen.begin_turn();
        type_in(&mut screen, "/s");
        press(&mut screen, KeyCode::Esc);
        assert!(screen.picker.is_none());
        assert_eq!(
            screen.textarea.placeholder_text(),
            QUEUE_PLACEHOLDER,
            "the box still belongs to the turn that is running"
        );
    }

    /// A session as the picker sees it. Its path is never read: the front end is
    /// handed the list, it does not go looking for it.
    fn session_info(id: &str, messages: usize, preview: &str) -> session::SessionInfo {
        session::SessionInfo {
            id: id.to_owned(),
            path: std::path::PathBuf::from(format!("/nowhere/{id}.jsonl")),
            modified: 0,
            message_count: messages,
            preview: preview.to_owned(),
        }
    }

    #[test]
    fn sessions_are_offered_newest_first_with_what_is_in_them() {
        let mut screen = State::default();
        assert!(screen.open_sessions(&[
            session_info("20260910-120000", 12, "what does this do?"),
            session_info("20260909-090000", 3, "hello"),
        ]));
        let picker = screen.picker.as_ref().unwrap();
        assert_eq!(picker.kind, Choosing::Session);
        let shown: Vec<_> = picker
            .choices
            .iter()
            .map(|c| (c.label.as_str(), c.detail.as_str()))
            .collect();
        assert_eq!(
            shown,
            vec![
                ("20260910-120000", "12 messages · what does this do?"),
                ("20260909-090000", "3 messages · hello"),
            ],
            "the order is the list's, and the detail is what is in the session"
        );
    }

    #[test]
    fn choosing_a_session_submits_the_line_that_switches_to_it() {
        // The choice is not a completion: it is the line the plain prompt would
        // have been given, submitted, so switching itself has one implementation.
        let mut screen = State::default();
        screen.open_sessions(&[
            session_info("newest", 1, "hi"),
            session_info("older", 2, "yo"),
        ]);
        press(&mut screen, KeyCode::Down);
        assert!(matches!(
            press(&mut screen, KeyCode::Enter),
            Submitted::Line
        ));
        assert_eq!(screen.text(), "/resume older");
        assert!(screen.picker.is_none(), "it has been chosen");
    }

    #[test]
    fn tab_takes_the_highlighted_session_too() {
        let mut screen = State::default();
        screen.open_sessions(&[session_info("only", 1, "hi")]);
        assert!(matches!(press(&mut screen, KeyCode::Tab), Submitted::Line));
        assert_eq!(screen.text(), "/resume only");
    }

    #[test]
    fn typing_does_not_turn_a_session_list_into_a_command_search() {
        // It was asked for in full, and it stays until it is answered or
        // dismissed: recomputing matches under the user's fingers would replace
        // the list with something they did not ask for.
        let mut screen = State::default();
        screen.open_sessions(&[session_info("one", 1, "hi")]);
        type_in(&mut screen, "/he");
        let picker = screen.picker.as_ref().expect("still the session list");
        assert_eq!(picker.kind, Choosing::Session);
        assert_eq!(picker.choices.len(), 1);
        press(&mut screen, KeyCode::Esc);
        assert!(screen.picker.is_none(), "escape is how it is dismissed");
        assert_eq!(
            screen.text(),
            "/he",
            "a row list is not the line's: what was typed since it opened is a \
             draft, not a query, and dismissing the list has no claim on it"
        );
    }

    #[test]
    fn nothing_to_offer_leaves_the_line_alone() {
        // With no sessions on disk the command has to reach the handler, which is
        // where both front ends say what an id is for.
        let mut screen = State::default();
        assert!(!screen.open_sessions(&[]));
        assert!(screen.picker.is_none());
    }

    #[test]
    fn the_session_list_is_drawn_like_the_command_one() {
        let mut screen = screen_for_test(60, 20);
        screen
            .state
            .open_sessions(&[session_info("20260910-124721", 3, "what is in this file?")]);
        screen.draw().unwrap();
        let rows = all_rows(&screen);
        assert!(
            rows.iter().any(|r| r.contains("20260910-124721")
                && r.contains("3 messages · what is in this file?")),
            "one row per session: {rows:?}"
        );
    }

    #[test]
    fn a_provider_menu_reads_by_name_and_a_model_menu_by_id() {
        // The two menus read differently on purpose: a provider is picked by the
        // name a person knows it by, a model by the `<provider id>/<modelid>` the
        // flags, the session and the status line all carry.
        let mut screen = screen_for_test(70, 20);
        screen
            .state
            .open_choices(Choosing::Provider, choice_rows(config::provider_choices()));
        screen.draw().unwrap();
        let rows = all_rows(&screen);
        assert!(rows.iter().any(|r| r.contains("DeepSeek")), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.contains("Z.AI Coding CN")),
            "{rows:?}"
        );
        assert!(
            !rows.iter().any(|r| r.contains("deepseek ")),
            "the menu does not show what is typed at: {rows:?}"
        );

        screen.state.open_choices(
            Choosing::Model,
            choice_rows(config::model_menu("deepseek/deepseek-flash")),
        );
        screen.draw().unwrap();
        let rows = all_rows(&screen);
        assert!(
            rows.iter().any(|r| r.contains("deepseek/deepseek-v4-pro")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.contains("zai-coding-cn/glm-5.3")),
            "{rows:?}"
        );
    }

    #[test]
    fn up_and_down_reach_the_history_once_the_picker_is_closed() {
        // The two never compete: the picker is only open while a command is
        // being named, and browsing closes it.
        let mut screen = State::default();
        screen.remember("first thing");
        screen.remember("second thing");
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.text(), "second thing", "the most recent first");
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.text(), "first thing");
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.text(), "first thing", "the oldest is the end");
        press(&mut screen, KeyCode::Down);
        assert_eq!(screen.text(), "second thing");
        press(&mut screen, KeyCode::Down);
        assert_eq!(screen.text(), "", "past the newest is the empty draft");
    }

    #[test]
    fn browsing_gives_back_what_was_being_typed() {
        let mut screen = State::default();
        screen.remember("old line");
        type_in(&mut screen, "half a thought");
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.text(), "old line");
        press(&mut screen, KeyCode::Down);
        assert_eq!(screen.text(), "half a thought", "the draft came back");
    }

    #[test]
    fn browsing_an_empty_history_does_nothing() {
        let mut screen = State::default();
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.text(), "");
        assert!(screen.browsing.is_none());
    }

    #[test]
    fn a_recalled_command_does_not_open_the_picker() {
        // It would take the very keys being used to browse.
        let mut screen = State::default();
        screen.remember("/help");
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.text(), "/help");
        assert!(screen.picker.is_none());
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.text(), "/help", "still browsing, not completing");
    }

    #[test]
    fn repeating_a_line_is_not_recorded_twice() {
        let mut screen = State::default();
        screen.remember("same");
        screen.remember("same");
        screen.remember("other");
        screen.remember("same");
        assert_eq!(screen.history, vec!["same", "other", "same"]);
        screen.remember("");
        assert_eq!(screen.history.len(), 3, "an empty line is not a line");
    }

    #[test]
    fn the_history_keeps_only_the_most_recent_entries() {
        let mut screen = State::default();
        for i in 0..(history::MAX_ENTRIES + 5) {
            screen.remember(&format!("line {i}"));
        }
        assert_eq!(screen.history.len(), history::MAX_ENTRIES);
        assert_eq!(screen.history[0], "line 5", "the oldest went first");
    }

    #[test]
    fn submitting_empties_the_box_and_the_browsing_state() {
        let mut screen = State::default();
        screen.remember("earlier");
        press(&mut screen, KeyCode::Up);
        assert_eq!(screen.text(), "earlier");
        assert_eq!(screen.take_line(), "earlier");
        assert!(screen.text().is_empty());
        assert!(screen.browsing.is_none());
        assert!(screen.draft.is_empty());
    }

    #[test]
    fn a_window_holds_the_row_it_is_scrolled_to() {
        // What a window costs the screen: the rows it draws, plus a row for each
        // count it carries. A window costing more than the room it was given is a
        // window that eats the rows behind it.
        let cost =
            |w: &Window| (w.last - w.first) + usize::from(w.above > 0) + usize::from(w.below > 0);
        // The longest run of rows that holds the selection and fits the room with
        // the counts it is cut at, found by looking at every run there is rather
        // than by the arithmetic the front end uses: the two have to agree.
        let expected = |total: usize, selected: usize, room: usize| -> Window {
            let mut best: Option<Window> = None;
            for shown in 1..=room.min(total) {
                for first in 0..=total - shown {
                    let last = first + shown;
                    if !(first <= selected && selected < last) {
                        continue;
                    }
                    let window = Window {
                        first,
                        last,
                        above: first,
                        below: total - last,
                    };
                    if cost(&window) > room {
                        continue;
                    }
                    // Longer runs win, and runs of a length are looked at left to
                    // right, so the first one wins: the selection sits at the end
                    // of the window it moved into.
                    if best.is_none_or(|b| shown > b.last - b.first) {
                        best = Some(window);
                    }
                }
            }
            // No run fits even with no count at all: the selection alone, which is
            // what the front end falls back to as well.
            best.unwrap_or(Window {
                first: selected,
                last: selected + 1,
                above: 0,
                below: 0,
            })
        };
        for total in 1..24usize {
            for selected in 0..total {
                for room in 1..=PICKER_ROWS {
                    let got = picker_window(total, selected, room);
                    let what = format!("{total} rows, {selected} selected, room {room}");
                    assert_eq!(got, expected(total, selected, room), "{what}");
                    assert!(
                        got.first <= selected && selected < got.last,
                        "{what}: selection"
                    );
                    assert!(got.last <= total, "{what}: past the end");
                    assert!(cost(&got) <= room, "{what}: rows drawn");
                    // Either both counts are drawn and they are the truth, or
                    // neither is: and neither only where they would not fit.
                    if cost(&got) > got.last - got.first {
                        assert_eq!(
                            (got.above, got.below),
                            (got.first, total - got.last),
                            "{what}: what the counts say"
                        );
                    } else if total > room {
                        assert!(cost(&got) + 2 > room, "{what}: a cut with no count");
                    }
                }
            }
        }
    }

    #[test]
    fn the_picker_keeps_its_highlight_on_the_screen() {
        // The regression: rows were drawn from the first choice down, so a menu
        // longer than the picker could be scrolled past its own end -- and `Enter`
        // then chose a row the screen never named. Nine sessions, the ninth
        // selected: the row under the highlight is the one that has to be drawn.
        let mut screen = screen_for_test(60, 20);
        let choices: Vec<Choice> = (0..9)
            .map(|i| Choice {
                label: format!("session-{i}"),
                argument: format!("session-{i}"),
                detail: format!("{i} messages"),
            })
            .collect();
        screen.state.open_choices(Choosing::Session, choices);
        for _ in 0..8 {
            screen.state.down();
        }
        screen.draw().unwrap();

        let buf = screen.terminal.backend().buffer();
        let highlighted: Vec<String> = (0..buf.area.height)
            .filter(|&y| {
                (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::REVERSED))
            })
            .map(|y| row(&screen, y))
            .collect();
        let chosen: Vec<&String> = highlighted
            .iter()
            .filter(|r| r.contains("session-"))
            .collect();
        assert_eq!(
            chosen.len(),
            1,
            "one picker row is highlighted: {highlighted:?}"
        );
        assert!(chosen[0].contains("session-8"), "{:?}", chosen[0]);
        // And the rows the window is not showing are counted, not dropped: four
        // of the nine are above it.
        let drawn = all_rows(&screen);
        assert!(drawn.iter().any(|r| r.contains("… 4 more")), "{drawn:?}");
        // The count costs rows, so the block still fits what the transcript can
        // spare: five rows of menu and the count.
        assert_eq!(
            drawn.iter().filter(|r| r.contains("session-")).count(),
            5,
            "{drawn:?}"
        );
    }

    #[test]
    fn the_queue_counts_the_rows_it_is_not_showing() {
        let mut screen = screen_for_test(60, 20);
        for line in ["/new", "/sessions", "/model"] {
            screen.state.queued.push_back(line.into());
        }
        // A queue with room to spare: every line, and nothing said about rows that
        // are not there.
        let drawn = rendered(&screen.state.queue_lines(60));
        assert_eq!(drawn.len(), 3, "{drawn:?}");
        assert!(
            drawn.iter().all(|(text, _)| !text.contains("more")),
            "{drawn:?}"
        );

        // One more line than the cap, and the row that does not fit is counted on
        // one of the rows the cap allows: the queue never costs more than three.
        screen.state.queued.push_back("/help".into());
        let drawn = rendered(&screen.state.queue_lines(60));
        assert_eq!(drawn.len(), QUEUE_ROWS, "{drawn:?}");
        assert!(drawn[0].0.contains("… 2 more"), "{drawn:?}");
        assert!(drawn[1].0.contains("/model"), "{drawn:?}");
        assert!(drawn[2].0.contains("/help"), "{drawn:?}");
    }

    #[test]
    fn the_picker_is_drawn_over_the_live_area_with_one_row_highlighted() {
        let mut screen = screen_for_test(40, 20);
        type_in(&mut screen.state, "/");
        screen.draw().unwrap();
        let rows = all_rows(&screen);
        let shows = |needle: &str| {
            let at = rows.iter().position(|r| r.contains(needle));
            at.expect("the picker was drawn")
        };
        let help = shows("/help");
        let resume = shows("/resume");
        assert_eq!(resume, help + 3, "the whole list, in table order");
        // The first entry is highlighted, and only it.
        let selected = screen
            .terminal
            .backend()
            .buffer()
            .content
            .iter()
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .count();
        assert!(selected > 0, "something is highlighted");
    }

    #[test]
    fn the_transcript_gets_the_rows_the_pinned_region_leaves() {
        // Asked of the layout rather than worked out here. What a terminal too
        // short for the pinned region has to say about it is the whole point: the
        // transcript gets no rows at all there, and a window that believed the
        // arithmetic instead would be scrolling against a row that was never
        // drawn.
        let transcript = |height: u16, input: u16, queued: u16| {
            screen_rows(Rect::new(0, 0, 80, height), input, queued)[0].height
        };
        assert_eq!(
            transcript(24, BOX_ROWS, 0),
            24 - PINNED_ROWS - BOX_ROWS,
            "the box has its borders and the status line its row"
        );
        assert_eq!(
            transcript(5, BOX_ROWS, 0),
            1,
            "one row, on the shortest terminal that can spare it"
        );
        assert_eq!(
            transcript(1, BOX_ROWS, 0),
            0,
            "and none at all when there is nothing to spare"
        );
        assert_eq!(transcript(1, 0, 0), 0, "the status line has the only row");
    }

    #[test]
    fn the_box_grows_with_the_lines_in_it() {
        // One row per line, borders on top and bottom -- and Ctrl-J is what puts
        // lines in it, so this is the arithmetic that makes that key visible.
        assert_eq!(box_rows(0, 20), BOX_ROWS, "empty is still a box");
        assert_eq!(box_rows(1, 20), BOX_ROWS);
        assert_eq!(box_rows(2, 20), 4);
        assert_eq!(box_rows(6, 20), 8);
    }

    #[test]
    fn the_box_draws_no_caret_of_its_own() {
        // One caret per screen. The editor drew one by reversing whatever was under
        // it, and `place_cursor` put the terminal's own on the same cell: two
        // carets on one cell, and the one that was kept is the terminal's -- it is
        // the one that blinks, that a bar-shaped cursor can be seen in, and that
        // says where a typed character lands.
        let mut screen = screen_for_test(40, 20);
        screen.draw().unwrap();
        let last = screen.terminal.backend().buffer().area.height - 1;
        let buf = screen.terminal.backend().buffer();
        assert!(
            !(0..buf.area.height).any(|y| (0..buf.area.width).any(|x| buf[(x, y)]
                .style()
                .add_modifier
                .contains(Modifier::REVERSED))),
            "nothing on an idle screen is reversed"
        );
        let cursor = screen.terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!(cursor.y, last - 2, "the box's line, between its rules");
        assert_eq!(cursor.x, BOX_GUTTER, "and the column the draft starts in");
    }

    #[test]
    fn a_failure_is_drawn_by_weight_and_not_only_by_color() {
        // The color of a failure is a palette slot the terminal's theme picked for a
        // background this code cannot see, and the dark end of a default palette is
        // chosen for a light one: red at its darkest on black is a line that cannot
        // be read, on the one line that must be. The weight is what carries it.
        let mut screen = screen_for_test(40, 20);
        screen
            .state
            .apply(Notice::Error("the backend said 402".into()));
        screen.state.apply(Notice::ToolStart {
            name: "Bash".into(),
            args: r#"{"command":"ls"}"#.into(),
        });
        screen.draw().unwrap();
        let buf = screen.terminal.backend().buffer();
        let at = |needle: &str| -> (u16, u16) {
            for y in 0..buf.area.height {
                let text = row(&screen, y);
                if let Some(x) = text.find(needle) {
                    return (x as u16, y);
                }
            }
            panic!("{needle:?} was not drawn");
        };
        for needle in ["error:", "Bash"] {
            let (x, y) = at(needle);
            let style = buf[(x, y)].style();
            assert!(
                style.add_modifier.contains(Modifier::BOLD),
                "{needle:?} is drawn bold"
            );
            assert!(style.fg.is_some(), "{needle:?} keeps its color too");
        }
    }

    #[test]
    fn the_box_is_drawn_on_a_terminal_with_no_room_to_draw_it_in() {
        // The field and the marker are cut out of the box's own area, and both of
        // them take columns from it. A terminal narrower than the gutter, or shorter
        // than the two rules, leaves neither with anything -- and a draw is the one
        // thing that cannot be allowed to fail there, since it is the draw that has
        // to put the smaller screen on the screen.
        for (width, height) in [(1, 1), (1, 3), (2, 2), (3, 3), (80, 2), (80, 1)] {
            let mut screen = screen_for_test(width, height);
            screen.draw().unwrap_or_else(|e| {
                panic!("{width}x{height} would not draw: {e:?}");
            });
        }
        // And the field is asked for what is there rather than for what it wants:
        // a negative width is not a rect.
        let area = Rect::new(0, 0, 80, 3);
        let field = box_field(area);
        assert_eq!(field.x, BOX_GUTTER);
        assert_eq!(field.width, 80 - BOX_GUTTER);
        assert_eq!(field.height, 1, "between the rules");
        assert_eq!(box_field(Rect::new(0, 0, 1, 3)).width, 0);
        assert_eq!(box_field(Rect::new(0, 0, 1, 1)).height, 0);
    }

    #[test]
    fn a_box_taller_than_the_screen_gives_way_to_the_transcript() {
        // A draft longer than the terminal can show still leaves a row of
        // transcript: past that the box scrolls inside itself.
        let height = 12;
        let most = height - PINNED_ROWS - 1;
        assert_eq!(box_rows(100, height), most);
        assert_eq!(
            screen_rows(Rect::new(0, 0, 80, height), box_rows(100, height), 0)[0].height,
            1
        );
        // And a terminal too short for the box, the status line and a transcript
        // row all at once takes it out of the box. Asking for the three rows the
        // box wants on a two-row terminal is asking the layout for rows it does
        // not have: it answers with a one-row box, and the transcript's own answer
        // is nothing -- which is how the transcript ends up windowed against a row
        // nobody drew.
        assert_eq!(
            screen_rows(Rect::new(0, 0, 80, 2), BOX_ROWS, 0)
                .iter()
                .map(|r| r.height)
                .collect::<Vec<_>>(),
            vec![0, 0, 1, 1],
            "what the layout does with a request it cannot grant"
        );
        assert_eq!(box_rows(100, 2), 0, "so the box asks for nothing instead");
        assert_eq!(
            screen_rows(Rect::new(0, 0, 80, 2), box_rows(100, 2), 0)[0].height,
            1,
            "and the transcript gets the row the box no longer holds"
        );
        assert_eq!(box_rows(100, 3), 1, "a box with one row is the next line");
        assert_eq!(
            screen_rows(Rect::new(0, 0, 80, 3), box_rows(100, 3), 0)[0].height,
            1,
            "and the transcript keeps its row"
        );
    }

    #[test]
    fn a_cramped_screen_windows_the_rows_the_layout_gave_it() {
        // Every height, however short: what the window is measured against has to
        // be the rows the transcript was actually drawn into. The two were worked
        // out separately, and below the height the pinned region needs they
        // disagreed -- so the window scrolled against a row that was never drawn,
        // and `drawn_rows` is what paging and staying put both read.
        for height in 1..8u16 {
            let mut screen = screen_for_test(40, height);
            screen.state.transcript.push(Cell::Content("one".into()));
            screen.state.transcript.push(Cell::Content("two".into()));
            screen.draw().unwrap();
            let area = origin(&mut screen);
            let input = screen.state.input_rows(height);
            let queued = screen.state.queue_lines(40).len() as u16;
            assert_eq!(
                screen.state.drawn_rows,
                screen_rows(area, input, queued)[0].height as usize,
                "a {height}-row terminal"
            );
        }
        // And the row it drew is the one the window asked for: the room the
        // layout gave it, taken from the end of the transcript. Five rows is the
        // shortest terminal with room for the box as well as a transcript row.
        let mut screen = screen_for_test(40, 5);
        screen.state.transcript.push(Cell::Content("one".into()));
        screen.state.transcript.push(Cell::Content("two".into()));
        screen.draw().unwrap();
        assert_eq!(row(&screen, 0), "two", "the last line of the transcript");
        assert!(row(&screen, 1).starts_with('─'), "then the box");
        assert!(row(&screen, 4).contains("cache"), "and the status line");
    }

    #[test]
    fn a_draw_lays_out_only_what_arrived_since_the_last_one() {
        // What is pinned here is invisible on the screen -- a draw that wrapped
        // the whole session would draw the same picture -- and it is the whole
        // point of the layout being kept: a session is as long as the
        // conversation has been, and a fragment arrives many times a second.
        let mut screen = screen_for_test(40, 20);
        screen.state.transcript.push(Cell::Content("one".into()));
        screen.state.transcript.push(Cell::Content("two".into()));
        screen.state.laid_cells = 0;
        screen.draw().unwrap();
        assert_eq!(screen.state.laid_cells, 2, "both of them, once");
        screen.draw().unwrap();
        assert_eq!(screen.state.laid_cells, 2, "and not again");

        // A fragment of a running turn is not a cell yet: the block it is writing
        // is wrapped as it is drawn, and it becomes a cell -- one to lay out --
        // when it closes.
        screen.state.apply(Notice::Content("three".into()));
        screen.draw().unwrap();
        assert_eq!(screen.state.laid_cells, 2, "still the two cells");
        screen.state.apply(Notice::FinishTurn);
        screen.draw().unwrap();
        assert_eq!(screen.state.laid_cells, 3, "the block it left behind");

        // A resize re-lays the whole transcript: every line was wrapped to a
        // width, so a new width is a new layout of every cell there is.
        screen.terminal.backend_mut().resize(30, 20);
        screen.draw().unwrap();
        assert_eq!(
            screen.state.laid_cells, 6,
            "all three again, at the new width"
        );
    }

    #[test]
    fn a_window_is_the_same_lines_as_the_transcript_it_is_a_window_on() {
        // Keeping the layout is a way of getting the same lines, not a second way
        // of deciding them: what the window hands the painter has to be the lines
        // the same cells wrap to at the width they were laid at.
        let mut screen = screen_for_test(24, 12);
        for i in 0..20 {
            screen
                .state
                .transcript
                .push(Cell::Content(format!("line {i}")));
        }
        screen.draw().unwrap();
        let width = 24;
        let whole: Vec<Line> = screen
            .state
            .transcript
            .iter()
            .flat_map(|cell| cell_lines(cell, width))
            .collect();
        assert_eq!(screen.state.lines(width), whole, "the whole transcript");
        assert_eq!(
            screen.state.window_lines(3, 9, &[], &[]),
            whole[3..9].to_vec(),
            "and a window in the middle of one cell"
        );
    }

    #[test]
    fn everything_that_changes_the_screen_moves_the_revision() {
        let mut state = State::default();
        let mut moved = Vec::new();
        let mut step = |state: &State| moved.push(state.revision);
        step(&state);
        state.apply(Notice::Content("hello".into()));
        step(&state);
        state.key(Event::Key(KeyEvent::from(KeyCode::Char('h'))));
        step(&state);
        let (reply, _answer) = oneshot::channel();
        state.open_question(reply);
        step(&state);
        state.close_question();
        step(&state);
        let mut sorted = moved.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted, moved, "every step moved it: {moved:?}");
    }

    /// A backend that counts the frames it is asked to paint, so that a draw the
    /// screen decided to skip is something a test can observe rather than infer.
    struct Counting {
        inner: ratatui::backend::TestBackend,
        frames: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Backend for Counting {
        type Error = std::convert::Infallible;

        fn draw<'a, I>(&mut self, content: I) -> Result<(), std::convert::Infallible>
        where
            I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
        {
            self.frames.set(self.frames.get() + 1);
            self.inner.draw(content)
        }

        fn hide_cursor(&mut self) -> Result<(), std::convert::Infallible> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> Result<(), std::convert::Infallible> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(
            &mut self,
        ) -> Result<ratatui::layout::Position, std::convert::Infallible> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<ratatui::layout::Position>>(
            &mut self,
            position: P,
        ) -> Result<(), std::convert::Infallible> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> Result<(), std::convert::Infallible> {
            self.inner.clear()
        }

        fn clear_region(
            &mut self,
            clear_type: ratatui::backend::ClearType,
        ) -> Result<(), std::convert::Infallible> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> Result<ratatui::layout::Size, std::convert::Infallible> {
            self.inner.size()
        }

        fn window_size(
            &mut self,
        ) -> Result<ratatui::backend::WindowSize, std::convert::Infallible> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> Result<(), std::convert::Infallible> {
            self.inner.flush()
        }
    }

    #[test]
    fn an_unchanged_screen_is_not_drawn_again() {
        let frames = std::rc::Rc::new(std::cell::Cell::new(0));
        let backend = Counting {
            inner: ratatui::backend::TestBackend::new(40, 20),
            frames: frames.clone(),
        };
        let mut screen = Screen {
            terminal: fullscreen(backend).unwrap(),
            state: State::default(),
            tty: None,
            drawn: None,
        };
        screen.draw_if_changed().unwrap();
        assert_eq!(frames.get(), 1);
        screen.draw_if_changed().unwrap();
        assert_eq!(frames.get(), 1, "nothing changed, so nothing was drawn");
        // A keystroke changes what the box holds, so the next draw happens.
        screen
            .state
            .key(Event::Key(KeyEvent::from(KeyCode::Char('h'))));
        screen.draw_if_changed().unwrap();
        assert_eq!(frames.get(), 2);
        // So does a turn starting: the box's placeholder says so, which is a
        // change the revision has to carry.
        screen.state.begin_turn();
        screen.draw_if_changed().unwrap();
        assert_eq!(frames.get(), 3);
    }

    #[test]
    fn closing_a_turn_is_worth_a_draw() {
        // The turn's end is the one thing that changes the screen without a
        // notice arriving, so it has to move the revision itself.
        let mut screen = screen_for_test(40, 20);
        screen.state.stream(Style::Plain, "half a line");
        screen.draw_if_changed().unwrap();
        let drawn = screen.state.revision;
        screen.commit();
        assert_ne!(screen.state.revision, drawn, "the draw cannot be skipped");
    }

    #[test]
    fn both_front_ends_satisfy_the_handle_the_repl_needs() {
        // Compile-time guard: `repl::handle` is written against `Front`, and the
        // interactive front end must keep satisfying it alongside the plain one.
        fn assert_front<T: Front>() {}
        assert_front::<Notifier>();
        assert_front::<Renderer>();
    }

    #[test]
    fn the_notifier_reaches_the_loop_through_the_channel() {
        // The `Ui` side must not touch the terminal: everything it is told has to
        // come out as a notice.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut n = Notifier { tx };
        n.content_delta("hi");
        assert!(matches!(rx.try_recv(), Ok(Notice::Content(s)) if s == "hi"));
    }

    #[test]
    fn what_the_user_says_becomes_part_of_the_transcript() {
        // Replay reads the user's line out of the log, so a live turn has to put it
        // in the same place: the same session must not read two ways depending on
        // when it was looked at.
        let mut state = State::default();
        state.submit("look at src/main.rs");
        assert_eq!(
            state.transcript,
            vec![Cell::User("look at src/main.rs".into())]
        );
        assert_eq!(state.history, vec!["look at src/main.rs"]);
    }

    #[test]
    fn a_command_is_not_part_of_the_transcript() {
        // The handler answers commands without the model seeing them, so a replayed
        // session does not have them either.
        let mut state = State::default();
        state.submit("/help");
        assert!(state.transcript.is_empty(), "nothing to replay");
        assert_eq!(state.history, vec!["/help"], "but it is worth recalling");
    }

    #[test]
    fn a_submitted_line_is_drawn_above_what_the_turn_says() {
        let mut screen = screen_for_test(40, 20);
        screen.state.submit("look at src/main.rs");
        screen.state.transcript.push(Cell::Content("on it".into()));
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        assert_eq!(row(&screen, top), "› look at src/main.rs");
        assert_eq!(row(&screen, top + 1), "on it");
    }

    #[test]
    fn the_gate_is_answered_by_typing_at_it_while_the_turn_runs() {
        // The answer is typed while a turn runs, where the box is otherwise the
        // next line's: this is the one path where a keystroke is not the queue's,
        // and it has to work -- an answer that never arrives leaves the turn
        // waiting on a question nobody can see.
        let mut state = State::default();
        let (cancel, _cancelled) = watch::channel(false);
        let (reply, answer) = oneshot::channel();
        state.open_question(reply);
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char('y'))), &cancel);
        assert_eq!(state.textarea.lines(), ["y"], "it went into the box");
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
        assert_eq!(answer.blocking_recv(), Ok(true));
        assert!(state.reply.is_none(), "the gate is closed again");
    }

    #[test]
    fn a_blank_answer_to_the_gate_denies() {
        let mut state = State::default();
        let (cancel, _cancelled) = watch::channel(false);
        let (reply, answer) = oneshot::channel();
        state.open_question(reply);
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
        assert_eq!(answer.blocking_recv(), Ok(false));
    }

    /// Type into the box the way the event loop delivers a key while a turn runs.
    fn type_while_working(state: &mut State, text: &str, cancel: &watch::Sender<bool>) {
        for c in text.chars() {
            state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(c))), cancel);
        }
    }

    #[test]
    fn a_line_typed_while_a_turn_runs_is_queued_and_not_dropped() {
        // A turn is not the place to start anything -- but it is the place to say
        // what should follow it, and that is what the box is for while it runs.
        let mut state = State::default();
        let (cancel, cancelled) = watch::channel(false);
        type_while_working(&mut state, "the next thing", &cancel);
        assert_eq!(state.textarea.lines(), ["the next thing"]);
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
        assert_eq!(state.queued, ["the next thing"]);
        assert!(
            state.textarea.is_empty(),
            "the box is freed for the one after"
        );
        assert!(
            state.transcript.is_empty(),
            "nothing has run, so nothing is part of the session yet"
        );
        assert!(!*cancelled.borrow(), "queuing is not cancelling");
    }

    #[test]
    fn a_key_meant_for_the_prompt_is_dropped_only_where_it_would_leave() {
        // Ctrl-D leaves the session at the prompt, and a turn in flight is not the
        // place to leave from: it is dropped, and the turn goes on.
        let mut state = State::default();
        let (cancel, _cancelled) = watch::channel(false);
        state.key_while_working(
            Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            &cancel,
        );
        assert!(state.queued.is_empty(), "not queued as a line");
        assert!(state.textarea.is_empty(), "and not typed into the box");
    }

    #[test]
    fn the_cancel_key_is_still_the_cancel_key_while_a_line_is_being_queued() {
        let mut state = State::default();
        let (cancel, cancelled) = watch::channel(false);
        type_while_working(&mut state, "half a thought", &cancel);
        state.key_while_working(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &cancel,
        );
        assert!(*cancelled.borrow(), "Ctrl-C cancels the turn");
        assert!(state.queued.is_empty(), "and is not a line of its own");
        assert_eq!(
            state.textarea.lines(),
            ["half a thought"],
            "what was being typed is still there: the cancellation is the turn's, \
             not the box's"
        );
    }

    #[test]
    fn the_queue_runs_from_its_head() {
        let mut state = State::default();
        state.enqueue("first".into());
        state.enqueue("second".into());
        assert_eq!(state.dequeue().as_deref(), Some("first"));
        assert_eq!(state.dequeue().as_deref(), Some("second"));
        assert_eq!(state.dequeue(), None, "and then the keyboard is waited on");
    }

    #[test]
    fn the_box_says_what_enter_will_do() {
        // Three states, three invitations: the answer to a question, the line for
        // after the turn, and the next message. The box is where all three are
        // typed, so it is the only place that can say which one it is.
        let mut state = State::default();
        assert_eq!(state.textarea.placeholder_text(), IDLE_PLACEHOLDER);

        state.begin_turn();
        assert_eq!(state.textarea.placeholder_text(), QUEUE_PLACEHOLDER);

        let (reply, _answer) = oneshot::channel();
        state.open_question(reply);
        assert_eq!(state.textarea.placeholder_text(), ANSWER_PLACEHOLDER);

        state.close_question();
        assert_eq!(
            state.textarea.placeholder_text(),
            QUEUE_PLACEHOLDER,
            "the turn is still running"
        );

        state.end_turn();
        assert_eq!(state.textarea.placeholder_text(), IDLE_PLACEHOLDER);
    }

    #[test]
    fn a_line_being_typed_is_held_aside_while_the_gate_is_open() {
        // The answer to "run it?" is a `y`, and a sentence that was already in the
        // box is not one. The gate takes the box for its answer and gives it back.
        let mut state = State::default();
        let (cancel, _cancelled) = watch::channel(false);
        state.begin_turn();
        type_while_working(&mut state, "and then refactor", &cancel);
        let (reply, answer) = oneshot::channel();
        state.open_question(reply);
        assert!(
            state.textarea.is_empty(),
            "the answer starts from an empty box"
        );

        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char('y'))), &cancel);
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
        assert_eq!(answer.blocking_recv(), Ok(true), "the gate got its answer");
        assert_eq!(
            state.textarea.lines(),
            ["and then refactor"],
            "and the line came back"
        );
        assert!(state.queued.is_empty(), "it was never submitted");
    }

    #[test]
    fn a_question_is_answered_by_what_the_user_submits() {
        let mut screen = State::default();
        let (reply, answer) = oneshot::channel();
        screen.open_question(reply);
        assert!(screen.reply.is_some(), "the answer has somewhere to go");
        screen.textarea.insert_str("yes");
        screen.close_question();
        assert_eq!(answer.blocking_recv(), Ok(true));
        assert!(screen.question.is_none());
        assert!(screen.textarea.is_empty());
    }

    #[test]
    fn a_secret_is_typed_into_the_box_and_answered_with_what_was_typed() {
        // `/login`'s question. What goes back is the text, not the dots: the
        // masking is how it is drawn, and a box that answered with its own
        // drawing would send bullets to the provider.
        let mut state = State::default();
        let (cancel, _cancelled) = watch::channel(false);
        let (reply, answer) = oneshot::channel();
        state.apply(Notice::Secret {
            prompt: "glm API key".into(),
            reply,
        });
        assert_eq!(
            state.textarea.mask_char(),
            Some(SECRET_MASK),
            "the text is not on the screen while it is typed"
        );
        type_while_working(&mut state, "sk-test", &cancel);
        assert_eq!(state.textarea.lines(), ["sk-test"], "the box holds it");
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
        assert_eq!(answer.blocking_recv(), Ok(Some("sk-test".to_owned())));
        assert!(
            state.queued.is_empty(),
            "an answer is not a line to run next"
        );
        assert!(state.reply.is_none(), "the question is closed");
        assert_eq!(state.textarea.mask_char(), None, "the box is a box again");
    }

    #[test]
    fn an_empty_answer_cancels_a_secret() {
        let mut state = State::default();
        let (cancel, _cancelled) = watch::channel(false);
        let (reply, answer) = oneshot::channel();
        state.apply(Notice::Secret {
            prompt: "glm API key".into(),
            reply,
        });
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
        assert_eq!(answer.blocking_recv(), Ok(None));
    }

    #[test]
    fn the_answer_to_a_secret_never_reaches_the_transcript_or_the_queue() {
        // The whole reason it is a question rather than a line: a key typed at
        // the prompt would be in the session the moment it was submitted.
        let mut screen = screen_for_test(60, 20);
        let (cancel, _cancelled) = watch::channel(false);
        let (reply, answer) = oneshot::channel();
        screen.state.begin_turn();
        screen.state.apply(Notice::Secret {
            prompt: "glm API key".into(),
            reply,
        });
        type_while_working(&mut screen.state, "sk-do-not-keep-me", &cancel);
        screen.draw().unwrap();
        let drawn = all_rows(&screen).join("\n");
        assert!(
            drawn.contains("glm API key"),
            "the question is on the screen"
        );
        assert!(drawn.contains(SECRET_MASK), "masked, not in the clear");
        assert!(!drawn.contains("sk-do-not-keep-me"), "{drawn}");
        screen
            .state
            .key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
        assert_eq!(
            answer.blocking_recv(),
            Ok(Some("sk-do-not-keep-me".to_owned()))
        );
        assert!(
            screen.state.transcript.is_empty(),
            "nothing was written down"
        );
        assert!(screen.state.queued.is_empty());
    }

    #[test]
    fn what_is_drawn_while_a_secret_is_asked_for_says_what_the_box_wants() {
        let mut screen = screen_for_test(60, 20);
        let (reply, _answer) = oneshot::channel();
        screen.state.apply(Notice::Secret {
            prompt: "glm API key".into(),
            reply,
        });
        screen.draw().unwrap();
        let last = screen.terminal.backend().buffer().area.height - 1;
        assert!(
            row(&screen, last - 2).contains(SECRET_PLACEHOLDER),
            "the box says what Enter does with it: {:?}",
            row(&screen, last - 2)
        );
    }

    #[test]
    fn a_line_being_typed_is_held_aside_while_a_secret_is_open() {
        let mut state = State::default();
        let (cancel, _cancelled) = watch::channel(false);
        state.begin_turn();
        type_while_working(&mut state, "and then refactor", &cancel);
        let (reply, _answer) = oneshot::channel();
        state.apply(Notice::Secret {
            prompt: "deepseek API key".into(),
            reply,
        });
        assert!(
            state.textarea.is_empty(),
            "the key starts from an empty box"
        );
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
        assert_eq!(
            state.textarea.lines(),
            ["and then refactor"],
            "the draft came back"
        );
        assert!(state.queued.is_empty());
    }

    #[test]
    fn a_provider_or_model_row_is_submitted_as_the_line_it_stands_for() {
        // The pickers `/login` and `/model` open: the row is the argument, and
        // the command that takes it is the one the plain prompt would be given,
        // so choosing is implemented once for both front ends.
        let mut state = State::default();
        assert!(state.open_choices(
            Choosing::Provider,
            vec![
                Choice {
                    label: "DeepSeek".into(),
                    argument: "deepseek".into(),
                    detail: "key stored".into(),
                },
                Choice {
                    label: "Z.AI Coding CN".into(),
                    argument: "zai-coding-cn".into(),
                    detail: "no key".into(),
                },
            ]
        ));
        state.down();
        assert!(state.choose());
        assert_eq!(
            state.textarea.lines(),
            ["/login zai-coding-cn"],
            "the row is read by name and submitted by id"
        );

        assert!(state.open_choices(
            Choosing::Model,
            named_rows(vec![("zai-coding-cn/glm-5.3".into(), "current".into())])
        ));
        assert!(state.choose());
        assert_eq!(state.textarea.lines(), ["/model zai-coding-cn/glm-5.3"]);
    }

    #[test]
    fn an_offered_list_of_providers_survives_typing_like_a_session_list_does() {
        // It was asked for in full; recomputing completions over it would take
        // the list away the moment a letter was typed.
        let mut state = State::default();
        state.open_choices(
            Choosing::Provider,
            named_rows(vec![("Z.AI Coding CN".into(), "no key".into())]),
        );
        state.refresh_picker();
        assert_eq!(state.picker.as_ref().unwrap().kind, Choosing::Provider);
    }

    #[test]
    fn anything_that_is_not_yes_denies() {
        for typed in ["", "n", "no", "maybe"] {
            let mut screen = State::default();
            let (reply, answer) = oneshot::channel();
            screen.open_question(reply);
            screen.textarea.insert_str(typed);
            screen.close_question();
            assert_eq!(answer.blocking_recv(), Ok(false), "typed {typed:?}");
        }
    }

    #[test]
    fn enter_submits_only_when_there_is_something_to_submit() {
        let mut screen = State::default();
        let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(screen.key(enter), Submitted::Nothing));
        screen.textarea.insert_str("hello");
        assert!(matches!(
            screen.key(Event::Key(KeyEvent::new(
                KeyCode::Enter,
                KeyModifiers::NONE
            ))),
            Submitted::Line
        ));
        let taken = screen.take_line();
        assert_eq!(taken, "hello");
        assert!(
            screen.textarea.is_empty(),
            "the box is ready for the next line"
        );
    }

    #[test]
    fn ctrl_j_is_left_to_the_box_so_input_can_be_multiline() {
        let mut screen = State::default();
        screen.textarea.insert_str("first");
        screen.key(Event::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        )));
        screen.textarea.insert_str("second");
        assert_eq!(screen.take_line(), "first\nsecond");
    }

    #[test]
    fn ctrl_c_clears_the_line_and_ctrl_d_on_an_empty_box_leaves() {
        let mut screen = State::default();
        screen.textarea.insert_str("half typed");
        screen.key(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(screen.textarea.is_empty(), "Ctrl-C clears");

        let ctrl_d = || Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(matches!(screen.key(ctrl_d()), Submitted::Exit));
        // ... but only on an empty box: otherwise it is just a keystroke.
        screen.textarea.insert_str("text");
        assert!(matches!(screen.key(ctrl_d()), Submitted::Nothing));
    }

    #[test]
    fn ctrl_c_during_a_turn_cancels_it_and_nothing_else_does() {
        let mut screen = State::default();
        let (tx, rx) = watch::channel(false);
        screen.key_while_working(
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            &tx,
        );
        assert!(!*rx.borrow(), "only the cancel key cancels");
        screen.key_while_working(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &tx,
        );
        assert!(*rx.borrow(), "Ctrl-C during a turn cancels it");
    }

    #[test]
    fn a_paste_lands_in_the_box_whole() {
        let mut screen = State::default();
        screen.key(Event::Paste("pasted\nlines".into()));
        assert_eq!(screen.take_line(), "pasted\nlines");
    }

    #[test]
    fn zz_visual_review_dump() {
        let mut screen = screen_for_test(96, 30);
        screen.state.status.set_model("deepseek/deepseek-flash");
        screen.state.show(Cell::Notice(
            "caocli \u{b7} session 20260910-224129 (12 messages) \u{b7} deepseek/deepseek-flash"
                .into(),
        ));
        screen
            .state
            .transcript
            .push(Cell::User("why is the build slow?".into()));
        screen.state.transcript.push(Cell::Reasoning(
            "The user asks about build time. I should look at the Cargo profile and maybe check if there are heavy dependencies. Let me start by reading Cargo.toml and then check the target directory size.".into(),
        ));
        screen.state.transcript.push(Cell::Content(
            "Two things usually dominate: an unoptimized dev profile and relinking every dependency on each edit. Let me look.".into(),
        ));
        screen.state.transcript.push(Cell::tool_call(
            "Bash",
            r#"{"command":"ls -la target/debug | head -20"}"#,
        ));
        screen.state.transcript.push(Cell::ToolResult(
            "total 4823136\ndrwxr-xr-x 12 user user 4096 ...\n".into(),
        ));
        screen.state.transcript.push(Cell::tool_call(
            "Edit",
            r#"{"file_path":"Cargo.toml","old_string":"[profile.dev]\ndebug = 2","new_string":"[profile.dev]\ndebug = 0"}"#,
        ));
        screen
            .state
            .transcript
            .push(Cell::ToolResult("edited Cargo.toml\n".into()));
        screen.state.transcript.push(Cell::Content(
            "Setting `debug = 0` alone is usually worth a third of the link time. The other half is the linker: with `lld` the final link stops being the long pole.".into(),
        ));
        screen.state.apply(Notice::Usage(
            Usage {
                prompt_tokens: 12480,
                total_tokens: 12980,
                completion_tokens: 500,
                prompt_cache_hit_tokens: 12000,
                prompt_cache_miss_tokens: 480,
                prompt_tokens_details: None,
            },
            Duration::from_secs(9),
        ));
        screen.state.transcript.push(Cell::Failure(
            "no API key for DeepSeek: run /login deepseek".into(),
        ));
        screen.state.transcript.push(Cell::Interrupted);
        screen
            .state
            .transcript
            .push(Cell::Notice("/help for commands".into()));
        screen.draw().unwrap();
        let buf = screen.terminal.backend().buffer();
        for y in 0..buf.area.height {
            let mut line = String::new();
            for x in 0..buf.area.width {
                let c = &buf[(x, y)];
                let m = c.style().add_modifier;
                let tag = if m.contains(Modifier::DIM) {
                    "d"
                } else if c.style().fg == Some(Color::Yellow) {
                    "Y"
                } else if c.style().fg == Some(Color::Green) {
                    "G"
                } else if c.style().fg == Some(Color::Red) {
                    "R"
                } else {
                    " "
                };
                line.push_str(tag);
            }
            println!("STYLE {y:02} {line}");
            println!("TEXT  {y:02} |{}|", row(&screen, y));
        }
    }
}

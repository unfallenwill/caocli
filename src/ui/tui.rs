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

use std::future::Future;
use std::io::{self, Stdout};
use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style as RStyle};
use ratatui::text::{Line, Span as RSpan, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use ratatui_textarea::{ScreenCursor, TextArea};
use tokio::sync::{mpsc, oneshot, watch};

use crate::agent::{Agent, Approve, Interrupt};
use crate::history;
use crate::repl;
use crate::session;
use crate::types::{Message, ToolCall, Usage};
use crate::ui::Front;

use super::Ui;
use super::cell::{self, Cell, Span, Style};
use super::status::Status;

/// Rows the pinned region needs: the status line, the tip line, then the input box
/// (border, text, border).
const PINNED_ROWS: u16 = 1 + 1 + 3;

/// How many rows of the transcript a terminal `height` rows tall shows.
///
/// Never zero, even on a terminal too short for the pinned region: the box is
/// worth a cramped transcript, where an empty screen is worth nothing.
fn transcript_rows(height: u16) -> u16 {
    height.saturating_sub(PINNED_ROWS).max(1)
}

/// The spinner, advanced while work is in progress. One column each, so it can
/// sit on the working line without moving it.
const SPINNER: [&str; 6] = ["·", "✢", "✳", "✶", "✽", "✻"];

/// How long each spinner frame is shown. Long enough to read, short enough that
/// the line looks alive.
const SPINNER_FRAME: Duration = Duration::from_millis(100);

/// How many spinner frames fit in `elapsed`: the tick a moment belongs to.
///
/// Derived from the elapsed time rather than counted, so a redraw that was
/// skipped cannot leave the spinner behind: the frame is a function of *when*,
/// not of how many times anything ran. Everything on the working line that moves
/// on its own -- the frame, the timer -- moves on this tick, so one number
/// answers both "which frame" and "has anything changed".
fn spinner_step(elapsed: Duration) -> u64 {
    elapsed.as_millis() as u64 / SPINNER_FRAME.as_millis().max(1) as u64
}

/// Which spinner frame belongs to a moment in time.
fn spinner_frame(elapsed: Duration) -> &'static str {
    SPINNER[spinner_step(elapsed) as usize % SPINNER.len()]
}

/// The words a turn is introduced by, one per turn. The machine underneath is the
/// same every time; the point of the word is that a long wait has something in it
/// to read.
const VERBS: [&str; 14] = [
    "Pondering",
    "Noodling",
    "Julienning",
    "Percolating",
    "Ruminating",
    "Simmering",
    "Whittling",
    "Mulling",
    "Sifting",
    "Tinkering",
    "Brewing",
    "Sketching",
    "Untangling",
    "Kneading",
];

/// The word a turn that began at `seed` is introduced by.
///
/// The seed is a clock reading rather than a counter, so two turns in a row are
/// unlikely to be the same word without anything having to remember the last one.
fn verb_for(seed: u128) -> &'static str {
    VERBS[(seed as usize) % VERBS.len()]
}

/// A clock reading that can seed anything wanting one. Nanoseconds, because the
/// seconds a session runs for are few enough that a coarser reading would repeat.
fn seed() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default()
}

/// How long a turn has been running, as the working line shows it: `12m 10s`.
///
/// Rounded to the second, because that is the scale a turn is watched at, and
/// because a division that lands exactly on the spinner's tick is what keeps the
/// redraw and the text in step.
fn duration_label(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s}s"),
        (m, s) => format!("{m}m {s}s"),
    }
}

/// The thinking block's ground and foreground, by their numbers in ANSI's
/// 256-colour palette. The same pair the plain front end writes as an escape
/// sequence; here as numbers because ratatui takes colours rather than sequences.
const REASONING_GROUND: u8 = 236;
const REASONING_FOREGROUND: u8 = 245;

/// The colour the word on the working line is painted in: a warm amber, which is
/// what separates "something is happening" from the dim text around it.
const WORKING_FOREGROUND: u8 = 209;

/// The gutter the tip line starts with: the same mark the transcript uses for the
/// lines that belong to something else, so a tip reads as an aside rather than as
/// part of the session.
const TIP_GUTTER: &str = "⎿  ";

/// How long one tip is shown before the next replaces it. Long enough to read
/// without reading it twice, which is what makes the row worth its space.
const TIP_PERIOD: Duration = Duration::from_secs(20);

/// The tips, in the order they come round. One line each, short enough to read at
/// a glance, and every one of them true: a tip about a key that does nothing is
/// worse than no tip at all.
const TIPS: [&str; 7] = [
    "Tab completes · Up and Down browse what you typed",
    "Ctrl-J adds a line · Enter sends it",
    "PageUp and PageDown read back through the session",
    "Ctrl-C stops a turn, and the model is told",
    "/resume switches session · /new starts one",
    "Start with --ask to approve a tool before it runs",
    "/help lists every command",
];

/// How many command rows the picker shows at once. It draws over the bottom of the
/// transcript, so it has to leave the transcript somewhere to live.
const PICKER_ROWS: usize = 6;

/// The columns the name gets before the detail starts. Wide enough for a session
/// id, the longest name the picker shows, so both kinds of row line up.
const PICKER_NAME_COLUMNS: usize = 17;

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
    ToolStart { name: String, args: String },
    ToolResult(String),
    Usage(Usage),
    Interrupted,
    Approval { name: String, args: String },
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
    fn usage(&mut self, u: &Usage) {
        self.send(Notice::Usage(u.clone()));
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
    /// Which part of the transcript is on screen.
    scroll: Scroll,
    /// What the last draw saw: how long the transcript was, and how many rows of
    /// it there were room for. Paging and staying put while lines arrive both
    /// need them, and both are the terminal's to say rather than the state's.
    drawn_lines: usize,
    drawn_rows: usize,
    /// When the front end started, which is the clock the tip line runs on. Its
    /// own clock rather than the turn's: the tips keep coming round whether or not
    /// anything is happening.
    started: Instant,
    /// The text block being streamed, with the style it is drawn in. The style is
    /// the block's identity, so a fragment in the other style opens a new block.
    live: Option<(Style, String)>,
    /// The approval gate's question, while one is open.
    question: Option<Cell>,
    /// Where to send the answer to that question.
    reply: Option<oneshot::Sender<bool>>,
    /// The answer being typed.
    textarea: TextArea<'static>,
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
    /// When the current turn started, while one is running. One field answers
    /// both "is a turn running" and "how long has it been", which are the same
    /// question asked twice otherwise.
    turn_started: Option<Instant>,
    /// What the running turn is doing at this moment.
    phase: Phase,
    /// The word the running turn is introduced by, picked when it began.
    verb: &'static str,
    /// Output tokens the provider has reported for the running turn, or `None`
    /// while it has reported none.
    turn_tokens: Option<u64>,
    /// Bumped by everything that changes what the screen should show, so a draw
    /// can be skipped when nothing has.
    revision: u64,
}

/// What a running turn is doing, as far as the working line is concerned.
///
/// One field rather than two, because "is a tool running" and "which one" are only
/// ever asked together, and a turn is in exactly one of these states at a time.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Phase {
    /// Thinking, or between things: the state a turn opens in.
    #[default]
    Thinking,
    /// The answer itself is being written.
    Responding,
    /// A tool is executing.
    Running(String),
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
}

/// One row of the picker: what it says, and what it is called.
struct Choice {
    name: String,
    detail: String,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: Status::default(),
            transcript: Vec::new(),
            scroll: Scroll::default(),
            drawn_lines: 0,
            drawn_rows: 0,
            started: Instant::now(),
            live: None,
            question: None,
            reply: None,
            textarea: input_box(),
            picker: None,
            history: Vec::new(),
            browsing: None,
            draft: String::new(),
            turn_started: None,
            phase: Phase::default(),
            verb: VERBS[0],
            turn_tokens: None,
            revision: 0,
        }
    }
}

impl State {
    /// The whole transcript as lines, in draw order: the session's finished
    /// cells, then the block still streaming, then a pending question.
    fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = cell_lines(&self.transcript, width);
        if let Some((style, text)) = &self.live {
            lines.extend(wrapped_lines(&[Span::new(*style, text.clone())], width));
        }
        if let Some(question) = &self.question {
            // Wrapped like everything else. The question is what the answer is
            // about, and the call in it can be a long command: one that is
            // clipped gives the user nothing to decide with.
            lines.extend(wrapped_lines(&question.spans(), width));
        }
        lines
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

    /// Page through the transcript a screen at a time; `-1` is back, `+1` forward.
    fn page(&mut self, step: isize) {
        let (total, rows) = (self.drawn_lines, self.drawn_rows);
        self.scroll.by(step * rows as isize, total, rows);
    }

    /// Back to the end, which is where a new line will appear.
    fn follow(&mut self) {
        self.scroll.bottom();
    }

    /// The pinned status line: what is running while a turn is, and the session
    /// summary when nothing is.
    ///
    /// The two do not share the line. What a turn is doing is the thing to look at
    /// while it does it, and the model and cache statistics are what you read when
    /// you are about to type rather than while you wait.
    fn status_line(&self, width: usize, now: Instant) -> Line<'static> {
        match self.working(width, now) {
            Some(line) => line,
            // One column is left free so the write cannot trigger autowrap.
            None => Line::from(self.status.line(width.saturating_sub(1))),
        }
    }

    /// The working line, while a turn is running.
    ///
    /// The word, how long it has been going, what it has written, and which part of
    /// the turn it is in -- in that order, because that is the order the question is
    /// asked in: something is happening, for this long, this much of it, at this.
    fn working(&self, width: usize, now: Instant) -> Option<Line<'static>> {
        let elapsed = self.elapsed(now)?;
        let frame = spinner_frame(elapsed);
        let spent = duration_label(elapsed);
        let state = match &self.phase {
            Phase::Thinking => "thinking".to_owned(),
            Phase::Responding => "responding".to_owned(),
            Phase::Running(tool) => format!("running {tool}"),
        };
        // The token count is the one segment that can be missing: it is what the
        // provider reported, and until it reports anything there is nothing to
        // show but a number this process made up.
        let tokens = match self.turn_tokens {
            Some(n) => format!(" \u{b7} \u{2193} {n} tokens"),
            None => String::new(),
        };
        let head = format!("{frame} {}\u{2026}", self.verb);
        let rest = format!(" ({spent}{tokens} \u{b7} {state})");
        // One row, which cannot wrap: on a line too narrow for both, the detail goes
        // and the word stays, since a word on its own still says something is
        // happening.
        let head_width = super::text::width(&head);
        let rest_width = super::text::width(&rest);
        let detail = if head_width + rest_width < width {
            rest
        } else {
            String::new()
        };
        Some(Line::from(vec![
            RSpan::styled(head, RStyle::new().fg(Color::Indexed(WORKING_FOREGROUND))),
            RSpan::styled(detail, RStyle::new().add_modifier(Modifier::DIM)),
        ]))
    }

    /// How long the current turn has been running, if one is.
    fn elapsed(&self, now: Instant) -> Option<Duration> {
        Some(now.saturating_duration_since(self.turn_started?))
    }

    /// The tip to show at `now`, which is the one the tip clock has come round to.
    fn tip(&self, now: Instant) -> &'static str {
        TIPS[self.tip_step(now) as usize % TIPS.len()]
    }

    /// How many tip periods have passed since the front end started. The clock
    /// reading the line and the tick that decides whether to redraw are the same
    /// number, so the tip changes exactly when the screen is repainted for it.
    fn tip_step(&self, now: Instant) -> u64 {
        let period = TIP_PERIOD.as_millis().max(1);
        (now.saturating_duration_since(self.started).as_millis() / period) as u64
    }

    /// The tip line: the gutter, then the tip.
    fn tip_line(&self, now: Instant) -> Line<'static> {
        Line::styled(
            format!("{TIP_GUTTER}Tip: {}", self.tip(now)),
            RStyle::new().add_modifier(Modifier::DIM),
        )
    }

    /// The moment on the front end's own clock: the spinner's while a turn runs,
    /// the tip line's while nothing does.
    ///
    /// Either way it is one number that says whether anything has moved without
    /// being told to, and it is part of what decides whether to redraw -- without
    /// that, a screen left idle would keep showing the tip it opened with.
    fn tick(&self, now: Instant) -> u64 {
        match self.elapsed(now) {
            Some(elapsed) => spinner_step(elapsed),
            None => self.tip_step(now),
        }
    }

    /// A turn is starting: start the clock the status line reads, and pick the word
    /// it will be introduced by.
    fn begin_turn(&mut self, now: Instant) {
        self.revision += 1;
        self.turn_started = Some(now);
        self.verb = verb_for(seed());
        self.turn_tokens = None;
    }

    /// The turn is over: stop the clock, and forget what it was doing, which a turn
    /// can end without ever saying -- an interrupted tool reports no result.
    fn end_turn(&mut self) {
        self.revision += 1;
        self.turn_started = None;
        self.phase = Phase::default();
    }

    /// Fold one notice into the state.
    fn apply(&mut self, notice: Notice) {
        self.revision += 1;
        match notice {
            Notice::Reasoning(text) => {
                self.phase = Phase::Thinking;
                self.stream(Style::Reasoning, &text);
            }
            Notice::Content(text) => {
                self.phase = Phase::Responding;
                self.stream(Style::Plain, &text);
            }
            Notice::FinishTurn => self.end_block(),
            Notice::ToolStart { name, args } => {
                self.end_block();
                self.transcript.push(Cell::tool_call(&name, &args));
                // The working line reports the tool by name while it runs, which is
                // the part of a turn that can take a long time.
                self.phase = Phase::Running(name);
            }
            Notice::ToolResult(result) => {
                self.end_block();
                self.transcript.push(Cell::ToolResult(result));
                // What follows a result is the model reading it, so the turn is back
                // to thinking until it says otherwise.
                self.phase = Phase::Thinking;
            }
            Notice::Usage(u) => {
                self.status.record(&u);
                *self.turn_tokens.get_or_insert(0) += u.completion_tokens;
            }
            Notice::Interrupted => {
                self.end_block();
                self.transcript.push(Cell::Interrupted);
            }
            Notice::Approval { name, args } => {
                self.end_block();
                self.question = Some(Cell::approval(&name, &args));
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
    /// ends that block and opens a new one -- the same rule the plain front end
    /// applies, because the style *is* the block's identity.
    fn stream(&mut self, style: Style, text: &str) {
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
            let cell = match style {
                Style::Reasoning => Cell::Reasoning(text),
                _ => Cell::Content(text),
            };
            self.revision += 1;
            self.transcript.push(cell);
        }
    }

    /// Handle a key at the prompt.
    fn key(&mut self, event: Event) -> Submitted {
        self.revision += 1;
        let key = match event {
            Event::Key(key) => key,
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
                // A list of sessions is a list of things to do rather than a name
                // being typed, so Enter takes the highlighted row and submits the
                // line it stands for.
                if self.choose_session() {
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
                // Completing a command leaves it in the box; choosing a session
                // is the whole action, so it submits.
                if self.choose_session() {
                    return Submitted::Line;
                }
                self.complete();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                self.picker = None;
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

    /// Replace what is in the box, leaving the cursor after it.
    fn set_text(&mut self, text: &str) {
        let mut box_ = input_box();
        if !text.is_empty() {
            box_.insert_str(text);
        }
        self.textarea = box_;
    }

    /// Recompute what the picker offers for what is in the box, keeping the
    /// highlight on the same command while it is still among the matches.
    fn refresh_picker(&mut self) {
        // A session list is not a completion: it was asked for in full, and it
        // stays until it is answered or dismissed, whatever is typed next.
        if self
            .picker
            .as_ref()
            .is_some_and(|p| p.kind == Choosing::Session)
        {
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
            .map(|c| c.name.clone());
        let selected = previous
            .and_then(|name| matches.iter().position(|c| c.name == name))
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: Choosing::Command,
            choices: matches
                .into_iter()
                .map(|c| Choice {
                    name: c.name.to_owned(),
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
        self.revision += 1;
        if list.is_empty() {
            self.picker = None;
            return false;
        }
        self.picker = Some(Picker {
            kind: Choosing::Session,
            choices: list
                .iter()
                .map(|s| Choice {
                    name: s.id.clone(),
                    detail: format!("{} messages · {}", s.message_count, s.preview),
                })
                .collect(),
            selected: 0,
        });
        true
    }

    /// Take the highlighted row, if the picker is offering sessions. The row
    /// becomes the line the plain prompt would have been given, so switching has
    /// one implementation rather than one per front end.
    fn choose_session(&mut self) -> bool {
        let Some(picker) = &self.picker else {
            return false;
        };
        if picker.kind != Choosing::Session {
            return false;
        }
        let Some(name) = picker.choices.get(picker.selected).map(|c| c.name.clone()) else {
            return false;
        };
        self.picker = None;
        self.set_text(&format!("/resume {name}"));
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
    /// argument may still be wanted.
    fn complete(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        if let Some(choice) = picker.choices.get(picker.selected) {
            self.set_text(&choice.name);
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

    /// The picker as it is drawn: one row per choice, the highlighted one
    /// reversed.
    fn picker_lines(&self) -> Vec<Line<'static>> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        picker
            .choices
            .iter()
            .enumerate()
            .map(|(i, choice)| {
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
                    RSpan::styled(format!(" {:<PICKER_NAME_COLUMNS$}", choice.name), name),
                    RSpan::styled(format!(" {}", choice.detail), detail),
                ])
            })
            .collect()
    }

    /// Handle a key while a turn is running.
    ///
    /// Two things take typing here: the cancel key, always, and the answer to an
    /// approval question, while the gate is waiting for one. Everything else is
    /// dropped, because a turn is not the place to start composing the next line.
    fn key_while_working(&mut self, event: Event, cancel: &watch::Sender<bool>) {
        // A resize arrives here too, and it changes the layout, so anything
        // arriving at all is reason enough to redraw -- and a draw that was not
        // needed costs one comparison.
        self.revision += 1;
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let _ = cancel.send(true);
            return;
        }
        // Everything below concerns the gate, and there is no gate open for the
        // rest of a turn.
        if self.reply.is_none() {
            return;
        }
        // Enter submits the answer whether or not anything was typed: a blank
        // line denies, which is the rule the plain front end reads from stdin.
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

    /// Take the submitted line out of the box, leaving it empty for the next one.
    fn take_line(&mut self) -> String {
        let line = self.text();
        self.textarea = input_box();
        self.picker = None;
        self.browsing = None;
        self.draft.clear();
        line
    }

    /// The approval gate is asking: remember who to answer.
    fn open_question(&mut self, reply: oneshot::Sender<bool>) {
        self.revision += 1;
        self.reply = Some(reply);
        // The box is where the answer goes, so it says so rather than inviting
        // the next message: nothing else can be typed while a turn runs.
        self.textarea
            .set_placeholder_text("y to allow · anything else denies");
    }

    /// Answer the open question from what the user submitted, if anything. A line
    /// starting with `y` allows and anything else denies, which is the rule the
    /// plain front end applies to a line of stdin.
    fn close_question(&mut self) {
        self.revision += 1;
        if let Some(reply) = self.reply.take() {
            let answer = self.textarea.lines().join("\n").trim().to_lowercase();
            let _ = reply.send(answer.starts_with('y'));
            self.textarea = input_box();
        }
        self.question = None;
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

/// The terminal, held by the screen that took it.
///
/// A type of its own because a `Drop` impl cannot be written for a single
/// instantiation of a generic one: the screen is generic over its backend so that a
/// test can draw into memory, and the terminal that has to be given back is not
/// generic at all.
struct Tty;

impl Tty {
    /// Take the terminal over: raw mode, then the alternate screen.
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
            crossterm::event::EnableBracketedPaste
        )?;
        Ok(taken)
    }
}

/// Give the terminal back: leave the alternate screen, so what the user had on it
/// reappears, and put the cursor where a shell prompt expects to find it.
fn restore() -> io::Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )
}

/// Restoring the terminal does not depend on the success path running. Raw mode is
/// process-wide and would wreck the shell if it survived an unwind, and the
/// alternate screen would hide everything the user had on it -- so both are given
/// back by dropping what took them, which an unwind does too.
impl Drop for Tty {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = restore();
    }
}

/// What the screen would show, as a value that can be compared.
///
/// Equal keys mean a redraw cannot change a pixel, so it is skipped. The
/// revision covers everything the state knows about itself; the tick covers what
/// moves without the state changing, which is the spinner's clock while a turn
/// runs and the tip line's while none does; the size is there because the rows are
/// laid out from it. Reading the size is an
/// `ioctl`, not a round trip to the terminal, so it is cheap enough to be part
/// of a check that runs on every tick of the loop.
#[derive(PartialEq, Eq, Clone, Copy)]
struct ViewKey {
    revision: u64,
    tick: u64,
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
    fn view_key(&self, now: Instant) -> Result<ViewKey, B::Error> {
        let size = self.terminal.size()?;
        Ok(ViewKey {
            revision: self.state.revision,
            tick: self.state.tick(now),
            width: size.width,
            height: size.height,
        })
    }

    /// Repaint, unless the screen already shows this.
    ///
    /// The loop wakes on a tick to stay responsive to the keyboard, and most of
    /// those ticks have nothing new to show: a turn spends its time waiting to be
    /// told something, and repainting an unchanged screen would be a whole-screen
    /// write every tick.
    fn draw_if_changed(&mut self) -> Result<(), B::Error> {
        let now = Instant::now();
        let key = self.view_key(now)?;
        if self.drawn == Some(key) {
            return Ok(());
        }
        self.draw_at(now)?;
        // Only after the draw succeeded: a failed one leaves the screen showing
        // something else, so the next call has to try again.
        self.drawn = Some(key);
        Ok(())
    }

    /// Draw the screen as of now, for tests that do not care about the clock.
    #[cfg(test)]
    fn draw(&mut self) -> Result<(), B::Error> {
        self.draw_at(Instant::now())
    }

    /// Draw the screen: the window on the transcript, the status line, the input
    /// box.
    fn draw_at(&mut self, now: Instant) -> Result<(), B::Error> {
        let size = self.terminal.size()?;
        let width = size.width as usize;
        let lines = self.state.lines(width);
        let rows = transcript_rows(size.height);
        // The window over the transcript: its end unless the reader scrolled back.
        // The picker belongs to the line being typed, so it takes the box's end of
        // the transcript with it.
        let picker = self.state.picker_lines();
        let first = if picker.is_empty() {
            self.state.window(lines.len(), rows as usize)
        } else {
            self.state.follow();
            lines.len().saturating_sub(rows as usize)
        };
        let last = (first + rows as usize).min(lines.len());
        let transcript = Text::from(lines[first..last].to_vec());
        let status = self.state.status_line(width, now);
        let tip = self.state.tip_line(now);
        let cursor = self.state.textarea.screen_cursor();

        self.terminal.draw(|frame| {
            let rows = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(frame.area());
            frame.render_widget(Paragraph::new(transcript), rows[0]);
            if !picker.is_empty() {
                let height = picker.len().min(PICKER_ROWS) as u16;
                let area = Rect {
                    x: rows[0].x,
                    y: rows[0].bottom().saturating_sub(height),
                    width: rows[0].width,
                    height,
                };
                // Cleared first: a shorter list must not leave the tail of a
                // longer one behind it.
                frame.render_widget(Clear, area);
                frame.render_widget(Paragraph::new(Text::from(picker)), area);
            }
            frame.render_widget(Paragraph::new(status), rows[1]);
            frame.render_widget(Paragraph::new(tip), rows[2]);
            frame.render_widget(&self.state.textarea, rows[3]);
            place_cursor(frame, rows[3], cursor);
        })?;
        Ok(())
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

/// The input box: a bordered editor. Enter submits and Ctrl-J inserts a newline,
/// matching the plain prompt's keys.
fn input_box() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(RStyle::new().add_modifier(Modifier::DIM)),
    );
    textarea.set_placeholder_text("›  type a message · /help for commands");
    textarea.set_cursor_line_style(RStyle::new());
    textarea
}

/// The lines cells occupy at `width`.
///
/// One function, because the transcript on screen, the window over it and the
/// session log folded back into cells are three places the same cells are laid
/// out, and a session that reads differently in any of them is a session that was
/// not really one transcript.
fn cell_lines(cells: &[Cell], width: usize) -> Vec<Line<'static>> {
    cells
        .iter()
        .flat_map(|cell| {
            let fill = fill_of(cell);
            wrapped_lines(&cell.spans(), width)
                .into_iter()
                .map(move |mut line| {
                    if let Some(style) = fill {
                        // A style on the line itself would not do it: a paragraph
                        // renders a line by writing its styled graphemes and leaving
                        // the columns past the text alone, so the ground would stop
                        // where the words stop and read as a highlight rather than
                        // as a block. The blanks are written out instead.
                        let used = line.width();
                        if used < width {
                            line.spans
                                .push(RSpan::styled(" ".repeat(width - used), style));
                        }
                    }
                    line
                })
        })
        .collect()
}

/// The style a cell's whole width is painted in, for the cells that read as blocks
/// rather than as lines of text.
fn fill_of(cell: &Cell) -> Option<RStyle> {
    match cell {
        Cell::Reasoning(_) => Some(style_of(Style::Reasoning)),
        _ => None,
    }
}

/// Wrap styled spans into the terminal lines they need at `width` columns.
///
/// Both halves of the front end need this, for the same reason: nothing here may
/// be left to the terminal's own soft wrapping. The transcript is a region of a
/// fixed width, so an unwrapped line would be cut off at the edge,
/// and `insert_before` renders into a fixed-width buffer, where an over-long line
/// is silently cut off -- unlike the plain front end, where the terminal wraps
/// and nothing is lost.
///
/// Progress is guaranteed even when a single character is wider than the whole
/// field: the first character is taken regardless, so a narrow terminal degrades
/// to a clipped wide glyph rather than looping forever.
fn wrapped_lines(spans: &[Span], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<RSpan<'static>> = Vec::new();
    let mut used = 0usize;
    // Whether the line being built is still empty, and, if so, whether the width
    // is what emptied it. Without the second flag an explicit newline landing on a
    // line the width already ended would look like a blank line in the text.
    let mut fresh = true;
    let mut ended_by_width = false;

    for span in spans {
        let style = style_of(span.style);
        let mut rest = span.text.as_str();
        loop {
            let (piece, after) = match rest.find('\n') {
                Some(at) => (&rest[..at], Some(&rest[at + 1..])),
                None => (rest, None),
            };
            let mut piece = piece;
            while !piece.is_empty() {
                let mut head = super::text::truncate(piece, width - used);
                if head.is_empty() {
                    if used == 0 {
                        // Even alone it does not fit; take it anyway.
                        let ch = piece.chars().next().expect("piece is not empty");
                        head = &piece[..ch.len_utf8()];
                    } else {
                        lines.push(Line::from(std::mem::take(&mut current)));
                        used = 0;
                        fresh = true;
                        ended_by_width = true;
                        continue;
                    }
                }
                current.push(RSpan::styled(head.to_owned(), style));
                used += super::text::width(head);
                fresh = false;
                ended_by_width = false;
                piece = &piece[head.len()..];
                if used >= width {
                    lines.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                    fresh = true;
                    ended_by_width = true;
                }
            }
            match after {
                Some(after) => {
                    // An explicit break ends the line -- and means a blank one
                    // when nothing was written since the last break.
                    if !fresh {
                        lines.push(Line::from(std::mem::take(&mut current)));
                        used = 0;
                        fresh = true;
                    } else if !ended_by_width {
                        lines.push(Line::from(""));
                    }
                    ended_by_width = false;
                    rest = after;
                }
                None => break,
            }
        }
    }
    if !fresh {
        lines.push(Line::from(current));
    }
    lines
}

/// Translate the input box's own cursor into a position on the screen, so the
/// terminal's caret sits where the next character will go.
///
/// The box reports its cursor relative to the area inside its borders, which is
/// the same inner area the widget renders into.
fn place_cursor(frame: &mut Frame, area: Rect, cursor: ScreenCursor) {
    let inner = Block::default().borders(Borders::ALL).inner(area);
    let x = inner.x + cursor.col as u16;
    let y = inner.y + cursor.row as u16;
    if x < inner.right() && y < inner.bottom() {
        frame.set_cursor_position((x, y));
    }
}

/// Our style, as ratatui sees it. This is what [`Style`] being data buys: the
/// mapping happens once per front end, instead of at every call site.
fn style_of(style: Style) -> RStyle {
    match style {
        Style::Plain => RStyle::new(),
        Style::Dim => RStyle::new().add_modifier(Modifier::DIM),
        // The same dark ground and light foreground the plain front end writes, so
        // the thinking reads the same way in both. Indexed colours rather than
        // RGB: a terminal that has 256 of them is the one this is drawn for.
        Style::Reasoning => RStyle::new()
            .fg(Color::Indexed(REASONING_FOREGROUND))
            .bg(Color::Indexed(REASONING_GROUND)),
        Style::Yellow => RStyle::new().fg(Color::Yellow),
        Style::Green => RStyle::new().fg(Color::Green),
        Style::Red => RStyle::new().fg(Color::Red),
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
    screen.state.status.set_model(&agent.session.meta.model);
    screen.state.show(Cell::Notice(banner.to_owned()));
    screen.state.transcript.extend(cell::from_messages(history));

    let (tx, mut notices) = mpsc::unbounded_channel();
    let (ask_tx, mut asked) = mpsc::unbounded_channel();
    let mut handle = Notifier { tx };

    let result: anyhow::Result<()> = loop {
        // Idle: draw, then wait for something to submit.
        if let Err(e) = screen.draw_if_changed() {
            break Err(e.into());
        }
        let line = loop {
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
        if line.is_empty() {
            break Ok(());
        }
        screen.state.submit(&line);
        // `/resume` with nothing to resume is a request for the list rather than a
        // command to run: the plain front end can only say so, and this one can
        // offer it. The chosen row is submitted as `/resume <id>`, which is the
        // line the plain prompt would have been given, so the switching itself is
        // unchanged. An empty directory falls through to that same reply.
        if line.trim() == "/resume" && screen.state.open_sessions(&session::list(sdir)?) {
            continue;
        }

        // A turn: it races against the keyboard, so Ctrl-C can reach it.
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
        // The clock starts here rather than when the line was submitted: what the
        // status line reports is the turn, and a turn is what is being waited for.
        screen.state.begin_turn(Instant::now());
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
            screen.draw_if_changed()?;
            tokio::select! {
                outcome = &mut turn => break outcome,
                // Nothing else to wait on: the tick above paces the loop, and
                // this branch only gives the turn a real waker so that it is
                // driven by readiness rather than by the tick.
                _ = tokio::time::sleep(TICK) => {}
            }
        };
        // Drain once more before closing the turn. The turn is polled *inside*
        // the select above, so its last notifications are sent after the loop's
        // last drain and are still queued when it resolves; closing the block
        // without them would leave the tail of the turn to open one of its own.
        for notice in drain(&mut notices) {
            screen.state.apply(notice);
        }
        for reply in drain(&mut asked) {
            screen.state.open_question(reply);
        }
        screen.state.end_turn();
        screen.state.close_question();
        screen.commit();
        match outcome {
            Ok(repl::Outcome::Exit) => break Ok(()),
            Ok(repl::Outcome::Continue) => {}
            Err(e) => {
                // `repl::handle` reports turn failures itself; anything escaping
                // it is a session-level problem worth showing.
                screen.state.show(Cell::Failure(format!("{e:#}")));
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
        screen.apply(Notice::Usage(Usage {
            prompt_tokens: 6,
            total_tokens: 10,
            completion_tokens: 4,
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 4,
            prompt_tokens_details: None,
        }));
        assert_eq!(
            screen.status.full_line(),
            "m-1 · cache 60.0% · hit 6 · miss 4"
        );
        screen.apply(Notice::ResetStats);
        assert_eq!(screen.status.full_line(), "m-1 · cache —");
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

    /// A state with a turn that has been running for `elapsed`, and the moment it
    /// is asked about. The clock is the test's to decide, so what the status line
    /// shows is asserted rather than waited for.
    fn running_for(elapsed: Duration) -> (State, Instant) {
        let mut state = State::default();
        let now = Instant::now();
        state.begin_turn(now - elapsed);
        (state, now)
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
        // the status line, the tip, then the box, on the last rows of the terminal.
        let mut screen = screen_for_test(60, 20);
        screen.state.status.set_model("m-1");
        screen.draw().unwrap();
        let last = screen.terminal.backend().buffer().area.height - 1;
        assert_eq!(row(&screen, last - 4), "m-1 · cache —");
        assert_eq!(
            row(&screen, last - 3),
            format!("{TIP_GUTTER}Tip: {}", TIPS[0])
        );
        assert!(row(&screen, last - 2).starts_with('┌'), "the box's top");
        assert!(row(&screen, last - 1).starts_with("│ ›"), "its text");
        assert!(row(&screen, last).starts_with('└'), "its bottom");
    }

    #[test]
    fn the_status_line_says_what_the_turn_is_doing() {
        // The line as drawn, not as formatted: the word is painted in its own
        // colour, and the row is the only place that can be seen.
        let mut screen = screen_for_test(60, 20);
        let now = Instant::now();
        screen.state.begin_turn(now);
        screen.state.verb = "Julienning";
        screen.state.apply(Notice::Usage(Usage {
            completion_tokens: 259,
            ..Usage::default()
        }));
        screen.state.apply(Notice::ToolStart {
            name: "read_file".into(),
            args: "{}".into(),
        });
        screen.draw_at(now).unwrap();
        let pinned = screen.terminal.backend().buffer().area.height - PINNED_ROWS;
        assert_eq!(
            row(&screen, pinned),
            "· Julienning… (0s · ↓ 259 tokens · running read_file)"
        );
        let buf = screen.terminal.backend().buffer();
        assert_eq!(
            buf[(0, pinned)].fg,
            Color::Indexed(WORKING_FOREGROUND),
            "the word is the one thing on the line that is not dim"
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
        assert_eq!(row(&screen, top + 2), "x".repeat(14));
    }

    #[test]
    fn thinking_is_drawn_as_a_block_that_reaches_the_edge() {
        // Its own ground, painted across the whole row rather than under the
        // characters only: a block that stops where the text stops does not read as
        // a block, and the answer below it must not be caught by it.
        let mut screen = screen_for_test(40, 20);
        screen.state.transcript.push(Cell::Reasoning("hmm".into()));
        screen.state.transcript.push(Cell::Content("answer".into()));
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        let buf = screen.terminal.backend().buffer();
        assert_eq!(row(&screen, top), "hmm");
        assert_eq!(buf[(0, top)].bg, Color::Indexed(REASONING_GROUND));
        assert_eq!(buf[(3, top)].bg, Color::Indexed(REASONING_GROUND));
        assert_eq!(
            buf[(39, top)].bg,
            Color::Indexed(REASONING_GROUND),
            "all the way to the edge"
        );
        assert_eq!(
            buf[(0, top)].fg,
            Color::Indexed(REASONING_FOREGROUND),
            "and readable on it"
        );
        assert_eq!(row(&screen, top + 1), "answer");
        assert_eq!(
            buf[(0, top + 1)].bg,
            Color::Reset,
            "the answer keeps its own"
        );
        assert_eq!(buf[(39, top + 1)].bg, Color::Reset);
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
        let rows = transcript_rows(20) as usize;
        for i in 0..(rows + 5) {
            screen
                .state
                .transcript
                .push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        assert_eq!(row(&screen, 0), "line 5", "the first five are off the top");
        assert_eq!(row(&screen, rows as u16 - 1), format!("line {}", rows + 4));
    }

    #[test]
    fn paging_back_moves_the_window_and_paging_forward_returns_it() {
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20) as usize;
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
        assert_eq!(row(&screen, 0), format!("line {}", last - rows * 2));
        assert_eq!(
            row(&screen, rows as u16 - 1),
            format!("line {}", last - rows - 1)
        );
        // Forward again, one page at a time.
        press(&mut screen.state, KeyCode::PageDown);
        screen.draw().unwrap();
        assert_eq!(row(&screen, rows as u16 - 1), format!("line {}", last - 1));
        // And no further: there is nothing past the end.
        press(&mut screen.state, KeyCode::PageDown);
        screen.draw().unwrap();
        assert_eq!(row(&screen, rows as u16 - 1), format!("line {}", last - 1));
    }

    #[test]
    fn lines_arriving_do_not_move_a_reader_who_scrolled_back() {
        // A turn keeps writing while the user reads what came before: the window
        // has to stay on the line they were on instead of sliding to the end
        // under them.
        let mut screen = screen_for_test(40, 20);
        let rows = transcript_rows(20) as usize;
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
        for i in 0..(transcript_rows(20) as usize * 2) {
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
        let joined: String = rows[..transcript_rows(30) as usize].concat();
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
        let rows = all_rows(&screen);
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

    fn press(state: &mut State, code: KeyCode) -> Submitted {
        state.key(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn type_in(state: &mut State, text: &str) {
        for c in text.chars() {
            press(state, KeyCode::Char(c));
        }
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
                .map(|c| c.name.as_str())
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
    fn escape_dismisses_the_picker_without_touching_the_line() {
        let mut screen = State::default();
        type_in(&mut screen, "/s");
        press(&mut screen, KeyCode::Esc);
        assert!(screen.picker.is_none());
        assert_eq!(screen.text(), "/s", "what was typed is kept");
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
            .map(|c| (c.name.as_str(), c.detail.as_str()))
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
        assert_eq!(transcript_rows(24), 24 - PINNED_ROWS);
        assert_eq!(transcript_rows(1), 1, "never zero, even on a tiny terminal");
        assert_eq!(transcript_rows(0), 1);
    }

    #[test]
    fn the_spinner_frame_comes_from_the_clock_not_from_a_counter() {
        let at = Duration::from_millis;
        assert_eq!(spinner_frame(at(0)), SPINNER[0]);
        assert_eq!(spinner_frame(at(99)), SPINNER[0], "still the first frame");
        assert_eq!(spinner_frame(at(100)), SPINNER[1]);
        assert_eq!(spinner_frame(at(600)), SPINNER[0], "it comes round");
        // The same moment is always the same frame, so a redraw that was skipped
        // cannot leave the spinner behind.
        assert_eq!(spinner_frame(at(1_234)), spinner_frame(at(1_234)));
    }

    /// The working line as the screen would read it, styling and all.
    fn working_line(state: &State, now: Instant) -> String {
        state
            .working(80, now)
            .expect("a turn is running")
            .to_string()
    }

    #[test]
    fn the_working_line_says_what_the_turn_is_doing_and_for_how_long() {
        let (mut state, now) = running_for(Duration::from_millis(12_300));
        state.verb = "Julienning";
        // The frame comes from the clock, so the test reads it from there too.
        let frame = spinner_frame(Duration::from_millis(12_300));
        assert_eq!(
            working_line(&state, now),
            format!("{frame} Julienning… (12s · thinking)"),
            "the minutes only appear once there are any"
        );
        state.apply(Notice::ToolStart {
            name: "read_file".into(),
            args: "{}".into(),
        });
        assert_eq!(
            working_line(&state, now),
            format!("{frame} Julienning… (12s · running read_file)")
        );
        // What follows a result is the model reading it.
        state.apply(Notice::ToolResult("ok".into()));
        assert_eq!(
            working_line(&state, now),
            format!("{frame} Julienning… (12s · thinking)")
        );
        // An answer being written is the third thing a turn can be doing.
        state.apply(Notice::Content("here it is".into()));
        assert_eq!(
            working_line(&state, now),
            format!("{frame} Julienning… (12s · responding)")
        );
    }

    #[test]
    fn the_working_line_reports_the_tokens_the_provider_counted() {
        let (mut state, now) = running_for(Duration::from_millis(65_400));
        state.verb = "Julienning";
        let frame = spinner_frame(Duration::from_millis(65_400));
        // Nothing reported yet: no number, rather than one this side made up.
        assert_eq!(
            working_line(&state, now),
            format!("{frame} Julienning… (1m 5s · thinking)")
        );
        state.apply(Notice::Usage(Usage {
            completion_tokens: 259,
            ..Usage::default()
        }));
        assert_eq!(
            working_line(&state, now),
            format!("{frame} Julienning… (1m 5s · ↓ 259 tokens · thinking)")
        );
        // Every sub-request of the turn goes into the same count.
        state.apply(Notice::Usage(Usage {
            completion_tokens: 41,
            ..Usage::default()
        }));
        assert_eq!(
            working_line(&state, now),
            format!("{frame} Julienning… (1m 5s · ↓ 300 tokens · thinking)")
        );
    }

    #[test]
    fn a_narrow_working_line_keeps_the_word_and_drops_the_detail() {
        let (mut state, now) = running_for(Duration::from_millis(1_500));
        state.verb = "Julienning";
        state.apply(Notice::Usage(Usage {
            completion_tokens: 259,
            ..Usage::default()
        }));
        let frame = spinner_frame(Duration::from_millis(1_500));
        let full = format!("{frame} Julienning… (1s · ↓ 259 tokens · thinking)");
        assert_eq!(working_line(&state, now), full);
        assert_eq!(
            state
                .working(super::super::text::width(&full), now)
                .unwrap()
                .to_string(),
            format!("{frame} Julienning…"),
            "a line with no room for both keeps the word, whole"
        );
        assert_eq!(
            state.working(12, now).unwrap().to_string(),
            format!("{frame} Julienning…"),
            "and what is left still says something is happening"
        );
    }

    #[test]
    fn the_word_is_picked_when_the_turn_begins() {
        // One word per turn, out of the clock: the machine underneath is the same
        // every time, and the word is what makes a long wait readable.
        let mut state = State::default();
        state.begin_turn(Instant::now());
        let picked = state.verb;
        assert!(VERBS.contains(&picked), "{picked:?} is not one of them");
        assert_eq!(state.turn_tokens, None, "and the count starts empty");
        for seed in [0u128, 1, 13, 999_999_999_999, u128::MAX] {
            assert!(VERBS.contains(&verb_for(seed)));
        }
    }

    #[test]
    fn an_idle_line_is_the_summary_and_nothing_else() {
        let (state, now) = running_for(Duration::from_millis(50));
        assert_eq!(
            state.status_line(40, now).to_string(),
            format!(
                "{} {}… (0s · thinking)",
                spinner_frame(Duration::from_millis(50)),
                state.verb
            )
        );
        let mut idle = State::default();
        idle.status.set_model("m");
        assert_eq!(idle.status_line(40, now).to_string(), "m · cache —");
        // Narrow enough that the summary loses a segment of its own: with nothing
        // running, the line is the summary and nothing competes with it.
        assert_eq!(idle.status_line(9, now).to_string(), "m");
    }

    #[test]
    fn the_tick_moves_with_the_spinner_while_a_turn_runs() {
        let mut state = State::default();
        let now = Instant::now();
        state.begin_turn(now);
        assert_eq!(state.tick(now), 0);
        assert_eq!(state.tick(now + SPINNER_FRAME), 1, "a frame later");
        // The same moment is the same tick, which is what lets a redraw be skipped.
        assert_eq!(state.tick(now + SPINNER_FRAME), 1);
        state.end_turn();
        assert_eq!(state.phase, Phase::Thinking, "and stops naming a tool");
    }

    #[test]
    fn an_idle_screen_still_has_a_clock_because_the_tip_changes_on_one() {
        // The tip line comes round on a period of its own, which is what moves the
        // tick while nothing else does: without it, the screen would keep the tip
        // it opened with for as long as the session lasted.
        let state = State::default();
        let started = state.started;
        assert_eq!(state.tick(started), 0);
        assert_eq!(state.tick(started + TIP_PERIOD), 1);
        assert_eq!(
            state.tick(started + TIP_PERIOD - Duration::from_millis(1)),
            0
        );
    }

    #[test]
    fn the_tip_comes_round_on_its_own_clock() {
        let state = State::default();
        let started = state.started;
        assert_eq!(state.tip(started), TIPS[0]);
        assert_eq!(state.tip(started + TIP_PERIOD), TIPS[1]);
        // Round the whole list, and back to the first: a session that runs long
        // enough does not run out of tips.
        assert_eq!(state.tip(started + TIP_PERIOD * TIPS.len() as u32), TIPS[0]);
        assert_eq!(
            state.tip(started + TIP_PERIOD * 3 + Duration::from_secs(5)),
            TIPS[3]
        );
    }

    #[test]
    fn a_turn_that_ends_forgets_the_tool_it_was_running() {
        // A tool whose turn was interrupted never reports a result, so the name
        // has to be dropped by the turn ending rather than by the result.
        let mut state = State::default();
        let now = Instant::now();
        state.begin_turn(now);
        state.apply(Notice::ToolStart {
            name: "run_command".into(),
            args: "{}".into(),
        });
        assert!(working_line(&state, now).contains("running run_command"));
        state.end_turn();
        assert!(state.working(80, now).is_none(), "nothing is running");
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
        // So does a running turn: the spinner has to keep moving.
        screen.state.begin_turn(Instant::now());
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
        // The answer is typed during a turn, when every other key is dropped, so
        // this is the one path that has to let it through: an answer that never
        // arrives leaves the turn waiting on a question nobody can see.
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

    #[test]
    fn keys_are_dropped_while_a_turn_runs_and_no_gate_is_open() {
        let mut state = State::default();
        let (cancel, cancelled) = watch::channel(false);
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char('h'))), &cancel);
        assert!(state.textarea.is_empty(), "not the place for the next line");
        state.key_while_working(
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            &cancel,
        );
        assert!(*cancelled.borrow(), "the cancel key is the exception");
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
}

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
use std::time::Duration;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style as RStyle};
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

/// The rows the input box takes when it holds `lines` lines of text: one row each
/// -- a line added with Ctrl-J is a line the box has to show -- plus the two
/// borders.
///
/// Capped at what a terminal `height` rows tall can spare once the pinned line
/// and a row of transcript are accounted for: a draft taller than the
/// screen scrolls inside the box rather than leaving the screen with nothing but
/// a box on it.
fn box_rows(lines: usize, height: u16) -> u16 {
    let wanted = u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(BOX_ROWS - 1);
    let most = height.saturating_sub(PINNED_ROWS + 1).max(BOX_ROWS);
    wanted.clamp(BOX_ROWS, most)
}

/// How many rows of the transcript a terminal `height` rows tall shows, with an
/// input box `input` rows tall and `queued` rows given to the queue.
///
/// Never zero, even on a terminal too short for the pinned region: the box is
/// worth a cramped transcript, where an empty screen is worth nothing.
fn transcript_rows(height: u16, input: u16, queued: u16) -> u16 {
    height.saturating_sub(PINNED_ROWS + input + queued).max(1)
}

/// The lines one wheel notch moves the window over the transcript: the step a
/// terminal's own scrollback takes, so a notch here reads like a notch anywhere
/// else. Not a page -- a notebook's worth of lines per flick of a wheel is a way
/// of losing the place rather than of reading.
const WHEEL_LINES: isize = 3;

/// The thinking block's ground and foreground, by their numbers in ANSI's
/// 256-colour palette. The same pair the plain front end writes as an escape
/// sequence; here as numbers because ratatui takes colours rather than sequences.
const REASONING_GROUND: u8 = 236;
const REASONING_FOREGROUND: u8 = 245;

/// How many rows of the queue are drawn above the input box. The queue is what
/// was asked for while a turn ran, and a long one costs the transcript rows it is
/// capped at: what is worth seeing is that the line arrived.
const QUEUE_ROWS: usize = 3;

/// What the box says while a turn runs: the line being typed is not this turn's
/// message, it is the one to run when this turn ends.
const QUEUE_PLACEHOLDER: &str = "›  the turn is running · Enter queues this line";

/// What the box says while nothing runs.
const IDLE_PLACEHOLDER: &str = "›  type a message · /help for commands";

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
    Usage(Usage),
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
        self.refresh_placeholder();
    }

    /// The turn is over.
    fn end_turn(&mut self) {
        self.revision += 1;
        self.turn_running = false;
        self.refresh_placeholder();
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
            Notice::Usage(u) => self.status.record(&u),
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

    /// The picker as it is drawn: one row per choice, the highlighted one
    /// reversed.
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
                    RSpan::styled(
                        format!(" {}", super::text::padded(&choice.label, width)),
                        name,
                    ),
                    RSpan::styled(format!(" {}", choice.detail), detail),
                ])
            })
            .collect()
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
    /// which is the one just typed and the one being waited for.
    fn queue_lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for line in &self.queued {
            lines.extend(wrapped_lines(
                &[Span::new(Style::Dim, format!("› {line}"))],
                width,
            ));
        }
        lines.split_off(lines.len().saturating_sub(QUEUE_ROWS))
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
    /// told something, and repainting an unchanged screen would be a whole-screen
    /// write every tick.
    fn draw_if_changed(&mut self) -> Result<(), B::Error> {
        let key = self.view_key()?;
        if self.drawn == Some(key) {
            return Ok(());
        }
        self.draw_at()?;
        // Only after the draw succeeded: a failed one leaves the screen showing
        // something else, so the next call has to try again.
        self.drawn = Some(key);
        Ok(())
    }

    /// Draw the screen as of now, for tests that do not care about the clock.
    #[cfg(test)]
    fn draw(&mut self) -> Result<(), B::Error> {
        self.draw_at()
    }

    /// Draw the screen: the window on the transcript, the input box, and the
    /// status line under it. The box is sized for what is in it, so the transcript
    /// gives up rows to a multi-line draft and takes them back when the line is
    /// submitted.
    fn draw_at(&mut self) -> Result<(), B::Error> {
        let size = self.terminal.size()?;
        let width = size.width as usize;
        let input = self.state.input_rows(size.height);
        // What is waiting to run, drawn at the bottom of the transcript: the
        // session, then what comes next, then the box, and under the box the
        // session summary. Asked for before the layout, because how many rows it
        // takes is what the transcript gives up.
        let queue = Text::from(self.state.queue_lines(width));
        let queued = queue.height() as u16;
        self.state.reset_box_scroll(input);
        let lines = self.state.lines(width);
        let rows = transcript_rows(size.height, input, queued);
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
        let status = self.state.status_line(width);
        let cursor = self.state.textarea.screen_cursor();

        self.terminal.draw(|frame| {
            let rows = Layout::vertical([
                Constraint::Min(0),
                Constraint::Length(queued),
                Constraint::Length(input),
                Constraint::Length(1),
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
            frame.render_widget(Paragraph::new(queue), rows[1]);
            frame.render_widget(&self.state.textarea, rows[2]);
            place_cursor(frame, rows[2], cursor);
            frame.render_widget(Paragraph::new(status), rows[3]);
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

/// The input box: an editor ruled off above and below. Enter submits and Ctrl-J
/// inserts a newline, matching the plain prompt's keys.
fn input_box() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_block(
        Block::default()
            .borders(BOX_BORDERS)
            .border_style(RStyle::new().add_modifier(Modifier::DIM)),
    );
    textarea.set_placeholder_text(IDLE_PLACEHOLDER);
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
/// the same inner area the widget renders into -- so the borders it is asked
/// about have to be the box's own.
fn place_cursor(frame: &mut Frame, area: Rect, cursor: ScreenCursor) {
    let inner = Block::default().borders(BOX_BORDERS).inner(area);
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
        // are waiting behind it either way.
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
            vec!["› line 2", "› line 3", "› line 4"]
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
            let drawn = row(&screen, (top + i) as u16);
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
        screen.state.apply(Notice::Usage(Usage {
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 4,
            ..Usage::default()
        }));
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
        let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
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
            row(&screen, rows as u16 - 1),
            format!("line {}", last - 1 - WHEEL_LINES as usize)
        );
        // Down: back to where the writing ends, and no further, since there is
        // nothing past the end for the window to show.
        for _ in 0..2 {
            screen.state.key(mouse(MouseEventKind::ScrollDown));
        }
        screen.draw().unwrap();
        assert_eq!(row(&screen, rows as u16 - 1), format!("line {}", last - 1));
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
        let joined: String = rows[..transcript_rows(30, BOX_ROWS, 0) as usize].concat();
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
        assert_eq!(
            transcript_rows(24, BOX_ROWS, 0),
            24 - PINNED_ROWS - BOX_ROWS
        );
        assert_eq!(
            transcript_rows(1, BOX_ROWS, 0),
            1,
            "never zero, even on a tiny terminal"
        );
        assert_eq!(transcript_rows(0, BOX_ROWS, 0), 1);
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
    fn a_box_taller_than_the_screen_gives_way_to_the_transcript() {
        // A draft longer than the terminal can show still leaves a row of
        // transcript: past that the box scrolls inside itself.
        let height = 12;
        let most = height - PINNED_ROWS - 1;
        assert_eq!(box_rows(100, height), most);
        assert_eq!(transcript_rows(height, box_rows(100, height), 0), 1);
        // And a terminal too short for even that still gets its one transcript
        // row, because the alternative is an empty screen.
        assert_eq!(box_rows(100, 2), BOX_ROWS);
        assert_eq!(transcript_rows(2, box_rows(100, 2), 0), 1);
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
}

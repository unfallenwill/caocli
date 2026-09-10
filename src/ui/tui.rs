//! The interactive front end: an inline viewport pinned to the bottom of the
//! terminal, with finished turns flowing into the terminal's own scrollback.
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
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use ratatui_textarea::{ScreenCursor, TextArea};
use tokio::sync::{mpsc, oneshot, watch};

use crate::agent::{Agent, Approve, Interrupt};
use crate::history;
use crate::repl;
use crate::types::{Message, ToolCall, Usage};
use crate::ui::Front;

use super::Ui;
use super::cell::{self, Cell, Span, Style};
use super::status::Status;

/// Rows the live area occupies above the pinned region. It shows the tail of what
/// the current turn is producing; a finished turn moves up into scrollback.
const LIVE_ROWS: u16 = 10;

/// Rows the pinned region needs: the status line, then the input box (border,
/// text, border).
const PINNED_ROWS: u16 = 1 + 3;

/// The whole inline viewport.
const VIEWPORT_ROWS: u16 = LIVE_ROWS + PINNED_ROWS;

/// The spinner, advanced while work is in progress. One column each, so it can
/// sit on the status line without moving it.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// How long each spinner frame is shown. Long enough to read, short enough that
/// the line looks alive.
const SPINNER_FRAME: Duration = Duration::from_millis(100);

/// How many spinner frames fit in `elapsed`: the tick a moment belongs to.
///
/// Derived from the elapsed time rather than counted, so a redraw that was
/// skipped cannot leave the spinner behind: the frame is a function of *when*,
/// not of how many times anything ran. Everything on the status line that moves
/// on its own -- the frame, the timer -- moves on this tick, so one number
/// answers both "which frame" and "has anything changed".
fn spinner_step(elapsed: Duration) -> u64 {
    elapsed.as_millis() as u64 / SPINNER_FRAME.as_millis().max(1) as u64
}

/// Which spinner frame belongs to a moment in time.
fn spinner_frame(elapsed: Duration) -> &'static str {
    SPINNER[spinner_step(elapsed) as usize % SPINNER.len()]
}

/// A duration as the status line shows it: tenths of a second.
///
/// Formatted from integer milliseconds so that it changes on exactly the tick
/// [`spinner_step`] counts. A float would round at boundaries of its own, and a
/// redraw is skipped by comparing the tick.
fn seconds(elapsed: Duration) -> String {
    let tenths = elapsed.as_millis() as u64 / 100;
    format!("{}.{}s", tenths / 10, tenths % 10)
}

/// The least room the session summary is worth showing in. Below this it is
/// dropped rather than clipped to a stub.
const MIN_STATUS_COLUMNS: usize = 12;

/// How many command rows the picker shows at once. It draws over the bottom of
/// the live area, so it has to leave the transcript somewhere to live.
const PICKER_ROWS: usize = 6;

/// How many lines one commit may push into scrollback at a time. Asking a
/// terminal to scroll further than it has rows is not something it can do, so a
/// long turn is committed in batches.
fn commit_batch(terminal_height: u16) -> usize {
    terminal_height.saturating_sub(VIEWPORT_ROWS).max(1) as usize
}

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
/// loop is shaped the way it is: an inline viewport issues a cursor-position
/// query to place itself and again on every commit, and that query reads from the
/// same handle. A background reader parked in `event::read` steals the answer, and
/// the query then times out -- measured, and intermittent, which is exactly what a
/// stolen read looks like.
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
    /// Cells produced since the last commit: the turn in flight.
    pending: Vec<Cell>,
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
    /// The tool being executed, while one is.
    tool: Option<String>,
    /// Bumped by everything that changes what the screen should show, so a draw
    /// can be skipped when nothing has.
    revision: u64,
}

/// The command picker: what matched, and which one is highlighted.
struct Picker {
    matches: Vec<&'static repl::Command>,
    selected: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            status: Status::default(),
            pending: Vec::new(),
            live: None,
            question: None,
            reply: None,
            textarea: input_box(),
            picker: None,
            history: Vec::new(),
            browsing: None,
            draft: String::new(),
            turn_started: None,
            tool: None,
            revision: 0,
        }
    }
}

impl State {
    /// Everything not yet committed to scrollback, in draw order: the turn's
    /// finished cells, then the block still streaming, then a pending question.
    fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();
        for cell in &self.pending {
            lines.push(one_line(&cell.spans()));
        }
        if let Some((style, text)) = &self.live {
            lines.extend(wrapped_lines(&[Span::new(*style, text.clone())], width));
        }
        if let Some(question) = &self.question {
            lines.push(one_line(&question.spans()));
        }
        lines
    }

    /// The pinned status line: the session summary, and then what is running.
    fn status_line(&self, width: usize, now: Instant) -> String {
        // One column is left free so the write cannot trigger autowrap.
        let width = width.saturating_sub(1);
        let Some(progress) = self.progress(now) else {
            return self.status.line(width);
        };
        // What is running is what the user is watching, so it is measured first
        // and the session summary gives up the room; on a narrow line the summary
        // goes entirely rather than being clipped mid-word.
        let spent = super::text::width(&progress) + 3;
        let room = width.saturating_sub(spent);
        let summary = if room < MIN_STATUS_COLUMNS {
            String::new()
        } else {
            self.status.line(room)
        };
        if summary.is_empty() {
            progress
        } else {
            format!("{summary} · {progress}")
        }
    }

    /// What is running, if anything: a spinner, what it is, and how long it has
    /// taken.
    fn progress(&self, now: Instant) -> Option<String> {
        let elapsed = self.elapsed(now)?;
        let frame = spinner_frame(elapsed);
        let spent = seconds(elapsed);
        Some(match &self.tool {
            Some(tool) => format!("{frame} {tool} · {spent}"),
            None => format!("{frame} thinking · {spent}"),
        })
    }

    /// How long the current turn has been running, if one is.
    fn elapsed(&self, now: Instant) -> Option<Duration> {
        Some(now.saturating_duration_since(self.turn_started?))
    }

    /// The moment on the status line's own clock, if anything is moving on it.
    fn tick(&self, now: Instant) -> Option<u64> {
        Some(spinner_step(self.elapsed(now)?))
    }

    /// A turn is starting: start the clock the status line reads.
    fn begin_turn(&mut self, now: Instant) {
        self.revision += 1;
        self.turn_started = Some(now);
    }

    /// The turn is over: stop the clock, and stop naming the tool it was running,
    /// which a turn can end without -- an interrupted tool reports no result.
    fn end_turn(&mut self) {
        self.revision += 1;
        self.turn_started = None;
        self.tool = None;
    }

    /// Fold one notice into the state.
    fn apply(&mut self, notice: Notice) {
        self.revision += 1;
        match notice {
            Notice::Reasoning(text) => self.stream(Style::Dim, &text),
            Notice::Content(text) => self.stream(Style::Plain, &text),
            Notice::FinishTurn => self.end_block(),
            Notice::ToolStart { name, args } => {
                self.end_block();
                self.pending.push(Cell::tool_call(&name, &args));
                // The status line reports the tool by name while it runs, which
                // is the part of a turn that can take a long time.
                self.tool = Some(name);
            }
            Notice::ToolResult(result) => {
                self.end_block();
                self.pending.push(Cell::ToolResult(result));
                self.tool = None;
            }
            Notice::Usage(u) => self.status.record(&u),
            Notice::Interrupted => {
                self.end_block();
                self.pending.push(Cell::Interrupted);
            }
            Notice::Approval { name, args } => {
                self.end_block();
                self.question = Some(Cell::approval(&name, &args));
            }
            Notice::Replay(messages) => {
                self.end_block();
                self.pending.extend(cell::from_messages(&messages));
            }
            Notice::Info(text) => {
                self.end_block();
                self.pending.push(Cell::Notice(text));
            }
            Notice::Error(text) => {
                self.end_block();
                self.pending.push(Cell::Failure(text));
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
    fn end_block(&mut self) {
        if let Some((style, text)) = self.live.take() {
            let cell = match style {
                Style::Dim => Cell::Reasoning(text),
                _ => Cell::Content(text),
            };
            self.pending.push(cell);
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
        let matches = repl::completions(&self.text());
        if matches.is_empty() {
            self.picker = None;
            return;
        }
        let previous = self
            .picker
            .as_ref()
            .and_then(|p| p.matches.get(p.selected))
            .map(|c| c.name);
        let selected = previous
            .and_then(|name| matches.iter().position(|c| c.name == name))
            .unwrap_or(0);
        self.picker = Some(Picker { matches, selected });
    }

    /// Up: the previous command, or the previous line typed.
    fn up(&mut self) {
        if let Some(picker) = &mut self.picker {
            let last = picker.matches.len() - 1;
            picker.selected = if picker.selected == 0 {
                last
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
            picker.selected = (picker.selected + 1) % picker.matches.len();
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
        let Some(command) = picker.matches.get(picker.selected) else {
            return;
        };
        self.set_text(command.name);
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

    /// The picker as it is drawn: one row per command, the highlighted one
    /// reversed.
    fn picker_lines(&self) -> Vec<Line<'static>> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        picker
            .matches
            .iter()
            .enumerate()
            .map(|(i, command)| {
                let selected = i == picker.selected;
                let name = if selected {
                    RStyle::new().add_modifier(Modifier::REVERSED)
                } else {
                    RStyle::new()
                };
                let description = if selected {
                    RStyle::new().add_modifier(Modifier::REVERSED)
                } else {
                    RStyle::new().add_modifier(Modifier::DIM)
                };
                Line::from(vec![
                    RSpan::styled(format!(" {:<10}", command.name), name),
                    RSpan::styled(format!(" {}", command.description), description),
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

/// The viewport: a terminal, and the state it shows.
///
/// Generic over the backend so that what it draws can be asserted on. The
/// terminal-touching half of this front end is exactly the half nothing else can
/// check, and ratatui ships a backend that keeps the screen in memory -- which
/// makes the drawing testable instead of merely inspected by hand.
struct Screen<B: Backend> {
    terminal: Terminal<B>,
    state: State,
    /// The view key of the last draw, or `None` when the screen has to be
    /// repainted whatever the key says: nothing has been drawn yet, or something
    /// outside this screen (`insert_before`) has moved it.
    drawn: Option<ViewKey>,
}

/// What the viewport would show, as a value that can be compared.
///
/// Equal keys mean a redraw cannot change a pixel, so it is skipped. The
/// revision covers everything the state knows about itself; the tick covers what
/// moves without the state changing, which is the turn's own clock; the size is
/// there because the viewport is laid out from it. Reading the size is an
/// `ioctl`, not a round trip to the terminal, so it is cheap enough to be part
/// of a check that runs on every tick of the loop.
#[derive(PartialEq, Eq, Clone, Copy)]
struct ViewKey {
    revision: u64,
    tick: Option<u64>,
    width: u16,
    height: u16,
}

/// An inline viewport on `backend`: the last [`VIEWPORT_ROWS`] rows of the
/// terminal, with everything above it left to the terminal's own scrolling.
fn viewport<B: Backend>(backend: B) -> Result<Terminal<B>, B::Error> {
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_ROWS),
        },
    )
}

impl Screen<CrosstermBackend<Stdout>> {
    /// Enter the viewport.
    ///
    /// Fallible on purpose. An inline viewport has to ask the terminal where its
    /// cursor is, and a terminal that does not answer leaves ratatui no way to
    /// place itself; the caller falls back to the plain front end rather than
    /// failing to start. Raw mode is restored before the error is returned, so a
    /// failed attempt leaves no trace.
    fn enter() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        match Self::try_enter() {
            Ok(screen) => Ok(screen),
            Err(e) => {
                let _ = crossterm::terminal::disable_raw_mode();
                Err(e)
            }
        }
    }

    fn try_enter() -> io::Result<Self> {
        // The viewport is placed first: it is the step that can fail, and it asks
        // the terminal where its cursor is. Anything that changes terminal state
        // waits until that has succeeded, so a declined attempt emits nothing but
        // the query itself.
        let terminal = viewport(CrosstermBackend::new(std::io::stdout()))?;
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)?;
        Ok(Self {
            terminal,
            state: State::default(),
            drawn: None,
        })
    }

    /// Leave the viewport. No alternate screen was entered, so this only undoes
    /// raw mode and parks the cursor where a shell prompt can follow.
    fn leave(&mut self) -> io::Result<()> {
        crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::event::DisableBracketedPaste,
            crossterm::cursor::Show
        )?;
        self.terminal.show_cursor()?;
        println!();
        Ok(())
    }
}

impl<B: Backend> Screen<B> {
    /// What the viewport would show right now.
    fn view_key(&self, now: Instant) -> Result<ViewKey, B::Error> {
        let size = self.terminal.size()?;
        Ok(ViewKey {
            revision: self.state.revision,
            tick: self.state.tick(now),
            width: size.width,
            height: size.height,
        })
    }

    /// Repaint, unless the viewport already shows this.
    ///
    /// The loop wakes on a tick to stay responsive to the keyboard, and most of
    /// those ticks have nothing new to show: a turn spends its time waiting, and
    /// repainting an unchanged viewport would be a full-screen write every tick.
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

    /// Draw the viewport as of now, for tests that do not care about the clock.
    #[cfg(test)]
    fn draw(&mut self) -> Result<(), B::Error> {
        self.draw_at(Instant::now())
    }

    /// Draw the viewport: the tail of the live area, the pinned status line, the
    /// input box.
    fn draw_at(&mut self, now: Instant) -> Result<(), B::Error> {
        let size = self.terminal.size()?;
        let width = size.width as usize;
        let lines = self.state.lines(width);
        // Only the tail of the turn fits; scrolling is what makes it a tail
        // rather than a head.
        let scroll = lines.len().saturating_sub(LIVE_ROWS as usize) as u16;
        let live = Text::from(lines);
        let status = self.state.status_line(width, now);
        let cursor = self.state.textarea.screen_cursor();
        // The picker draws over the bottom of the live area rather than beside
        // it: it belongs to the line being typed, which is what it sits above.
        let picker = self.state.picker_lines();

        self.terminal.draw(|frame| {
            let rows = Layout::vertical([
                Constraint::Length(LIVE_ROWS),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(frame.area());
            frame.render_widget(Paragraph::new(live).scroll((scroll, 0)), rows[0]);
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
            frame.render_widget(&self.state.textarea, rows[2]);
            place_cursor(frame, rows[2], cursor);
        })?;
        Ok(())
    }

    /// Push the finished cells into the terminal's own scrollback.
    ///
    /// Batched: each insert costs a cursor-position round trip to the terminal
    /// (measured), so a turn is committed in as few passes as the screen height
    /// allows rather than a line at a time.
    fn commit(&mut self) -> Result<(), B::Error> {
        self.state.end_block();
        if self.state.pending.is_empty() {
            return Ok(());
        }
        let size = self.terminal.size()?;
        let width = size.width as usize;
        let batch = commit_batch(size.height);
        let cells = std::mem::take(&mut self.state.pending);
        // Each cell wraps on its own: a cell boundary is always a line boundary.
        let lines: Vec<Line<'static>> = cells
            .iter()
            .flat_map(|c| wrapped_lines(&c.spans(), width))
            .collect();
        for chunk in lines.chunks(batch) {
            let chunk = chunk.to_vec();
            self.terminal.insert_before(chunk.len() as u16, |buf| {
                Paragraph::new(Text::from(chunk.clone())).render(buf.area, buf);
            })?;
        }
        // The insertion moved everything the viewport shows, so what was drawn
        // last is not what is on the screen any more.
        self.drawn = None;
        Ok(())
    }
}

/// Raw mode is process-wide and would wreck the shell if it survived an unwind,
/// so restoring it does not depend on the success path running.
impl<B: Backend> Drop for Screen<B> {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
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

/// Our styled spans, as one ratatui line.
fn one_line(spans: &[Span]) -> Line<'static> {
    Line::from(
        spans
            .iter()
            .map(|s| RSpan::styled(s.text.clone(), style_of(s.style)))
            .collect::<Vec<_>>(),
    )
}

/// Wrap styled spans into the terminal lines they need at `width` columns.
///
/// Both halves of the front end need this, for the same reason: nothing here may
/// be left to the terminal's own soft wrapping. The live area is a fixed-height
/// region, so an unwrapped line would push the pinned rows out of the viewport,
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
        Style::Yellow => RStyle::new().fg(Color::Yellow),
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
/// prompt; here they go into scrollback first, so a resumed session reads the
/// same way either way.
/// Returns `Ok(false)` when this terminal cannot host the viewport, which is the
/// caller's signal to fall back to the plain front end rather than to fail: an
/// inline viewport has to be told where the cursor is, and not every terminal
/// answers. `Ok(true)` means the front end ran and the session is over.
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
    screen.state.pending.push(Cell::Notice(banner.to_owned()));
    screen.state.pending.extend(cell::from_messages(history));
    screen.commit()?;

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
        screen.state.remember(&line);

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
        // Drain once more before committing. The turn is polled *inside* the
        // select above, so its last notifications are sent after the loop's last
        // drain and are still queued when it resolves; committing without them
        // would push a half-finished turn into scrollback and leave the rest to
        // surface during the next idle wait.
        for notice in drain(&mut notices) {
            screen.state.apply(notice);
        }
        for reply in drain(&mut asked) {
            screen.state.open_question(reply);
        }
        screen.state.end_turn();
        screen.state.close_question();
        if let Err(e) = screen.commit() {
            break Err(e.into());
        }
        match outcome {
            Ok(repl::Outcome::Exit) => break Ok(()),
            Ok(repl::Outcome::Continue) => {}
            Err(e) => {
                // `repl::handle` reports turn failures itself; anything escaping
                // it is a session-level problem worth showing.
                screen.state.pending.push(Cell::Failure(format!("{e:#}")));
                let _ = screen.commit();
            }
        }
    };

    screen.leave()?;
    // Written on the way out rather than per line: the file is small, and a
    // rewrite per keystroke would be work for nothing.
    if let Err(e) = history::save(&history_path, &screen.state.history) {
        screen.state.pending.push(Cell::Failure(format!("{e:#}")));
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
            screen.pending,
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
            screen.pending,
            vec![Cell::Reasoning("ab".into())],
            "the reasoning block closed when the style changed"
        );
        screen.apply(Notice::FinishTurn);
        assert_eq!(screen.pending[1], Cell::Content("x".into()));
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
            screen.pending,
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
            terminal: viewport(backend).unwrap(),
            state: State::default(),
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

    /// One row of what was drawn, without the padding.
    fn row(screen: &Screen<ratatui::backend::TestBackend>, y: u16) -> String {
        let buf = screen.terminal.backend().buffer();
        (0..buf.area.width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim_end()
            .to_owned()
    }

    /// Where the viewport was placed. Its origin is not asserted on: an inline
    /// viewport sits at the cursor it found, which on a real terminal is the
    /// bottom of the screen and in an in-memory backend is the top.
    fn origin(screen: &mut Screen<ratatui::backend::TestBackend>) -> Rect {
        screen.terminal.get_frame().area()
    }

    /// Every row of the screen, so a test can ask whether something was drawn
    /// without pinning down where the viewport happened to be.
    fn all_rows(screen: &Screen<ratatui::backend::TestBackend>) -> Vec<String> {
        let height = screen.terminal.backend().buffer().area.height;
        (0..height).map(|y| row(screen, y)).collect()
    }

    #[test]
    fn the_viewport_pins_the_status_line_above_the_input_box() {
        // The shape the pinned region has to keep, whatever the live area does.
        let mut screen = screen_for_test(40, 20);
        screen.state.status.set_model("m-1");
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        assert_eq!(row(&screen, top + LIVE_ROWS), "m-1 · cache —");
        assert!(
            row(&screen, top + LIVE_ROWS + 1).starts_with('┌'),
            "the box's top"
        );
        assert!(
            row(&screen, top + LIVE_ROWS + 2).starts_with("│ ›"),
            "its text"
        );
        assert!(
            row(&screen, top + LIVE_ROWS + 3).starts_with('└'),
            "its bottom"
        );
    }

    #[test]
    fn the_status_line_says_what_the_turn_is_doing() {
        let mut screen = screen_for_test(40, 20);
        screen.state.status.set_model("m");
        let now = Instant::now();
        screen.state.begin_turn(now);
        screen.state.apply(Notice::ToolStart {
            name: "read_file".into(),
            args: "{}".into(),
        });
        screen.draw_at(now).unwrap();
        let top = origin(&mut screen).y;
        assert_eq!(
            row(&screen, top + LIVE_ROWS),
            "m · cache — · ⠋ read_file · 0.0s"
        );
    }

    #[test]
    fn the_live_area_shows_the_tail_of_a_long_turn() {
        // The area is a fixed height: what does not fit scrolls off the top of it
        // rather than pushing the pinned rows away.
        let mut screen = screen_for_test(40, 20);
        for i in 0..(LIVE_ROWS + 5) {
            screen.state.pending.push(Cell::Notice(format!("line {i}")));
        }
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        assert_eq!(row(&screen, top), "line 5", "the first five scrolled off");
        assert_eq!(row(&screen, top + LIVE_ROWS - 1), "line 14");
    }

    #[test]
    fn committing_moves_the_turn_into_scrollback_and_empties_it() {
        let mut screen = screen_for_test(40, 20);
        screen.state.pending.push(Cell::Notice("first".into()));
        screen.state.pending.push(Cell::Notice("second".into()));
        screen.commit().unwrap();
        assert!(screen.state.pending.is_empty(), "nothing is left to redraw");
        let rows = all_rows(&screen);
        let first = rows.iter().position(|r| r == "first").expect("committed");
        assert_eq!(rows[first + 1], "second", "in order, on the next row");
        assert!(
            !screen
                .state
                .lines(40)
                .iter()
                .any(|l| format!("{l:?}").contains("first")),
            "and gone from the live area"
        );
    }

    #[test]
    fn what_is_committed_is_wrapped_not_clipped() {
        // The regression this guards: `insert_before` renders into a fixed-width
        // buffer, where an over-long line is silently cut off.
        let mut screen = screen_for_test(10, 30);
        let long = "abcdefghijklmnopqrstuvwxyz"; // 26 columns at width 10
        screen.state.pending.push(Cell::Notice(long.into()));
        screen.commit().unwrap();
        let rows = all_rows(&screen);
        let joined: String = rows
            .iter()
            .filter(|r| !r.is_empty() && !r.starts_with(['┌', '│', '└']))
            .cloned()
            .collect();
        assert_eq!(joined, long, "every column survived, in order");
    }

    #[test]
    fn an_empty_turn_commits_nothing() {
        let mut screen = screen_for_test(40, 20);
        let before = all_rows(&screen);
        screen.commit().unwrap();
        assert_eq!(all_rows(&screen), before);
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
            picker.matches.iter().map(|c| c.name).collect::<Vec<_>>(),
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
        let count = screen.picker.as_ref().unwrap().matches.len();
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
    fn the_commit_batch_leaves_room_for_the_viewport() {
        // Scrolling further than the screen has rows is not something a terminal
        // can do, so the batch is capped by the rows above the viewport.
        assert_eq!(commit_batch(24), 24 - VIEWPORT_ROWS as usize);
        assert_eq!(commit_batch(1), 1, "never zero, even on a tiny terminal");
        assert_eq!(commit_batch(0), 1);
    }

    #[test]
    fn the_spinner_frame_comes_from_the_clock_not_from_a_counter() {
        let at = Duration::from_millis;
        assert_eq!(spinner_frame(at(0)), SPINNER[0]);
        assert_eq!(spinner_frame(at(99)), SPINNER[0], "still the first frame");
        assert_eq!(spinner_frame(at(100)), SPINNER[1]);
        assert_eq!(spinner_frame(at(1_000)), SPINNER[0], "it comes round");
        // The same moment is always the same frame, so a redraw that was skipped
        // cannot leave the spinner behind.
        assert_eq!(spinner_frame(at(1_234)), spinner_frame(at(1_234)));
    }

    #[test]
    fn the_status_line_shows_what_the_turn_is_doing_and_for_how_long() {
        let (mut state, now) = running_for(Duration::from_millis(12_300));
        state.status.set_model("m");
        assert_eq!(
            state.status_line(80, now),
            "m · cache — · ⠸ thinking · 12.3s"
        );
        state.apply(Notice::ToolStart {
            name: "read_file".into(),
            args: "{}".into(),
        });
        assert_eq!(
            state.status_line(80, now),
            "m · cache — · ⠸ read_file · 12.3s"
        );
        state.apply(Notice::ToolResult("ok".into()));
        assert_eq!(
            state.status_line(80, now),
            "m · cache — · ⠸ thinking · 12.3s"
        );
    }

    #[test]
    fn an_idle_line_is_the_summary_and_nothing_else() {
        let (mut state, now) = running_for(Duration::from_millis(50));
        state.status.set_model("m");
        assert_eq!(
            state.status_line(40, now),
            "m · cache — · ⠋ thinking · 0.0s"
        );
        let idle = State::default();
        assert_eq!(idle.status_line(40, now), "cache —");
        // Narrow enough that the summary would be clipped to a stub: with nothing
        // running there is no reason to drop it.
        assert_eq!(idle.status_line(9, now), "cache —");
    }

    #[test]
    fn a_narrow_line_drops_the_summary_rather_than_clipping_it() {
        let (state, now) = running_for(Duration::from_millis(1_500));
        let progress = "⠴ thinking · 1.5s";
        assert_eq!(super::super::text::width(progress), 17);
        // One column short of room for a summary worth reading, so the summary
        // goes whole rather than being clipped mid-word.
        assert_eq!(state.status_line(32, now), progress);
        // One column more, and it fits with its separator.
        assert_eq!(state.status_line(33, now), "cache — · ⠴ thinking · 1.5s");
    }

    #[test]
    fn the_view_key_moves_with_the_clock_while_a_turn_runs() {
        let mut state = State::default();
        let now = Instant::now();
        assert_eq!(state.tick(now), None, "nothing is running, nothing to draw");
        state.begin_turn(now);
        let first = state.tick(now);
        assert_eq!(first, Some(0));
        assert_eq!(state.tick(now + SPINNER_FRAME), Some(1), "a frame later");
        assert_eq!(state.tick(now + SPINNER_FRAME), first.map(|_| 1));
        state.end_turn();
        assert_eq!(state.tick(now), None, "the clock stops with the turn");
        assert!(state.tool.is_none(), "and stops naming a tool");
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
        assert!(state.progress(now).unwrap().contains("run_command"));
        state.end_turn();
        assert_eq!(state.progress(now), None);
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
    fn an_unchanged_viewport_is_not_drawn_again() {
        let frames = std::rc::Rc::new(std::cell::Cell::new(0));
        let backend = Counting {
            inner: ratatui::backend::TestBackend::new(40, 20),
            frames: frames.clone(),
        };
        let mut screen = Screen {
            terminal: viewport(backend).unwrap(),
            state: State::default(),
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
    fn committing_forces_the_next_draw() {
        // `insert_before` moves everything the viewport shows, so what was drawn
        // last is stale whatever the revision says.
        let mut screen = screen_for_test(40, 20);
        screen.draw_if_changed().unwrap();
        screen.state.pending.push(Cell::Notice("done".into()));
        screen.commit().unwrap();
        assert!(screen.drawn.is_none(), "the next draw cannot be skipped");
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

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

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style as RStyle};
use ratatui::text::{Line, Span as RSpan, Text};
use ratatui::widgets::{Block, Borders, Paragraph, Widget};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use ratatui_textarea::{ScreenCursor, TextArea};
use tokio::sync::{mpsc, oneshot, watch};

use crate::agent::{Agent, Approve, Interrupt};
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
const TICK: std::time::Duration = std::time::Duration::from_millis(8);

/// Read a key if one is waiting, without blocking for longer than a tick.
///
/// This is deliberately *not* a reader thread, and that is the whole reason the
/// loop is shaped the way it is: an inline viewport issues a cursor-position
/// query to place itself and again on every commit, and that query reads from the
/// same handle. A background reader parked in `event::read` steals the answer, and
/// the query then times out -- measured, and intermittent, which is exactly what a
/// stolen read looks like.
fn poll_key(timeout: std::time::Duration) -> io::Result<Option<Event>> {
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
    /// Whether a turn is running, which the status line reports.
    working: bool,
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
            working: false,
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

    /// The pinned status line, plus whether a turn is running.
    fn status_line(&self, width: usize) -> String {
        // One column is left free so the write cannot trigger autowrap.
        let label = self.status.line(width.saturating_sub(1));
        if self.working {
            format!("{label} · working…")
        } else {
            label
        }
    }

    /// Fold one notice into the state.
    fn apply(&mut self, notice: Notice) {
        match notice {
            Notice::Reasoning(text) => self.stream(Style::Dim, &text),
            Notice::Content(text) => self.stream(Style::Plain, &text),
            Notice::FinishTurn => self.end_block(),
            Notice::ToolStart { name, args } => {
                self.end_block();
                self.pending.push(Cell::tool_call(&name, &args));
            }
            Notice::ToolResult(result) => {
                self.end_block();
                self.pending.push(Cell::ToolResult(result));
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
            _ => {
                self.textarea.input(Event::Key(key));
                Submitted::Nothing
            }
        }
    }

    /// Handle a key while a turn is running: only the cancel key does anything,
    /// because a turn is not the place to start composing the next line.
    fn key_while_working(&mut self, event: Event, cancel: &watch::Sender<bool>) {
        if let Event::Key(key) = event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let _ = cancel.send(true);
        }
    }

    /// Take the submitted line out of the box, leaving it empty for the next one.
    fn take_line(&mut self) -> String {
        let line = self.textarea.lines().join("\n");
        self.textarea = input_box();
        line
    }

    /// The approval gate is asking: remember who to answer.
    fn open_question(&mut self, reply: oneshot::Sender<bool>) {
        self.reply = Some(reply);
        self.working = true;
    }

    /// Answer the open question from what the user submitted, if anything. A line
    /// starting with `y` allows and anything else denies, which is the rule the
    /// plain front end applies to a line of stdin.
    fn close_question(&mut self) {
        if let Some(reply) = self.reply.take() {
            let answer = self.textarea.lines().join("\n").trim().to_lowercase();
            let _ = reply.send(answer.starts_with('y'));
            self.textarea = input_box();
        }
        self.question = None;
        self.working = false;
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
    /// Draw the viewport: the tail of the live area, the pinned status line, the
    /// input box.
    fn draw(&mut self) -> Result<(), B::Error> {
        let size = self.terminal.size()?;
        let width = size.width as usize;
        let lines = self.state.lines(width);
        // Only the tail of the turn fits; scrolling is what makes it a tail
        // rather than a head.
        let scroll = lines.len().saturating_sub(LIVE_ROWS as usize) as u16;
        let live = Text::from(lines);
        let status = self.state.status_line(width);
        let cursor = self.state.textarea.screen_cursor();

        self.terminal.draw(|frame| {
            let rows = Layout::vertical([
                Constraint::Length(LIVE_ROWS),
                Constraint::Length(1),
                Constraint::Length(3),
            ])
            .split(frame.area());
            frame.render_widget(Paragraph::new(live).scroll((scroll, 0)), rows[0]);
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
    screen.state.status.set_model(&agent.session.meta.model);
    screen.state.pending.push(Cell::Notice(banner.to_owned()));
    screen.state.pending.extend(cell::from_messages(history));
    screen.commit()?;

    let (tx, mut notices) = mpsc::unbounded_channel();
    let (ask_tx, mut asked) = mpsc::unbounded_channel();
    let mut handle = Notifier { tx };

    let result: anyhow::Result<()> = loop {
        // Idle: draw, then wait for something to submit.
        if let Err(e) = screen.draw() {
            break Err(e.into());
        }
        let line = loop {
            for notice in drain(&mut notices) {
                screen.state.apply(notice);
            }
            screen.draw()?;
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
        screen.state.working = true;
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
            screen.draw()?;
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
        screen.state.working = false;
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
        }
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
    fn the_status_line_says_when_a_turn_is_running() {
        let mut screen = screen_for_test(40, 20);
        screen.state.status.set_model("m");
        screen.state.working = true;
        screen.draw().unwrap();
        let top = origin(&mut screen).y;
        assert_eq!(row(&screen, top + LIVE_ROWS), "m · cache — · working…");
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

    #[test]
    fn the_commit_batch_leaves_room_for_the_viewport() {
        // Scrolling further than the screen has rows is not something a terminal
        // can do, so the batch is capped by the rows above the viewport.
        assert_eq!(commit_batch(24), 24 - VIEWPORT_ROWS as usize);
        assert_eq!(commit_batch(1), 1, "never zero, even on a tiny terminal");
        assert_eq!(commit_batch(0), 1);
    }

    #[test]
    fn the_status_line_reports_whether_a_turn_is_running() {
        let mut screen = State::default();
        screen.status.set_model("m");
        assert_eq!(screen.status_line(40), "m · cache —");
        screen.working = true;
        assert_eq!(screen.status_line(40), "m · cache — · working…");
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
    fn a_question_is_answered_by_what_the_user_submits() {
        let mut screen = State::default();
        let (reply, answer) = oneshot::channel();
        screen.open_question(reply);
        assert!(screen.working, "the status line shows the gate is open");
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

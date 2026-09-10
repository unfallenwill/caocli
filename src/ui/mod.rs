mod cell;
mod contract;
mod status;
pub(crate) mod text;
pub mod tui;

pub use contract::{Front, Ui};

use std::future::Future;
use std::io::{IsTerminal, Write};
use std::pin::Pin;
use std::time::Duration;

use crate::types::{Message, Usage};

use cell::{Cell, Span, Style};
use status::Status;

const RESET: &str = "\x1b[0m";

/// Terminal size (rows, cols). None on non-unix or when the ioctl fails.
#[cfg(unix)]
fn terminal_size() -> Option<(u16, u16)> {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: STDOUT_FILENO is a valid fd, and ws has the winsize layout that
    // TIOCGWINSZ requires
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    (rc == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
}

#[cfg(not(unix))]
fn terminal_size() -> Option<(u16, u16)> {
    None
}

/// The line a session opens with: what this is, which session it is, how much is
/// in it, and the model it is talking to.
///
/// Sized to the terminal it is going into. Whole segments are dropped from the
/// right when the line does not fit -- the rule the status line keeps to -- so
/// nothing is ever cut in the middle of a word: a summary that wraps reads as two
/// half-sentences, and on an 80-column terminal that is exactly what the line
/// used to do to the session's own path.
///
/// The path is not here at all. It is `~/.caocli/sessions/<id>.jsonl`, so it says
/// nothing the id does not, and it was the longest thing on the line; `--list`
/// prints it for anyone who wants the file itself.
pub(crate) fn banner(id: &str, messages: usize, model: &str) -> String {
    // No terminal, no width to fit: a file or a pipe can hold the whole line.
    banner_at(id, messages, model, available_columns())
}

/// The same, measured against a width given rather than one asked for, so that
/// what it does with an 80-column terminal can be a test.
fn banner_at(id: &str, messages: usize, model: &str, columns: Option<usize>) -> String {
    // The segments in the order they are dropped from the right: the hint first
    // (the box says it too), then the model (the status line has it), then the
    // count (the id already says which session this is). Each carries the
    // separator that introduces it, because the count is set off from the session
    // it counts rather than joined to it the way the top-level segments are.
    let parts: [(&str, String); 5] = [
        ("", "caocli".to_owned()),
        (" · ", format!("session {id}")),
        (
            " ",
            match messages {
                1 => "(1 message)".to_owned(),
                n => format!("({n} messages)"),
            },
        ),
        (" · ", model.to_owned()),
        (" · ", "/help for commands".to_owned()),
    ];
    let line = |keep: usize| {
        parts[..keep]
            .iter()
            .map(|(sep, text)| format!("{sep}{text}"))
            .collect::<String>()
    };
    // The longest run of segments that fits, and never fewer than the two that
    // say what this is and which session it is: a line too short even for those
    // is left whole for the front end to clip, rather than emptied here.
    let mut keep = parts.len();
    if let Some(columns) = columns {
        while keep > 2 && text::width(&line(keep)) > columns {
            keep -= 1;
        }
    }
    line(keep)
}

/// The columns a transcript line has to write in: the terminal's width, less the
/// columns every line that is not an answer is set in from the left edge.
fn available_columns() -> Option<usize> {
    terminal_size().map(|(_, cols)| (cols as usize).saturating_sub(cell::MARKER_COLUMNS))
}

/// Fixed status bar at the bottom: it occupies the terminal's last line and the
/// scroll region is restricted to 1..rows-1, so scrolling output cannot push the
/// bar off the screen.
/// Cost: lines that scroll out of the scroll region never reach the terminal's
/// scrollback buffer (history has to come from the session file).
/// Not enabled at all with `--no-status-bar` or when stdout is not a TTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StatusBar {
    rows: u16,
    cols: u16,
}

impl StatusBar {
    /// Enabled only with at least 3 rows: the status bar line + at least one line
    /// of output area + one row of slack.
    const MIN_ROWS: u16 = 3;

    /// Give up immediately when `tty=false` or TERM=dumb, without issuing ioctl.
    fn detect(tty: bool) -> Option<Self> {
        if !tty || std::env::var("TERM").is_ok_and(|t| t == "dumb") {
            return None;
        }
        Self::from_size(terminal_size())
    }

    fn from_size(size: Option<(u16, u16)>) -> Option<Self> {
        match size {
            Some((rows, cols)) if rows >= Self::MIN_ROWS && cols > 1 => Some(Self { rows, cols }),
            _ => None,
        }
    }

    /// Usable width: the last column is left free, so writing there cannot
    /// trigger automatic wrapping.
    fn width(&self) -> usize {
        self.cols as usize - 1
    }

    /// Set the scroll region → move the cursor to its bottom.
    fn setup(&self, out: &mut dyn Write) {
        let _ = write!(
            out,
            "\x1b[{};1H\x1b[2K\x1b[1;{}r\x1b[{};1H",
            self.rows,
            self.rows - 1,
            self.rows - 1
        );
        let _ = out.flush();
    }

    /// Reset the scroll region → clear the status bar line → newline, so the
    /// shell prompt lands on a clean line.
    fn teardown(&self, out: &mut dyn Write) {
        let _ = write!(out, "\x1b[r\x1b[{};1H\x1b[2K\r\n", self.rows);
        let _ = out.flush();
    }

    /// Right-aligned redraw: save the cursor → clear the line → write padding +
    /// text → restore the cursor.
    /// `visible` is used to compute the width (it carries no color codes), while
    /// `painted` is what actually gets written. The measurement is in columns,
    /// not chars, so a wide character is charged for both of its columns.
    fn render(&self, out: &mut dyn Write, visible: &str, painted: &str) {
        let pad = self.width().saturating_sub(text::width(visible));
        let _ = write!(
            out,
            "\x1b7\x1b[{};1H\x1b[2K{}{}\x1b8",
            self.rows,
            " ".repeat(pad),
            painted
        );
        let _ = out.flush();
    }
}

/// A text block being streamed.
///
/// Deltas are written as they arrive rather than buffered, so the block is never
/// held whole: only its kind has to be remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Reasoning,
    Content,
}

impl Block {
    /// The style the whole block is written in.
    ///
    /// The same styles the cells use, which is what keeps a streamed block and the
    /// cell replay builds out of the same text painted identically.
    fn style(self) -> Style {
        match self {
            Block::Reasoning => Style::Reasoning,
            Block::Content => Style::Plain,
        }
    }

    /// An empty cell of this kind, used to ask the spacing rule where the block
    /// belongs without having to duplicate the rule here.
    fn cell(self) -> Cell {
        match self {
            Block::Reasoning => Cell::Reasoning(String::new()),
            Block::Content => Cell::Content(String::new()),
        }
    }
}

/// One line of stdin, read on a blocking task: the terminal's own line editing is
/// what ends the line, and a runtime worker is not what should be waiting on it.
/// `None` is an end of input.
async fn read_answer_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(n) if n > 0 => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
            _ => None,
        }
    })
    .await
    .ok()
    .flatten()
}

/// Echo off for as long as this value lives, and back on when it is dropped —
/// including on the early returns, which is the whole reason it is a value and
/// not a pair of calls. A terminal that will not take the setting (stdin is a
/// pipe, or there is no terminal at all) leaves nothing to silence, and reading
/// it is not a failure.
#[cfg(unix)]
struct EchoOff {
    saved: libc::termios,
}

#[cfg(unix)]
impl EchoOff {
    fn new() -> Option<Self> {
        let fd = libc::STDIN_FILENO;
        // SAFETY: fd is a valid descriptor and `saved` has the termios layout
        // the call fills in; a failure leaves both untouched, and it is only
        // used when a touch is wanted.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            return None;
        }
        Some(Self { saved })
    }
}

#[cfg(unix)]
impl Drop for EchoOff {
    fn drop(&mut self) {
        // SAFETY: `saved` came from tcgetattr on this same descriptor.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved) };
    }
}

/// Where termios does not exist there is nothing to silence and nothing to
/// restore. The value still exists, so the asker stays one code on every
/// platform: the answer is read the same way, and what the terminal keeps
/// showing while it is typed is the price of a key on a machine without a
/// termios -- the same decline-and-still-work the plain front end accepts
/// everywhere else it cannot take the terminal's full cooperation.
#[cfg(not(unix))]
struct EchoOff;

#[cfg(not(unix))]
impl EchoOff {
    fn new() -> Option<Self> {
        Some(Self)
    }
}

/// Streaming renderer. Thinking and body text are two independent render blocks:
/// - thinking is dim, body text is the normal color
/// - blocks are separated by a newline; switching from thinking to body adds an
///   extra blank line
/// - with the NO_COLOR environment variable or when not a TTY no color codes are
///   emitted, but the block separation is kept
///
/// Everything the renderer writes is a [`Cell`]: the live notifications become
/// cells as they arrive and replayed history becomes cells up front, so both go
/// through the same painter and cannot drift apart. Only two things are carried
/// between calls: the block currently streaming, and whether the cell written
/// last was a text block (all the spacing rule needs).
pub struct Renderer {
    out: Box<dyn Write>,
    color: bool,
    /// The text block currently being streamed, if any.
    live: Option<Block>,
    /// Whether the cell written last was a text block. Only this much of the
    /// previous cell is kept: it is all the spacing rule needs, and the
    /// transcript itself lives in the session log.
    prev_was_block: bool,
    /// What the status bar reports: the model, the effort tier and the session's
    /// cache statistics.
    status: Status,
    bar: Option<StatusBar>,
}

impl Renderer {
    pub fn new() -> Self {
        let color = std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal();
        Self {
            out: Box::new(std::io::stdout()),
            color,
            live: None,
            prev_was_block: false,
            status: Status::default(),
            bar: None,
        }
    }

    #[cfg(test)]
    fn with_buffer(color: bool) -> (Self, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
        let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let r = Self {
            out: Box::new(tests::SharedBuf(buf.clone())),
            color,
            live: None,
            prev_was_block: false,
            status: Status::default(),
            bar: None,
        };
        (r, buf)
    }

    /// Sync the status bar against the current terminal size: called when the REPL
    /// starts and before each input, which also handles window resizes (if the
    /// size changed, the bar is torn down and rebuilt).
    pub fn refresh_status_bar(&mut self) {
        let current = StatusBar::detect(std::io::stdout().is_terminal());
        self.apply_status_bar(current);
    }

    fn apply_status_bar(&mut self, current: Option<StatusBar>) {
        match (self.bar, current) {
            (None, None) => {}
            (None, Some(bar)) => {
                bar.setup(self.out.as_mut());
                self.bar = Some(bar);
                self.redraw_status_bar();
            }
            (Some(old), None) => {
                old.teardown(self.out.as_mut());
                self.bar = None;
            }
            (Some(old), Some(bar)) if old == bar => self.redraw_status_bar(),
            (Some(old), Some(bar)) => {
                old.teardown(self.out.as_mut());
                bar.setup(self.out.as_mut());
                self.bar = Some(bar);
                self.redraw_status_bar();
            }
        }
    }

    /// Redraw the status bar (without re-detecting the size).
    ///
    /// Picks the richest segment combination that fits, so a narrow terminal
    /// loses detail by whole segments instead of having a number cut in half.
    fn redraw_status_bar(&mut self) {
        let Some(bar) = self.bar else { return };
        let width = bar.width();
        // The status picks the richest segment combination that fits: a narrow
        // bar loses detail by whole segments rather than by clipping, and only
        // an over-long shortest segment is clipped below.
        let label = self.status.line(width);
        let visible = text::truncate(&label, width);
        let painted = self.paint(Style::Dim, visible);
        bar.render(self.out.as_mut(), visible, &painted);
    }

    /// Set the model id shown in the status bar (called when a session is created
    /// or switched, since it follows the session meta).
    /// Cache statistics accumulated for the current session (the status bar's data
    /// source). Read by tests only.
    #[cfg(test)]
    pub fn stats(&self) -> status::CacheStats {
        self.status.stats()
    }

    /// Restore the terminal before exiting (reset the scroll region, clear the
    /// status bar line). Idempotent.
    pub fn teardown(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.teardown(self.out.as_mut());
        }
    }

    fn raw(&mut self, s: &str) {
        let _ = self.out.write_all(s.as_bytes());
        let _ = self.out.flush();
    }

    /// The escape that opens `style`, empty when colors are off.
    fn open_style(&self, style: Style) -> &'static str {
        if self.color { style.code() } else { "" }
    }

    /// The escape that closes a style, empty when colors are off.
    fn close_style(&self) -> &'static str {
        if self.color { RESET } else { "" }
    }

    /// Wrap `s` in `style`. Styles are applied per span, so no call site has to
    /// know whether colors are enabled.
    fn paint(&self, style: Style, s: &str) -> String {
        format!("{}{s}{}", self.open_style(style), self.close_style())
    }

    /// Paint a run of spans: one style code per run rather than one per span, and the
    /// columns the cell continues in after every line break inside it.
    ///
    /// A `\n` in a cell is a line of that cell, not a line the terminal wrapped, so it
    /// is set in like every other line of it. For this front end that means writing
    /// the continuation columns itself -- without them a change's lines would come out
    /// two columns left of the call they belong to, in the column the answers are in.
    /// A line the terminal wraps because it is longer than the screen is the one thing
    /// neither front end can set in: only the terminal knows where it breaks.
    ///
    /// The opened style runs across the break and over the columns, so the
    /// continuation is painted in the style of the line it continues rather than in the
    /// gutter's own. The two differ only for a cell whose marker is not blank, and for
    /// that one they are the same style by construction.
    fn paint_spans(&self, spans: &[Span], rest: &str) -> String {
        let mut out = String::new();
        let mut open: Option<Style> = None;
        for span in spans {
            if open != Some(span.style) {
                if open.is_some() {
                    out.push_str(self.close_style());
                }
                out.push_str(self.open_style(span.style));
                open = Some(span.style);
            }
            for (i, piece) in span.text.split('\n').enumerate() {
                if i > 0 {
                    out.push('\n');
                    out.push_str(rest);
                }
                out.push_str(piece);
            }
        }
        if open.is_some() {
            out.push_str(self.close_style());
        }
        out
    }

    /// Write a cell: the blank line separating it from the previous one, its marker
    /// in its gutter, its spans, then its line ending.
    fn paint_cell(&mut self, cell: &Cell) {
        if cell.gap_after(self.prev_was_block) {
            self.raw("\n");
        }
        // The marker first, then the cell's own spans: a cell is set in past its
        // gutter, and this front end is the one that writes those columns itself --
        // the front end that owns the screen lays the cell out, and would otherwise
        // write the marker twice.
        let gutter = cell.gutter();
        let led = gutter.map(|g| Span::new(g.style, g.head));
        let spans: Vec<Span> = led.into_iter().chain(cell.spans()).collect();
        let painted = self.paint_spans(&spans, gutter.map_or("", |g| g.rest));
        self.raw(&painted);
        if cell.ends_line() {
            self.raw("\n");
        }
        self.prev_was_block = cell.is_text_block();
    }

    /// Start streaming a text block: write the separating blank line and open the
    /// block's style, so the deltas that follow inherit it. A no-op when the block
    /// is already open, which is what keeps a run of deltas to a single style run.
    ///
    /// The block's marker goes on after its style is open, and not before: the marker
    /// belongs to the block -- a gutter's style is the style of the cell it opens --
    /// so inheriting it here is what keeps a streamed block to one run, and what
    /// makes what is streamed and what is replayed the same bytes.
    ///
    /// Only the first line can be set in here. This front end writes a line as it
    /// arrives and leaves the wrapping to the terminal, so the columns of a line it
    /// never sees are not its to choose; the front end that owns the screen lays the
    /// whole block out and sets in every line of it.
    fn open_block(&mut self, block: Block) {
        if self.live == Some(block) {
            return;
        }
        self.close_block();
        if block.cell().gap_after(self.prev_was_block) {
            self.raw("\n");
        }
        self.raw(self.open_style(block.style()));
        if let Some(gutter) = block.cell().gutter() {
            self.raw(gutter.head);
        }
        self.live = Some(block);
    }

    /// End the streaming block: close its style and its line.
    fn close_block(&mut self) {
        if self.live.take().is_none() {
            return;
        }
        self.raw(self.close_style());
        self.raw("\n");
        self.prev_was_block = true;
    }

    /// Replay history messages (used when resuming a session).
    ///
    /// The messages become the same cells a live turn produces and go through
    /// the same painter, so a resumed session is laid out exactly like the one
    /// that was watched live -- including the tool summaries, which are a
    /// property of the cell and so cannot be forgotten here.
    fn replay_messages(&mut self, messages: &[Message]) {
        for cell in cell::from_messages(messages) {
            self.paint_cell(&cell);
        }
        // Separate the replayed history from the prompt that follows it.
        self.raw("\n");
    }
}

impl Front for Renderer {
    fn replay(&mut self, messages: &[Message]) {
        self.replay_messages(messages);
    }

    fn info(&mut self, s: &str) {
        self.paint_cell(&Cell::Notice(s.to_owned()));
    }

    fn error(&mut self, s: &str) {
        // Straight to the error stream: the plain front end does not own the
        // screen, so an error survives a redirected stdout.
        eprintln!("{}", self.paint(Style::Red, s));
    }

    fn set_model(&mut self, model: &str) {
        self.status.set_model(model);
        self.redraw_status_bar();
    }

    fn set_effort(&mut self, effort: &str) {
        self.status.set_effort(effort);
        self.redraw_status_bar();
    }

    fn reset_stats(&mut self) {
        self.status.reset_stats();
        self.redraw_status_bar();
    }

    fn ask_secret(&mut self, prompt: &str) -> Pin<Box<dyn Future<Output = Option<String>> + '_>> {
        // The echo goes off before the prompt does, not after: the answer can
        // arrive the moment the question is on the screen, and the only copy of
        // it that may exist is the one this returns. The prompt itself is
        // written now rather than awaited -- an answer nobody knows is being
        // asked for is not an answer -- and the read waits in the future, where
        // the caller awaits it.
        let echo = EchoOff::new();
        let _ = writeln!(self.out, "{}", self.paint(Style::Dim, prompt));
        let _ = self.out.flush();
        Box::pin(async move {
            let answer = read_answer_line().await;
            drop(echo);
            answer
        })
    }
}

impl Ui for Renderer {
    fn reasoning_delta(&mut self, s: &str) {
        self.open_block(Block::Reasoning);
        self.raw(s);
    }

    fn content_delta(&mut self, s: &str) {
        self.open_block(Block::Content);
        self.raw(s);
    }

    fn finish_turn(&mut self) {
        self.close_block();
    }

    fn tool_start(&mut self, name: &str, args: &str) {
        self.paint_cell(&Cell::tool_call(name, args));
    }

    fn tool_result(&mut self, result: &str) {
        self.paint_cell(&Cell::ToolResult(result.to_owned()));
    }

    fn interrupted(&mut self) {
        // A block that was still streaming when the turn was cancelled is closed
        // first, so the notice starts on a line of its own.
        self.close_block();
        self.paint_cell(&Cell::Interrupted);
    }

    fn approval_requested(&mut self, name: &str, args: &str) {
        self.paint_cell(&Cell::approval(name, args));
    }

    fn usage(&mut self, u: &Usage, stream: Duration) {
        self.paint_cell(&Cell::Usage {
            usage: u.clone(),
            stream,
        });
        self.status.record(u);
        self.redraw_status_bar();
    }
}

#[cfg(test)]
mod tests;

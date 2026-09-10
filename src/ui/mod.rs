mod cell;
mod status;
pub(crate) mod text;
pub mod tui;

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

/// The machine → UI notification vocabulary (the Notice channel).
/// Discipline: notifications only, never returning data back; implementations
/// must not block, and a failed terminal write counts as fatal.
/// The machine (agent) depends only on this trait, not on a concrete renderer;
/// the only in-process implementation is [`Renderer`].
pub trait Ui {
    /// A thinking fragment (arrives before the body text).
    fn reasoning_delta(&mut self, s: &str);
    /// A body-text fragment.
    fn content_delta(&mut self, s: &str);
    /// One streaming round is over; blocks are reset.
    fn finish_turn(&mut self);
    /// A tool is starting: echo the tool name and an argument summary.
    fn tool_start(&mut self, name: &str, args: &str);
    /// A tool result summary.
    fn tool_result(&mut self, result: &str);
    /// Token usage for a sub-request, with the wall time the stream took (also
    /// accumulates session-level cache stats). The duration is what a
    /// tokens-per-second figure is computed from; a front end that shows none
    /// ignores it.
    fn usage(&mut self, u: &Usage, stream: Duration);
    /// The turn was cancelled by the user (Ctrl-C): close the streaming block and
    /// print an interruption notice.
    fn interrupted(&mut self);
    /// Approval gate question: echo the tool and an argument summary and prompt
    /// y/N (the answer is read through the Input channel, not through this trait —
    /// a Notice never returns data).
    fn approval_requested(&mut self, name: &str, args: &str);
}

/// What a front end offers the *application*, as opposed to [`Ui`], which is what
/// the *machine* sends.
///
/// The two are kept apart on purpose: `Ui` is the machine's notification
/// vocabulary and stays that, while this is the handful of operations the
/// submitted-line handling needs from whichever front end is running -- the
/// plain renderer, or the interactive one that owns the terminal.
pub trait Front: Ui {
    /// Draw a resumed session's history.
    fn replay(&mut self, messages: &[Message]);
    /// A dim informational line.
    fn info(&mut self, s: &str);
    /// A failure. The plain front end writes it to the error stream so it
    /// survives a redirected stdout; an interactive one has to place it in its
    /// own output, which is why this takes `&mut self`.
    fn error(&mut self, s: &str);
    /// Adopt the model id the status line reports.
    fn set_model(&mut self, model: &str);
    /// Adopt the reasoning effort tier the status line reports.
    fn set_effort(&mut self, effort: &str);
    /// Clear the session's cache statistics.
    fn reset_stats(&mut self);
    /// Ask for a secret: `prompt` says what it is for, and the answer is the
    /// text typed in reply. `None` means nothing was entered — a cancellation,
    /// an end of input, or a front end that has gone away.
    ///
    /// A question rather than a line to submit, because an answer that reached
    /// the session log or the transcript would no longer be a secret. The
    /// interactive front end draws the prompt and takes the answer in its box
    /// with the text hidden; the plain one writes the prompt and reads a line
    /// from its own input with the terminal's echo off.
    fn ask_secret(&mut self, prompt: &str) -> Pin<Box<dyn Future<Output = Option<String>> + '_>>;
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
            out: Box::new(SharedBuf(buf.clone())),
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
struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

#[cfg(test)]
impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Role;
    use status::CacheStats;

    /// The whole line, as an 80-column terminal would show it: the terminal it is
    /// most likely to be read on is the one the old line broke in half.
    #[test]
    fn the_banner_fits_the_terminal_it_is_going_into() {
        let full = "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands";
        assert_eq!(
            banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", None),
            full
        );
        assert_eq!(
            banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(100)),
            full,
            "a wide terminal keeps the hint"
        );
        // 80 columns: the hint is the first thing to go -- the box repeats it.
        assert_eq!(
            banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(80)),
            "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro"
        );
        // Then the model, which the status line has anyway.
        assert_eq!(
            banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(60)),
            "caocli · session 20260910-213122 (12 messages)"
        );
        // Then the count. Which session this is is the one thing nothing else
        // says, so the id is the last of the optional ones to go -- and what is
        // left is what fits a 40-column terminal whole.
        assert_eq!(
            banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(40)),
            "caocli · session 20260910-213122"
        );
        // Below that, the line comes back whole to be clipped by whoever writes
        // it: an over-long line reads better than a line with nothing on it.
        assert_eq!(
            banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(12)),
            "caocli · session 20260910-213122"
        );
    }

    #[test]
    fn the_banner_counts_one_message_in_the_singular() {
        let one = banner_at("20260910-213122", 1, "m", Some(80));
        assert!(one.contains("(1 message)"), "{one:?}");
        assert!(banner_at("20260910-213122", 2, "m", Some(80)).contains("(2 messages)"));
        assert!(banner_at("20260910-213122", 0, "m", Some(80)).contains("(0 messages)"));
    }

    #[test]
    fn the_banner_is_never_cut_mid_word() {
        // Every width, not just the round ones: what comes back is always one of
        // the segment joins -- never a piece of one -- so a word is never cut in
        // half however narrow the terminal is.
        let joins = [
            "caocli · session 20260910-213122",
            "caocli · session 20260910-213122 (12 messages)",
            "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro",
            "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands",
        ];
        for columns in 1..=120 {
            let line = banner_at(
                "20260910-213122",
                12,
                "deepseek/deepseek-v4-pro",
                Some(columns),
            );
            assert!(joins.contains(&line.as_str()), "{columns}: {line:?}");
            if text::width(&line) > columns {
                // Only the shortest form may overflow, and only to be clipped by
                // whoever writes it: an over-long line reads better than none.
                assert_eq!(line, joins[0], "{columns}: {line:?}");
            }
        }
    }

    #[test]
    fn the_banner_measures_wide_characters_by_column() {
        // A CJK model id is two columns per glyph, so a budget counted in chars
        // would let through a segment that does not fit: the ideographs are 8
        // columns and only 4 characters.
        let cjk = "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}";
        let with_model = "caocli · session id (1 message) · \u{6df1}\u{5ea6}\u{6c42}\u{7d22}";
        assert_eq!(text::width(with_model), 42);
        assert_eq!(banner_at("id", 1, cjk, Some(42)), with_model);
        assert_eq!(
            banner_at("id", 1, cjk, Some(41)),
            "caocli · session id (1 message)",
            "one column short of the model: the model goes whole"
        );
    }

    #[test]
    fn the_banner_keeps_everything_when_there_is_no_terminal() {
        // Output redirected to a file or a pipe: there is no width to fit, and a
        // file can hold the whole line. (A test's own stdout is a pipe, so this is
        // also the case the width is really asked for in.)
        assert_eq!(
            banner("20260910-213122", 12, "deepseek/deepseek-v4-pro"),
            "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands"
        );
    }

    #[test]
    fn reasoning_then_content_are_separate_blocks() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("thinking...");
        r.content_delta("answer");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\x1b[2m┆ thinking...\x1b[0m\n\nanswer\x1b[0m\n"
        );
    }

    #[test]
    fn content_then_reasoning_second_subturn_separated() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.content_delta("partial");
        r.reasoning_delta("more thinking");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "partial\x1b[0m\n\n\x1b[2m┆ more thinking\x1b[0m\n"
        );
    }

    #[test]
    fn no_color_still_separates_blocks() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.reasoning_delta("thought");
        r.content_delta("text");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "┆ thought\n\ntext\n"
        );
    }

    #[test]
    fn content_only_has_no_leading_separator() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.content_delta("hi");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "hi\x1b[0m\n"
        );
    }

    #[test]
    fn reasoning_only_block_closes_cleanly() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("hmm");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\x1b[2m┆ hmm\x1b[0m\n"
        );
    }

    #[test]
    fn same_mode_deltas_do_not_reopen_block() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("a");
        r.reasoning_delta("b"); // still Reasoning: no repeated color code
        r.content_delta("x");
        r.content_delta("y"); // still Content: no new block
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\x1b[2m┆ ab\x1b[0m\n\nxy\x1b[0m\n"
        );
    }

    #[test]
    fn tool_start_extracts_command_hint() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.tool_start("Bash", r#"{"command":"ls -la"}"#);
        r.tool_start("Read", r#"{"file_path":"/a/b.txt"}"#);
        r.tool_start("Write", "not json at all");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("▸ Bash ls -la"), "{s}");
        assert!(s.contains("▸ Read /a/b.txt"), "{s}");
        assert!(s.contains("▸ Write not json at all"), "{s}"); // bad JSON falls back to raw text
    }

    #[test]
    fn replay_renders_history_compactly_with_colors() {
        use crate::types::{ToolCall, ToolCallFunction};
        let (mut r, buf) = Renderer::with_buffer(true);
        r.replay(&[
            Message::user("take a look"),
            Message {
                role: Role::Assistant,
                content: Some("running it".into()),
                reasoning_content: Some("let me think".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: "Bash".into(),
                        arguments: r#"{"command":"ls -la"}"#.into(),
                    },
                }]),
                tool_call_id: None,
            },
            Message::tool("call_1", "exit_code: 0\n--- stdout ---\nSECRET_BODY"),
            Message::system("must not appear"),
        ]);
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("\x1b[2m› \x1b[0mtake a look"), "{s}");
        assert!(
            s.contains("\x1b[2m┆ let me think\x1b[0m"),
            "thinking is set in behind its own rule: {s}"
        );
        assert!(s.contains("running it"), "{s}");
        assert!(s.contains("▸ Bash ls -la"), "tool calls are yellow: {s}");
        // tool messages only get a summary, never the full text
        assert!(s.contains("exit_code: 0 · 39 bytes"), "{s}");
        assert!(
            !s.contains("SECRET_BODY"),
            "tool output must not be replayed: {s}"
        );
        assert!(
            !s.contains("must not appear"),
            "system messages are not replayed: {s}"
        );
    }

    #[test]
    fn replay_empty_history_emits_only_blank_line() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.replay(&[]);
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\n"
        );
    }

    #[test]
    fn replay_skips_empty_assistant_fields() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.replay(&[Message {
            role: Role::Assistant,
            content: Some(String::new()),
            reasoning_content: Some(String::new()),
            tool_calls: None,
            tool_call_id: None,
        }]);
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\n"
        );
    }

    /// The point of the cell model: a turn is laid out identically whether it is
    /// watched live or replayed from the log, because both paths paint the same
    /// cells.
    ///
    /// Before the cells existed, `replay` carried its own copy of the layout
    /// rules and silently disagreed with the live renderer: live put a blank line
    /// between thinking and the answer, replay did not.
    #[test]
    fn live_and_replay_lay_out_a_turn_identically() {
        use crate::types::{Message, ToolCall, ToolCallFunction};

        // The order the agent drives the renderer in: a streaming round first,
        // then the tool calls it asked for.
        let (mut live, live_buf) = Renderer::with_buffer(true);
        live.reasoning_delta("let me think");
        live.content_delta("running it");
        live.finish_turn();
        live.tool_start("Bash", r#"{"command":"ls -la"}"#);
        live.tool_result("exit_code: 0\n--- stdout ---\nBODY");

        let (mut replayed, replay_buf) = Renderer::with_buffer(true);
        replayed.replay(&[
            Message {
                role: Role::Assistant,
                content: Some("running it".into()),
                reasoning_content: Some("let me think".into()),
                tool_calls: Some(vec![ToolCall {
                    id: "call_1".into(),
                    r#type: "function".into(),
                    function: ToolCallFunction {
                        name: "Bash".into(),
                        arguments: r#"{"command":"ls -la"}"#.into(),
                    },
                }]),
                tool_call_id: None,
            },
            Message::tool("call_1", "exit_code: 0\n--- stdout ---\nBODY"),
        ]);

        // Replay additionally separates the history from the prompt after it.
        assert_eq!(buf_of(&replay_buf), format!("{}\n", buf_of(&live_buf)));
    }

    #[test]
    fn interrupted_closes_block_and_prints_notice() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("thinking");
        r.interrupted();
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("interrupted"), "{s}");
        assert!(s.ends_with("\x1b[0m\n"), "color reset closes the line: {s}");
    }

    #[test]
    fn approval_requested_asks_without_newline() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.approval_requested("Bash", r#"{"command":"rm -rf /"}"#);
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("▸ Bash rm -rf /"), "{s}");
        assert!(
            s.ends_with("[y/N] "),
            "ends on the y/N prompt without a newline: {s:?}"
        );
    }

    #[test]
    fn tool_result_shows_exit_line_and_bytes() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.tool_result("exit_code: 3\n--- stdout ---\nhello");
        r.tool_result("");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("exit_code: 3 · 33 bytes"), "{s}");
        assert!(s.contains(" · 0 bytes"), "{s}"); // empty result
    }

    #[test]
    fn usage_and_info_render_dims() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.usage(
            &Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_cache_hit_tokens: 6,
                prompt_cache_miss_tokens: 4,
                ..Default::default()
            },
            Duration::ZERO,
        );
        r.info("session abc");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert!(s.contains("tokens: in 10/15 (hit 6/miss 4) · out 5"), "{s}");
        assert!(s.contains("session abc"), "{s}");
    }

    #[test]
    fn color_variants_render_codes_and_plain() {
        // color=true: info/usage take paint's colored branch; error goes red
        let (mut r, buf) = Renderer::with_buffer(true);
        r.info("ok");
        let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        assert_eq!(s, "\x1b[2m  ok\x1b[0m\n");
        r.error("boom"); // eprintln, does not write to buf; only checks it does not
        // panic and that the red paint call is covered
    }

    #[test]
    fn no_color_paint_returns_plain_text() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.info("plain");
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "  plain\n"
        );
    }

    /// A block watched live and the same block replayed after a resume come out as
    /// the same bytes.
    ///
    /// This is what the marker belonging to the cell buys. A streamed block writes it
    /// through the block's own open style; a replayed cell paints it as the gutter of
    /// the cell it opens. The two agree because a gutter's style is the style of the
    /// cell it sets in -- and they would stop agreeing the day one of them drifted,
    /// which is a comparison of escape sequences that no reader makes by eye.
    #[test]
    fn a_streamed_block_and_the_same_block_replayed_are_the_same_bytes() {
        let (mut live, live_buf) = Renderer::with_buffer(true);
        live.reasoning_delta("thinking");
        live.content_delta("answer");
        live.finish_turn();

        let (mut replayed, replay_buf) = Renderer::with_buffer(true);
        replayed.replay(&[Message {
            role: Role::Assistant,
            content: Some("answer".into()),
            reasoning_content: Some("thinking".into()),
            tool_calls: None,
            tool_call_id: None,
        }]);

        // The replay's own trailing separator is the one thing that is not the same:
        // it is what sets the history off from the prompt that follows it.
        assert_eq!(buf_of(&live_buf).trim_end(), buf_of(&replay_buf).trim_end());
    }

    /// A line break inside a cell is a line of that cell, not a line the terminal
    /// wrapped, so it is set in with the rest of them.
    ///
    /// The two are easy to confuse and they are not the same thing: what the terminal
    /// wraps is beyond either front end's reach, and what a cell breaks itself cannot be
    /// left out without a change's lines landing in the column the answers are in.
    #[test]
    fn a_cells_own_line_breaks_are_set_in_too() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.tool_start(
            "Edit",
            r#"{"file_path":"a.txt","old_string":"one","new_string":"two"}"#,
        );
        let drawn = buf_of(&buf);
        assert!(
            drawn.contains("▸ Edit a.txt\n  - one\n  + two"),
            "a change is set in under its call: {drawn:?}"
        );

        let (mut r, buf) = Renderer::with_buffer(false);
        r.replay(&[Message::user("alpha\nbeta")]);
        let drawn = buf_of(&buf);
        assert!(
            drawn.contains("› alpha\n  beta"),
            "a draft that was sent with a line in it reads back the same way: {drawn:?}"
        );
    }

    fn usage_fixture(hit: u64, miss: u64) -> Usage {
        Usage {
            prompt_tokens: hit + miss,
            completion_tokens: 0,
            total_tokens: hit + miss,
            prompt_cache_hit_tokens: hit,
            prompt_cache_miss_tokens: miss,
            ..Default::default()
        }
    }

    fn buf_of(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> String {
        String::from_utf8(buf.lock().unwrap().clone()).unwrap()
    }

    #[test]
    fn status_bar_from_size_guards() {
        assert_eq!(StatusBar::from_size(None), None);
        assert_eq!(StatusBar::from_size(Some((2, 80))), None); // too few rows
        assert_eq!(StatusBar::from_size(Some((24, 1))), None); // too narrow
        assert_eq!(
            StatusBar::from_size(Some((24, 80))),
            Some(StatusBar { rows: 24, cols: 80 })
        );
    }

    #[test]
    fn status_bar_detect_rejects_non_tty() {
        assert_eq!(StatusBar::detect(false), None);
    }

    /// Regression: the bar used to measure its label in chars, so every wide
    /// character was undercharged by one column and the label spilled past the
    /// column the bar deliberately leaves free (which is what keeps the write
    /// from triggering autowrap on the last row).
    #[test]
    fn status_bar_render_charges_wide_chars_two_columns() {
        let bar = StatusBar { rows: 10, cols: 20 }; // width = 19
        let (mut r, buf) = Renderer::with_buffer(false);
        // two ideographs + space + rocket = 7 columns but only 4 chars
        let label = "\u{6df1}\u{5ea6} \u{1f680}";
        assert_eq!(label.chars().count(), 4);
        assert_eq!(text::width(label), 7);
        let visible = text::truncate(label, bar.width());
        bar.render(r.out.as_mut(), visible, visible);
        let s = buf_of(&buf);
        // 19 - 7 = 12 columns of padding. Counting chars would have written 15
        // and pushed the label 3 columns past the right edge.
        assert!(
            s.contains(&format!("\x1b[2K{}{label}\x1b8", " ".repeat(12))),
            "{s:?}"
        );
    }

    #[test]
    fn status_bar_setup_teardown_sequences() {
        let bar = StatusBar { rows: 10, cols: 40 };
        let (mut r, buf) = Renderer::with_buffer(false);
        bar.setup(r.out.as_mut());
        bar.teardown(r.out.as_mut());
        let s = buf_of(&buf);
        assert!(s.contains("\x1b[10;1H\x1b[2K\x1b[1;9r\x1b[9;1H"), "{s:?}");
        assert!(s.contains("\x1b[r\x1b[10;1H\x1b[2K\r\n"), "{s:?}");
    }

    #[test]
    fn status_bar_render_right_aligns_and_paints() {
        let bar = StatusBar { rows: 10, cols: 40 }; // width = 39
        let (mut r, buf) = Renderer::with_buffer(true);
        let label = "cache 98.6% · hit 32384 · miss 461"; // 34 characters
        let painted = r.paint(Style::Dim, label);
        bar.render(r.out.as_mut(), label, &painted);
        let s = buf_of(&buf);
        // 39 - 34 = 5 spaces of left padding, right aligned
        assert!(
            s.contains(&format!(
                "\x1b7\x1b[10;1H\x1b[2K     \x1b[2m{label}\x1b[0m\x1b8"
            )),
            "{s:?}"
        );
    }

    #[test]
    fn apply_status_bar_transitions() {
        let a = StatusBar { rows: 10, cols: 40 };
        let b = StatusBar { rows: 12, cols: 50 };

        // (None, None): no output
        let (mut r, buf) = Renderer::with_buffer(false);
        r.apply_status_bar(None);
        assert!(buf_of(&buf).is_empty());

        // (None, Some): create the bar + first draw
        r.apply_status_bar(Some(a));
        let s = buf_of(&buf);
        assert!(s.contains("\x1b[1;9r"), "{s:?}");
        assert!(s.contains("cache 0.0% · hit 0 · miss 0"), "{s:?}");
        assert_eq!(r.bar, Some(a));

        // (Some, Some(same)): redraw only
        let before = buf_of(&buf).len();
        r.apply_status_bar(Some(a));
        assert!(buf_of(&buf)[before..].contains("\x1b[10;1H\x1b[2K"));

        // (Some, Some(different)): tear down the old one, build the new one
        let before = buf_of(&buf).len();
        r.apply_status_bar(Some(b));
        let tail = &buf_of(&buf)[before..];
        assert!(tail.contains("\x1b[r"), "{tail:?}");
        assert!(tail.contains("\x1b[1;11r"), "{tail:?}");
        assert_eq!(r.bar, Some(b));

        // (Some, None): tear the bar down
        let before = buf_of(&buf).len();
        r.apply_status_bar(None);
        assert!(buf_of(&buf)[before..].contains("\x1b[r"));
        assert_eq!(r.bar, None);
    }

    #[test]
    fn usage_paints_session_cache_bar_and_reset_clears_it() {
        let bar = StatusBar { rows: 10, cols: 60 };
        let (mut r, buf) = Renderer::with_buffer(false);
        r.apply_status_bar(Some(bar));
        r.usage(&usage_fixture(6, 4), Duration::ZERO);
        r.usage(&usage_fixture(12, 8), Duration::ZERO);
        let s = buf_of(&buf);
        assert!(
            s.contains("tokens: in 10/10 (hit 6/miss 4) · out 0"),
            "{s:?}"
        );
        // session accumulation: 18 hit / 12 miss = 60.0%
        assert!(s.contains("cache 60.0% · hit 18 · miss 12"), "{s:?}");

        r.reset_stats();
        let tail = &buf_of(&buf)[s.len()..];
        assert!(tail.contains("cache 0.0% · hit 0 · miss 0"), "{tail:?}");
        assert_eq!(r.stats(), CacheStats::default());
    }

    /// The status bar line the renderer draws at the given terminal width, with
    /// a model and cache statistics already in place.
    fn bar_line(cols: u16, model: &str) -> String {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.set_model(model);
        r.usage(&usage_fixture(6, 4), Duration::ZERO);
        // The bar is attached last, so the redraw it triggers is the first one
        // that has anything to draw.
        r.apply_status_bar(Some(StatusBar { rows: 10, cols }));
        buf_of(&buf)
    }

    /// Assert that the bar's content is exactly `label`: clipping it would
    /// break the `\x1b8` cursor restore that immediately follows.
    fn assert_bar_exactly(cols: u16, model: &str, label: &str) {
        let s = bar_line(cols, model);
        assert!(
            s.contains(&format!("{label}\x1b8")),
            "cols={cols} expected {label:?} in {s:?}"
        );
    }

    /// Progressive disclosure: a bar too narrow for everything drops whole
    /// segments from the end instead of cutting a number in half.
    #[test]
    fn status_bar_drops_whole_segments_on_narrow_terminals() {
        let model = "deepseek-v4-flash";
        // 79 columns: everything fits
        assert_bar_exactly(
            80,
            model,
            "deepseek-v4-flash · cache 60.0% · hit 6 · miss 4",
        );
        // 34 columns: the counts go, the model and rate stay
        assert_bar_exactly(35, model, "deepseek-v4-flash · cache 60.0%");
        // 19 columns: only the model is left
        assert_bar_exactly(20, model, "deepseek-v4-flash");
    }

    /// When not even the shortest combination fits, it is clipped rather than
    /// leaving the bar blank.
    #[test]
    fn status_bar_clips_the_shortest_segment_as_a_last_resort() {
        assert_bar_exactly(10, "deepseek-v4-flash", "deepseek-");
    }

    /// Variants are chosen by display width, not by char count: at 21 columns
    /// the rate segment is 22 columns wide (18 chars), so it has to be dropped.
    #[test]
    fn status_bar_chooses_variants_by_display_width() {
        let wide_model = "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}"; // 8 columns, 4 chars
        assert_bar_exactly(
            25,
            wide_model,
            "\u{6df1}\u{5ea6}\u{6c42}\u{7d22} · cache 60.0%",
        );
        assert_bar_exactly(21, wide_model, wide_model);
    }

    #[test]
    fn status_bar_shows_model_and_updates_on_switch() {
        let bar = StatusBar { rows: 10, cols: 80 };
        let (mut r, buf) = Renderer::with_buffer(false);
        r.apply_status_bar(Some(bar));
        r.set_model("deepseek-v4-flash");
        r.usage(&usage_fixture(6, 4), Duration::ZERO);
        let s = buf_of(&buf);
        assert!(
            s.contains("deepseek-v4-flash · cache 60.0% · hit 6 · miss 4"),
            "{s:?}"
        );

        // switching models: after the redraw only the new model is left
        r.set_model("deepseek-v4-pro");
        let tail = &buf_of(&buf)[s.len()..];
        assert!(tail.contains("deepseek-v4-pro · cache 60.0%"), "{tail:?}");
        assert!(!tail.contains("deepseek-v4-flash"), "{tail:?}");

        // reset_stats clears only the cache stats and keeps the model
        let before = buf_of(&buf).len();
        r.reset_stats();
        let tail = &buf_of(&buf)[before..];
        assert!(
            tail.contains("deepseek-v4-pro · cache 0.0% · hit 0 · miss 0"),
            "{tail:?}"
        );
    }

    #[test]
    fn refresh_status_bar_without_tty_is_noop_and_teardown_idempotent() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.refresh_status_bar(); // cargo test's stdout is a pipe → not enabled
        assert!(buf_of(&buf).is_empty());
        r.teardown(); // idempotent when not enabled
        assert!(buf_of(&buf).is_empty());
    }
}

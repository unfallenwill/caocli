mod cell;
mod status;
pub(crate) mod text;
pub mod tui;

use std::future::Future;
use std::io::{IsTerminal, Write};
use std::pin::Pin;

use crate::types::{Message, Usage};

use cell::{Cell, Style};
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
    /// Token usage for a sub-request (also accumulates session-level cache stats).
    fn usage(&mut self, u: &Usage);
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
    /// What the status bar reports: the model and the session's cache statistics.
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

    /// Write a cell: the blank line separating it from the previous one, its
    /// spans, then its line ending.
    fn paint_cell(&mut self, cell: &Cell) {
        if cell.gap_after(self.prev_was_block) {
            self.raw("\n");
        }
        let painted: String = cell
            .spans()
            .iter()
            .map(|s| self.paint(s.style, &s.text))
            .collect();
        self.raw(&painted);
        if cell.ends_line() {
            self.raw("\n");
        }
        self.prev_was_block = cell.is_text_block();
    }

    /// Start streaming a text block: write the separating blank line and open the
    /// block's style, so the deltas that follow inherit it. A no-op when the
    /// block is already open, which is what keeps a run of deltas to a single
    /// style run.
    fn open_block(&mut self, block: Block) {
        if self.live == Some(block) {
            return;
        }
        self.close_block();
        if block.cell().gap_after(self.prev_was_block) {
            self.raw("\n");
        }
        self.raw(self.open_style(block.style()));
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

    fn usage(&mut self, u: &Usage) {
        self.paint_cell(&Cell::Usage(u.clone()));
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

    #[test]
    fn reasoning_then_content_are_separate_blocks() {
        let (mut r, buf) = Renderer::with_buffer(true);
        r.reasoning_delta("thinking...");
        r.content_delta("answer");
        r.finish_turn();
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "\x1b[38;5;245;48;5;236mthinking...\x1b[0m\n\nanswer\x1b[0m\n"
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
            "partial\x1b[0m\n\n\x1b[38;5;245;48;5;236mmore thinking\x1b[0m\n"
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
            "thought\n\ntext\n"
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
            "\x1b[38;5;245;48;5;236mhmm\x1b[0m\n"
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
            "\x1b[38;5;245;48;5;236mab\x1b[0m\n\nxy\x1b[0m\n"
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
            s.contains("\x1b[38;5;245;48;5;236mlet me think\x1b[0m"),
            "thinking has a ground of its own: {s}"
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
        r.usage(&Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 4,
            ..Default::default()
        });
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
        assert_eq!(s, "\x1b[2mok\x1b[0m\n");
        r.error("boom"); // eprintln, does not write to buf; only checks it does not
        // panic and that the red paint call is covered
    }

    #[test]
    fn no_color_paint_returns_plain_text() {
        let (mut r, buf) = Renderer::with_buffer(false);
        r.info("plain");
        assert_eq!(
            String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
            "plain\n"
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
        r.usage(&usage_fixture(6, 4));
        r.usage(&usage_fixture(12, 8));
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
        r.usage(&usage_fixture(6, 4));
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
        r.usage(&usage_fixture(6, 4));
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

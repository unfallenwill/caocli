//! The plain front end: what the machine's notifications look like when they are
//! written to a scrolling stream.
//!
//! Two halves, kept apart on purpose:
//!
//! - [`plain_writer::PlainWriter`] is the bytes-on-the-wire side: cells turned
//!   into SGR sequences, the open streaming block, and the spacing rule's two
//!   flags. It is what a cell becomes.
//! - [`Renderer`] is the session side: the status bar that lives at the bottom
//!   of the terminal, the model/effort labels the status bar carries, and the
//!   terminal the plain front end asked for. It is what the session is.
//!
//! The split is the shape the `Front`/`Ui` traits see: `paint_cell` lives on
//! the writer half, `set_model` on the renderer half, and the runtime calls
//! reach for whichever holds what they need.

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::time::Duration;

use crate::types::{Message, Usage};

use super::cell::{Cell, Style};
use super::contract::{Front, Ui};
use super::plain_writer::PlainWriter;
use super::status::Status;
use super::status_bar::StatusBar;
use super::terminal::{RealTerminal, Terminal};

/// Streaming renderer. Thinking and body text are two independent render blocks:
/// - thinking is dim, body text is the normal color
/// - blocks are separated by a newline; switching from thinking to body adds an
///   extra blank line
/// - with the NO_COLOR environment variable or when not a TTY no color codes are
///   emitted, but the block separation is kept
///
/// Everything the renderer writes is a [`Cell`]: the live notifications become
/// cells as they arrive and replayed history becomes cells up front, so both go
/// through the same painter and cannot drift apart. The cell-writing half lives
/// on [`PlainWriter`]; what [`Renderer`] keeps of its own is the status line,
/// the status bar that pins to it, and the terminal the front end asked for.
pub struct Renderer {
    /// The plain front end's writer: SGR bytes, streaming block, spacing rule.
    pub(super) writer: PlainWriter,
    /// The terminal this front end is attached to: everything it knows about the
    /// process's own terminal -- its width, whether it can take escape
    /// sequences, how to silence its echo -- comes from here.
    term: Box<dyn Terminal>,
    /// What the status bar reports: the model, the effort tier and the session's
    /// cache statistics.
    status: Status,
    pub(super) bar: Option<StatusBar>,
    /// Whether the front end currently has the tty in raw mode. The prompt sets
    /// it before reading keys; `ask_secret` reads it to decide which read path
    /// to use -- in raw mode the kernel is not collecting a line, so a blocking
    /// read on stdin never returns.
    in_raw_mode: bool,
}

impl Renderer {
    pub fn new() -> Self {
        let term = RealTerminal;
        let color = term.wants_color();
        Self::on(Box::new(std::io::stdout()), color, Box::new(term))
    }

    /// The same, with the writer, the palette and the terminal handed in rather
    /// than taken from the process: what a test drives, and the reason nothing
    /// below this line reads a global.
    pub(crate) fn on(out: Box<dyn Write>, color: bool, term: Box<dyn Terminal>) -> Self {
        Self {
            writer: PlainWriter::new(out, color),
            term,
            status: Status::default(),
            bar: None,
            in_raw_mode: false,
        }
    }

    /// Mark the tty as in raw mode. `ask_secret` reads this flag to pick
    /// between the cooked-mode blocking read (default) and a key-by-key
    /// read that works in raw mode (where the kernel does not collect a
    /// line for us).
    pub fn set_raw_mode(&mut self, raw: bool) {
        self.in_raw_mode = raw;
    }

    /// Sync the status bar against the current terminal size: called when the REPL
    /// starts and before each input, which also handles window resizes (if the
    /// size changed, the bar is torn down and rebuilt).
    pub fn refresh_status_bar(&mut self) {
        let current = StatusBar::detect(self.term.as_ref());
        self.apply_status_bar(current);
    }

    pub(super) fn apply_status_bar(&mut self, current: Option<StatusBar>) {
        match (self.bar, current) {
            (None, None) => {}
            (None, Some(bar)) => {
                bar.setup(self.writer.out.as_mut());
                self.bar = Some(bar);
                self.redraw_status_bar();
            }
            (Some(old), None) => {
                old.teardown(self.writer.out.as_mut());
                self.bar = None;
            }
            (Some(old), Some(bar)) if old == bar => self.redraw_status_bar(),
            (Some(old), Some(bar)) => {
                old.teardown(self.writer.out.as_mut());
                bar.setup(self.writer.out.as_mut());
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
        let visible = super::text::truncate(&label, width);
        let painted = self.writer.paint(Style::Dim, visible);
        bar.render(self.writer.out.as_mut(), visible, &painted);
    }

    /// Cache statistics accumulated for the current session, the status bar's
    /// data source. Read by tests -- here and in the agent's, where the
    /// accumulation across sub-requests is asserted -- so it exists only under
    /// `cfg(test)`, the way `Status::stats` does.
    #[cfg(test)]
    pub fn stats(&self) -> super::status::CacheStats {
        self.status.stats()
    }

    /// Restore the terminal before exiting (reset the scroll region, clear the
    /// status bar line). Idempotent.
    pub fn teardown(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.teardown(self.writer.out.as_mut());
        }
    }
}

impl Front for Renderer {
    fn replay(&mut self, messages: &[Message]) {
        self.writer.replay_messages(messages);
    }

    fn info(&mut self, s: &str) {
        self.writer.paint_cell(&Cell::Notice(s.to_owned()));
    }

    fn error(&mut self, s: &str) {
        // Straight to the error stream: the plain front end does not own the
        // screen, so an error survives a redirected stdout.
        eprintln!("{}", self.writer.paint(Style::Red, s));
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
        // Two read paths, picked by the tty's state:
        // - cooked mode: the kernel collects a line for us. The echo goes off
        //   so the answer is hidden, and a blocking read on stdin is what
        //   gives us back a line.
        // - raw mode: the kernel is not collecting a line, so a blocking read
        //   on stdin never returns. Read key by key with crossterm, mask the
        //   characters as they come in, and treat Enter/Ctrl-D/Ctrl-C the way
        //   the rest of the prompt does.
        if self.in_raw_mode {
            let prompt = prompt.to_owned();
            Box::pin(async move { read_secret_raw(&prompt).await })
        } else {
            // The echo goes off before the prompt does, not after: the answer can
            // arrive the moment the question is on the screen, and the only copy of
            // it that may exist is the one this returns. The prompt itself is
            // written now rather than awaited -- an answer nobody knows is being
            // asked for is not an answer -- and the read waits in the future, where
            // the caller awaits it.
            let echo = self.term.echo_off();
            let _ = writeln!(self.writer.out, "{}", self.writer.paint(Style::Dim, prompt));
            let _ = self.writer.out.flush();
            Box::pin(async move {
                let answer = self.term.read_line().await;
                drop(echo);
                answer
            })
        }
    }
}

/// Read a secret in raw mode: one event at a time, with the typed character
/// replaced by a mask. Enter submits, Ctrl-D on an empty buffer cancels (the
/// same shape as the prompt's Ctrl-D-on-empty), Ctrl-C also cancels. The
/// prompt is written dim; characters mask as a bullet.
///
/// Reads crossterm directly rather than through the renderer, because the
/// renderer is borrowed by the caller and a future inside a trait method is
/// not the place to reach into it. The bytes this function writes are the same
/// shape the rest of the prompt uses (`\r\x1b[2K` to clear, plain dim SGR for
/// the prompt, a mask char for each character) -- what is different is that
/// they are the prompt's own bytes, not a `Cell`.
async fn read_secret_raw(prompt: &str) -> Option<String> {
    use std::io::Write;
    let mut stdout = std::io::stdout();
    let dim_open = "\x1b[2m";
    let dim_close = "\x1b[0m";
    let mask: String = "•".repeat(0);
    let _ = write!(
        stdout,
        "\r\x1b[2K{dim_open}{prompt}{dim_close}\n\r\x1b[2K› {mask}"
    );
    let _ = stdout.flush();
    let mut buf = String::new();
    loop {
        let event = match crossterm::event::read() {
            Ok(ev) => ev,
            Err(_) => return None,
        };
        match event {
            crossterm::event::Event::Key(key)
                if key.kind == crossterm::event::KeyEventKind::Press =>
            {
                match (key.code, key.modifiers) {
                    (crossterm::event::KeyCode::Enter, _) => {
                        let _ = writeln!(stdout);
                        let _ = stdout.flush();
                        return Some(buf);
                    }
                    (
                        crossterm::event::KeyCode::Char('c'),
                        crossterm::event::KeyModifiers::CONTROL,
                    ) => {
                        let _ = writeln!(stdout);
                        let _ = stdout.flush();
                        return None;
                    }
                    (
                        crossterm::event::KeyCode::Char('d'),
                        crossterm::event::KeyModifiers::CONTROL,
                    ) => {
                        if buf.is_empty() {
                            let _ = writeln!(stdout);
                            let _ = stdout.flush();
                            return None;
                        }
                        buf.pop();
                        let mask: String = "•".repeat(buf.chars().count());
                        let _ = write!(stdout, "\r\x1b[2K› {mask}");
                        let _ = stdout.flush();
                    }
                    (crossterm::event::KeyCode::Backspace, _) => {
                        buf.pop();
                        let mask: String = "•".repeat(buf.chars().count());
                        let _ = write!(stdout, "\r\x1b[2K› {mask}");
                        let _ = stdout.flush();
                    }
                    (crossterm::event::KeyCode::Char(c), _) => {
                        buf.push(c);
                        let mask: String = "•".repeat(buf.chars().count());
                        let _ = write!(stdout, "\r\x1b[2K› {mask}");
                        let _ = stdout.flush();
                    }
                    _ => {}
                }
            }
            crossterm::event::Event::Paste(text) => {
                buf.push_str(&text);
                let mask: String = "•".repeat(buf.chars().count());
                let _ = write!(stdout, "\r\x1b[2K› {mask}");
                let _ = stdout.flush();
            }
            _ => {}
        }
    }
}

impl Ui for Renderer {
    fn truncated(&mut self, notice: &str) {
        // The wire writes the sentence itself ("...; what is in the log is
        // incomplete"), so what the front end shows is the wire's words. Not
        // an error: the model stopped on a normal cause, and the transcript's
        // error styling is the wrong place for it.
        self.writer.paint_cell(&Cell::Notice(notice.to_owned()));
    }

    fn reasoning_delta(&mut self, s: &str) {
        self.writer.open_block(Style::Reasoning);
        self.writer.emit(s);
    }

    fn content_delta(&mut self, s: &str) {
        self.writer.open_block(Style::Plain);
        self.writer.emit(s);
    }

    fn finish_turn(&mut self) {
        self.writer.close_block();
    }

    fn tool_start(&mut self, name: &str, args: &str) {
        self.writer.paint_cell(&Cell::from_tool_call(name, args));
    }

    fn tool_output(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.writer.open_block(Style::Dim);
        // The command's own lines are set in like every other line of a cell, and
        // this is the only layer that sees where they break: what arrives is a run
        // of bytes, so a `\n` in it is a line of the cell that has to carry the
        // continuation columns itself. A line the terminal wraps is still the
        // terminal's, since only it knows where the screen ends.
        let rest = Style::Dim
            .stream_cell(String::new())
            .gutter()
            .map_or("", |gutter| gutter.rest);
        let mut lines = chunk.split('\n').peekable();
        while let Some(line) = lines.next() {
            // A line with nothing on it is a line the block leaves empty: it takes no
            // columns, and the line the cursor is standing at the start of is still
            // the one the next chunk continues.
            if !line.is_empty() {
                if self.writer.at_line_start {
                    self.writer.emit(rest);
                }
                self.writer.at_line_start = false;
                self.writer.emit(line);
            }
            if lines.peek().is_some() {
                self.writer.emit("\n");
                self.writer.at_line_start = true;
            }
        }
    }

    fn tool_result(&mut self, result: &str) {
        self.writer.paint_cell(&Cell::ToolResult(result.to_owned()));
    }

    fn instructions(&mut self, dir: &str) {
        self.writer
            .paint_cell(&Cell::Notice(crate::agents_md::notice_text(dir)));
    }

    fn interrupted(&mut self) {
        // A block that was still streaming when the turn was cancelled is closed
        // first, so the notice starts on a line of its own.
        self.writer.close_block();
        self.writer.paint_cell(&Cell::Interrupted);
    }

    fn approval_requested(&mut self, name: &str, args: &str) {
        self.writer.paint_cell(&Cell::approval(name, args));
    }

    fn usage(&mut self, usage: &Usage, stream: Duration) {
        self.writer.paint_cell(&Cell::Usage {
            usage: usage.clone(),
            stream,
        });
        self.status.record(usage);
        self.redraw_status_bar();
    }
}

#[cfg(test)]
mod tests {
    use super::super::plain_writer::style_code;
    use crate::ui::cell::Style;

    /// The plain front end's mapping: a semantic [`Style`] to the SGR escape
    /// sequence it becomes when colors are enabled. The mapping lives here
    /// rather than on the [`Style`] enum, on purpose: the cell is data and the
    /// escape sequence is the renderer that owns the wire.
    #[test]
    fn styles_map_to_their_escape_sequences() {
        assert_eq!(style_code(Style::Plain), "");
        assert_eq!(style_code(Style::Dim), "\x1b[2m");
        // The same weight as `Dim`, and deliberately: thinking is told apart from a
        // tool result by the rule in its gutter, which the terminal cannot lose, not
        // by a color it may or may not honor.
        assert_eq!(style_code(Style::Reasoning), "\x1b[2m");
        // Painted styles carry the weight as well as the color: the color comes
        // from a palette the terminal chose for a background this code cannot see.
        assert_eq!(style_code(Style::Yellow), "\x1b[1;33m");
        assert_eq!(style_code(Style::Green), "\x1b[1;32m");
        assert_eq!(style_code(Style::Red), "\x1b[1;31m");
    }
}

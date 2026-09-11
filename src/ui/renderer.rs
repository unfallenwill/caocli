//! The plain front end: what the machine's notifications look like when they are
//! written to a scrolling stream.
//!
//! Everything it writes is a [`Cell`], whether it arrived as a live notification
//! or was folded out of the session log when resuming, so the two paths cannot
//! drift apart. What it keeps between calls is only what the spacing rule needs:
//! the block currently streaming, and whether the cell written last was text.
//!
//! It reads no global: the writer, the palette and the terminal it draws on are
//! handed in through [`Renderer::on`], which is also what a test drives.

use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::time::Duration;

use crate::types::{Message, Usage};

use super::cell::{Cell, Span, Style};
use super::contract::{Front, Ui};
use super::status::Status;
use super::status_bar::StatusBar;
use super::terminal::{RealTerminal, Terminal};

const RESET: &str = "\x1b[0m";

/// A text block being streamed.
///
/// Deltas are written as they arrive rather than buffered, so the block is never
/// held whole: only its kind has to be remembered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Block {
    Reasoning,
    Content,
    /// A running command's output: the one block whose text is not the model's, and
    /// the only one that is not a part of the answer's own two.
    Output,
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
            Block::Output => Style::Dim,
        }
    }

    /// An empty cell of this kind, used to ask the spacing rule where the block
    /// belongs without having to duplicate the rule here.
    fn spacing_cell(self) -> Cell {
        match self {
            Block::Reasoning => Cell::Reasoning(String::new()),
            Block::Content => Cell::Content(String::new()),
            Block::Output => Cell::ToolOutput(String::new()),
        }
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
    pub(super) out: Box<dyn Write>,
    /// The terminal this front end is attached to: everything it knows about the
    /// process's own terminal -- its width, whether it can take escape
    /// sequences, how to silence its echo -- comes from here.
    term: Box<dyn Terminal>,
    color: bool,
    /// The text block currently being streamed, if any.
    live: Option<Block>,
    /// Whether the cursor is at the start of a line of the block being streamed,
    /// which is a fact only a command's output has a use for: a chunk can end in the
    /// middle of a line, and the line it continues is set in like the rest of the
    /// block. A block of the model's own has no lines this front end breaks.
    at_line_start: bool,
    /// Whether the cell written last was a text block. Only this much of the
    /// previous cell is kept: it is all the spacing rule needs, and the
    /// transcript itself lives in the session log.
    prev_was_block: bool,
    /// What the status bar reports: the model, the effort tier and the session's
    /// cache statistics.
    status: Status,
    pub(super) bar: Option<StatusBar>,
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
            out,
            term,
            color,
            live: None,
            at_line_start: false,
            prev_was_block: false,
            status: Status::default(),
            bar: None,
        }
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
        let visible = super::text::truncate(&label, width);
        let painted = self.paint(Style::Dim, visible);
        bar.render(self.out.as_mut(), visible, &painted);
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
            bar.teardown(self.out.as_mut());
        }
    }

    fn emit(&mut self, s: &str) {
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
    pub(super) fn paint(&self, style: Style, s: &str) -> String {
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
        // A cell is a line of its own: a block still streaming when one arrives --
        // which is what a running command's output leaves open -- ends here rather
        // than running on into it.
        self.close_block();
        if cell.gap_after(self.prev_was_block) {
            self.emit("\n");
        }
        // The marker first, then the cell's own spans: a cell is set in past its
        // gutter, and this front end is the one that writes those columns itself --
        // the front end that owns the screen lays the cell out, and would otherwise
        // write the marker twice.
        let gutter = cell.gutter();
        let led = gutter.map(|g| Span::new(g.style, g.head));
        let spans: Vec<Span> = led.into_iter().chain(cell.spans()).collect();
        let painted = self.paint_spans(&spans, gutter.map_or("", |g| g.rest));
        self.emit(&painted);
        if cell.ends_line() {
            self.emit("\n");
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
        if block.spacing_cell().gap_after(self.prev_was_block) {
            self.emit("\n");
        }
        self.emit(self.open_style(block.style()));
        if let Some(gutter) = block.spacing_cell().gutter() {
            self.emit(gutter.head);
        }
        self.live = Some(block);
    }

    /// End the streaming block: close its style and its line.
    fn close_block(&mut self) {
        if self.live.take().is_none() {
            return;
        }
        self.emit(self.close_style());
        // A block whose last chunk ended with a line break has nothing left on the
        // line it is standing on, and an empty line before the next cell is not a
        // line of anything.
        if !self.at_line_start {
            self.emit("\n");
        }
        self.at_line_start = false;
        self.prev_was_block = true;
    }

    /// Replay history messages (used when resuming a session).
    ///
    /// The messages become the same cells a live turn produces and go through
    /// the same painter, so a resumed session is laid out exactly like the one
    /// that was watched live -- including the tool summaries, which are a
    /// property of the cell and so cannot be forgotten here.
    fn replay_messages(&mut self, messages: &[Message]) {
        for cell in super::cell::from_messages(messages) {
            self.paint_cell(&cell);
        }
        // Separate the replayed history from the prompt that follows it.
        self.emit("\n");
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
        let echo = self.term.echo_off();
        let _ = writeln!(self.out, "{}", self.paint(Style::Dim, prompt));
        let _ = self.out.flush();
        Box::pin(async move {
            let answer = self.term.read_line().await;
            drop(echo);
            answer
        })
    }
}

impl Ui for Renderer {
    fn reasoning_delta(&mut self, s: &str) {
        self.open_block(Block::Reasoning);
        self.emit(s);
    }

    fn content_delta(&mut self, s: &str) {
        self.open_block(Block::Content);
        self.emit(s);
    }

    fn finish_turn(&mut self) {
        self.close_block();
    }

    fn tool_start(&mut self, name: &str, args: &str) {
        self.paint_cell(&Cell::tool_call(name, args));
    }

    fn tool_output(&mut self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        self.open_block(Block::Output);
        // The command's own lines are set in like every other line of a cell, and
        // this is the only layer that sees where they break: what arrives is a run
        // of bytes, so a `\n` in it is a line of the cell that has to carry the
        // continuation columns itself. A line the terminal wraps is still the
        // terminal's, since only it knows where the screen ends.
        let rest = Block::Output
            .spacing_cell()
            .gutter()
            .map_or("", |gutter| gutter.rest);
        let mut lines = chunk.split('\n').peekable();
        while let Some(line) = lines.next() {
            // A line with nothing on it is a line the block leaves empty: it takes no
            // columns, and the line the cursor is standing at the start of is still
            // the one the next chunk continues.
            if !line.is_empty() {
                if self.at_line_start {
                    self.emit(rest);
                }
                self.at_line_start = false;
                self.emit(line);
            }
            if lines.peek().is_some() {
                self.emit("\n");
                self.at_line_start = true;
            }
        }
    }

    fn tool_result(&mut self, result: &str) {
        self.paint_cell(&Cell::ToolResult(result.to_owned()));
    }

    fn instructions(&mut self, dir: &str) {
        self.paint_cell(&Cell::Notice(crate::agents_md::notice_text(dir)));
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

    fn usage(&mut self, usage: &Usage, stream: Duration) {
        self.paint_cell(&Cell::Usage {
            usage: usage.clone(),
            stream,
        });
        self.status.record(usage);
        self.redraw_status_bar();
    }
}

//! The plain front end's writer: cells turned into SGR bytes.
//!
//! The plain front end does not own a screen. It writes cells to whatever
//! `Write` it was handed (stdout in production, a `SharedBuf` in tests) and
//! lets the terminal fold the result. Two pieces of state travel with the
//! writing half:
//!
//! - the open streaming block, which the [`Stream`] tracks, and
//! - the spacing rule's two flags -- whether the cursor is at the start of a
//!   command-output line, and whether the cell written last was a text block.
//!
//! Both are the writer's, not the front end's: a renderer that only paints
//! cells keeps no other state, and the screen-owning front end reaches the
//! same stream through a different path. Splitting the writer out of
//! [`Renderer`] is what lets the rest of that struct stay about the status
//! bar and the terminal it asked for, rather than the cells it is rendering.
//!
//! What the [`Renderer`] takes from the writer: the SGR open/close codes (for
//! its status bar), the writer itself (for `ask_secret` to write its prompt).
//! Both go through this module.

use std::io::Write;

use crate::types::Message;
use crate::ui::cell::{Cell, Stream, Style, style_code};
use crate::ui::theme;

use super::cell::CellSink;

/// The plain front end's [`CellSink`]: turns the cell layer's calls into
/// SGR bytes on a `Write`. Holds the open style for the duration of one cell
/// so consecutive spans in the same style stay one style run, and the gutter's
/// continuation columns so an embedded `\n` is set in like the line it
/// continues.
struct SgrSink<'a> {
    out: &'a mut dyn Write,
    color: bool,
    open: Option<Style>,
    gutter_rest: &'static str,
}

impl<'a> SgrSink<'a> {
    fn new(out: &'a mut dyn Write, color: bool) -> Self {
        Self {
            out,
            color,
            open: None,
            gutter_rest: "",
        }
    }

    /// The SGR open on style change, the continuation columns on every `\n`,
    /// and the bytes in between. One style code per run rather than one per
    /// span, so consecutive spans in the same style stay one style run.
    ///
    /// A `\n` in a cell is a line of that cell, not a line the terminal
    /// wrapped, so it is set in like every other line of it: the writer writes
    /// the gutter's continuation columns itself, because a line the terminal
    /// wraps because it is longer than the screen is the one thing neither
    /// front end can set in -- only the terminal knows where it breaks.
    ///
    /// The opened style runs across the break and over the columns, so the
    /// continuation is painted in the style of the line it continues rather
    /// than in the gutter's own. The two differ only for a cell whose marker
    /// is not blank, and for that one they are the same style by construction.
    fn write_span(&mut self, style: Style, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.open != Some(style) {
            if self.open.is_some() {
                let _ = self.out.write_all(self.reset().as_bytes());
            }
            let code = if self.color {
                style_code(style)
            } else {
                String::new()
            };
            let _ = self.out.write_all(code.as_bytes());
            self.open = Some(style);
        }
        for (i, piece) in text.split('\n').enumerate() {
            if i > 0 {
                let _ = self.out.write_all(b"\n");
                if !self.gutter_rest.is_empty() {
                    let _ = self.out.write_all(self.gutter_rest.as_bytes());
                }
            }
            let _ = self.out.write_all(piece.as_bytes());
        }
    }

    /// The SGR reset, or empty when colors are off: an SGR byte the terminal
    /// has no use for is one the terminal could still echo back, and that is
    /// a byte the test suite would pin as a one-line oversight.
    fn reset(&self) -> &'static str {
        if self.color { theme::RESET } else { "" }
    }
}

impl<'a> CellSink for SgrSink<'a> {
    fn begin_cell(&mut self, gutter: Option<&super::cell::Gutter>) {
        self.open = None;
        self.gutter_rest = gutter.map_or("", |g| g.rest);
    }

    fn gap(&mut self) {
        let _ = self.out.write_all(b"\n");
    }

    fn span(&mut self, style: Style, text: &str) {
        self.write_span(style, text);
    }

    fn line_end(&mut self) {
        let _ = self.out.write_all(b"\n");
    }

    fn end_cell(&mut self) {
        // Close the open style, so the next cell starts unstyled. The plain
        // front end is the one place a style could still be open here: the
        // TUI's sink collects into spans and never opens anything.
        if self.open.is_some() {
            let _ = self.out.write_all(self.reset().as_bytes());
            self.open = None;
        }
    }
}

/// The plain front end's writer: the bytes-on-the-wire half of what a cell
/// becomes. Owns the open streaming block and the spacing rule's two flags,
/// and reaches for an [`SgrSink`] to write each cell out.
///
/// Kept apart from [`super::Renderer`] so that the renderer's other concerns
/// (the status bar, the terminal it asked for, the raw-mode flag) stay out
/// of the cell-rendering path: this struct is what a cell is, the renderer
/// is what the session is.
pub(super) struct PlainWriter {
    pub(super) out: Box<dyn Write>,
    /// Whether SGR colors are written. `false` when `NO_COLOR` is set or the
    /// terminal is not a tty -- the bytes the writer would have written are
    /// left out, but the block separation the spacing rule owns stays.
    pub(super) color: bool,
    /// The text block currently being streamed, if any. The shared
    /// [`Stream`] does the block tracking: the plain front end writes the
    /// bytes itself, the TUI files the closed cell into its transcript, and
    /// both ask the same answer to "what cell is a block in style X".
    pub(super) stream: Stream,
    /// Whether the cursor is at the start of a line of the block being
    /// streamed, which is a fact only a command's output has a use for: a
    /// chunk can end in the middle of a line, and the line it continues is
    /// set in like the rest of the block. A block of the model's own has no
    /// lines this front end breaks.
    pub(super) at_line_start: bool,
    /// Whether the cell written last was a text block. Only this much of the
    /// previous cell is kept: it is all the spacing rule needs, and the
    /// transcript itself lives in the session log.
    pub(super) prev_was_block: bool,
}

impl PlainWriter {
    pub(super) fn new(out: Box<dyn Write>, color: bool) -> Self {
        Self {
            out,
            color,
            stream: Stream::new(),
            at_line_start: false,
            prev_was_block: false,
        }
    }

    pub(crate) fn emit(&mut self, s: &str) {
        let _ = self.out.write_all(s.as_bytes());
        let _ = self.out.flush();
    }

    /// The escape that opens `style`, empty when colors are off.
    pub(crate) fn open_style(&self, style: Style) -> String {
        if self.color {
            style_code(style)
        } else {
            String::new()
        }
    }

    /// The escape that closes a style, empty when colors are off.
    pub(crate) fn close_style(&self) -> &'static str {
        if self.color { theme::RESET } else { "" }
    }

    /// Wrap `s` in `style`. Styles are applied per span, so no call site has to
    /// know whether colors are enabled.
    pub(crate) fn paint(&self, style: Style, s: &str) -> String {
        let open = self.open_style(style);
        if open.is_empty() {
            return s.to_owned();
        }
        format!("{open}{s}{}", self.close_style())
    }

    /// Write a cell: the blank line separating it from the previous one, its
    /// marker in its gutter, its spans, then its line ending.
    ///
    /// Rendering goes through [`Cell::render`] so the gap rule, the gutter's
    /// head and the line end all live on the cell layer; this front end is
    /// only the sink that turns each call into SGR bytes.
    pub(crate) fn paint_cell(&mut self, cell: &Cell) {
        // A cell is a line of its own: a block still streaming when one arrives
        // -- which is what a running command's output leaves open -- ends here
        // rather than running on into it.
        self.close_block();
        let mut sink = SgrSink::new(self.out.as_mut(), self.color);
        let finish = cell.render(self.prev_was_block, &mut sink);
        self.prev_was_block = finish.is_text_block;
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
    pub(crate) fn open_block(&mut self, style: Style) {
        // Same style already open: the stream's no-op means we write nothing.
        if self.stream.current().map(|(open, _)| open) == Some(style) {
            return;
        }
        self.close_block();
        // The spacing rule is per-cell-type, and a block in `style` becomes a
        // cell whose gutter and gap come from the same style: an empty cell of
        // that kind is what the painter used to build, and is what the stream's
        // `stream_cell("")` answers.
        let spacing = style.stream_cell(String::new());
        if spacing.gap_after(self.prev_was_block) {
            self.emit("\n");
        }
        self.emit(&self.open_style(style));
        if let Some(gutter) = spacing.gutter() {
            self.emit(gutter.head);
        }
        // Open the stream in this style. The empty text is the no-op path: the
        // stream is the one closing the previous block, not us.
        let _ = self.stream.append(style, "");
    }

    /// End the streaming block: close its style and its line.
    pub(crate) fn close_block(&mut self) {
        if self.stream.close().is_none() {
            return;
        }
        self.emit(self.close_style());
        // A block whose last chunk ended with a line break has nothing left on the
        // line it is standing, and an empty line before the next cell is not a
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
    pub(crate) fn replay_messages(&mut self, messages: &[Message]) {
        for cell in super::cell::from_messages(messages) {
            self.paint_cell(&cell);
        }
        // Separate the replayed history from the prompt that follows it.
        self.emit("\n");
    }
}

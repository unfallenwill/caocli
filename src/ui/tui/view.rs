//! The view: what the screen shows, in four pieces.
//!
//! - the cells the session has produced and the open streaming block that has
//!   not yet become a cell,
//! - the laid cache (cells already wrapped at the last width the screen drew
//!   at) and the scroll position over it,
//! - the gate question, if one is open,
//! - the status line the box sits above.
//!
//! Kept apart from [`super::State`] so the things that draw or query what is on
//! the screen do not have to walk through the keys a turn might be reading or
//! the history the box might be browsing. A test about laying out a
//! transcript does not have to invent a turn to test it.

use crossterm::event::MouseEventKind;
use ratatui::text::Line;

use crate::ui::cell::{Cell, Stream};
use crate::ui::status::Status;

use super::scroll::Scroll;

/// The lines one wheel notch moves the window over the transcript: the step a
/// terminal's own scrollback takes, so a notch here reads like a notch anywhere
/// else. Not a page -- a notebook's worth of lines per flick of a wheel is a way
/// of losing the place rather than of reading.
pub(crate) const WHEEL_LINES: isize = 3;

/// What the screen draws from, and the bookkeeping the draw needs to keep.
pub(crate) struct View {
    /// The session summary the box sits above.
    pub(crate) status: Status,
    /// Everything the session has produced, in draw order: replayed history, the
    /// turn in flight, and the lines the user submitted. The whole session, not
    /// an increment of it -- with the alternate screen there is no scrollback to
    /// hand finished lines to, and the terminal keeps no copy of its own.
    pub(crate) transcript: Vec<Cell>,
    /// The transcript's cells laid out at [`View::laid_width`], one entry per
    /// cell and in the same order. A cell is never laid out twice for one width:
    /// the cells are the session's and do not change once they are pushed, so
    /// what a draw has to wrap is only what has arrived since the last one.
    /// Everything else -- how long the transcript is, which lines a window shows
    /// -- is derived from this rather than from the cells again.
    ///
    /// `laid.len()` is how many cells are laid and the transcript may have more
    /// waiting: a prefix, never a different list.
    pub(crate) laid: Vec<Vec<Line<'static>>>,
    /// The width `laid` was laid at, or `None` when nothing is laid out. Every
    /// line is wrapped to a width, so a different one invalidates all of it.
    pub(crate) laid_width: Option<usize>,
    /// How many cells have been laid out here, over the life of this view.
    ///
    /// Nothing on the screen can show this: a draw that re-wrapped the whole
    /// session would draw the same picture. It is what a test has to read to pin
    /// the layout to being done once per cell rather than once per draw, and it
    /// is per view because the tests run side by side.
    #[cfg(test)]
    pub(crate) laid_cells: usize,
    /// Which part of the transcript is on screen.
    pub(crate) scroll: Scroll,
    /// What the last draw saw: how long the transcript was, and how many rows of
    /// it there were room for. Paging and staying put while lines arrive both
    /// need them, and both are the terminal's to say rather than the view's.
    pub(crate) drawn_lines: usize,
    pub(crate) drawn_rows: usize,
    /// How many lines the box held at the last draw, so that a box that has just
    /// lost lines can be told from one that has not.
    pub(crate) drawn_draft: usize,
    /// The text block being streamed. The shared [`Stream`] is the same one
    /// the plain front end uses: it tracks the open block and turns it into a
    /// [`Cell`] on close, so the two front ends cannot disagree on what
    /// "a block in style X becomes".
    pub(crate) stream: Stream,
    /// The question standing over the box, while one is open.
    pub(crate) question: Option<Cell>,
}

impl Default for View {
    fn default() -> Self {
        Self {
            status: Status::default(),
            transcript: Vec::new(),
            laid: Vec::new(),
            laid_width: None,
            #[cfg(test)]
            laid_cells: 0,
            scroll: Scroll::default(),
            drawn_lines: 0,
            drawn_rows: 0,
            drawn_draft: 0,
            stream: Stream::new(),
            question: None,
        }
    }
}

impl View {
    /// The first transcript line to draw: a window on the end, or the one the
    /// reader scrolled back to.
    ///
    /// Lines that arrive between two draws take the window with them rather than
    /// sliding under it, so a reader who scrolled back stays on the line they were
    /// reading instead of being carried to the end. Everything is clamped, because
    /// a resize re-wraps the transcript and the line count is not the one this was
    /// scrolled against.
    pub(crate) fn window(&mut self, total: usize, rows: usize) -> usize {
        if self.scroll.back > 0 {
            let grew = total.saturating_sub(self.drawn_lines);
            self.scroll.by(-(grew as isize), total, rows);
        }
        self.drawn_lines = total;
        self.drawn_rows = rows;
        self.scroll.first(total, rows)
    }

    /// Move the window over the transcript by `lines`, positive being towards the
    /// newest line. The bounds come from the last draw, which is the only thing
    /// that knows how long the transcript is and how many rows of it there are
    /// room for.
    pub(crate) fn scroll_by(&mut self, lines: isize) {
        let (total, rows) = (self.drawn_lines, self.drawn_rows);
        self.scroll.by(lines, total, rows);
    }

    /// Page through the transcript a screen at a time; `-1` is back, `+1` forward.
    pub(crate) fn page(&mut self, step: isize) {
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
    pub(crate) fn wheel(&mut self, kind: MouseEventKind) {
        match kind {
            MouseEventKind::ScrollUp => self.scroll_by(-WHEEL_LINES),
            MouseEventKind::ScrollDown => self.scroll_by(WHEEL_LINES),
            _ => {}
        }
    }

    /// Back to the end, which is where a new line will appear.
    pub(crate) fn follow(&mut self) {
        self.scroll.bottom();
    }
}

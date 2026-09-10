//! The layout: how the screen's rows are divided, and the arithmetic of the
//! regions the screen is made of.
//!
//! Pure geometry: nothing here draws, keeps state, or reads the terminal. The
//! regions answer with what they need and the frame decides what they get, so
//! the arithmetic and the answer can be tested without a screen.

use std::rc::Rc;

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Borders};

use crate::ui::cell;

/// Rows the pinned region needs besides the input box: the status line below it.
pub(super) const PINNED_ROWS: u16 = 1;

/// The rows the input box takes when it holds nothing: a border, the empty line,
/// a border. An empty box is still a box.
pub(super) const BOX_ROWS: u16 = 3;

/// The input box's borders: top and bottom only.
///
/// The box is a band the width of the screen, so a pair of vertical edges would
/// be two columns of frame with nothing between them. Without them the draft
/// starts in the column the transcript's own user lines start in.
pub(super) const BOX_BORDERS: Borders = Borders::TOP.union(Borders::BOTTOM);

/// The columns the box's marker takes, which is the same two the transcript's user
/// lines take: the draft is written where the same line will be written once it is
/// submitted, so submitting moves nothing on the screen.
pub(super) const BOX_GUTTER: u16 = cell::MARKER_COLUMNS as u16;

/// The rows the input box takes when it holds `lines` lines of text: one row each
/// -- a line added with Ctrl-J is a line the box has to show -- plus the two
/// borders.
///
/// Capped at what a terminal `height` rows tall can spare once the pinned line
/// and a row of transcript are accounted for: a draft taller than the
/// screen scrolls inside the box rather than leaving the screen with nothing but
/// a box on it.
///
/// The floor gives way with the cap. The pinned region and the transcript cannot
/// both have all of what they ask for on a short terminal, and the draft is the
/// one of the three that can be bounded without losing the session -- so the box
/// shrinks first, down to nothing at all. Asking for a three-row box on a
/// two-row terminal is asking the layout for rows it does not have, and the rows
/// it then takes come out of some other region's answer.
pub(super) fn box_rows(lines: usize, height: u16) -> u16 {
    let wanted = u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(BOX_ROWS - 1);
    let most = height.saturating_sub(PINNED_ROWS + 1);
    wanted.clamp(BOX_ROWS.min(most), most)
}

/// The screen's rows, from the area the frame is drawn into: the transcript, the
/// queue waiting to run, the input box, and the status line under it.
///
/// The transcript's height is the layout's to decide and is read back from it
/// rather than worked out a second time here: two copies of the arithmetic are
/// two answers, and only one of them is the area the frame was laid out with. A
/// terminal too short for the pinned region has no transcript row to give, and
/// the window has to hear that from the layout -- it is the difference between a
/// window over the end of the transcript and one over a row that was never
/// drawn.
pub(super) fn screen_rows(area: Rect, input: u16, queued: u16) -> Rc<[Rect]> {
    Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(queued),
        Constraint::Length(input),
        Constraint::Length(1),
    ])
    .split(area)
}

/// A window over a list of rows: the run of them it draws, and how many it leaves
/// behind at each end -- which is also what says whether that end carries a count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
    /// The run drawn, as the first row and the one after the last.
    pub(super) first: usize,
    pub(super) last: usize,
    /// Rows left out above the run, and below it, as the count drawn at that end
    /// says. Both are zero when there was no room for the counts at all -- the one
    /// case a cut window is drawn without them.
    pub(super) above: usize,
    pub(super) below: usize,
}

/// The window a list of `total` rows is seen through, for a selection at
/// `selected` and `room` rows to draw it in.
///
/// The window follows the selection, so the row being chosen is always one of the
/// rows drawn: a menu scrolled past its own highlight is a menu that cannot be
/// answered, because the one row `Enter` is about is the one row the reader cannot
/// see. The selection sits at the end of the window it last moved into, so the
/// window moves when the selection leaves it and not before.
///
/// A window that is cut says so, which costs a row at each end it is cut at: what
/// is drawn is the longest run that fits in `room` along with the counts it needs.
/// A list with room to spare is never counted, and is never cut.
pub(super) fn picker_window(total: usize, selected: usize, room: usize) -> Window {
    let room = room.max(1);
    let total = total.max(1);
    let selected = selected.min(total - 1);
    for shown in (1..=room.min(total)).rev() {
        // Where the run would sit with the selection at its end, and the furthest
        // down it can start while still holding the selection. Between them: the
        // first position whose counts fit is the one drawn, so that a count the
        // room does not have is given up for a row of the list, which is what the
        // row would have been spent on anyway.
        let last = total - shown;
        for first in selected.saturating_sub(shown - 1)..=last.min(selected) {
            let end = first + shown;
            let cut = usize::from(first > 0) + usize::from(end < total);
            if shown + cut <= room {
                return Window {
                    first,
                    last: end,
                    above: first,
                    below: total - end,
                };
            }
        }
    }
    // A room too small for a count at each end of a single row -- two rows, with
    // the selection in the middle of the list. The counts are what give way: the
    // row `Enter` is about is the one row that cannot.
    Window {
        first: selected,
        last: selected + 1,
        above: 0,
        below: 0,
    }
}

/// The columns inside the box's rules that the draft is written in: the whole
/// width less the marker's columns.
///
/// This is the area the editor is rendered into, and with no block of its own the
/// area it is given is also the area its cursor is reported against -- so the two
/// have to be worked out the same way, which is what this being one function is
/// for.
pub(super) fn box_field(area: Rect) -> Rect {
    let inner = Block::default().borders(BOX_BORDERS).inner(area);
    Rect {
        x: inner.x + BOX_GUTTER,
        width: inner.width.saturating_sub(BOX_GUTTER),
        ..inner
    }
}

/// The box's marker: the same one a user line carries in the transcript, in the
/// same column, so that what is being typed and what was said line up.
///
/// One row tall and over the first row the draft has, rather than centred or
/// repeated: the transcript sets a user line's marker against its first line, and
/// a box that moved its marker as the draft grew would be a box that moved under
/// the reader's eye.
pub(super) fn box_marker(area: Rect) -> Rect {
    let inner = Block::default().borders(BOX_BORDERS).inner(area);
    Rect {
        width: BOX_GUTTER.min(inner.width),
        height: inner.height.min(1),
        ..inner
    }
}

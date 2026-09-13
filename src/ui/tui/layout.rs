//! The layout: how the screen's rows are divided, and the arithmetic of the
//! regions the screen is made of.
//!
//! Pure geometry: nothing here draws, keeps state, or reads the terminal. The
//! regions answer with what they need and the frame decides what they get, so
//! the arithmetic and the answer can be tested without a screen.
//!
//! Cell-layer concerns live one module down in [`crate::ui::cell::layout`]:
//! the row budgets for the standing task list and the queue, and the
//! window-follows-selection rule. What is here is the TUI's frame shape: the
//! input box, the pinned regions, the rows each of them gets once the others
//! have taken what they need.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Borders};

use crate::ui::cell;
use crate::ui::cell::layout as cell_layout;

/// Rows the pinned region needs besides the input box: the status line below it.
pub(super) const PINNED_ROWS: u16 = 1;

/// The rows the standing task list takes: what it laid out, capped at
/// [`cell_layout::TODO_ROWS`].
///
/// Read off the lines the block actually produced rather than from its task
/// count, because a task is as many rows as its words take.
pub(super) fn todo_rows(lines: usize) -> u16 {
    u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .min(cell_layout::TODO_ROWS as u16)
}

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
/// Capped at what a terminal `height` rows tall can spare once the pinned
/// regions -- `todos` rows of standing task list, and the status line -- and a
/// row of transcript are accounted for: a draft taller than the screen scrolls
/// inside the box rather than leaving the screen with nothing but a box on it.
///
/// The floor gives way with the cap. The pinned regions and the transcript cannot
/// both have all of what they ask for on a short terminal, and the draft is the
/// one of the three that can be bounded without losing the session -- so the box
/// shrinks first, down to nothing at all. Asking for a three-row box on a
/// two-row terminal is asking the layout for rows it does not have, and the rows
/// it then takes come out of some other region's answer.
pub(super) fn box_rows(lines: usize, height: u16, todos: u16) -> u16 {
    let wanted = u16::try_from(lines)
        .unwrap_or(u16::MAX)
        .saturating_add(BOX_ROWS - 1);
    let most = height.saturating_sub(PINNED_ROWS + 1 + todos);
    wanted.clamp(BOX_ROWS.min(most), most)
}

/// The screen's rows, from the area the frame is drawn into: the transcript, the
/// standing task list, the queue waiting to run, the input box, and the status
/// line under it.
///
/// The transcript's height is the layout's to decide and is read back from it
/// rather than worked out a second time here: two copies of the arithmetic are
/// two answers, and only one of them is the area the frame was laid out with. A
/// terminal too short for the pinned regions has no transcript row to give, and
/// the window has to hear that from the layout -- it is the difference between a
/// window over the end of the transcript and one over a row that was never
/// drawn.
///
/// The list stands above the queue because the queue is about to run and the list
/// is what the running is for: what will still be standing when the turn ends
/// sits nearer the transcript, and the box keeps the company of the line that is
/// next.
pub(super) fn screen_rows(area: Rect, todos: u16, input: u16, queued: u16) -> std::rc::Rc<[Rect]> {
    Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(todos),
        Constraint::Length(queued),
        Constraint::Length(input),
        Constraint::Length(1),
    ])
    .split(area)
}

/// The window a list of `total` rows is seen through, for the picker with
/// `selected` highlighted and `room` rows to draw in.
///
/// The window holds the highlighted row whenever that can be managed: a menu
/// scrolled past its own end is a menu the highlighted row has been carried
/// away from, and the row `Enter` is about has to be on the screen.
pub(super) fn picker_window(total: usize, selected: usize, room: usize) -> cell_layout::Window {
    cell_layout::selection_window(total, selected, room)
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

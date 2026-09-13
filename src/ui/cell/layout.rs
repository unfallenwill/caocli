//! The screen's row budgets and the window rule: what a region of the screen is
//! allowed to take from the transcript, and the window-follows-selection rule
//! that paces how a list too long for its block is read.
//!
//! Lives on the cell layer rather than in the TUI layout module because the
//! budgets describe cells and lists, not the shape of the TUI's frame: a list
//! of twenty tasks would take the same rows of standing block from a screen of
//! any shape, and a queue of five lines costs the transcript the same rows
//! however wide the box is. The TUI layer's [`crate::ui::tui::layout`] is what
//! shapes those budgets into the frame the screen hands to ratatui.
//!
//! The window type ([`Window`]) and the [`selection_window`] rule are kept
//! here for the same reason: a list that scrolls follows its selection
//! wherever the list lives, whether that is the standing task list, the
//! picker or some other list the cells name later.

use crate::tools::todo::Status;

/// Rows the standing task list may take, its own two ends included.
///
/// The block is pinned, so every row it takes is a row the transcript does not
/// have: it is worth enough of them to show a plan, and not enough to become the
/// screen. A list that does not fit in this is a list the model was asked not to
/// write.
pub(crate) const TODO_ROWS: usize = 8;

/// The rows of that budget the block spends on its own two ends before it reaches
/// its first task: the blank row that sets it off from the transcript, so that it
/// cannot be mistaken for the tail of one, and the title that says what it is and
/// how far it has got. What is left over is the room the tasks get.
pub(crate) const TODO_HEADS: usize = 2;

/// The rows the queue is allowed to take from the transcript.
///
/// Three lines: enough to read the head and the count without reading so much
/// that the answer is pushed off the screen. A queue longer than that -- more
/// lines, or longer ones -- costs the transcript those rows and no more.
pub(crate) const QUEUE_ROWS: usize = 3;

/// A window over a list of rows: the run of them it draws, and how many it leaves
/// behind at each end -- which is also what says whether that end carries a count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Window {
    /// The run drawn, as the first row and the one after the last.
    pub(crate) first: usize,
    pub(crate) last: usize,
    /// Rows left out above the run, and below it, as the count drawn at that end
    /// says. Both are zero when there was no room for the counts at all -- the one
    /// case a cut window is drawn without them.
    pub(crate) above: usize,
    pub(crate) below: usize,
}

/// A window that follows a selection: the run is placed so `selected` is at its
/// end, and shortened until the counts at the cut ends fit in `room`.
///
/// The window follows the selection, so the row being chosen is always one of the
/// rows drawn: a menu scrolled past its own highlight is a menu that cannot be
/// answered, because the one row `Enter` is about is the one row the reader
/// cannot see. The selection sits at the end of the window it last moved into,
/// so the window moves when the selection leaves it and not before.
///
/// A window that is cut says so, which costs a row at each end it is cut at:
/// what is drawn is the longest run that fits in `room` along with the counts
/// it needs. A list with room to spare is never counted, and is never cut.
/// A room too small for a count at each end of a single row falls back to a
/// bare window around `selected` -- the counts are what give way, the row the
/// selection is on is the one row that cannot.
///
/// The inner loop is what tries every `first` between "selection at the end"
/// and "selection at the start" -- a larger first can lower the cut at the
/// bottom end (when the run reaches the last row), and that is sometimes the
/// only way a window fits.
pub(crate) fn selection_window(total: usize, selected: usize, room: usize) -> Window {
    let room = room.max(1);
    let total = total.max(1);
    let selected = selected.min(total - 1);
    for shown in (1..=room.min(total)).rev() {
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
    Window {
        first: selected,
        last: selected + 1,
        above: 0,
        below: 0,
    }
}

/// The window a list of `total` tasks is seen through, for the task at `active`
/// and for the `room` rows there are to draw it in.
///
/// The window holds the task in hand whenever that can be managed: a list too
/// long for the block is a list whose one interesting row is the task being
/// worked on now, and a block pinned to the head of it would hide exactly that
/// row. With nothing in hand -- a plan not started yet, or one with everything
/// finished -- it shows the head, which is the plan itself.
pub(crate) fn todo_window(total: usize, active: Option<usize>, room: usize) -> Window {
    // No active task is the head of the list: row zero, where the plan itself
    // sits. An active task that is past the end (a stale cursor) is treated as
    // no active task for the same reason.
    let active = active.filter(|at| *at < total);
    selection_window(total, active.unwrap_or(0), room)
}

/// Pick the in-hand task: the position of the first `Status::InProgress`, or
/// `None` when nothing is in progress.
pub(crate) fn active_task(todos: &[crate::tools::todo::Todo]) -> Option<usize> {
    todos
        .iter()
        .position(|todo| todo.status == Status::InProgress)
}

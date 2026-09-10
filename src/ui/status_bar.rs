//! The status bar's placement: the last line of the terminal, kept there by a
//! scroll region that stops at it.
//!
//! What the line says is `status::Status`'s business; this is where it goes and
//! how it is painted. The two are separate so the line's content can be read
//! without a terminal and its placement written without a session -- and so the
//! escape sequences all live in one file.

use std::io::Write;

use super::terminal::Terminal;
use super::text;

/// Fixed status bar at the bottom: it occupies the terminal's last line and the
/// scroll region is restricted to 1..rows-1, so scrolling output cannot push the
/// bar off the screen.
/// Cost: lines that scroll out of the scroll region never reach the terminal's
/// scrollback buffer (history has to come from the session file).
/// Not enabled at all with `--no-status-bar` or when stdout is not a TTY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StatusBar {
    pub(super) rows: u16,
    pub(super) cols: u16,
}

impl StatusBar {
    /// Enabled only with at least 3 rows: the status bar line + at least one line
    /// of output area + one row of slack.
    const MIN_ROWS: u16 = 3;

    /// Give up immediately when stdout is not a terminal or the terminal says it
    /// cannot address the screen (`TERM=dumb`), without asking for a size.
    pub(super) fn detect(term: &dyn Terminal) -> Option<Self> {
        if !term.is_tty() || term.is_dumb() {
            return None;
        }
        Self::from_size(term.size())
    }

    pub(super) fn from_size(size: Option<(u16, u16)>) -> Option<Self> {
        match size {
            Some((rows, cols)) if rows >= Self::MIN_ROWS && cols > 1 => Some(Self { rows, cols }),
            _ => None,
        }
    }

    /// Usable width: the last column is left free, so writing there cannot
    /// trigger automatic wrapping.
    pub(super) fn width(&self) -> usize {
        self.cols as usize - 1
    }

    /// Set the scroll region → move the cursor to its bottom.
    pub(super) fn setup(&self, out: &mut dyn Write) {
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
    pub(super) fn teardown(&self, out: &mut dyn Write) {
        let _ = write!(out, "\x1b[r\x1b[{};1H\x1b[2K\r\n", self.rows);
        let _ = out.flush();
    }

    /// Right-aligned redraw: save the cursor → clear the line → write padding +
    /// text → restore the cursor.
    /// `visible` is used to compute the width (it carries no color codes), while
    /// `painted` is what actually gets written. The measurement is in columns,
    /// not chars, so a wide character is charged for both of its columns.
    pub(super) fn render(&self, out: &mut dyn Write, visible: &str, painted: &str) {
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

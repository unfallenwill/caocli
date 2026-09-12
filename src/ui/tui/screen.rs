//! The screen: the terminal this front end owns, and the frame it draws.
//!
//! Owning the terminal is why raw mode clears `ISIG` and why Ctrl-C arrives as
//! a key event; owning the frame is why the layout is the application's to
//! decide. The state is drawn from, never drawn to: everything here reads
//! [`State`] and writes pixels.

use std::io::{self, Stdout};

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use ratatui_textarea::ScreenCursor;

use crate::ui::cell::{self, Style};
use crate::ui::tui::layout::{box_field, box_marker, screen_rows, todo_rows};
use crate::ui::tui::paint::{self, style_of};
use crate::ui::tui::picker::PICKER_ROWS;
use crate::ui::tui::render;
use crate::ui::tui::state::State;

/// The screen: a terminal, and the state it shows.
///
/// Generic over the backend so that what it draws can be asserted on. The
/// terminal-touching half of this front end is exactly the half nothing else can
/// check, and ratatui ships a backend that keeps the screen in memory -- which
/// makes the drawing testable instead of merely inspected by hand.
pub(super) struct Screen<B: Backend> {
    pub(super) terminal: Terminal<B>,
    pub(super) state: State,
    /// The terminal this screen took, for as long as it holds it. `None` in a test,
    /// which draws into memory and has nothing to give back.
    pub(super) tty: Option<Tty>,
    /// The view key of the last draw, or `None` when the screen has to be
    /// repainted whatever the key says: nothing has been drawn yet.
    pub(super) drawn: Option<ViewKey>,
}

/// The mouse, as far as this front end wants it: the wheel, and nothing else.
///
/// `?1000` is what reports a button press, and a wheel notch is one -- buttons 4
/// and 5 -- so asking for it is asking for the wheel without the movement reports
/// `?1003` adds: those arrive for every cell the pointer crosses, which over a
/// whole screen is a stream of events and a repaint for each of them. `?1006` is
/// the encoding that spells a notch out as a sequence of its own, rather than
/// folding its coordinates into the bytes that name the button.
///
/// Written out rather than taken from crossterm's own `EnableMouseCapture`, which
/// asks for `?1003` as well. What it also costs is the terminal's own selection:
/// a terminal that has handed the mouse over keeps it, and gives it back under
/// `Shift`.
const MOUSE_ON: &str = "\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &str = "\x1b[?1000l\x1b[?1006l";

/// The terminal, held by the screen that took it.
///
/// A type of its own because a `Drop` impl cannot be written for a single
/// instantiation of a generic one: the screen is generic over its backend so that a
/// test can draw into memory, and the terminal that has to be given back is not
/// generic at all.
pub(super) struct Tty;

impl Tty {
    /// Take the terminal over: raw mode, then the alternate screen, then the wheel.
    ///
    /// Written so that a failure at any step undoes the steps before it: the value
    /// exists before the first escape sequence is written, so an error drops it and
    /// the drop is what gives the terminal back.
    pub(super) fn take() -> io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        let taken = Self;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::style::Print(MOUSE_ON)
        )?;
        Ok(taken)
    }
}

/// Give the terminal back: the mouse, the alternate screen, so what the user had
/// on it reappears, and the cursor where a shell prompt expects to find it.
fn restore() -> io::Result<()> {
    crossterm::execute!(
        std::io::stdout(),
        crossterm::style::Print(MOUSE_OFF),
        crossterm::event::DisableBracketedPaste,
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )
}

/// Restoring the terminal does not depend on the success path running. Raw mode is
/// process-wide and would wreck the shell if it survived an unwind, the alternate
/// screen would hide everything the user had on it -- so both are given back by
/// dropping what took them, which an unwind does too. The mouse is the same
/// bargain: a terminal left reporting it hands nothing to the shell that follows.
impl Drop for Tty {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = restore();
    }
}

/// What the screen would show, as a value that can be compared.
///
/// Equal keys mean a redraw cannot change a pixel, so it is skipped. The
/// revision covers everything the state knows about itself -- nothing on the
/// screen moves without the state changing, so it is the whole of what a draw
/// could differ in besides the size, which is there because the rows are
/// laid out from it. Reading the size is an
/// `ioctl`, not a round trip to the terminal, so it is cheap enough to be part
/// of a check that runs on every tick of the loop.
#[derive(PartialEq, Eq, Clone, Copy)]
pub(super) struct ViewKey {
    revision: u64,
    width: u16,
    height: u16,
}

/// A terminal that draws over the whole screen.
///
/// Nothing here has to ask the terminal where its cursor is -- that question is
/// what an inline viewport lives by -- so the only way this fails is a terminal
/// that cannot be put into raw mode at all.
pub(super) fn fullscreen<B: Backend>(backend: B) -> Result<Terminal<B>, B::Error> {
    Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Fullscreen,
        },
    )
}

impl Screen<CrosstermBackend<Stdout>> {
    /// Enter the screen.
    ///
    /// Fallible on purpose: raw mode is process-wide and the alternate screen hides
    /// everything the user had on it, so an attempt that cannot be finished has to
    /// leave the terminal as it was found. An error here is the caller's signal to
    /// fall back to the plain front end rather than to fail to start.
    pub(super) fn enter() -> io::Result<Self> {
        Ok(Self {
            tty: Some(Tty::take()?),
            terminal: fullscreen(CrosstermBackend::new(std::io::stdout()))?,
            state: State::default(),
            drawn: None,
        })
    }

    /// Leave the screen: giving the terminal back is the whole of it.
    pub(super) fn leave(&mut self) {
        drop(self.tty.take());
    }
}

impl<B: Backend> Screen<B> {
    /// What the screen would show right now.
    pub(super) fn view_key(&self) -> Result<ViewKey, B::Error> {
        let size = self.terminal.size()?;
        Ok(ViewKey {
            revision: self.state.revision,
            width: size.width,
            height: size.height,
        })
    }

    /// Repaint, unless the screen already shows this.
    ///
    /// The loop wakes on a tick to stay responsive to the keyboard, and most of
    /// those ticks have nothing new to show: a turn spends its time waiting to be
    /// told something. The framework compares each frame with the last and writes
    /// only the difference, so an unchanged frame costs no write -- what it costs
    /// is being built, and this check is what keeps a tick from paying for that.
    /// It is a coarser question than the framework's (one frame against another)
    /// and it is asked on purpose: what it is sparing is the building, not the
    /// writing.
    pub(super) fn draw_if_changed(&mut self) -> Result<(), B::Error> {
        let key = self.view_key()?;
        if self.drawn == Some(key) {
            return Ok(());
        }
        let area = self.draw_at()?;
        // Stamped from the frame that was drawn rather than from the size the
        // check above was made against, and only after the draw succeeded: a
        // failed draw leaves the screen showing something else, and a resize that
        // lands between the check and the draw leaves a key that describes an area
        // the screen is not showing -- either way the next call has to try again.
        self.drawn = Some(ViewKey {
            revision: self.state.revision,
            width: area.width,
            height: area.height,
        });
        Ok(())
    }

    /// Draw the screen as of now, for tests that do not care about the clock.
    #[cfg(test)]
    pub(super) fn draw(&mut self) -> Result<(), B::Error> {
        self.draw_at().map(|_| ())
    }

    /// Draw the screen: the window on the transcript, the input box, and the
    /// status line under it. The box is sized for what is in it, so the transcript
    /// gives up rows to a multi-line draft and takes them back when the line is
    /// submitted.
    ///
    /// Everything that depends on the size is asked of the frame inside the
    /// callback: the width the text is wrapped at and the rows each region gets
    /// come from the same [`Rect`], so the layout and the text laid out for it
    /// cannot disagree. The area drawn is returned, so the caller stamps its
    /// change key with what was drawn rather than with what it expected.
    pub(super) fn draw_at(&mut self) -> Result<Rect, B::Error> {
        let drawn = self.terminal.draw(|frame| {
            let state = &mut self.state;
            let area = frame.area();
            let width = area.width as usize;
            // The standing task list is laid out before anything is measured,
            // because it is the one region whose height depends on its text: a task
            // is as many rows as its words take. Asked for before the box, which
            // gives way to it, and before the layout, which is what the rows it
            // asks for come out of.
            let todos = Text::from(render::todo_lines(state, width));
            let todo = todo_rows(todos.height());
            let input = state.input_rows(area.height, todo);
            // What is waiting to run, drawn at the bottom of the transcript: the
            // session, then what comes next, then the box, and under the box the
            // session summary. Asked for before the layout, because how many rows
            // it takes is what the transcript gives up.
            let queue = Text::from(render::queue_lines(state, width));
            let queued = queue.height() as u16;
            let rows = screen_rows(area, todo, input, queued);
            state.reset_box_scroll(input);
            // What the transcript has to show, in the three pieces it is made of:
            // the cells that are laid out and kept, then the block still being
            // written, then a question if one is open.
            render::ensure_laid(state, width);
            let live = render::live_lines(state, width);
            let question = render::question_lines(state, width);
            let total = render::laid_rows(state) + live.len() + question.len();
            // The rows the transcript really has: the layout's answer, not a copy
            // of its arithmetic.
            let room = rows[0].height as usize;
            // The window over the transcript: its end unless the reader scrolled
            // back. The picker belongs to the line being typed, so it takes the
            // box's end of the transcript with it.
            let picker = state.picker_lines();
            let panel = state.panel_lines(width);
            let first = if picker.is_empty() {
                state.window(total, room)
            } else {
                state.follow();
                total.saturating_sub(room)
            };
            let last = (first + room).min(total);
            // A window that is cut says so on the row it was cut at -- but only
            // while the reader is somewhere other than the end: at the end the
            // newest line is the one being watched, and the top being off the
            // screen is the ordinary state of a long session rather than something
            // to report.
            let cut = state.scroll.back > 0;
            let above = cut && first > 0;
            let below = cut && last < total;
            // The two counts take their rows from the window they stand for, so
            // that neither report is a line out: a row that hides a line it does
            // not count is a window lying about how much there is.
            let first = first + usize::from(above);
            let last = last - usize::from(below);
            // A session shorter than the window starts at the bottom of it: the
            // newest line belongs next to the box, where the eye already is,
            // rather than at the top of a screen with a gap under it. A cut window
            // fills the region and has nothing to pad with.
            let taken = usize::from(above) + usize::from(below);
            let mut lines: Vec<Line> =
                vec![Line::default(); room.saturating_sub(last - first + taken)];
            if above {
                lines.push(paint::edge_line(true, first));
            }
            lines.extend(render::window_lines(state, first, last, &live, &question));
            if below {
                lines.push(paint::edge_line(false, total - last));
            }
            let transcript = Text::from(lines);

            frame.render_widget(Paragraph::new(transcript), rows[0]);
            draw_over(frame, &picker, rows[0], PICKER_ROWS);
            // The panel stands over the transcript too, and is given whatever the
            // transcript has: a question cannot be answered by a reader who
            // cannot see all of it.
            draw_over(frame, &panel, rows[0], rows[0].height as usize);
            frame.render_widget(Paragraph::new(todos), rows[1]);
            frame.render_widget(Paragraph::new(queue), rows[2]);
            draw_box(frame, state, rows[3]);
            draw_status(frame, state, rows[4]);
        })?;
        Ok(drawn.area)
    }

    /// A turn is over: close the block that was still being streamed.
    ///
    /// Nothing leaves the screen. The transcript is the state's, and a turn is part
    /// of it from the moment it arrives; this only closes the block, so that the
    /// next turn's first fragment opens one of its own.
    pub(super) fn commit(&mut self) {
        self.state.end_block();
    }
}

/// Translate the input box's own cursor into a position on the screen, so the
/// terminal's caret sits where the next character will go.
///
/// The box reports its cursor relative to the area it is rendered into, which --
/// with no block of its own -- is the field [`box_field`] cuts out. So this is
/// handed the rect the editor was rendered into rather than working out an area of
/// its own: a second attempt at the same arithmetic is a caret in a column the
/// character after it will not be in.
fn place_cursor(frame: &mut Frame, field: Rect, cursor: ScreenCursor) {
    let x = field.x + cursor.col as u16;
    let y = field.y + cursor.row as u16;
    if x < field.right() && y < field.bottom() {
        frame.set_cursor_position((x, y));
    }
}

/// A list of lines drawn over the bottom of the transcript it stands for: the
/// picker's menu, and the question tool's panel. `cap` is the most rows it may
/// take from the transcript, however long the list is.
fn draw_over(frame: &mut Frame, lines: &[Line<'static>], transcript: Rect, cap: usize) {
    if lines.is_empty() {
        return;
    }
    let height = lines.len().min(cap).min(usize::from(transcript.height)) as u16;
    let over = Rect {
        x: transcript.x,
        y: transcript.bottom().saturating_sub(height),
        width: transcript.width,
        height,
    };
    // Cleared first: a shorter list must not leave the tail of a longer one
    // behind it.
    frame.render_widget(Clear, over);
    frame.render_widget(Paragraph::new(Text::from(lines.to_vec())), over);
}

/// The input box: its rules, its marker, and the columns the draft is written in
/// between them.
///
/// The box's marker is the box's own rather than a character of the draft or of
/// the placeholder, so it is drawn whether the box holds a line, a hint or
/// nothing at all -- and typing cannot take it away, which is what the marker
/// inside the placeholder did.
fn draw_box(frame: &mut Frame, state: &State, area: Rect) {
    frame.render_widget(render::box_rule(state, area.width as usize), area);
    frame.render_widget(
        Paragraph::new(Line::styled(cell::USER_MARKER, style_of(Style::Dim))),
        box_marker(area),
    );
    let field = box_field(area);
    frame.render_widget(&state.textarea, field);
    let cursor = state.textarea.screen_cursor();
    place_cursor(frame, field, cursor);
}

/// The session summary, pinned under the box.
fn draw_status(frame: &mut Frame, state: &State, area: Rect) {
    let status = render::status_line(state, area.width as usize);
    frame.render_widget(Paragraph::new(status), area);
}

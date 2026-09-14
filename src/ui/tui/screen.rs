//! The screen: the terminal this front end owns, and the frame it draws.
//!
//! Owning the terminal is why raw mode clears `ISIG` and why Ctrl-C arrives as
//! a key event; owning the frame is why the layout is the application's to
//! decide. The state is drawn from, never drawn to: everything here reads
//! [`State`] and writes pixels.

use std::io::{self, Stdout};
use std::rc::Rc;

use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::Rect;
use ratatui::text::{Line, Text};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};
use ratatui_textarea::ScreenCursor;

use crate::ui::cell::{self, Cell, Style, style_of};
use crate::ui::paint;
use crate::ui::tui::layout::{box_field, box_marker, screen_rows, todo_rows};
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
            // The thinking widget, expanded: the snapshot replaces
            // the whole screen. Everything else — the transcript,
            // the box, the status — is hidden, because the user
            // explicitly asked to see the thought body full-window,
            // and "you can only type again after Ctrl-O" is the
            // contract.
            if let Some(snapshot) = expanded_snapshot(state) {
                draw_expanded(frame, state, area, snapshot);
            } else {
                let layout = compute_layout(state, area);
                let transcript = compose_transcript(state, &layout);
                paint(&*state, frame, &layout, transcript);
            }
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

/// What the frame draws into: the row each region gets and the text the
/// pinned ones were already laid out as. The transcript's text is recomputed
/// from this by [`compose_transcript`]; the rest is handed to the frame as-is.
struct Layout {
    rows: Rc<[Rect]>,
    width: usize,
    todos: Text<'static>,
    queue: Text<'static>,
}

/// Work out the regions: the standing task list and the queue, the box's
/// height for the draft it holds, and the row each region gets once the others
/// have taken what they need.
///
/// Todos first, because they are the one region whose height depends on its
/// text -- a task is as many rows as its words take. The queue is laid out
/// next, because how many rows it costs the transcript is also its text. Both
/// are measured before the layout splits the area, so the rows they ask for
/// are the rows they get.
fn compute_layout(state: &mut State, area: Rect) -> Layout {
    let width = area.width as usize;
    let todos = Text::from(render::todo_lines(&state.view, width));
    let todo = todo_rows(todos.height());
    let input = state.input_rows(area.height, todo);
    let queue = Text::from(render::queue_lines(&state.turn, width));
    let queued = queue.height() as u16;
    let rows = screen_rows(area, todo, input, queued);
    state.reset_box_scroll(input);
    Layout {
        rows,
        width,
        todos,
        queue,
    }
}

/// The transcript's lines: the window on the cells, the block still being
/// streamed, a question if one is open, and the two edge counts that say what
/// was scrolled past.
///
/// Picker and panel are computed here because they stand over the transcript
/// and their height is what decides whether to follow the reader to the end
/// or jump there to make room.
/// The transcript's lines: the window on the cells, the block still being
/// streamed, a question if one is open, and the two edge counts that say what
/// was scrolled past.
///
/// Picker and panel are computed here because they stand over the transcript
/// and their height is what decides whether to follow the reader to the end
/// or jump there to make room.
fn compose_transcript(state: &mut State, layout: &Layout) -> Text<'static> {
    let width = layout.width;
    let room = layout.rows[0].height as usize;
    // What the transcript has to show, in the three pieces it is made of:
    // the cells that are laid out and kept, then the block still being
    // written, then a question if one is open.
    render::ensure_laid(&mut state.view, width);
    let live = render::live_lines(&state.view, width);
    let question = render::question_lines(&state.view, width);
    let total = render::laid_rows(&state.view) + live.len() + question.len();
    // The picker belongs to the line being typed, so it takes the box's end of
    // the transcript with it: a window on the end, not where the reader
    // scrolled back to. The panel is read in `paint` once the transcript's
    // shape is decided.
    let picker = state.picker_lines();
    let first = if picker.is_empty() {
        state.window(total, room)
    } else {
        state.follow();
        total.saturating_sub(room)
    };
    let last = (first + room).min(total);
    // A window that is cut says so on the row it was cut at -- but only while
    // the reader is somewhere other than the end: at the end the newest line
    // is the one being watched, and the top being off the screen is the
    // ordinary state of a long session rather than something to report.
    let cut = state.view.scroll.back > 0;
    let above = cut && first > 0;
    let below = cut && last < total;
    // The two counts take their rows from the window they stand for, so that
    // neither report is a line out: a row that hides a line it does not count
    // is a window lying about how much there is.
    let first = first + usize::from(above);
    let last = last - usize::from(below);
    // A session shorter than the window starts at the bottom of it: the
    // newest line belongs next to the box, where the eye already is, rather
    // than at the top of a screen with a gap under it. A cut window fills the
    // region and has nothing to pad with.
    let taken = usize::from(above) + usize::from(below);
    let mut lines: Vec<Line> = vec![Line::default(); room.saturating_sub(last - first + taken)];
    if above {
        lines.push(paint::edge_line(true, first));
    }
    lines.extend(render::window_lines(
        &state.view,
        first,
        last,
        &live,
        &question,
    ));
    if below {
        lines.push(paint::edge_line(false, total - last));
    }
    Text::from(lines)
}

/// Hand the regions to the frame: the transcript, the overlays that stand over
/// it, and the three pinned regions under it.
fn paint(state: &State, frame: &mut Frame, layout: &Layout, transcript: Text) {
    let rows = &layout.rows;
    frame.render_widget(Paragraph::new(transcript), rows[0]);
    // The picker and the panel stand over the bottom of the transcript -- the
    // picker is capped at its own budget, the panel is given the whole
    // transcript height because a question cannot be answered by a reader who
    // cannot see all of it.
    let picker = state.picker_lines();
    let panel = state.panel_lines(layout.width);
    draw_over(frame, &picker, rows[0], PICKER_ROWS);
    draw_over(frame, &panel, rows[0], rows[0].height as usize);
    frame.render_widget(Paragraph::new(layout.todos.clone()), rows[1]);
    frame.render_widget(Paragraph::new(layout.queue.clone()), rows[2]);
    draw_box(frame, state, rows[3]);
    draw_status(frame, state, rows[4]);
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
    frame.render_widget(
        render::box_rule(
            &state.overlay,
            &state.turn,
            state.thought.as_ref(),
            area.width as usize,
        ),
        area,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(cell::USER_MARKER, style_of(Style::Dim))),
        box_marker(area),
    );
    let field = box_field(area);
    frame.render_widget(&state.edit.textarea, field);
    let cursor = state.edit.textarea.screen_cursor();
    place_cursor(frame, field, cursor);
}

/// The session summary, pinned under the box.
fn draw_status(frame: &mut Frame, state: &State, area: Rect) {
    let status = render::status_line(&state.view, area.width as usize);
    frame.render_widget(Paragraph::new(status), area);
}

/// The snapshot the expanded view should render, if any. The
/// active thought's snapshot when it is expanded; the most recent
/// historical block's when it was expanded at the moment it
/// closed and the active block is no longer expanded.
///
/// `None` for the folded case, which is what lets the caller fall
/// back to the regular transcript-and-box layout.
fn expanded_snapshot(state: &State) -> Option<&[Cell]> {
    if let Some(active) = state.thought.as_ref()
        && active.expanded
    {
        return Some(&active.snapshot);
    }
    state
        .historical
        .last()
        .filter(|b| b.expanded)
        .map(|b| b.snapshot.as_slice())
}

/// The expanded body drawn over the entire screen: the snapshot,
/// full width, every cell rendered in order with the gaps a normal
/// transcript keeps between them.
fn draw_expanded(frame: &mut Frame, _state: &State, area: Rect, snapshot: &[Cell]) {
    let width = area.width as usize;
    let lines = render::expanded_thought_lines(snapshot, width, true);
    // The snapshot is what the user is reading; pad with blanks if
    // it is shorter than the screen, so the absence of rows reads
    // as "nothing more to read", not "the layout broke".
    let height = area.height as usize;
    let mut all_lines = lines;
    if all_lines.len() < height {
        all_lines.resize(height, Line::default());
    } else if all_lines.len() > height {
        all_lines.truncate(height);
    }
    frame.render_widget(Paragraph::new(Text::from(all_lines)), area);
}

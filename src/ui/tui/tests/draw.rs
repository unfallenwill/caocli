//! Tests for `draw_if_changed` and the other draw branches: the path that
//! paints the screen only when something on it has moved (so a session that
//! has not changed does not pay for a paint), and the two cases nothing else
//! covers -- a picker forcing the window to follow the end, and the cursor
//! being clipped to the field when a long line pushes it past the right edge.

use std::cell::Cell as CellCount;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::backend::{Backend, TestBackend};
use tokio::sync::oneshot;

use crate::ui::cell::Style;

use super::super::notice::MachineNotice;
use super::super::screen::Screen;
use super::super::screen::fullscreen;
use super::super::state::State;
use super::screen_for_test;

#[test]
fn a_draw_lays_out_only_what_arrived_since_the_last_one() {
    // What is pinned here is invisible on the screen -- a draw that wrapped
    // the whole session would draw the same picture -- and it is the whole
    // point of the layout being kept: a session is as long as the
    // conversation has been, and a fragment arrives many times a second.
    let mut screen = screen_for_test(40, 20);
    screen
        .state
        .view
        .transcript
        .push(crate::ui::cell::Cell::Content("one".into()));
    screen
        .state
        .view
        .transcript
        .push(crate::ui::cell::Cell::Content("two".into()));
    screen.state.view.laid_cells = 0;
    screen.draw().unwrap();
    assert_eq!(screen.state.view.laid_cells, 2, "both of them, once");
    screen.draw().unwrap();
    assert_eq!(screen.state.view.laid_cells, 2, "and not again");

    // A fragment of a running turn is not a cell yet: the block it is writing
    // is wrapped as it is drawn, and it becomes a cell -- one to lay out --
    // when it closes.
    screen.state.apply(MachineNotice::Content("three".into()));
    screen.draw().unwrap();
    assert_eq!(screen.state.view.laid_cells, 2, "still the two cells");
    screen.state.apply(MachineNotice::FinishTurn);
    screen.draw().unwrap();
    assert_eq!(screen.state.view.laid_cells, 3, "the block it left behind");

    // A resize re-lays the whole transcript: every line was wrapped to a
    // width, so a new width is a new layout of every cell there is.
    screen.terminal.backend_mut().resize(30, 20);
    screen.draw().unwrap();
    assert_eq!(
        screen.state.view.laid_cells, 6,
        "all three again, at the new width"
    );
}

#[test]
fn everything_that_changes_the_screen_moves_the_revision() {
    let mut state = State::default();
    let mut moved = Vec::new();
    let mut step = |state: &State| moved.push(state.revision);
    step(&state);
    state.apply(MachineNotice::Content("hello".into()));
    step(&state);
    state.key(Event::Key(KeyEvent::from(KeyCode::Char('h'))));
    step(&state);
    let (reply, _answer) = oneshot::channel();
    state.open_question(reply);
    step(&state);
    state.close_question();
    step(&state);
    let mut sorted = moved.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, moved, "every step moved it: {moved:?}");
}

/// A backend that counts the frames it is asked to paint, so that a draw the
/// screen decided to skip is something a test can observe rather than infer.
struct Counting {
    inner: TestBackend,
    frames: Rc<CellCount<usize>>,
}

impl Backend for Counting {
    type Error = Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), Infallible>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.frames.set(self.frames.get() + 1);
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), Infallible> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), Infallible> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(&mut self) -> Result<ratatui::layout::Position, Infallible> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> Result<(), Infallible> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), Infallible> {
        self.inner.clear()
    }

    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> Result<(), Infallible> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<ratatui::layout::Size, Infallible> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<ratatui::backend::WindowSize, Infallible> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), Infallible> {
        self.inner.flush()
    }
}

#[test]
fn an_unchanged_screen_is_not_drawn_again() {
    let frames = Rc::new(CellCount::new(0));
    let backend = Counting {
        inner: TestBackend::new(40, 20),
        frames: frames.clone(),
    };
    let mut screen = Screen {
        terminal: fullscreen(backend).unwrap(),
        state: State::default(),
        tty: None,
        drawn: None,
    };
    screen.draw_if_changed().unwrap();
    assert_eq!(frames.get(), 1);
    screen.draw_if_changed().unwrap();
    assert_eq!(frames.get(), 1, "nothing changed, so nothing was drawn");
    // A keystroke changes what the box holds, so the next draw happens.
    screen
        .state
        .key(Event::Key(KeyEvent::from(KeyCode::Char('h'))));
    screen.draw_if_changed().unwrap();
    assert_eq!(frames.get(), 2);
    // So does a turn starting: the box's placeholder says so, which is a
    // change the revision has to carry.
    screen.state.begin_turn(Instant::now());
    screen.draw_if_changed().unwrap();
    assert_eq!(frames.get(), 3);
}

#[test]
fn closing_a_turn_is_worth_a_draw() {
    // The turn's end is the one thing that changes the screen without a
    // notice arriving, so it has to move the revision itself.
    let mut screen = screen_for_test(40, 20);
    screen.state.stream(Style::Plain, "half a line");
    screen.draw_if_changed().unwrap();
    let drawn = screen.state.revision;
    screen.commit();
    assert_ne!(screen.state.revision, drawn, "the draw cannot be skipped");
}

/// A picker always stands over the bottom of the transcript, and the window
/// follows the end of the session while it does -- a reader who has scrolled
/// back loses the scroll the moment a menu opens, because the menu is being
/// chosen against the lines the picker sits on.
///
/// This is the `else` branch in `draw_at`: the window otherwise answers
/// `state.window(...)`, but a non-empty picker takes the floor with
/// `state.follow()`. The arithmetic is `total.saturating_sub(room)`, which
/// becomes "the last `room` lines", regardless of where `state.view.scroll.back`
/// was pointing before the menu opened.
#[test]
fn a_picker_forces_the_window_to_the_end_regardless_of_scroll() {
    let mut screen = screen_for_test(40, 20);
    let rows = super::transcript_rows(20, super::super::layout::BOX_ROWS, 0) as usize;
    for i in 0..(rows * 3) {
        screen
            .state
            .view
            .transcript
            .push(crate::ui::cell::Cell::Notice(format!("line {i}")));
    }
    // The first draw is what teaches the scroll how many rows the transcript
    // has to page over: without `drawn_rows`, a PageUp has nothing to clamp
    // against and the scroll never moves.
    screen.draw().unwrap();
    for _ in 0..6 {
        super::press(&mut screen.state, KeyCode::PageUp);
    }
    screen.draw().unwrap();
    assert!(
        screen.state.view.scroll.back > 0,
        "the reader scrolled back"
    );
    assert!(
        super::body(&screen, 0).contains("line"),
        "and was reading a real line, not an edge report: {:?}",
        super::all_rows(&screen)
    );

    // Open the picker: the scroll is dropped, the window pins to the end.
    screen.state.open_choices(
        super::super::picker::Choosing::Model,
        super::super::picker::named_rows(vec![
            ("deepseek/deepseek-chat".into(), "current".into()),
            ("deepseek/deepseek-reasoner".into(), "$0.55/M".into()),
        ]),
    );
    screen.draw().unwrap();
    assert_eq!(
        screen.state.view.scroll.back, 0,
        "the picker pinned the window to the end"
    );
    // The newest line is the last one we put in, and the picker is the last
    // thing on the screen -- they meet.
    let last = screen.terminal.backend().buffer().area.height - 1;
    let total = rows * 3;
    assert!(
        super::body(&screen, 0).contains(&format!("line {}", total - rows)),
        "the window slid to the end: {:?}",
        super::all_rows(&screen)
    );
    // And the menu's row is on the screen.
    let rows = super::all_rows(&screen);
    assert!(
        rows.iter().any(|r| r.contains("deepseek/deepseek-chat")),
        "the picker is somewhere in the draw: {rows:?}"
    );
    let _ = last;
}

/// The input box's cursor is the terminal's, and `place_cursor` puts it on the
/// cell a character will land in. The cell is clipped to the field: a line
/// longer than the box has the cursor fall off the right, and the terminal's
/// caret has to stay inside the field or the next keystroke would land in a
/// column nothing drew.
///
/// The test backend leaves `get_cursor_position` at the default until
/// `set_cursor_position` is called on it, so a cursor the guard rejected is a
/// cursor the backend never heard from -- the only observable proof.
#[test]
fn a_cursor_past_the_right_edge_of_the_box_is_clipped() {
    let mut screen = screen_for_test(40, 20);
    super::type_in(&mut screen.state, &"x".repeat(60));
    screen.draw().unwrap();
    let pos = screen.terminal.get_cursor_position().unwrap();
    assert_eq!(
        (pos.x, pos.y),
        (0, 0),
        "the long line put the textarea cursor past the field, and the guard \
         kept `set_cursor_position` from being called: {pos:?}"
    );

    // The other side: a cursor that *is* inside the field does reach the backend.
    let mut screen = screen_for_test(40, 20);
    super::type_in(&mut screen.state, "hi");
    screen.draw().unwrap();
    let pos = screen.terminal.get_cursor_position().unwrap();
    let top = super::box_top(&screen);
    assert!(
        pos.y >= top && pos.y < top + super::super::layout::BOX_ROWS,
        "y={} inside the box rows {}..{}",
        pos.y,
        top,
        top + super::super::layout::BOX_ROWS
    );
    assert!(
        pos.x >= 3 && pos.x <= 5,
        "the cursor sat past 'hi', past the marker, and inside the field: x={}",
        pos.x
    );
}

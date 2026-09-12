//! Tests for `draw_if_changed`: the path that paints the screen only when
//! something on it has moved, so a session that has not changed does not
//! pay for a paint.

use std::cell::Cell as CellCount;
use std::convert::Infallible;
use std::rc::Rc;
use std::time::Instant;

use crossterm::event::{Event, KeyCode, KeyEvent};
use ratatui::backend::{Backend, TestBackend};
use tokio::sync::oneshot;

use crate::ui::cell::Style;

use super::super::notice::Notice;
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
        .transcript
        .push(crate::ui::cell::Cell::Content("one".into()));
    screen
        .state
        .transcript
        .push(crate::ui::cell::Cell::Content("two".into()));
    screen.state.laid_cells = 0;
    screen.draw().unwrap();
    assert_eq!(screen.state.laid_cells, 2, "both of them, once");
    screen.draw().unwrap();
    assert_eq!(screen.state.laid_cells, 2, "and not again");

    // A fragment of a running turn is not a cell yet: the block it is writing
    // is wrapped as it is drawn, and it becomes a cell -- one to lay out --
    // when it closes.
    screen.state.apply(Notice::Content("three".into()));
    screen.draw().unwrap();
    assert_eq!(screen.state.laid_cells, 2, "still the two cells");
    screen.state.apply(Notice::FinishTurn);
    screen.draw().unwrap();
    assert_eq!(screen.state.laid_cells, 3, "the block it left behind");

    // A resize re-lays the whole transcript: every line was wrapped to a
    // width, so a new width is a new layout of every cell there is.
    screen.terminal.backend_mut().resize(30, 20);
    screen.draw().unwrap();
    assert_eq!(
        screen.state.laid_cells, 6,
        "all three again, at the new width"
    );
}

#[test]
fn everything_that_changes_the_screen_moves_the_revision() {
    let mut state = State::default();
    let mut moved = Vec::new();
    let mut step = |state: &State| moved.push(state.revision);
    step(&state);
    state.apply(Notice::Content("hello".into()));
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

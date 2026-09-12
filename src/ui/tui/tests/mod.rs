//! Tests for the interactive front end, split by concern.
//!
//! Each submodule owns its tests and the helpers only it reaches for; the
//! harness here -- a screen with a `TestBackend` and the row-level readers
//! that go with one -- is shared by the tests in every submodule. Submodules
//! are listed by what they cover, not by alphabetic order, so the table of
//! contents reads the way a reader of `tui/` would scan the modules.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use super::super::cell::{self, Cell};
use super::input::Submitted;
use super::layout::screen_rows;
use super::screen::{Screen, fullscreen};
use super::state::State;
use std::time::{Duration, Instant};
use tokio::sync::watch;

// ----------------------------------------------------------------- harness ---

/// A front end drawing into memory instead of a terminal.
///
/// Every submodule uses this; the alternatives would be a single test backend
/// shared by every test (which would not let one test isolate its state) or
/// per-test setup that copied the boilerplate out. One builder, the same one
/// `Screen::enter` calls, is the smallest thing that works for both.
pub(super) fn screen_for_test(width: u16, height: u16) -> Screen<ratatui::backend::TestBackend> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    Screen {
        terminal: fullscreen(backend).unwrap(),
        state: State::default(),
        tty: None,
        drawn: None,
    }
}

/// One row of what was drawn, without the padding.
pub(super) fn row(screen: &Screen<ratatui::backend::TestBackend>, y: u16) -> String {
    let buf = screen.terminal.backend().buffer();
    (0..buf.area.width)
        .map(|x| buf[(x, y)].symbol())
        .collect::<String>()
        .trim_end()
        .to_owned()
}

/// A transcript row with its gutter taken off, for the tests that ask which
/// line is where rather than which columns it is set in.
///
/// The gutter itself is what the tests about the left edge are for: a test
/// about scrolling that had to name the marker would fail the day the marker
/// changed, and would have failed for a reason that has nothing to do with
/// scrolling.
pub(super) fn body(screen: &Screen<ratatui::backend::TestBackend>, y: u16) -> String {
    unset(&row(screen, y))
}

/// The same, for a test that already holds the drawn rows.
pub(super) fn unset(drawn: &str) -> String {
    // Skipped by character and not by column: every marker is one column, which
    // is the whole reason the gutters are one width.
    drawn.chars().skip(cell::MARKER_COLUMNS).collect()
}

/// Where the drawn area is. A full screen starts at the origin, and a test
/// asks rather than assumes: the frame is what says where the rows are.
pub(super) fn origin(screen: &mut Screen<ratatui::backend::TestBackend>) -> Rect {
    screen.terminal.get_frame().area()
}

/// Every row of the screen, so a test can ask whether something was drawn
/// without pinning down which row it landed on.
pub(super) fn all_rows(screen: &Screen<ratatui::backend::TestBackend>) -> Vec<String> {
    let height = screen.terminal.backend().buffer().area.height;
    (0..height).map(|y| row(screen, y)).collect()
}

/// The text of a rendered line, with whatever width it occupies.
pub(super) fn rendered(lines: &[ratatui::text::Line<'static>]) -> Vec<(String, usize)> {
    lines
        .iter()
        .map(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            let width = super::super::text::width(&text);
            (text, width)
        })
        .collect()
}

/// How many rows a test's transcript has on a terminal `height` rows tall,
/// with a box `input` rows tall and `queued` rows for the queue: the layout's
/// answer, asked the same way the screen asks it, so that a test that fills
/// the transcript fills the rows that are really there.
pub(super) fn transcript_rows(height: u16, input: u16, queued: u16) -> u16 {
    screen_rows(Rect::new(0, 0, 40, height), 0, input, queued)[0].height
}

/// The row the input box's top rule is drawn on, or the height when no rule was
/// drawn at all.
pub(super) fn box_top(screen: &Screen<ratatui::backend::TestBackend>) -> u16 {
    let height = screen.terminal.backend().buffer().area.height;
    (0..height)
        .find(|y| row(screen, *y).starts_with('\u{2500}'))
        .unwrap_or(height)
}

/// The cell a todo call leaves in the transcript, built the way the transcript
/// itself builds it: out of the call's own arguments.
pub(super) fn written(todos: serde_json::Value) -> Cell {
    Cell::tool_call("TodoWrite", &todos.to_string())
}

/// Type a key into a state, as if it were a key event from the terminal.
pub(super) fn press(state: &mut State, code: KeyCode) -> Submitted {
    state.key(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

/// A mouse event of a given kind. Where the pointer is does not matter: this
/// front end reads the kind and nothing else.
pub(super) fn mouse(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    })
}

/// Type a string into the box, one key at a time, the way a user would.
pub(super) fn type_in(state: &mut State, text: &str) {
    for c in text.chars() {
        press(state, KeyCode::Char(c));
    }
}

/// Ctrl-J: the newline key, and so the one the box has to make room for.
pub(super) fn ctrl_j() -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
}

/// Type into the box the way the event loop delivers a key while a turn runs.
pub(super) fn type_while_working(state: &mut State, text: &str, cancel: &watch::Sender<bool>) {
    for c in text.chars() {
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(c))), cancel);
    }
}

/// A state with a turn running, its clock and its stream pre-loaded.
pub(super) fn working(elapsed: Duration, chars: usize, chars_per_token: f64) -> State {
    State {
        turn_running: true,
        turn_started: Some(Instant::now() - elapsed),
        streamed_chars: chars,
        chars_since_usage: chars,
        chars_per_token,
        ..State::default()
    }
}

// ----------------------------------------------------------------- concerns ---

mod _review;
mod draw;
mod gate;
mod history;
mod input;
mod notice;
mod notifier;
mod panel;
mod picker;
mod pinned;
mod render;
mod transcript;
mod working;
mod wrap;

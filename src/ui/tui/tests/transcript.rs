//! Tests for the transcript's window onto the lines the cells wrap to.
//!
//! What this is about is scrolling: a session that has more lines than fit on
//! the screen, and what wheel / paging / scroll-back do to the view. The
//! window is held against the rows the layout actually gave the transcript,
//! so every test here either drives the keyboard and watches the visible
//! rows, or asks the layout's `Scroll` directly when the property is a
//! property of the arithmetic.

use crossterm::event::{KeyCode, MouseButton, MouseEventKind};

use super::super::input::Submitted;
use super::super::layout::{BOX_ROWS, Window, picker_window};
use super::super::paint::cell_lines;
use super::super::picker::PICKER_ROWS;
use super::super::render as render_mod;
use crate::ui::cell::Cell;

use super::super::state::WHEEL_LINES;
use super::all_rows;
use super::body;
use super::mouse;
use super::press;
use super::row;
use super::screen_for_test;
use super::transcript_rows;
use super::unset;

#[test]
fn the_transcript_fills_the_screen_and_shows_its_end() {
    // The whole screen is the transcript, less the pinned rows -- and what
    // does not fit is off the top, because the end is what was just written.
    let mut screen = screen_for_test(40, 20);
    let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
    for i in 0..(rows + 5) {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("line {i}")));
    }
    screen.draw().unwrap();
    assert_eq!(body(&screen, 0), "line 5", "the first five are off the top");
    assert_eq!(body(&screen, rows as u16 - 1), format!("line {}", rows + 4));
}

#[test]
fn paging_back_moves_the_window_and_paging_forward_returns_it() {
    let mut screen = screen_for_test(40, 20);
    let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
    for i in 0..(rows * 3) {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("line {i}")));
    }
    screen.draw().unwrap();
    let last = rows * 3;
    // A screen back: the window is the one above the end.
    press(&mut screen.state, KeyCode::PageUp);
    screen.draw().unwrap();
    assert_eq!(body(&screen, 0), format!("line {}", last - rows * 2));
    assert_eq!(
        body(&screen, rows as u16 - 1),
        format!("line {}", last - rows - 1)
    );
    // Forward again, one page at a time.
    press(&mut screen.state, KeyCode::PageDown);
    screen.draw().unwrap();
    assert_eq!(body(&screen, rows as u16 - 1), format!("line {}", last - 1));
    // And no further: there is nothing past the end.
    press(&mut screen.state, KeyCode::PageDown);
    screen.draw().unwrap();
    assert_eq!(body(&screen, rows as u16 - 1), format!("line {}", last - 1));
}

#[test]
fn a_wheel_notch_moves_the_window_three_lines() {
    let mut screen = screen_for_test(40, 20);
    let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
    for i in 0..(rows * 3) {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("line {i}")));
    }
    screen.draw().unwrap();
    let last = rows * 3;
    // Up: the window moves a notch, not a page.
    assert!(matches!(
        screen.state.key(mouse(MouseEventKind::ScrollUp)),
        Submitted::Nothing
    ));
    screen.draw().unwrap();
    assert_eq!(
        body(&screen, rows as u16 - 1),
        format!("line {}", last - 1 - WHEEL_LINES as usize)
    );
    // Down: back to where the writing ends, and no further, since there is
    // nothing past the end for the window to show.
    for _ in 0..2 {
        screen.state.key(mouse(MouseEventKind::ScrollDown));
    }
    screen.draw().unwrap();
    assert_eq!(body(&screen, rows as u16 - 1), format!("line {}", last - 1));
    assert_eq!(screen.state.scroll.back, 0, "following the end again");
}

#[test]
fn the_wheel_does_not_browse_the_history_the_box_holds() {
    // What this is here for: a terminal that has not been asked for the mouse
    // sends Up and Down in place of a notch, and both of those recall a line
    // -- which is a wheel that reads back through what was typed instead of
    // through what was said.
    let mut screen = screen_for_test(40, 20);
    let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
    for i in 0..(rows * 3) {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("line {i}")));
    }
    screen.draw().unwrap();
    screen.state.history = vec!["look at src/main.rs".into()];
    screen.state.key(mouse(MouseEventKind::ScrollUp));
    assert!(
        screen.state.textarea.is_empty(),
        "the box was not the thing a notch moved"
    );
    assert!(screen.state.scroll.back > 0, "the transcript was");
}

#[test]
fn a_click_is_not_a_notch_and_moves_nothing() {
    let mut screen = screen_for_test(40, 20);
    let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
    for i in 0..(rows * 3) {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("line {i}")));
    }
    screen.draw().unwrap();
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
        MouseEventKind::Drag(MouseButton::Left),
        MouseEventKind::Moved,
        MouseEventKind::ScrollLeft,
        MouseEventKind::ScrollRight,
    ] {
        assert!(matches!(screen.state.key(mouse(kind)), Submitted::Nothing));
    }
    assert_eq!(screen.state.scroll.back, 0, "the window did not move");
    assert!(screen.state.textarea.is_empty());
}

#[test]
fn the_wheel_reads_back_while_a_turn_runs() {
    // A turn is when there is most to read: the window is the one thing a key
    // pressed during one is allowed to move.
    let mut screen = screen_for_test(40, 20);
    let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
    for i in 0..(rows * 3) {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("line {i}")));
    }
    screen.draw().unwrap();
    let (cancel, _cancelled) = tokio::sync::watch::channel(false);
    screen
        .state
        .key_while_working(mouse(MouseEventKind::ScrollUp), &cancel);
    assert!(screen.state.scroll.back > 0, "the window moved");
    assert!(
        screen.state.textarea.is_empty(),
        "and the box stayed out of it"
    );
}

#[test]
fn lines_arriving_do_not_move_a_reader_who_scrolled_back() {
    // A turn keeps writing while the user reads what came before: the window
    // has to stay on the line they were on instead of sliding to the end
    // under them.
    let mut screen = screen_for_test(40, 20);
    let rows = transcript_rows(20, BOX_ROWS, 0) as usize;
    for i in 0..(rows * 3) {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("line {i}")));
    }
    screen.draw().unwrap();
    press(&mut screen.state, KeyCode::PageUp);
    screen.draw().unwrap();
    let was = row(&screen, 0);
    for i in 0..5 {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("more {i}")));
    }
    screen.draw().unwrap();
    assert_eq!(row(&screen, 0), was, "still looking at the same line");
}

#[test]
fn submitting_a_line_returns_to_the_end_of_the_transcript() {
    let mut screen = screen_for_test(40, 20);
    for i in 0..(transcript_rows(20, BOX_ROWS, 0) as usize * 2) {
        screen
            .state
            .transcript
            .push(Cell::Notice(format!("line {i}")));
    }
    screen.draw().unwrap();
    press(&mut screen.state, KeyCode::PageUp);
    assert!(screen.state.scroll.back > 0, "scrolled back");
    screen.state.submit("look at this");
    assert_eq!(screen.state.scroll.back, 0, "back to where it is written");
}

#[test]
fn a_long_line_is_wrapped_into_the_window_not_clipped() {
    // The transcript is drawn into a region of a fixed width, where an
    // over-long line would otherwise be cut off at the edge.
    let mut screen = screen_for_test(10, 30);
    let long = "abcdefghijklmnopqrstuvwxyz"; // 26 columns at width 10
    screen.state.transcript.push(Cell::Notice(long.into()));
    screen.draw().unwrap();
    let rows = all_rows(&screen);
    let joined: String = rows[..transcript_rows(30, BOX_ROWS, 0) as usize]
        .iter()
        .map(|r| unset(r))
        .collect();
    assert_eq!(joined, long, "every column survived, in order");
}

#[test]
fn a_turn_stays_on_screen_when_it_ends() {
    // Nothing is handed to the terminal's scrollback any more, so what is
    // drawn has to outlive the turn that produced it.
    let mut screen = screen_for_test(40, 20);
    screen.state.transcript.push(Cell::Notice("first".into()));
    screen.state.transcript.push(Cell::Notice("second".into()));
    screen.commit();
    screen.draw().unwrap();
    let rows: Vec<String> = all_rows(&screen).iter().map(|r| unset(r)).collect();
    let first = rows.iter().position(|r| r == "first").expect("still there");
    assert_eq!(rows[first + 1], "second", "in order, on the next row");
}

#[test]
fn an_empty_turn_commits_nothing() {
    let mut screen = screen_for_test(40, 20);
    screen.draw().unwrap();
    let before = all_rows(&screen);
    screen.commit();
    screen.draw().unwrap();
    assert_eq!(all_rows(&screen), before);
}

#[test]
fn committing_closes_the_block_that_was_still_streaming() {
    // `commit` ends the turn, so the block open at that moment becomes a
    // finished cell: the next turn's first fragment must open one of its own.
    let mut screen = screen_for_test(40, 20);
    screen
        .state
        .stream(crate::ui::cell::Style::Plain, "half a line");
    screen.commit();
    assert!(screen.state.live.is_none(), "nothing left open");
    screen
        .state
        .stream(crate::ui::cell::Style::Plain, " and the rest");
    screen.commit();
    assert_eq!(
        screen.state.transcript,
        vec![
            Cell::Content("half a line".into()),
            Cell::Content(" and the rest".into()),
        ],
        "one cell per turn, not one for the two of them"
    );
}

#[test]
fn a_window_holds_the_row_it_is_scrolled_to() {
    // What a window costs the screen: the rows it draws, plus a row for each
    // count it carries. A window costing more than the room it was given is a
    // window that eats the rows behind it.
    let cost =
        |w: &Window| (w.last - w.first) + usize::from(w.above > 0) + usize::from(w.below > 0);
    // The longest run of rows that holds the selection and fits the room with
    // the counts it is cut at, found by looking at every run there is rather
    // than by the arithmetic the front end uses: the two have to agree.
    let expected = |total: usize, selected: usize, room: usize| -> Window {
        let mut best: Option<Window> = None;
        for shown in 1..=room.min(total) {
            for first in 0..=total - shown {
                let last = first + shown;
                if !(first <= selected && selected < last) {
                    continue;
                }
                let window = Window {
                    first,
                    last,
                    above: first,
                    below: total - last,
                };
                if cost(&window) > room {
                    continue;
                }
                // Longer runs win, and runs of a length are looked at left to
                // right, so the first one wins: the selection sits at the end
                // of the window it moved into.
                if best.is_none_or(|b| shown > b.last - b.first) {
                    best = Some(window);
                }
            }
        }
        // No run fits even with no count at all: the selection alone, which is
        // what the front end falls back to as well.
        best.unwrap_or(Window {
            first: selected,
            last: selected + 1,
            above: 0,
            below: 0,
        })
    };
    for total in 1..24usize {
        for selected in 0..total {
            for room in 1..=PICKER_ROWS {
                let got = picker_window(total, selected, room);
                let what = format!("{total} rows, {selected} selected, room {room}");
                assert_eq!(got, expected(total, selected, room), "{what}");
                assert!(
                    got.first <= selected && selected < got.last,
                    "{what}: selection"
                );
                assert!(got.last <= total, "{what}: past the end");
                assert!(cost(&got) <= room, "{what}: rows drawn");
                // Either both counts are drawn and they are the truth, or
                // neither is: and neither only where they would not fit.
                if cost(&got) > got.last - got.first {
                    assert_eq!(
                        (got.above, got.below),
                        (got.first, total - got.last),
                        "{what}: what the counts say"
                    );
                } else if total > room {
                    assert!(cost(&got) + 2 > room, "{what}: a cut with no count");
                }
            }
        }
    }
}

#[test]
fn a_window_is_the_same_lines_as_the_transcript_it_is_a_window_on() {
    // Keeping the layout is a way of getting the same lines, not a second way
    // of deciding them: what the window hands the painter has to be the lines
    // the same cells wrap to at the width they were laid at.
    let mut screen = screen_for_test(24, 12);
    for i in 0..20 {
        screen
            .state
            .transcript
            .push(Cell::Content(format!("line {i}")));
    }
    screen.draw().unwrap();
    let width = 24;
    let whole: Vec<ratatui::text::Line> = screen
        .state
        .transcript
        .iter()
        .flat_map(|cell| cell_lines(cell, width))
        .collect();
    assert_eq!(
        render_mod::lines(&mut screen.state, width),
        whole,
        "the whole transcript"
    );
    assert_eq!(
        render_mod::window_lines(&screen.state, 3, 9, &[], &[]),
        whole[3..9].to_vec(),
        "and a window in the middle of one cell"
    );
}

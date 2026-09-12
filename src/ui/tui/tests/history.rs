//! Tests for the history: the lines the user has typed, recalled by the up
//! and down keys while the box is otherwise empty.
//!
//! The box and the picker are the two consumers of these keys, and the rule
//! is that they never compete: the picker is open only while a command is
//! being named, the history opens after.

use crossterm::event::KeyCode;

use super::super::state::State;
use super::press;
use super::type_in;

#[test]
fn up_and_down_reach_the_history_once_the_picker_is_closed() {
    // The two never compete: the picker is only open while a command is
    // being named, and browsing closes it.
    let mut screen = State::default();
    screen.remember("first thing");
    screen.remember("second thing");
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.text(), "second thing", "the most recent first");
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.text(), "first thing");
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.text(), "first thing", "the oldest is the end");
    press(&mut screen, KeyCode::Down);
    assert_eq!(screen.text(), "second thing");
    press(&mut screen, KeyCode::Down);
    assert_eq!(screen.text(), "", "past the newest is the empty draft");
}

#[test]
fn browsing_gives_back_what_was_being_typed() {
    let mut screen = State::default();
    screen.remember("old line");
    type_in(&mut screen, "half a thought");
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.text(), "old line");
    press(&mut screen, KeyCode::Down);
    assert_eq!(screen.text(), "half a thought", "the draft came back");
}

#[test]
fn browsing_an_empty_history_does_nothing() {
    let mut screen = State::default();
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.text(), "");
    assert!(screen.browsing.is_none());
}

#[test]
fn a_recalled_command_does_not_open_the_picker() {
    // It would take the very keys being used to browse.
    let mut screen = State::default();
    screen.remember("/help");
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.text(), "/help");
    assert!(screen.picker.is_none());
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.text(), "/help", "still browsing, not completing");
}

#[test]
fn repeating_a_line_is_not_recorded_twice() {
    let mut screen = State::default();
    screen.remember("same");
    screen.remember("same");
    screen.remember("other");
    screen.remember("same");
    assert_eq!(screen.history, vec!["same", "other", "same"]);
    screen.remember("");
    assert_eq!(screen.history.len(), 3, "an empty line is not a line");
}

#[test]
fn the_history_keeps_only_the_most_recent_entries() {
    let mut screen = State::default();
    for i in 0..(crate::history::MAX_ENTRIES + 5) {
        screen.remember(&format!("line {i}"));
    }
    assert_eq!(screen.history.len(), crate::history::MAX_ENTRIES);
    assert_eq!(screen.history[0], "line 5", "the oldest went first");
}

#[test]
fn submitting_empties_the_box_and_the_browsing_state() {
    let mut screen = State::default();
    screen.remember("earlier");
    press(&mut screen, KeyCode::Up);
    assert_eq!(screen.text(), "earlier");
    assert_eq!(screen.take_line(), "earlier");
    assert!(screen.text().is_empty());
    assert!(screen.browsing.is_none());
    assert!(screen.draft.is_empty());
}

//! Tests for the keys.
//!
//! What the front end does with the key stream is what it is for, and the
//! tests here cover the three states a key finds the box in: the prompt is
//! idle, a turn is running and the box holds the next line, and a gate is
//! open and the box is the answer's. Each state has its own rules about
//! which keys move the transcript and which are dropped or queued.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::watch;

use super::super::input::Submitted;
use super::super::state::State;
use super::ctrl_j;
use super::press;
use super::row;
use super::transcript_top;
use super::type_in;
use super::type_while_working;

#[test]
fn ctrl_j_makes_the_box_a_line_taller() {
    // The box is the draft's shape: the line Ctrl-J adds has a row to be typed
    // on, and the transcript gives one up for it.
    let mut screen = super::screen_for_test(40, 20);
    type_in(&mut screen.state, "first");
    screen.draw().unwrap();
    let last = screen.terminal.backend().buffer().area.height - 1;
    assert!(
        row(&screen, last - 3).starts_with('─'),
        "one line, three rows"
    );

    screen.state.key(ctrl_j());
    type_in(&mut screen.state, "second");
    screen.draw().unwrap();
    assert!(
        row(&screen, last - 4).starts_with('─'),
        "two lines, four rows"
    );
    assert!(row(&screen, last - 3).contains("first"), "the first line");
    assert!(row(&screen, last - 2).contains("second"), "and the second");
    assert!(
        row(&screen, last - 1).starts_with('─'),
        "the bottom stays put"
    );
}

#[test]
fn a_submitted_line_gives_its_rows_back_to_the_transcript() {
    // The box is as tall as the draft and no taller: what a multi-line line
    // borrowed goes back when the line is sent.
    let mut screen = super::screen_for_test(40, 20);
    let last = screen.terminal.backend().buffer().area.height - 1;
    type_in(&mut screen.state, "first");
    screen.state.key(ctrl_j());
    type_in(&mut screen.state, "second");
    screen.draw().unwrap();
    assert!(
        row(&screen, last - 4).starts_with('─'),
        "two lines, four rows"
    );

    screen.state.take_line();
    screen.draw().unwrap();
    assert!(row(&screen, last - 3).starts_with('─'), "empty, three rows");
}

#[test]
fn a_draft_that_has_lost_lines_is_drawn_from_its_first_one() {
    // A draft taller than the box scrolls inside it. When lines are deleted
    // the box gets shorter with them, and the rows it was scrolled to must not
    // hide the top of what is left -- the editor would otherwise draw from
    // the row it remembered and leave the rest of the box blank.
    let height = 12;
    let shows = usize::from(super::super::layout::box_rows(usize::MAX, height, 0)) - 2;
    let (draft, kept) = (shows + 4, shows - 2);
    let letters: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().take(draft).collect();
    let mut screen = super::screen_for_test(40, height);
    for (i, letter) in letters.iter().enumerate() {
        if i > 0 {
            screen.state.key(ctrl_j());
        }
        type_in(&mut screen.state, &letter.to_string());
    }
    screen.draw().unwrap();

    // Each line is a letter and a newline, so this leaves the first `kept` of
    // them and the cursor on the last.
    for _ in 0..(2 * (draft - kept)) {
        press(&mut screen.state, KeyCode::Backspace);
    }
    screen.draw().unwrap();

    // The box is ruled off above and below, so its lines are what lies between
    // the rules: the first line is the row after the top one.
    let top = 1 + super::all_rows(&screen)
        .iter()
        .position(|r| r.starts_with('─'))
        .expect("the box is drawn");
    for (i, letter) in letters[..kept].iter().enumerate() {
        let drawn = super::body(&screen, (top + i) as u16);
        assert!(drawn.starts_with(*letter), "{drawn:?}");
    }
    assert!(
        row(&screen, (top + kept) as u16).starts_with('─'),
        "and nothing below the last line"
    );
}

#[test]
fn escape_while_a_turn_runs_gives_the_queue_line_back() {
    let mut screen = State::default();
    screen.begin_turn(std::time::Instant::now());
    type_in(&mut screen, "/s");
    press(&mut screen, KeyCode::Esc);
    assert!(screen.picker.is_none());
    assert_eq!(
        screen.textarea.placeholder_text(),
        super::super::input::QUEUE_PLACEHOLDER,
        "the box still belongs to the turn that is running"
    );
}

#[test]
fn what_the_user_says_becomes_part_of_the_transcript() {
    // Replay reads the user's line out of the log, so a live turn has to put it
    // in the same place: the same session must not read two ways depending on
    // when it was looked at.
    let mut state = State::default();
    state.submit("look at src/main.rs");
    assert_eq!(
        state.transcript,
        vec![crate::ui::cell::Cell::user("look at src/main.rs")]
    );
    assert_eq!(state.history, vec!["look at src/main.rs"]);
}

#[test]
fn a_command_is_not_part_of_the_transcript() {
    // The handler answers commands without the model seeing them, so a replayed
    // session does not have them either.
    let mut state = State::default();
    state.submit("/help");
    assert!(state.transcript.is_empty(), "nothing to replay");
    assert_eq!(state.history, vec!["/help"], "but it is worth recalling");
}

#[test]
fn a_submitted_line_is_drawn_above_what_the_turn_says() {
    let mut screen = super::screen_for_test(40, 20);
    screen.state.submit("look at src/main.rs");
    screen
        .state
        .transcript
        .push(crate::ui::cell::Cell::Content("on it".into()));
    screen.draw().unwrap();
    let top = transcript_top(&screen, 2);
    assert_eq!(row(&screen, top), "› look at src/main.rs");
    assert_eq!(row(&screen, top + 1), "on it");
}

#[test]
fn a_line_typed_while_a_turn_runs_is_queued_and_not_dropped() {
    // A turn is not the place to start anything -- but it is the place to say
    // what should follow it, and that is what the box is for while it runs.
    let mut state = State::default();
    let (cancel, cancelled) = watch::channel(false);
    type_while_working(&mut state, "the next thing", &cancel);
    assert_eq!(state.textarea.lines(), ["the next thing"]);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(state.queued, ["the next thing"]);
    assert!(
        state.textarea.is_empty(),
        "the box is freed for the one after"
    );
    assert!(
        state.transcript.is_empty(),
        "nothing has run, so nothing is part of the session yet"
    );
    assert!(!*cancelled.borrow(), "queuing is not cancelling");
}

#[test]
fn a_key_meant_for_the_prompt_is_dropped_only_where_it_would_leave() {
    // Ctrl-D leaves the session at the prompt, and a turn in flight is not the
    // place to leave from: it is dropped, and the turn goes on.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    state.key_while_working(
        Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
        &cancel,
    );
    assert!(state.queued.is_empty(), "not queued as a line");
    assert!(state.textarea.is_empty(), "and not typed into the box");
}

#[test]
fn the_cancel_key_is_still_the_cancel_key_while_a_line_is_being_queued() {
    let mut state = State::default();
    let (cancel, cancelled) = watch::channel(false);
    type_while_working(&mut state, "half a thought", &cancel);
    state.key_while_working(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &cancel,
    );
    assert!(*cancelled.borrow(), "Ctrl-C cancels the turn");
    assert!(state.queued.is_empty(), "and is not a line of its own");
    assert_eq!(
        state.textarea.lines(),
        ["half a thought"],
        "what was being typed is still there: the cancellation is the turn's, \
         not the box's"
    );
}

#[test]
fn the_queue_runs_from_its_head() {
    let mut state = State::default();
    state.enqueue("first".into());
    state.enqueue("second".into());
    assert_eq!(state.dequeue().as_deref(), Some("first"));
    assert_eq!(state.dequeue().as_deref(), Some("second"));
    assert_eq!(state.dequeue(), None, "and then the keyboard is waited on");
}

#[test]
fn enter_submits_only_when_there_is_something_to_submit() {
    let mut screen = State::default();
    let enter = Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert!(matches!(screen.key(enter), Submitted::Nothing));
    screen.textarea.insert_str("hello");
    assert!(matches!(
        screen.key(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE
        ))),
        Submitted::Line
    ));
    let taken = screen.take_line();
    assert_eq!(taken, "hello");
    assert!(
        screen.textarea.is_empty(),
        "the box is ready for the next line"
    );
}

#[test]
fn ctrl_j_is_left_to_the_box_so_input_can_be_multiline() {
    let mut screen = State::default();
    screen.textarea.insert_str("first");
    screen.key(Event::Key(KeyEvent::new(
        KeyCode::Char('j'),
        KeyModifiers::CONTROL,
    )));
    screen.textarea.insert_str("second");
    assert_eq!(screen.take_line(), "first\nsecond");
}

#[test]
fn ctrl_c_clears_the_line_and_ctrl_d_on_an_empty_box_leaves() {
    let mut screen = State::default();
    screen.textarea.insert_str("half typed");
    screen.key(Event::Key(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
    )));
    assert!(screen.textarea.is_empty(), "Ctrl-C clears");

    let ctrl_d = || Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
    assert!(matches!(screen.key(ctrl_d()), Submitted::Exit));
    // ... but only on an empty box: otherwise it is just a keystroke.
    screen.textarea.insert_str("text");
    assert!(matches!(screen.key(ctrl_d()), Submitted::Nothing));
}

#[test]
fn ctrl_c_during_a_turn_cancels_it_and_nothing_else_does() {
    let mut screen = State::default();
    let (tx, rx) = watch::channel(false);
    screen.key_while_working(
        Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        &tx,
    );
    assert!(!*rx.borrow(), "only the cancel key cancels");
    screen.key_while_working(
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        &tx,
    );
    assert!(*rx.borrow(), "Ctrl-C during a turn cancels it");
}

#[test]
fn a_paste_lands_in_the_box_whole() {
    let mut screen = State::default();
    screen.key(Event::Paste("pasted\nlines".into()));
    assert_eq!(screen.take_line(), "pasted\nlines");
}

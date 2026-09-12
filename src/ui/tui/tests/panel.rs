//! Tests for the question panel.
//!
//! `AskUserQuestion` puts a list of options on the screen and asks the user
//! to pick one. The cursor starts on the first option, arrow keys move it,
//! digits jump to a numbered option, space toggles it in a multi-select
//! question, and `Enter` submits the answers. `Esc` dismisses the whole
//! thing.
//!
//! A panel that could only answer with its own options would be a panel that
//! cannot ask an open question: typing always wins over the cursor.

use crossterm::event::{Event, KeyCode, KeyEvent};
use tokio::sync::{oneshot, watch};

use crate::tools::ask::{Answer, Choice, Question};

use super::super::panel::PANEL_PLACEHOLDER;
use super::super::state::State;
use super::all_rows;

/// One question with two options, the shape most of these tests use.
fn two_options(multi_select: bool) -> Question {
    Question {
        id: "auth".into(),
        header: "Auth".into(),
        question: "Which auth should we use?".into(),
        options: vec![
            Choice {
                label: "JWT".into(),
                description: "one token for the API".into(),
            },
            Choice {
                label: "Session cookie".into(),
                description: String::new(),
            },
        ],
        multi_select,
    }
}

/// Open the panel the way the event loop does, and hand back what the answers
/// come back on.
fn open(state: &mut State, questions: Vec<Question>) -> oneshot::Receiver<Option<Vec<Answer>>> {
    let (reply, answers) = oneshot::channel();
    state.open_panel(questions, reply);
    answers
}

#[test]
fn the_cursor_and_enter_answer_a_question() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let answers = open(&mut state, vec![two_options(false)]);
    assert!(state.panel_open());
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answers.blocking_recv(),
        Ok(Some(vec![Answer {
            id: "auth".into(),
            labels: vec!["JWT".into()],
        }])),
        "the option under the cursor is the answer"
    );
    assert!(!state.panel_open(), "and the panel is over");
}

#[test]
fn the_arrow_keys_and_the_digits_move_the_cursor() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let answers = open(&mut state, vec![two_options(false)]);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Down)), &cancel);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char('1'))), &cancel);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answers.blocking_recv().unwrap().unwrap()[0].labels,
        vec!["JWT".to_owned()],
        "the digit picked the first option back"
    );

    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let answers = open(&mut state, vec![two_options(false)]);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Up)), &cancel);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answers.blocking_recv().unwrap().unwrap()[0].labels,
        vec!["Session cookie".to_owned()],
        "up from the first wraps to the last"
    );
}

#[test]
fn a_question_that_takes_several_answers_with_what_was_toggled() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let answers = open(&mut state, vec![two_options(true)]);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(' '))), &cancel);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Down)), &cancel);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(' '))), &cancel);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answers.blocking_recv().unwrap().unwrap()[0].labels,
        vec!["JWT".to_owned(), "Session cookie".to_owned()],
        "both toggled options, in the order they were offered"
    );
}

#[test]
fn a_question_that_takes_one_answer_ignores_the_space_key() {
    // Space is an option's toggle only where there is something to toggle: in a
    // single-choice question it is a character of the answer being typed.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let _answers = open(&mut state, vec![two_options(false)]);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(' '))), &cancel);
    assert_eq!(state.textarea.lines(), [" "], "it went into the box");
}

#[test]
fn a_typed_answer_wins_over_the_cursor() {
    // A panel that could only answer with its own options would be a panel that
    // cannot ask an open question.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let answers = open(&mut state, vec![two_options(false)]);
    for c in "both, actually".chars() {
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(c))), &cancel);
    }
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answers.blocking_recv().unwrap().unwrap()[0].labels,
        vec!["both, actually".to_owned()]
    );
}

#[test]
fn the_panels_keys_are_the_panels_only_while_the_box_is_empty() {
    // A digit and a space choose only while there is nothing typed: once the user
    // is answering in words, they are characters like any other, because a space
    // is a space in a sentence and a number can be part of one.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let answers = open(&mut state, vec![two_options(true)]);
    for c in "a 12".chars() {
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(c))), &cancel);
    }
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answers.blocking_recv().unwrap().unwrap()[0].labels,
        vec!["a 12".to_owned()]
    );
}

#[test]
fn escape_dismisses_every_question() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let answers = open(&mut state, vec![two_options(false), two_options(false)]);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Esc)), &cancel);
    assert_eq!(answers.blocking_recv(), Ok(None));
    assert!(!state.panel_open());
}

#[test]
fn the_questions_are_asked_one_at_a_time() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let mut second = two_options(false);
    second.id = "store".into();
    second.question = "Where should it live?".into();
    let answers = open(&mut state, vec![two_options(false), second]);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert!(state.panel_open(), "there is another question to answer");
    assert!(
        state.textarea.is_empty(),
        "the box starts the next one empty"
    );
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Down)), &cancel);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answers.blocking_recv().unwrap().unwrap(),
        vec![
            Answer {
                id: "auth".into(),
                labels: vec!["JWT".into()],
            },
            Answer {
                id: "store".into(),
                labels: vec!["Session cookie".into()],
            },
        ]
    );
}

#[test]
fn an_open_question_with_nothing_typed_is_left_blank() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let answers = open(
        &mut state,
        vec![Question {
            id: "note".into(),
            header: String::new(),
            question: "Anything else?".into(),
            options: Vec::new(),
            multi_select: false,
        }],
    );
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answers.blocking_recv().unwrap().unwrap()[0].labels,
        Vec::<String>::new(),
        "which the result says as (no answer), rather than making one up"
    );
}

#[test]
fn the_line_being_written_is_held_while_the_questions_are_open() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    super::type_while_working(&mut state, "a sentence I was writing", &cancel);
    let _answers = open(&mut state, vec![two_options(false)]);
    assert!(
        state.textarea.is_empty(),
        "the box is the answer's while the questions are open"
    );
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Esc)), &cancel);
    assert_eq!(
        state.textarea.lines(),
        ["a sentence I was writing"],
        "and the draft comes back untouched"
    );
}

#[test]
fn the_line_being_written_comes_back_when_the_last_question_is_answered() {
    // Answering the last question closes the panel, and closing it is what gives
    // the box back: an answer typed into the next question's box would land on
    // top of the line its owner was still writing.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    super::type_while_working(&mut state, "a sentence I was writing", &cancel);
    let _answers = open(&mut state, vec![two_options(false)]);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert!(!state.panel_open());
    assert_eq!(state.textarea.lines(), ["a sentence I was writing"]);
}

#[test]
fn a_panel_left_open_when_the_turn_ends_answers_nothing() {
    let mut state = State::default();
    let answers = open(&mut state, vec![two_options(false)]);
    state.close_panel();
    assert!(!state.panel_open());
    assert!(
        answers.blocking_recv().is_err(),
        "the reply handle went with the panel: nobody answered"
    );
}

#[test]
fn the_box_says_what_it_is_for_while_a_question_is_open() {
    let mut state = State::default();
    let _answers = open(&mut state, vec![two_options(false)]);
    assert_eq!(state.placeholder(), PANEL_PLACEHOLDER);
}

#[test]
fn the_panel_draws_the_question_its_options_and_the_keys() {
    let mut screen = super::screen_for_test(60, 20);
    let (reply, _answers) = oneshot::channel();
    screen.state.open_panel(vec![two_options(true)], reply);
    screen.draw().unwrap();
    let rows = all_rows(&screen);
    assert!(
        rows.iter().any(|r| r == "Auth · question 1 of 1"),
        "the heading: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r == "Which auth should we use? · choose any"),
        "the question: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r == "❯   1. JWT — one token for the API"),
        "the cursor's row, with the mark column still empty: {rows:?}"
    );
    assert!(
        rows.iter().any(|r| r == "    2. Session cookie"),
        "the other option, with no description to show: {rows:?}"
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("↑/↓ move · space toggles · type to answer")),
        "and the keys are said: {rows:?}"
    );
}

#[test]
fn a_toggled_option_is_drawn_as_chosen() {
    let mut screen = super::screen_for_test(60, 20);
    let (reply, _answers) = oneshot::channel();
    screen.state.open_panel(vec![two_options(true)], reply);
    let (cancel, _cancelled) = watch::channel(false);
    screen
        .state
        .key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(' '))), &cancel);
    screen.draw().unwrap();
    let rows = all_rows(&screen);
    assert!(
        rows.iter().any(|r| r.contains("❯ ✓ 1. JWT")),
        "the cursor is on it and it is chosen: {rows:?}"
    );
}

//! Tests for the approval gate and the secret prompt.
//!
//! Both ask a question that is answered from the box while a turn runs. The
//! gate's question is yes-or-no; the secret's is a string that is masked
//! while it is being typed, never reaches the transcript, and is answered
//! with the empty line to mean "cancelled".
//!
//! The two share the box; what is here is what each one does to the line
//! that was being typed before it asked, and what each does to the line
//! the user types as the answer.

use crossterm::event::{Event, KeyCode, KeyEvent};
use tokio::sync::{oneshot, watch};

use crate::ui::Verdict;

use super::super::input::{SECRET_MASK, SECRET_PLACEHOLDER};
use super::super::notice::Notice;
use super::super::state::State;
use super::all_rows;
use super::row;
use super::type_while_working;

#[test]
fn the_gate_is_answered_by_typing_at_it_while_the_turn_runs() {
    // The answer is typed while a turn runs, where the box is otherwise the
    // next line's: this is the one path where a keystroke is not the queue's,
    // and it has to work -- an answer that never arrives leaves the turn
    // waiting on a question nobody can see.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let (reply, answer) = oneshot::channel();
    state.open_question(reply);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char('y'))), &cancel);
    assert_eq!(state.textarea.lines(), ["y"], "it went into the box");
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(answer.blocking_recv(), Ok(Verdict::Allowed));
    assert!(state.reply.is_none(), "the gate is closed again");
}

#[test]
fn a_blank_answer_to_the_gate_denies() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let (reply, answer) = oneshot::channel();
    state.open_question(reply);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(answer.blocking_recv(), Ok(Verdict::Denied));
}

#[test]
fn anything_that_is_not_yes_denies() {
    for typed in ["", "n", "no", "maybe"] {
        let mut screen = State::default();
        let (reply, answer) = oneshot::channel();
        screen.open_question(reply);
        screen.textarea.insert_str(typed);
        screen.close_question();
        assert_eq!(
            answer.blocking_recv(),
            Ok(Verdict::Denied),
            "typed {typed:?}"
        );
    }
}

#[test]
fn a_question_is_answered_by_what_the_user_submits() {
    let mut screen = State::default();
    let (reply, answer) = oneshot::channel();
    screen.open_question(reply);
    assert!(screen.reply.is_some(), "the answer has somewhere to go");
    screen.textarea.insert_str("yes");
    screen.close_question();
    assert_eq!(answer.blocking_recv(), Ok(Verdict::Allowed));
    assert!(screen.question.is_none());
    assert!(screen.textarea.is_empty());
}

#[test]
fn a_line_being_typed_is_held_aside_while_the_gate_is_open() {
    // The answer to "run it?" is a `y`, and a sentence that was already in the
    // box is not one. The gate takes the box for its answer and gives it back.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    state.begin_turn(std::time::Instant::now());
    type_while_working(&mut state, "and then refactor", &cancel);
    let (reply, answer) = oneshot::channel();
    state.open_question(reply);
    assert!(
        state.textarea.is_empty(),
        "the answer starts from an empty box"
    );

    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char('y'))), &cancel);
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answer.blocking_recv(),
        Ok(Verdict::Allowed),
        "the gate got its answer"
    );
    assert_eq!(
        state.textarea.lines(),
        ["and then refactor"],
        "and the line came back"
    );
    assert!(state.queued.is_empty(), "it was never submitted");
}

#[test]
fn a_secret_is_typed_into_the_box_and_answered_with_what_was_typed() {
    // `/login`'s question. What goes back is the text, not the dots: the
    // masking is how it is drawn, and a box that answered with its own
    // drawing would send bullets to the provider.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let (reply, answer) = oneshot::channel();
    state.apply(Notice::Secret {
        prompt: "glm API key".into(),
        reply,
    });
    assert_eq!(
        state.textarea.mask_char(),
        Some(SECRET_MASK),
        "the text is not on the screen while it is typed"
    );
    type_while_working(&mut state, "sk-test", &cancel);
    assert_eq!(state.textarea.lines(), ["sk-test"], "the box holds it");
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(answer.blocking_recv(), Ok(Some("sk-test".to_owned())));
    assert!(
        state.queued.is_empty(),
        "an answer is not a line to run next"
    );
    assert!(state.reply.is_none(), "the question is closed");
    assert_eq!(state.textarea.mask_char(), None, "the box is a box again");
}

#[test]
fn an_empty_answer_cancels_a_secret() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    let (reply, answer) = oneshot::channel();
    state.apply(Notice::Secret {
        prompt: "glm API key".into(),
        reply,
    });
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(answer.blocking_recv(), Ok(None));
}

#[test]
fn the_answer_to_a_secret_never_reaches_the_transcript_or_the_queue() {
    // The whole reason it is a question rather than a line: a key typed at
    // the prompt would be in the session the moment it was submitted.
    let mut screen = super::screen_for_test(60, 20);
    let (cancel, _cancelled) = watch::channel(false);
    let (reply, answer) = oneshot::channel();
    screen.state.begin_turn(std::time::Instant::now());
    screen.state.apply(Notice::Secret {
        prompt: "glm API key".into(),
        reply,
    });
    type_while_working(&mut screen.state, "sk-do-not-keep-me", &cancel);
    screen.draw().unwrap();
    let drawn = all_rows(&screen).join("\n");
    assert!(
        drawn.contains("glm API key"),
        "the question is on the screen"
    );
    assert!(drawn.contains(SECRET_MASK), "masked, not in the clear");
    assert!(!drawn.contains("sk-do-not-keep-me"), "{drawn}");
    screen
        .state
        .key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        answer.blocking_recv(),
        Ok(Some("sk-do-not-keep-me".to_owned()))
    );
    assert!(
        screen.state.transcript.is_empty(),
        "nothing was written down"
    );
    assert!(screen.state.queued.is_empty());
}

#[test]
fn what_is_drawn_while_a_secret_is_asked_for_says_what_the_box_wants() {
    let mut screen = super::screen_for_test(60, 20);
    let (reply, _answer) = oneshot::channel();
    screen.state.apply(Notice::Secret {
        prompt: "glm API key".into(),
        reply,
    });
    screen.draw().unwrap();
    let last = screen.terminal.backend().buffer().area.height - 1;
    assert!(
        row(&screen, last - 2).contains(SECRET_PLACEHOLDER),
        "the box says what Enter does with it: {:?}",
        row(&screen, last - 2)
    );
}

#[test]
fn a_line_being_typed_is_held_aside_while_a_secret_is_open() {
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    state.begin_turn(std::time::Instant::now());
    type_while_working(&mut state, "and then refactor", &cancel);
    let (reply, _answer) = oneshot::channel();
    state.apply(Notice::Secret {
        prompt: "deepseek API key".into(),
        reply,
    });
    assert!(
        state.textarea.is_empty(),
        "the key starts from an empty box"
    );
    state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Enter)), &cancel);
    assert_eq!(
        state.textarea.lines(),
        ["and then refactor"],
        "the draft came back"
    );
    assert!(state.queued.is_empty());
}

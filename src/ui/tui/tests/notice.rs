//! Tests for how a notice becomes a cell, or a status change, or nothing.
//!
//! The mapping is the front end's whole job: a notice that had no effect would
//! silently swallow the machine's output, and what `apply` returns is the
//! front end's promise that something visible happened.

use std::time::Duration;

use super::super::notice::Notice;
use super::super::render;
use super::super::state::State;
use crate::types::Message;
use crate::types::Role;
use crate::types::Usage;
use crate::ui::cell::Cell;

#[test]
fn every_notice_becomes_a_cell_or_a_status_change() {
    // The mapping is the front end's whole job; a notice with no effect would
    // silently swallow the machine's output.
    let mut screen = State::default();
    screen.apply(Notice::Reasoning("think".into()));
    screen.apply(Notice::Content("answer".into()));
    screen.apply(Notice::FinishTurn);
    screen.apply(Notice::ToolStart {
        name: "Bash".into(),
        args: r#"{"command":"ls"}"#.into(),
    });
    screen.apply(Notice::ToolOutput("out\n".into()));
    screen.apply(Notice::ToolResult("exit_code: 0\nbody".into()));
    screen.apply(Notice::Info("note".into()));
    screen.apply(Notice::Error("boom".into()));
    screen.apply(Notice::Interrupted);
    assert_eq!(
        screen.transcript,
        vec![
            Cell::Reasoning("think".into()),
            Cell::Content("answer".into()),
            Cell::tool_call("Bash", r#"{"command":"ls"}"#),
            Cell::ToolOutput("out\n".into()),
            Cell::ToolResult("exit_code: 0\nbody".into()),
            Cell::Notice("note".into()),
            Cell::Failure("boom".into()),
            Cell::Interrupted,
        ]
    );
}

#[test]
fn a_fragment_in_the_other_style_opens_a_new_block() {
    let mut screen = State::default();
    screen.apply(Notice::Reasoning("a".into()));
    screen.apply(Notice::Reasoning("b".into()));
    assert!(screen.live.is_some(), "still one open block");
    screen.apply(Notice::Content("x".into()));
    assert_eq!(
        screen.transcript,
        vec![Cell::Reasoning("ab".into())],
        "the reasoning block closed when the style changed"
    );
    screen.apply(Notice::FinishTurn);
    assert_eq!(screen.transcript[1], Cell::Content("x".into()));
    assert!(screen.live.is_none());
}

#[test]
fn status_notices_reach_the_status_line() {
    let mut screen = State::default();
    screen.apply(Notice::SetModel("m-1".into()));
    screen.apply(Notice::SetEffort("high".into()));
    screen.apply(Notice::Usage(
        Usage {
            prompt_tokens: 6,
            total_tokens: 10,
            completion_tokens: 4,
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 4,
            prompt_tokens_details: None,
        },
        Duration::ZERO,
    ));
    assert_eq!(
        screen.status.full_line(),
        "m-1 · effort high · cache 60.0% · hit 6 · miss 4"
    );
    screen.apply(Notice::ResetStats);
    assert_eq!(
        screen.status.full_line(),
        "m-1 · effort high · cache 0.0% · hit 0 · miss 0"
    );
}

#[test]
fn replay_and_the_live_stream_produce_the_same_cells() {
    // The invariant the plain front end is held to, held here too: a resumed
    // session must look like the one that was watched live.
    let mut screen = State::default();
    screen.apply(Notice::Replay(vec![
        Message {
            role: Role::Assistant,
            content: Some("running it".into()),
            reasoning_content: Some("let me think".into()),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
        },
        Message::tool("call_1", "exit_code: 0\n--- stdout ---\nbody"),
    ]));
    assert_eq!(
        screen.transcript,
        vec![
            Cell::Reasoning("let me think".into()),
            Cell::Content("running it".into()),
            Cell::ToolResult("exit_code: 0\n--- stdout ---\nbody".into()),
        ]
    );
}

#[test]
fn an_open_question_is_drawn_after_the_transcript() {
    let mut screen = State::default();
    screen.apply(Notice::Content("before".into()));
    screen.apply(Notice::Approval {
        name: "Bash".into(),
        args: r#"{"command":"rm -rf /"}"#.into(),
    });
    let lines = render::lines(&mut screen, 80);
    assert_eq!(lines.len(), 2, "answer, then the question");
    assert!(format!("{:?}", lines[1]).contains("run it?"));
}

//! Tests for how a notice becomes a cell, or a status change, or nothing.
//!
//! The mapping is the front end's whole job: a notice that had no effect would
//! silently swallow the machine's output, and what `apply` returns is the
//! front end's promise that something visible happened.

use std::time::Duration;

use super::super::notice::{AppNotice, MachineNotice};
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
    screen.apply(MachineNotice::Reasoning("think".into()));
    screen.apply(MachineNotice::Content("answer".into()));
    screen.apply(MachineNotice::FinishTurn);
    screen.apply(MachineNotice::ToolStart {
        name: "Bash".into(),
        args: r#"{"command":"ls"}"#.into(),
    });
    screen.apply(MachineNotice::ToolOutput("out\n".into()));
    screen.apply(MachineNotice::ToolResult("exit_code: 0\nbody".into()));
    screen.apply_app(AppNotice::Info("note".into()));
    screen.apply_app(AppNotice::Error("boom".into()));
    screen.apply(MachineNotice::Interrupted);
    // ToolStart opens a Step; ToolOutput appends to its children; ToolResult
    // settles it. The transcript now has one Cell::Step where it used to
    // have three cells.
    let mut expected_step = Cell::from_tool_call("Bash", r#"{"command":"ls"}"#);
    if let Cell::Step(step) = &mut expected_step {
        step.push_output("out\n");
        step.settle("exit_code: 0\nbody");
    } else {
        panic!("from_tool_call should produce a Step for Bash");
    }
    // StepId and `started_at` are process-global state that does not
    // reset between assertions: borrow both from the actual screen so
    // the comparison only walks the fields that describe what happened
    // here (verb, subject, status, children, verdict). The Step lives
    // in the transcript for a side-effectful tool, and inside a
    // `Cell::Thought` body for a safe one (`Bash ls` is on the
    // whitelist).
    let actual_step = screen
        .view
        .transcript
        .iter()
        .find_map(|c| match c {
            Cell::Step(s) => Some(s.clone()),
            Cell::Thought(t) => t.body.iter().find_map(|b| match b {
                Cell::Step(s) => Some(s.clone()),
                _ => None,
            }),
            _ => None,
        })
        .expect("a Step should be in the transcript or in the thought body");
    let expected_step_borrowed = match expected_step {
        Cell::Step(mut s) => {
            s.id = actual_step.id;
            s.started_at = actual_step.started_at;
            Cell::Step(s)
        }
        _ => unreachable!("from_tool_call should produce a Step for Bash"),
    };
    // The transcript now opens with a folded `Cell::Thought` (the
    // reasoning region) instead of a `Cell::Reasoning`: the state
    // machine folds the run into one cell, so what the transcript
    // sees is its fold line. `Bash ls` is on the safe whitelist, so
    // its Step belongs to the Thought body, not the transcript; the
    // transcript after the Info/Error/Interrupted trail is the Thought
    // (folded) and then three session-level cells.
    let expected_thought = match &screen.view.transcript[0] {
        Cell::Thought(t) => t.clone(),
        other => panic!("transcript[0] should be a Thought, got {other:?}"),
    };
    // `Bash ls` is on the safe whitelist but no Thought was open when it
    // arrived -- the `Content`/`FinishTurn` pair closed the prior
    // reasoning region. The Step therefore lands in the transcript
    // directly, not in a body.
    assert_eq!(
        screen.view.transcript,
        vec![
            Cell::Thought(expected_thought),
            Cell::Content("answer".into()),
            expected_step_borrowed,
            Cell::Notice("note".into()),
            Cell::Failure("boom".into()),
            Cell::Interrupted,
        ]
    );
}

#[test]
fn a_fragment_in_the_other_style_opens_a_new_block() {
    let mut screen = State::default();
    screen.apply(MachineNotice::Reasoning("a".into()));
    screen.apply(MachineNotice::Reasoning("b".into()));
    assert!(
        screen.view.stream.current().is_some(),
        "still one open block"
    );
    screen.apply(MachineNotice::Content("x".into()));
    // The reasoning is folded into a `Cell::Thought`; the body has the
    // Reasoning cell that was being streamed. The Content fragment is
    // itself a new open block in the stream -- it commits to the
    // transcript when the next boundary (FinishTurn, in this test)
    // closes it.
    assert_eq!(screen.view.transcript.len(), 1);
    match &screen.view.transcript[0] {
        Cell::Thought(t) => {
            assert_eq!(t.body.len(), 1);
            assert!(matches!(&t.body[0], Cell::Reasoning(s) if s == "ab"));
        }
        other => panic!("transcript[0] should be a Thought, got {other:?}"),
    }
    screen.apply(MachineNotice::FinishTurn);
    assert_eq!(screen.view.transcript[1], Cell::Content("x".into()));
    assert!(screen.view.stream.current().is_none());
}

/// `SetModel` and `SetEffort` notices update the model's metadata
/// (which rides above each user prompt), not the bottom status line.
/// `Usage` notices still record cache stats into the status bar.
#[test]
fn status_notices_reach_the_status_line() {
    let mut screen = State::default();
    screen.apply_app(AppNotice::SetModel("m-1".into()));
    screen.apply_app(AppNotice::SetEffort("high".into()));
    // The metadata row carries the model+effort in transcript cells.
    let meta = screen.metadata_text().expect("model+effort are set");
    assert_eq!(meta, "m-1 · effort high");
    screen.apply(MachineNotice::Usage(
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
        screen.view.status.full_line(),
        "m-1 · effort high · cache 60.0% · 6/4"
    );
    screen.apply_app(AppNotice::ResetStats);
    assert_eq!(
        screen.view.status.full_line(),
        "m-1 · effort high · cache 0.0% · 0/0"
    );
}

#[test]
fn replay_and_the_live_stream_produce_the_same_cells() {
    // The invariant the plain front end is held to, held here too: a resumed
    // session must look like the one that was watched live.
    let mut screen = State::default();
    screen.apply_app(AppNotice::Replay(vec![
        Message {
            role: Role::Assistant,
            content: Some("running it".into()),
            reasoning_content: Some("let me think".into()),
            tool_calls: None,
            tool_call_id: None,
            thinking: None,
            reasoning: None,
        },
        Message::tool("call_1", "exit_code: 0\n--- stdout ---\nbody"),
    ]));
    // The assistant message has no `tool_calls`, so the Tool message that
    // follows has no preceding open step to settle. The result text
    // becomes a dim Notice instead.
    assert_eq!(
        screen.view.transcript,
        vec![
            Cell::Reasoning("let me think".into()),
            Cell::Content("running it".into()),
            Cell::Notice("exit_code: 0\n--- stdout ---\nbody".into()),
        ]
    );
}

/// A submitted user line gets a metadata row above it on the transcript:
/// `provider/model · effort tier`, drawn dim, that names which model and
/// tier ran the line that follows it. The metadata is current at the
/// time the line is asked, so a session that switches models mid-history
/// reads the way the user asked it.
#[test]
fn submit_pushes_a_metadata_row_above_each_user_line() {
    let mut screen = State::for_test_with_meta("deepseek/deepseek-v4-flash", "max");
    screen.submit("first");
    screen.submit("second");

    // Two user lines, each preceded by its own metadata cell.
    let cells = &screen.view.transcript;
    assert!(cells.len() >= 4, "two metadata + two user lines: {cells:?}");
    let metadata_1 = cells[0].clone();
    let user_1 = cells[1].clone();
    let metadata_2 = cells[2].clone();
    let user_2 = cells[3].clone();
    if let Cell::Notice(t) = &metadata_1 {
        assert_eq!(
            t, "deepseek/deepseek-v4-flash · effort max",
            "metadata carries the model+effort pair"
        );
    } else {
        panic!("metadata_1 was not a Notice: {metadata_1:?}");
    }
    assert!(matches!(user_1, Cell::User { .. }));
    if let Cell::Notice(t) = &metadata_2 {
        assert_eq!(
            t, "deepseek/deepseek-v4-flash · effort max",
            "same metadata repeated for each user line"
        );
    } else {
        panic!("metadata_2 was not a Notice: {metadata_2:?}");
    }
    assert!(matches!(user_2, Cell::User { .. }));
}

/// Switching models mid-session: the metadata row reflects the model at
/// the time the line is asked, not the model the session began with.
#[test]
fn submit_picks_up_the_current_model_at_submit_time() {
    let mut screen = State::for_test_with_meta("deepseek/deepseek-v4-flash", "max");
    screen.submit("first");
    // The user switches models mid-history.
    screen.view.status.set_model("deepseek/deepseek-v4-pro");
    screen.submit("second");

    let cells = &screen.view.transcript;
    let metadata_1 = &cells[0];
    let user_1 = &cells[1];
    let metadata_2 = &cells[2];
    let user_2 = &cells[3];
    if let Cell::Notice(t) = metadata_1 {
        assert!(
            t.contains("v4-flash"),
            "first line keeps the original model: {metadata_1:?}"
        );
    } else {
        panic!("metadata_1 was not a Notice: {metadata_1:?}");
    }
    if let Cell::Notice(t) = metadata_2 {
        assert!(
            t.contains("v4-pro"),
            "second line picks up the switched model: {metadata_2:?}"
        );
    } else {
        panic!("metadata_2 was not a Notice: {metadata_2:?}");
    }
    assert!(matches!(user_1, Cell::User { .. }));
    assert!(matches!(user_2, Cell::User { .. }));
}

/// A session resumed from a log: each replayed user line picks up the
/// current model's metadata, so a resumed session reads the same as one
/// that was watched live. Live and replay produce identical cells.
#[test]
fn replay_pushes_metadata_above_each_replayed_user_line() {
    let mut screen = State::for_test_with_meta("zai-coding-cn/glm-5.3", "high");
    screen.apply_app(AppNotice::Replay(vec![
        Message::user("first"),
        Message::user("second"),
    ]));
    let cells = &screen.view.transcript;
    // Each user line gets its own metadata row above it.
    let metadata_1 = &cells[0];
    let user_1 = &cells[1];
    let metadata_2 = &cells[2];
    let user_2 = &cells[3];
    assert!(
        matches!(metadata_1, Cell::Notice(t) if t == "zai-coding-cn/glm-5.3 · effort high"),
        "first replayed line gets a metadata row"
    );
    assert!(matches!(user_1, Cell::User { .. }));
    assert!(matches!(metadata_2, Cell::Notice(_)));
    assert!(matches!(user_2, Cell::User { .. }));
}

#[test]
fn metadata_text_is_none_when_no_model_or_effort_is_set() {
    let screen = State::default();
    assert!(screen.metadata_text().is_none());
}

#[test]
fn metadata_text_omits_a_blank_field() {
    let screen = State::for_test_with_meta("m-1", "");
    assert_eq!(screen.metadata_text().as_deref(), Some("m-1"));
    let screen = State::for_test_with_meta("", "high");
    assert_eq!(screen.metadata_text().as_deref(), Some("effort high"));
}

/// Settled-Done step children stay hidden: the verbose toggle that used
/// to surface them on Ctrl-O is gone. A Done step is one line on screen,
/// the verdict alone; the detail lives in the thought body the step
/// belongs to (the thinking widget surfaces it there).
#[test]
fn settled_done_step_hides_its_children() {
    let mut screen = State::default();
    let step = {
        let mut s = Cell::from_tool_call("Bash", r#"{"command":"echo a; echo b"}"#);
        if let Cell::Step(s) = &mut s {
            s.push_output("alpha\nbeta\n");
            s.settle("exit_code: 0");
        }
        s
    };
    screen.view.transcript.push(step);

    let width = 80;
    let lines = render::lines(&mut screen.view, width);
    let text: String = lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !text.contains("alpha"),
        "settled children do not surface: {lines:?}"
    );
    assert!(!text.contains("beta"));
}

/// A failed step auto-expands regardless of mode: the reader is reading
/// it because something went wrong, and the detail is not optional.
/// The verbose toggle being gone does not change this.
#[test]
fn a_failed_step_auto_expands() {
    let mut screen = State::default();
    let step = {
        let mut s = Cell::from_tool_call("Bash", r#"{"command":"false"}"#);
        if let Cell::Step(s) = &mut s {
            s.push_output("boom\n");
            s.settle("exit_code: 1");
        }
        s
    };
    screen.view.transcript.push(step);

    let width = 80;
    let lines = render::lines(&mut screen.view, width);
    let text: String = lines
        .iter()
        .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("boom"),
        "failed step carries its children: {lines:?}"
    );
}

#[test]
fn an_open_question_is_drawn_after_the_transcript() {
    let mut screen = State::default();
    screen.apply(MachineNotice::Content("before".into()));
    screen.apply(MachineNotice::Approval {
        name: "Bash".into(),
        args: r#"{"command":"rm -rf /"}"#.into(),
    });
    let lines = render::lines(&mut screen.view, 80);
    assert_eq!(lines.len(), 2, "answer, then the question");
    assert!(format!("{:?}", lines[1]).contains("run it?"));
}

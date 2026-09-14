//! Tests for what gets drawn.
//!
//! What is here is the painter's contract: a cell becomes the lines it is
//! supposed to become, the status line carries the session summary regardless
//! of state, the gate's question is drawn whole, and the box grows and
//! stays put at the edges. Drawing is what the visible front end is for; the
//! rest is the wiring.

use std::time::{Duration, Instant};

use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

use crate::ui::cell::Cell;

use super::super::layout::{BOX_GUTTER, BOX_ROWS, PINNED_ROWS, box_field, box_rows, screen_rows};
use super::super::notice::{AppNotice, MachineNotice};
use super::super::render as render_mod;
use super::super::render::SPINNER;
use super::super::state::State;
use super::all_rows;
use super::rendered;
use super::row;
use super::screen_for_test;
use super::transcript_top;
use super::unset;
use crate::types::Usage;

#[test]
fn the_status_line_is_the_summary_whether_or_not_a_turn_runs() {
    // The line as drawn, not as formatted: while a turn runs the row is the
    // same session summary it is at rest, and the turn's own doings are the
    // transcript's cells, not the status line's. The model id has moved
    // to the per-prompt metadata row; the pinned row carries only the
    // cache stats that are truly session-level.
    let mut screen = screen_for_test(60, 20);
    screen.state.model = Some("m-1".to_owned());
    screen.state.apply(MachineNotice::Usage(
        Usage {
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 4,
            ..Usage::default()
        },
        Duration::ZERO,
    ));
    screen.draw().unwrap();
    let last = screen.terminal.backend().buffer().area.height - 1;
    assert_eq!(row(&screen, last), "cache 60.0% · 6/4");
    screen.state.begin_turn(Instant::now());
    screen.state.apply(MachineNotice::ToolStart {
        name: "read_file".into(),
        args: "{}".into(),
    });
    screen
        .state
        .apply(MachineNotice::Content("here it is".into()));
    screen.draw().unwrap();
    assert_eq!(
        row(&screen, last),
        "cache 60.0% · 6/4",
        "a running turn does not take the row over"
    );
}

#[test]
fn a_tool_call_that_changes_a_file_is_drawn_across_its_lines() {
    let mut screen = screen_for_test(40, 20);
    screen.state.view.transcript.push(Cell::from_tool_call(
        "Edit",
        r#"{"file_path":"a.rs","old_string":"one\ntwo","new_string":"three"}"#,
    ));
    screen.draw().unwrap();
    let top = transcript_top(&screen, 4);
    assert_eq!(row(&screen, top), "▸ Edit a.rs");
    // One hunk and no header: the change is the lines, and the `@@ … @@` that
    // says where in the file they are is only worth its row when there is more
    // than one hunk to tell apart.
    assert_eq!(row(&screen, top + 1), "  - one");
    assert_eq!(row(&screen, top + 2), "  - two");
    assert_eq!(row(&screen, top + 3), "  + three");
}

#[test]
fn a_long_line_of_a_change_is_wrapped_like_any_other() {
    // It is drawn into a fixed-width region: the terminal cannot be left to
    // wrap it, or the tail of the line is lost.
    let mut screen = screen_for_test(20, 20);
    let long = "x".repeat(30);
    screen.state.view.transcript.push(Cell::from_tool_call(
        "Write",
        &format!(r#"{{"file_path":"a.txt","content":"{long}"}}"#),
    ));
    screen.draw().unwrap();
    let top = transcript_top(&screen, 3);
    assert_eq!(row(&screen, top + 1), format!("  + {}", "x".repeat(16)));
    assert_eq!(row(&screen, top + 2), format!("  {}", "x".repeat(14)));
}

#[test]
fn a_call_with_nothing_to_name_shows_all_of_its_arguments() {
    // A call the summariser cannot name -- an MCP tool, or arguments that are
    // not JSON at all -- is shown by its arguments, and the arguments are
    // shown whole: the row is wrapped by the region, not cut by a column
    // count of the painter's own. A cut prefix would be a call whose subject
    // the reader cannot check before it runs.
    let mut screen = screen_for_test(40, 20);
    let args = format!(r#"{{"blob":"{}"}}"#, "x".repeat(90));
    screen
        .state
        .view
        .transcript
        .push(Cell::from_tool_call("Mcp", &args));
    screen.draw().unwrap();
    // The gutters off each row and the rows run together: the wrap breaks
    // inside the one long word, so what is on the screen is the arguments back
    // in the order they were written -- unless a column of them was dropped.
    let drawn: String = all_rows(&screen).iter().map(|r| unset(r)).collect();
    assert!(
        drawn.contains(&args),
        "the whole of the arguments is drawn, in order: {drawn}"
    );
}

#[test]
fn thinking_is_set_in_by_its_columns_alone() {
    // The thinking is the machinery around an answer rather than the answer, so
    // it is set in two columns -- faintly, and no longer on a ground of its own.
    // The columns are the whole of what sets it off: the marker it used to carry
    // is gone, and with it the one sign of "this is thinking" that survived a
    // terminal honouring neither colour nor dim. What still holds is the edge the
    // reader reads down -- the answer is the one line that starts in column zero,
    // and a think that wrapped back to it would read as an answer.
    let mut screen = screen_for_test(40, 20);
    screen
        .state
        .view
        .transcript
        .push(Cell::Reasoning("hmm".into()));
    screen
        .state
        .view
        .transcript
        .push(Cell::Content("answer".into()));
    screen.draw().unwrap();
    let top = transcript_top(&screen, 2);
    let buf = screen.terminal.backend().buffer();
    assert_eq!(row(&screen, top), "  hmm");
    assert_eq!(
        buf[(0, top)].bg,
        Color::Reset,
        "no ground to reach the edge with"
    );
    assert_eq!(buf[(39, top)].bg, Color::Reset);
    assert_eq!(
        row(&screen, top + 1),
        "answer",
        "and the answer keeps the edge"
    );
    assert_eq!(buf[(0, top + 1)].bg, Color::Reset);
}

#[test]
fn an_attached_image_is_drawn_under_the_line_it_came_with() {
    // The image is a line of the user's own cell and not a cell of its own: the
    // marker opens the cell for the words, and the image continues in the same
    // columns rather than back at the left edge the answer is read down.
    let mut screen = screen_for_test(40, 20);
    screen.state.view.transcript.push(Cell::User {
        text: "what is this?".into(),
        images: vec![crate::image::Note {
            format: "png".into(),
            bytes: 6,
        }],
    });
    screen
        .state
        .view
        .transcript
        .push(Cell::Content("a picture".into()));
    screen.draw().unwrap();
    let top = transcript_top(&screen, 3);
    assert_eq!(row(&screen, top), "› what is this?");
    assert_eq!(row(&screen, top + 1), "  [image png · 6 bytes]");
    assert_eq!(row(&screen, top + 2), "a picture");
}

#[test]
fn multiple_attached_images_are_one_line_not_separate_lines() {
    // Five pictures become one row, not five: the user is reading the answer,
    // not the count of the files they attached. The per-image format is left
    // to the message the backend sees -- it is in the data URL, and a resumed
    // session replays the same bytes it sent the first time.
    let mut screen = screen_for_test(40, 20);
    screen.state.view.transcript.push(Cell::User {
        text: "compare these".into(),
        images: vec![
            crate::image::Note {
                format: "png".into(),
                bytes: 6,
            },
            crate::image::Note {
                format: "jpeg".into(),
                bytes: 4,
            },
            crate::image::Note {
                format: "webp".into(),
                bytes: 2,
            },
        ],
    });
    screen
        .state
        .view
        .transcript
        .push(Cell::Content("answer".into()));
    screen.draw().unwrap();
    let top = transcript_top(&screen, 3);
    assert_eq!(row(&screen, top), "› compare these");
    assert_eq!(row(&screen, top + 1), "  [3 images · 12 bytes]");
    assert_eq!(row(&screen, top + 2), "answer");
}

#[test]
fn a_wrapped_think_keeps_the_columns_on_every_line() {
    // A continuation line that came back to the left edge would be a line that
    // reads as an answer, in the middle of a block that is not one.
    let mut screen = screen_for_test(12, 20);
    screen
        .state
        .view
        .transcript
        .push(Cell::Reasoning("aaaa bbbb cccc".into()));
    screen
        .state
        .view
        .transcript
        .push(Cell::Content("answer".into()));
    screen.draw().unwrap();
    let top = transcript_top(&screen, 3);
    assert_eq!(row(&screen, top), "  aaaa bbbb");
    assert_eq!(row(&screen, top + 1), "  cccc");
    assert_eq!(row(&screen, top + 2), "answer");
}

#[test]
fn a_long_think_is_drawn_whole() {
    // The think is the one block that grows without bound, and the window
    // pins to the newest line -- so a think that ran past the window is
    // scrolled to, not counted away. What is behind the fold is what the
    // reader opened the region to read, and the transcript is where a
    // session's own words are kept.
    let mut screen = screen_for_test(40, 30);
    let think = (0..30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    screen.state.view.transcript.push(Cell::Reasoning(think));
    screen
        .state
        .view
        .transcript
        .push(Cell::Content("answer".into()));
    screen.draw().unwrap();
    let drawn = all_rows(&screen).join("\n");
    assert!(
        drawn.contains("  line 29"),
        "the tail is on screen: {drawn}"
    );
    assert!(
        !drawn.contains("more lines"),
        "and nothing stood in for the lines above it: {drawn}"
    );
}

/// What a command prints while it runs is watched as it arrives, and the step
/// it forms settles into the transcript when the result lands: the run of
/// output is part of the same step cell, not a separate one.
#[test]
fn a_running_commands_output_is_watched_and_then_kept() {
    let mut screen = screen_for_test(40, 30);
    screen.state.apply(MachineNotice::ToolStart {
        name: "Bash".into(),
        args: r#"{"command":"echo one; echo two"}"#.into(),
    });
    screen
        .state
        .apply(MachineNotice::ToolOutput("one\n".into()));
    screen
        .state
        .apply(MachineNotice::ToolOutput("two\n".into()));
    screen.draw().unwrap();
    // While the tool runs, the open step carries the children on screen.
    let live = rendered(&render_mod::lines(&mut screen.state.view, 40));
    assert!(
        live.iter().any(|(text, _)| text.contains("one")),
        "on screen while the command runs: {live:?}"
    );
    screen
        .state
        .apply(MachineNotice::ToolResult("exit_code: 0".into()));
    screen.draw().unwrap();
    // One step in the transcript -- the open one that was settled, with its
    // children kept and a verdict filled in.
    let mut expected = Cell::from_tool_call("Bash", r#"{"command":"echo one; echo two"}"#);
    if let Cell::Step(s) = &mut expected {
        // Two `ToolOutput` notices arrive in the live sequence;
        // matching that order keeps the children list identical to
        // the screen's settled step.
        s.push_output("one\n");
        s.push_output("two\n");
        s.settle("exit_code: 0");
    }
    // StepId and started_at are process-global; borrow them from the
    // actual screen so the assertion compares only the fields that
    // describe what happened here.
    let actual_last = screen.state.view.transcript.last().expect("step settled");
    let expected = match (&mut expected, actual_last) {
        (Cell::Step(s), Cell::Step(actual)) => {
            s.id = actual.id;
            s.started_at = actual.started_at;
            Cell::Step(s.clone())
        }
        _ => unreachable!("both are Steps"),
    };
    assert_eq!(screen.state.view.transcript.last(), Some(&expected));
    let drawn = rendered(&render_mod::lines(&mut screen.state.view, 40));
    let drawn: String = drawn.into_iter().map(|(text, _)| text).collect();
    assert!(drawn.contains("one") && drawn.contains("two"), "{drawn}");
}

#[test]
fn a_folded_thought_is_one_ruled_line_no_matter_its_size() {
    // The fold is structural: a thought region with thirty lines of
    // reasoning takes one row on the transcript, not thirty. The fold
    // line names the activity ("thinking") and the elapsed seconds, and
    // it carries no marker of its own: the region is set in two columns
    // and the words are the whole of what the line says.
    let mut screen = screen_for_test(40, 30);
    let think = (0..30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    screen.state.apply(MachineNotice::Reasoning(think));
    screen.state.apply(MachineNotice::Content("answer".into()));
    screen.draw().unwrap();

    let lines = render_mod::lines(&mut screen.state.view, 40);
    // The transcript has three cells: the thought (1 row), the answer
    // (1 row), and nothing else.
    assert_eq!(
        lines.len(),
        2,
        "fold takes 1 row, answer takes 1 row: {lines:?}"
    );
    assert!(
        format!("{:?}", lines[0]).contains("Thought for "),
        "the fold line names the region: {:?}",
        lines[0]
    );
    let top = transcript_top(&screen, 2);
    assert_eq!(row(&screen, top), "  Thought for  0s");
    assert_eq!(
        row(&screen, top + 1),
        "answer",
        "and the answer keeps the edge"
    );
}

#[test]
fn every_thought_region_lands_a_fold_line() {
    // Two thoughts in the transcript (one for each reasoning -> content
    // stretch) means two fold lines on screen. The active third thought
    // never closes, so its fold line is what its `Reasoning` cell becomes.
    let mut screen = screen_for_test(40, 30);
    screen
        .state
        .apply(MachineNotice::Reasoning("first plan".into()));
    screen
        .state
        .apply(MachineNotice::Content("answer one".into()));
    screen
        .state
        .apply(MachineNotice::Reasoning("second plan".into()));
    screen
        .state
        .apply(MachineNotice::Content("answer two".into()));
    screen.draw().unwrap();

    let rendered = all_rows(&screen).join("");
    let fold_lines = rendered.matches("Thought").count();
    assert_eq!(
        fold_lines, 2,
        "two closed regions => two fold lines: {rendered}"
    );
}

#[test]
fn a_side_effectful_tool_closes_a_thought_before_its_cell_lands() {
    // The Edit that closes the thought comes after the fold line: the
    // region is the planning the model did, and the side-effect cell is
    // its own row.
    let mut screen = screen_for_test(40, 30);
    screen.state.apply(MachineNotice::Reasoning("plan".into()));
    screen.state.apply(MachineNotice::ToolStart {
        name: "Edit".into(),
        args: r#"{"file_path":"a.rs"}"#.into(),
    });
    screen.state.apply(MachineNotice::ToolResult("ok".into()));
    screen.draw().unwrap();

    let rendered = all_rows(&screen).join("");
    let edit_pos = rendered.find("Edit").expect("Edit cell drawn");
    let fold_pos = rendered.find("Thought").expect("fold line drawn");
    assert!(
        fold_pos < edit_pos,
        "fold line precedes the Edit that closed the thought: {rendered}"
    );
}

#[test]
fn an_active_thoughts_seconds_are_live_while_a_closed_ones_freeze() {
    // The active region's elapsed is read live (the same `now` the border
    // uses); a closed region's elapsed is frozen at the boundary.
    use crate::ui::cell::Thought;
    use crate::ui::cell::ThoughtStatus;

    let t0 = Instant::now();
    let mut active = Thought::open("thinking", Some(t0));
    let _line_early = active.live_secs(t0);
    let _line_late = active.live_secs(t0 + Duration::from_secs(3));
    assert_eq!(active.live_secs(t0), 0);
    assert_eq!(active.live_secs(t0 + Duration::from_secs(3)), 3);

    active.close(t0 + Duration::from_secs(2));
    assert_eq!(active.status, ThoughtStatus::Done);
    assert_eq!(
        active.live_secs(t0 + Duration::from_secs(999)),
        2,
        "closed regions do not age"
    );
}

#[test]
fn the_gate_is_drawn_whole_however_long_the_call_is() {
    // The question is what the answer is about: a command clipped by the
    // width leaves nothing to decide with.
    let mut screen = screen_for_test(20, 20);
    screen.state.view.question = Some(Cell::approval(
        "Bash",
        r#"{"command":"rm -rf /tmp/aaaaaaaaaaaaaaaaaaaa"}"#,
    ));
    screen.draw().unwrap();
    let drawn = all_rows(&screen).join("");
    assert!(drawn.contains("aaaa"), "the call is there: {drawn}");
    assert!(
        drawn.contains("run it? y/N"),
        "and so is the question: {drawn}"
    );
}

#[test]
fn the_box_grows_with_the_lines_in_it() {
    // One row per line, borders on top and bottom -- and Ctrl-J is what puts
    // lines in it, so this is the arithmetic that makes that key visible.
    assert_eq!(box_rows(0, 20, 0), BOX_ROWS, "empty is still a box");
    assert_eq!(box_rows(1, 20, 0), BOX_ROWS);
    assert_eq!(box_rows(2, 20, 0), 4);
    assert_eq!(box_rows(6, 20, 0), 8);
}

#[test]
fn the_box_draws_no_caret_of_its_own() {
    // One caret per screen. The editor drew one by reversing whatever was under
    // it, and `place_cursor` put the terminal's own on the same cell: two
    // carets on one cell, and the one that was kept is the terminal's -- it is
    // the one that blinks, that a bar-shaped cursor can be seen in, and that
    // says where a typed character lands.
    let mut screen = screen_for_test(40, 20);
    screen.draw().unwrap();
    let last = screen.terminal.backend().buffer().area.height - 1;
    let buf = screen.terminal.backend().buffer();
    assert!(
        !(0..buf.area.height).any(|y| (0..buf.area.width).any(|x| buf[(x, y)]
            .style()
            .add_modifier
            .contains(Modifier::REVERSED))),
        "nothing on an idle screen is reversed"
    );
    let cursor = screen.terminal.backend_mut().get_cursor_position().unwrap();
    assert_eq!(cursor.y, last - 2, "the box's line, between its rules");
    assert_eq!(cursor.x, BOX_GUTTER, "and the column the draft starts in");
}

#[test]
fn a_failure_is_drawn_by_weight_and_not_only_by_color() {
    // The color of a failure is a palette slot the terminal's theme picked for a
    // background this code cannot see, and the dark end of a default palette is
    // chosen for a light one: red at its darkest on black is a line that cannot
    // be read, on the one line that must be. The weight is what carries it.
    let mut screen = screen_for_test(40, 20);
    screen
        .state
        .apply_app(AppNotice::Error("the backend said 402".into()));
    screen.state.apply(MachineNotice::ToolStart {
        name: "Bash".into(),
        args: r#"{"command":"rm -rf /tmp/aaa"}"#.into(),
    });
    screen.draw().unwrap();
    let buf = screen.terminal.backend().buffer();
    let at = |needle: &str| -> (u16, u16) {
        for y in 0..buf.area.height {
            let text = row(&screen, y);
            if let Some(x) = text.find(needle) {
                return (x as u16, y);
            }
        }
        panic!("{needle:?} was not drawn");
    };
    for needle in ["error:", "Bash"] {
        let (x, y) = at(needle);
        let style = buf[(x, y)].style();
        assert!(
            style.add_modifier.contains(Modifier::BOLD),
            "{needle:?} is drawn bold"
        );
        assert!(style.fg.is_some(), "{needle:?} keeps its color too");
    }
}

#[test]
fn the_box_is_drawn_on_a_terminal_with_no_room_to_draw_it_in() {
    // The field and the marker are cut out of the box's own area, and both of
    // them take columns from it. A terminal narrower than the gutter, or shorter
    // than the two rules, leaves neither with anything -- and a draw is the one
    // thing that cannot be allowed to fail there, since it is the draw that has
    // to put the smaller screen on the screen.
    for (width, height) in [(1, 1), (1, 3), (2, 2), (3, 3), (80, 2), (80, 1)] {
        let mut screen = screen_for_test(width, height);
        screen.draw().unwrap_or_else(|e| {
            panic!("{width}x{height} would not draw: {e:?}");
        });
    }
    // And the field is asked for what is there rather than for what it wants:
    // a negative width is not a rect.
    let area = Rect::new(0, 0, 80, 3);
    let field = box_field(area);
    assert_eq!(field.x, BOX_GUTTER);
    assert_eq!(field.width, 80 - BOX_GUTTER);
    assert_eq!(field.height, 1, "between the rules");
    assert_eq!(box_field(Rect::new(0, 0, 1, 3)).width, 0);
    assert_eq!(box_field(Rect::new(0, 0, 1, 1)).height, 0);
}

#[test]
fn a_box_taller_than_the_screen_gives_way_to_the_transcript() {
    // A draft longer than the terminal can show still leaves a row of
    // transcript: past that the box scrolls inside itself.
    let height = 12;
    let most = height - PINNED_ROWS - 1;
    assert_eq!(box_rows(100, height, 0), most);
    assert_eq!(
        screen_rows(Rect::new(0, 0, 80, height), 0, box_rows(100, height, 0), 0)[0].height,
        1
    );
    // And a terminal too short for the box, the status line and a transcript
    // row all at once takes it out of the box. Asking for the three rows the
    // box wants on a two-row terminal is asking the layout for rows it does
    // not have: it answers with a one-row box, and the transcript's own answer
    // is nothing -- which is how the transcript ends up windowed against a row
    // nobody drew.
    assert_eq!(
        screen_rows(Rect::new(0, 0, 80, 2), 0, BOX_ROWS, 0)
            .iter()
            .map(|r| r.height)
            .collect::<Vec<_>>(),
        vec![0, 0, 0, 1, 1],
        "what the layout does with a request it cannot grant"
    );
    assert_eq!(
        box_rows(100, 2, 0),
        0,
        "so the box asks for nothing instead"
    );
    assert_eq!(
        screen_rows(Rect::new(0, 0, 80, 2), 0, box_rows(100, 2, 0), 0)[0].height,
        1,
        "and the transcript gets the row the box no longer holds"
    );
    assert_eq!(
        box_rows(100, 3, 0),
        1,
        "a box with one row is the next line"
    );
    assert_eq!(
        screen_rows(Rect::new(0, 0, 80, 3), 0, box_rows(100, 3, 0), 0)[0].height,
        1,
        "and the transcript keeps its row"
    );
}

#[test]
fn the_box_says_what_enter_will_do() {
    // Three states, three invitations: the answer to a question, the line for
    // after the turn, and the next message. The box is where all three are
    // typed, so it is the only place that can say which one it is.
    let mut state = State::default();
    assert_eq!(
        state.edit.textarea.placeholder_text(),
        super::super::input::IDLE_PLACEHOLDER
    );

    state.begin_turn(Instant::now());
    assert_eq!(
        state.edit.textarea.placeholder_text(),
        super::super::input::QUEUE_PLACEHOLDER
    );

    let (reply, _answer) = tokio::sync::oneshot::channel();
    state.open_question(reply);
    assert_eq!(
        state.edit.textarea.placeholder_text(),
        super::super::input::ANSWER_PLACEHOLDER
    );

    state.close_question();
    assert_eq!(
        state.edit.textarea.placeholder_text(),
        super::super::input::QUEUE_PLACEHOLDER,
        "the turn is still running"
    );

    state.end_turn();
    assert_eq!(
        state.edit.textarea.placeholder_text(),
        super::super::input::IDLE_PLACEHOLDER
    );
}

#[test]
fn the_working_indicator_is_not_dim_on_a_dim_rule() {
    // While the model is quiet, the spinner on the box's rule is the only thing
    // on the screen that moves: it is chrome that has to be seen, so it is the
    // one thing on the box painted at full strength. A dim indicator on a dim
    // rule is the signal painted out of sight.
    let mut screen = screen_for_test(60, 20);
    screen.state = super::working(Duration::from_secs(3), 0, 4.0);
    screen.draw().unwrap();
    let rule = screen.terminal.backend().buffer().area.height - 1 - 3;
    let buf = screen.terminal.backend().buffer();
    let at = (0..buf.area.width)
        .find(|&x| SPINNER.contains(&buf[(x, rule)].symbol().chars().next().unwrap_or(' ')))
        .expect("the indicator is drawn on the box's top rule");
    assert!(
        !buf[(at, rule)].style().add_modifier.contains(Modifier::DIM),
        "the indicator is lit"
    );
    assert!(
        buf[(0, rule)].style().add_modifier.contains(Modifier::DIM),
        "and the rule it sits on is still chrome"
    );
}

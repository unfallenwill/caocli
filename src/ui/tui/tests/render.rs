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
use super::super::notice::Notice;
use super::super::paint::THINKING_LINES;
use super::super::render as render_mod;
use super::super::render::SPINNER;
use super::super::state::State;
use super::all_rows;
use super::origin;
use super::rendered;
use super::row;
use super::screen_for_test;
use crate::types::Usage;

#[test]
fn the_status_line_is_the_summary_whether_or_not_a_turn_runs() {
    // The line as drawn, not as formatted: while a turn runs the row is the
    // same session summary it is at rest, and the turn's own doings are the
    // transcript's cells, not the status line's.
    let mut screen = screen_for_test(60, 20);
    screen.state.status.set_model("m-1");
    screen.state.apply(Notice::Usage(
        Usage {
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 4,
            ..Usage::default()
        },
        Duration::ZERO,
    ));
    screen.draw().unwrap();
    let last = screen.terminal.backend().buffer().area.height - 1;
    assert_eq!(row(&screen, last), "m-1 · cache 60.0% · hit 6 · miss 4");
    screen.state.begin_turn(Instant::now());
    screen.state.apply(Notice::ToolStart {
        name: "read_file".into(),
        args: "{}".into(),
    });
    screen.state.apply(Notice::Content("here it is".into()));
    screen.draw().unwrap();
    assert_eq!(
        row(&screen, last),
        "m-1 · cache 60.0% · hit 6 · miss 4",
        "a running turn does not take the row over"
    );
}

#[test]
fn a_tool_call_that_changes_a_file_is_drawn_across_its_lines() {
    let mut screen = screen_for_test(40, 20);
    screen.state.transcript.push(Cell::tool_call(
        "Edit",
        r#"{"file_path":"a.rs","old_string":"one\ntwo","new_string":"three"}"#,
    ));
    screen.draw().unwrap();
    let top = origin(&mut screen).y;
    assert_eq!(row(&screen, top), "▸ Edit a.rs");
    // The hunk header comes first: it names the line ranges in the old
    // and new files, the way `diff -u` does.
    assert_eq!(row(&screen, top + 1), "  @@ -1,2 +1,1 @@");
    assert_eq!(row(&screen, top + 2), "  - one");
    assert_eq!(row(&screen, top + 3), "  - two");
    assert_eq!(row(&screen, top + 4), "  + three");
}

#[test]
fn a_long_line_of_a_change_is_wrapped_like_any_other() {
    // It is drawn into a fixed-width region: the terminal cannot be left to
    // wrap it, or the tail of the line is lost.
    let mut screen = screen_for_test(20, 20);
    let long = "x".repeat(30);
    screen.state.transcript.push(Cell::tool_call(
        "Write",
        &format!(r#"{{"file_path":"a.txt","content":"{long}"}}"#),
    ));
    screen.draw().unwrap();
    let top = origin(&mut screen).y;
    // The hunk header is the first line after the call.
    assert_eq!(row(&screen, top + 1), "  @@ -0,0 +1,1 @@");
    assert_eq!(row(&screen, top + 2), format!("  + {}", "x".repeat(16)));
    assert_eq!(row(&screen, top + 3), format!("  {}", "x".repeat(14)));
}

#[test]
fn thinking_is_set_in_behind_a_rule_of_its_own() {
    // The thinking is the machinery around an answer rather than the answer, so
    // it is set in two columns -- faintly, behind a rule -- and no longer on a
    // ground of its own. The rule is the whole of what says "this is thinking",
    // which is what makes it a difference a terminal cannot lose: a ground is in
    // the colors and SGR 2 is not honored everywhere, while the columns are in
    // the layout.
    let mut screen = screen_for_test(40, 20);
    screen.state.transcript.push(Cell::Reasoning("hmm".into()));
    screen.state.transcript.push(Cell::Content("answer".into()));
    screen.draw().unwrap();
    let top = origin(&mut screen).y;
    let buf = screen.terminal.backend().buffer();
    assert_eq!(row(&screen, top), "┆ hmm");
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
    screen.state.transcript.push(Cell::User {
        text: "what is this?".into(),
        images: vec![crate::image::Note {
            format: "png".into(),
            bytes: 6,
        }],
    });
    screen
        .state
        .transcript
        .push(Cell::Content("a picture".into()));
    screen.draw().unwrap();
    let top = origin(&mut screen).y;
    assert_eq!(row(&screen, top), "› what is this?");
    assert_eq!(row(&screen, top + 1), "  [image png · 6 bytes]");
    assert_eq!(row(&screen, top + 2), "a picture");
}

#[test]
fn a_wrapped_think_keeps_the_rule_on_every_line() {
    // A continuation line that came back to the left edge would be a line that
    // reads as an answer, in the middle of a block that is not one.
    let mut screen = screen_for_test(12, 20);
    screen
        .state
        .transcript
        .push(Cell::Reasoning("aaaa bbbb cccc".into()));
    screen.state.transcript.push(Cell::Content("answer".into()));
    screen.draw().unwrap();
    let top = origin(&mut screen).y;
    assert_eq!(row(&screen, top), "┆ aaaa bbbb");
    assert_eq!(row(&screen, top + 1), "┆ cccc");
    assert_eq!(row(&screen, top + 2), "answer");
}

#[test]
fn a_long_think_folds_to_its_head_and_a_count() {
    // The think is the one block that grows without bound, and the window
    // pins to the newest line: kept whole, a hundred lines of faint text
    // would be exactly the thing standing between the reader and the answer
    // the turn was for.
    let mut screen = screen_for_test(40, 30);
    let think = (0..30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    screen.state.transcript.push(Cell::Reasoning(think));
    screen.state.transcript.push(Cell::Content("answer".into()));
    screen.draw().unwrap();
    let top = origin(&mut screen).y;
    for i in 0..THINKING_LINES {
        assert_eq!(row(&screen, top + i as u16), format!("┆ line {i}"));
    }
    assert_eq!(
        row(&screen, top + THINKING_LINES as u16),
        format!("┆ … {} more line(s)", 30 - THINKING_LINES),
        "the count wears the block's own rule and says what is behind it"
    );
    assert_eq!(
        row(&screen, top + THINKING_LINES as u16 + 1),
        "answer",
        "and the answer is still on the screen the think was folded for"
    );
}

#[test]
fn a_think_at_the_cap_is_not_folded() {
    // The count exists to say that something was left out; a block that
    // gave up nothing would be paying a row to say nothing.
    let mut screen = screen_for_test(40, 30);
    let think = (0..THINKING_LINES)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    screen.state.transcript.push(Cell::Reasoning(think));
    screen.draw().unwrap();
    let drawn = all_rows(&screen).join("\n");
    assert!(
        !drawn.contains("more line(s)"),
        "nothing is hidden, so nothing is counted: {drawn}"
    );
}

#[test]
fn only_a_think_folds() {
    // The fold is about dim machinery evicting the answer. The answer
    // itself is the thing the transcript is here for, and it is never
    // counted away.
    let mut screen = screen_for_test(40, 30);
    let long = (0..30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    screen.state.transcript.push(Cell::Content(long));
    screen.draw().unwrap();
    let drawn = all_rows(&screen).join("\n");
    assert!(drawn.contains("line 29"), "kept whole: {drawn}");
    assert!(!drawn.contains("more line(s)"), "{drawn}");
}

/// What a command prints while it runs is watched as it arrives, and the block it
/// arrives in is filed away as a cell of its own when the result lands: the frame
/// that files it away is not allowed to change anything either.
#[test]
fn a_running_commands_output_is_watched_and_then_kept() {
    let mut screen = screen_for_test(40, 30);
    screen.state.apply(Notice::ToolStart {
        name: "Bash".into(),
        args: r#"{"command":"echo one; echo two"}"#.into(),
    });
    screen.state.apply(Notice::ToolOutput("one\n".into()));
    screen.state.apply(Notice::ToolOutput("two\n".into()));
    screen.draw().unwrap();
    let live = rendered(&render_mod::lines(&mut screen.state, 40));
    assert!(
        live.iter().any(|(text, _)| text.contains("one")),
        "on screen while the command runs: {live:?}"
    );
    screen
        .state
        .apply(Notice::ToolResult("exit_code: 0".into()));
    screen.draw().unwrap();
    assert_eq!(
        screen.state.transcript.last(),
        Some(&Cell::ToolResult("exit_code: 0".into()))
    );
    assert_eq!(
        screen.state.transcript[screen.state.transcript.len() - 2],
        Cell::ToolOutput("one\ntwo\n".into()),
        "the run of output is one cell, whole"
    );
    let drawn = rendered(&render_mod::lines(&mut screen.state, 40));
    let drawn: String = drawn.into_iter().map(|(text, _)| text).collect();
    assert!(drawn.contains("one") && drawn.contains("two"), "{drawn}");
}

#[test]
fn a_folded_think_does_not_jump_open_when_it_closes() {
    // The live block and the cell it becomes go through the same layout, so
    // the frame that files the block away is allowed to change nothing: the
    // think the reader watched is the think the transcript keeps.
    let mut screen = screen_for_test(40, 30);
    let think = (0..30)
        .map(|i| format!("line {i}"))
        .collect::<Vec<_>>()
        .join("\n");
    screen.state.apply(Notice::Reasoning(think));
    screen.draw().unwrap();
    let live = render_mod::lines(&mut screen.state, 40);
    screen.state.apply(Notice::FinishTurn);
    screen.draw().unwrap();
    assert_eq!(
        render_mod::lines(&mut screen.state, 40),
        live,
        "the same lines either way"
    );
}

#[test]
fn the_gate_is_drawn_whole_however_long_the_call_is() {
    // The question is what the answer is about: a command clipped by the
    // width leaves nothing to decide with.
    let mut screen = screen_for_test(20, 20);
    screen.state.question = Some(Cell::approval(
        "Bash",
        r#"{"command":"rm -rf /tmp/aaaaaaaaaaaaaaaaaaaa"}"#,
    ));
    screen.draw().unwrap();
    let drawn = all_rows(&screen).join("");
    assert!(drawn.contains("aaaa"), "the call is there: {drawn}");
    assert!(
        drawn.contains("run it? [y/N]"),
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
        .apply(Notice::Error("the backend said 402".into()));
    screen.state.apply(Notice::ToolStart {
        name: "Bash".into(),
        args: r#"{"command":"ls"}"#.into(),
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
        state.textarea.placeholder_text(),
        super::super::input::IDLE_PLACEHOLDER
    );

    state.begin_turn(Instant::now());
    assert_eq!(
        state.textarea.placeholder_text(),
        super::super::input::QUEUE_PLACEHOLDER
    );

    let (reply, _answer) = tokio::sync::oneshot::channel();
    state.open_question(reply);
    assert_eq!(
        state.textarea.placeholder_text(),
        super::super::input::ANSWER_PLACEHOLDER
    );

    state.close_question();
    assert_eq!(
        state.textarea.placeholder_text(),
        super::super::input::QUEUE_PLACEHOLDER,
        "the turn is still running"
    );

    state.end_turn();
    assert_eq!(
        state.textarea.placeholder_text(),
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

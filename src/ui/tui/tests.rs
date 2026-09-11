//! The tests for the front end as a whole.
use super::input::{
    ANSWER_PLACEHOLDER, IDLE_PLACEHOLDER, QUEUE_PLACEHOLDER, QUEUE_ROWS, SECRET_MASK,
    SECRET_PLACEHOLDER, Submitted,
};
use super::layout::box_rows;
use super::layout::{BOX_GUTTER, BOX_ROWS, PINNED_ROWS, Window, picker_window};
use super::layout::{TODO_HEADS, TODO_ROWS, todo_rows, todo_window};
use super::layout::{box_field, screen_rows};
use super::notice::Notice;
use super::notice::Notifier;
use super::paint::{MEASURE, THINKING_LINES};
use super::paint::{cell_lines, wrapped_lines};
use super::picker::PICKER_ROWS;
use super::picker::{Choice, named_rows};
use super::picker::{Choosing, choice_rows};
use super::screen::Screen;
use super::screen::fullscreen;
use super::state::SPINNER;
use super::state::State;
use super::state::{Scroll, WHEEL_LINES};
use super::{CtrlC, Menu, menu_for};
use crate::config;
use crate::history;
use crate::session;
use crate::types::Message;
use crate::types::Usage;
use crate::ui::Renderer;
use crate::ui::Verdict;
use crate::ui::cell::Span;
use crate::ui::cell::Style;
use crate::ui::cell::{self, Cell};
use crate::ui::{Cancel, Front, Ui};
use crossterm::event::Event;
use crossterm::event::MouseEventKind;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use crossterm::event::{MouseButton, MouseEvent};
use ratatui::backend::Backend;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::text::Line;
use std::time::Duration;
use std::time::Instant;
use tokio::sync::{mpsc, oneshot, watch};

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
            role: crate::types::Role::Assistant,
            content: Some("running it".into()),
            reasoning_content: Some("let me think".into()),
            tool_calls: None,
            tool_call_id: None,
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
    let lines = screen.lines(80);
    assert_eq!(lines.len(), 2, "answer, then the question");
    assert!(format!("{:?}", lines[1]).contains("run it?"));
}

/// The text of a rendered line, with whatever width it occupies.
fn rendered(lines: &[Line<'static>]) -> Vec<(String, usize)> {
    lines
        .iter()
        .map(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            let width = super::super::text::width(&text);
            (text, width)
        })
        .collect()
}

fn wrap(text: &str, width: usize) -> Vec<(String, usize)> {
    rendered(&wrapped_lines(&[Span::new(Style::Plain, text)], width))
}

#[test]
fn a_long_line_is_wrapped_not_clipped() {
    // Nothing may be lost: the plain front end leaves this to the terminal's
    // soft wrapping, but `insert_before` renders into a fixed-width buffer,
    // where an over-long line is silently cut off.
    let text: String = "abcdefghij".repeat(3); // 30 columns
    let lines = wrap(&text, 8);
    assert_eq!(
        lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
        vec!["abcdefgh", "ijabcdef", "ghijabcd", "efghij"],
        "every column survives, in order"
    );
    assert!(lines.iter().all(|(_, w)| *w <= 8), "and none overflows");
}

#[test]
fn a_line_breaks_at_a_space_and_not_inside_a_word() {
    // The whole point of the break: a wrapped line reads as two lines instead of
    // as two halves of a word.
    let wrapped: Vec<String> = wrap("aaaa bbbb cccc", 10)
        .into_iter()
        .map(|(text, _)| text)
        .collect();
    assert_eq!(wrapped, vec!["aaaa bbbb", "cccc"]);
    // The space it broke at is the break and not a column of the text: no line
    // begins or ends with one.
    for width in 1..=20 {
        for (text, _) in wrap("alpha beta gamma", width) {
            assert!(!text.starts_with(' '), "{width}: {text:?}");
            assert!(!text.ends_with(' '), "{width}: {text:?}");
        }
    }
}

#[test]
fn a_word_wider_than_the_line_is_cut_where_it_falls() {
    // A word that fits no line at all is cut -- and cut on the line it started
    // on: moving it down would leave the columns before it empty without saving
    // the cut.
    let text = format!("aaaa {}", "b".repeat(14));
    let wrapped: Vec<String> = wrap(&text, 8).into_iter().map(|(t, _)| t).collect();
    assert_eq!(wrapped, vec!["aaaa bbb", "bbbbbbbb", "bbb"]);
    assert_eq!(wrapped.concat().replace(' ', ""), text.replace(' ', ""));
}

#[test]
fn wrapping_loses_no_column_of_what_is_not_a_break() {
    // Every width: no line over it, and every character that is not a space the
    // break was taken at still on the screen, in order.
    let text = "the quick brown fox jumps over the lazy dog";
    let letters: String = text.chars().filter(|c| *c != ' ').collect();
    for width in 1..=24 {
        let lines = wrap(text, width);
        assert!(lines.iter().all(|(_, w)| *w <= width), "{width}: {lines:?}");
        let joined: String = lines.iter().map(|(t, _)| t.as_str()).collect();
        let kept: String = joined.chars().filter(|c| *c != ' ').collect();
        assert_eq!(kept, letters, "{width}: {lines:?}");
    }
}

#[test]
fn a_wide_terminal_keeps_a_line_of_text_to_the_measure() {
    // The region can be wider than a line of text should be. What is laid out is
    // laid out to the measure, so the far columns of a wide terminal stay empty
    // and the eye does not have to run the whole way back.
    let mut screen = screen_for_test(200, 30);
    screen
        .state
        .transcript
        .push(Cell::Content("word ".repeat(60).trim_end().to_owned()));
    screen.draw().unwrap();
    let rows = all_rows(&screen);
    let transcript = &rows[..transcript_rows(30, BOX_ROWS, 0) as usize];
    let widest = transcript
        .iter()
        .map(|r| super::super::text::width(r.trim_end()))
        .max()
        .unwrap();
    assert!(
        widest <= MEASURE,
        "laid out to the measure: {widest} columns"
    );
    assert!(widest > MEASURE - 10, "and the measure is used: {widest}");
    assert!(
        transcript[0].starts_with("word word"),
        "the left edge is kept"
    );
}

#[test]
fn a_wide_character_is_never_split_across_lines() {
    // Ten ideographs are twenty columns; at eight, each line holds four.
    let text = "\u{6df1}".repeat(10);
    let lines = wrap(&text, 8);
    assert_eq!(lines.len(), 3, "8 + 8 + 4 columns");
    assert_eq!(lines[2].1, 4);
    assert_eq!(super::super::text::width(&text), 20, "nothing was dropped");
    assert!(
        lines
            .iter()
            .all(|(t, _)| t.chars().count() % 4 == 0 || t.chars().count() == 2)
    );
}

#[test]
fn a_character_wider_than_the_field_still_makes_progress() {
    // Degenerate, but a loop that cannot make progress would hang the front
    // end rather than look wrong for one frame.
    let lines = wrap("\u{6df1}\u{6df1}", 1);
    assert_eq!(lines.len(), 2, "one glyph per line, clipped");
    assert_eq!(lines[0].0, "\u{6df1}");
}

#[test]
fn explicit_line_breaks_survive_wrapping() {
    let lines = wrap("one\ntwo", 40);
    assert_eq!(
        lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
        vec!["one", "two"]
    );
}

#[test]
fn a_blank_line_in_the_text_stays_blank() {
    let lines = wrap("a\n\nb", 40);
    assert_eq!(
        lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
        vec!["a", "", "b"]
    );
}

#[test]
fn breaking_exactly_at_the_width_does_not_add_a_blank_line() {
    // The wrap already ended the line; the explicit newline that follows must
    // not look like a blank one.
    let lines = wrap("ab\ncd", 2);
    assert_eq!(
        lines.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
        vec!["ab", "cd"]
    );
}

#[test]
fn wrapping_keeps_each_span_in_its_own_style() {
    let spans = [Span::new(Style::Dim, "ab"), Span::new(Style::Plain, "cd")];
    let lines = wrapped_lines(&spans, 4);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].spans.len(), 2, "two styles on one line");
    assert_eq!(lines[0].spans[0].style.fg, None, "dim is a modifier");
}

/// A front end drawing into memory instead of a terminal.
/// The cell a todo call leaves in the transcript, built the way the transcript
/// itself builds it: out of the call's own arguments.
fn written(todos: serde_json::Value) -> Cell {
    Cell::tool_call("TodoWrite", &todos.to_string())
}

fn screen_for_test(width: u16, height: u16) -> Screen<ratatui::backend::TestBackend> {
    let backend = ratatui::backend::TestBackend::new(width, height);
    Screen {
        terminal: fullscreen(backend).unwrap(),
        state: State::default(),
        tty: None,
        drawn: None,
    }
}

/// How many rows a test's transcript has on a terminal `height` rows tall,
/// with a box `input` rows tall and `queued` rows for the queue: the layout's
/// answer, asked the same way the screen asks it, so that a test that fills
/// the transcript fills the rows that are really there.
fn transcript_rows(height: u16, input: u16, queued: u16) -> u16 {
    screen_rows(Rect::new(0, 0, 40, height), 0, input, queued)[0].height
}

#[test]
fn paging_to_either_end_stops_there() {
    let mut view = Scroll::default();
    let (total, height) = (100, 10);
    view.by(-1000, total, height);
    assert_eq!(view.first(total, height), 0, "the beginning of it");
    view.bottom();
    assert_eq!(view.first(total, height), 90, "and its end");
}

/// One row of what was drawn, without the padding.
fn row(screen: &Screen<ratatui::backend::TestBackend>, y: u16) -> String {
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
/// The gutter itself is what the tests about the left edge are for: a test about
/// scrolling that had to name the marker would fail the day the marker changed,
/// and would have failed for a reason that has nothing to do with scrolling.
fn body(screen: &Screen<ratatui::backend::TestBackend>, y: u16) -> String {
    unset(&row(screen, y))
}

/// The same, for a test that already holds the drawn rows.
fn unset(drawn: &str) -> String {
    // Skipped by character and not by column: every marker is one column, which
    // is the whole reason the gutters are one width.
    drawn.chars().skip(cell::MARKER_COLUMNS).collect()
}

/// Where the drawn area is. A full screen starts at the origin, and a test
/// asks rather than assumes: the frame is what says where the rows are.
fn origin(screen: &mut Screen<ratatui::backend::TestBackend>) -> Rect {
    screen.terminal.get_frame().area()
}

/// Every row of the screen, so a test can ask whether something was drawn
/// without pinning down which row it landed on.
fn all_rows(screen: &Screen<ratatui::backend::TestBackend>) -> Vec<String> {
    let height = screen.terminal.backend().buffer().area.height;
    (0..height).map(|y| row(screen, y)).collect()
}

#[test]
fn the_pinned_rows_take_the_bottom_of_the_screen() {
    // The shape the pinned region has to keep, whatever the transcript does:
    // the box, then the status line, on the last rows of the terminal.
    let mut screen = screen_for_test(60, 20);
    screen.state.status.set_model("m-1");
    screen.draw().unwrap();
    let last = screen.terminal.backend().buffer().area.height - 1;
    assert!(row(&screen, last - 3).starts_with('─'), "the box's top");
    assert!(
        // The editor puts a space in front of the placeholder, which is where
        // its cursor would stand.
        row(&screen, last - 2).ends_with(IDLE_PLACEHOLDER),
        "its text: {:?}",
        row(&screen, last - 2)
    );
    assert!(row(&screen, last - 1).starts_with('─'), "its bottom");
    assert_eq!(row(&screen, last), "m-1 · cache 0.0% · hit 0 · miss 0");
}

#[test]
fn the_queue_is_drawn_above_the_box_until_it_is_run() {
    // What was typed during a turn has to be visible somewhere, or the only
    // proof it arrived is that something happens later. It is drawn as the
    // user line it is about to become, dimmed to say it has not run, at the
    // foot of the transcript -- and it takes rows from the transcript rather
    // than covering it.
    let mut screen = screen_for_test(40, 20);
    screen.state.status.set_model("m-1");
    let last = screen.terminal.backend().buffer().area.height - 1;
    screen.state.enqueue("first".into());
    screen.state.enqueue("second".into());
    screen.draw().unwrap();
    assert_eq!(row(&screen, last - 5), "› first");
    assert_eq!(row(&screen, last - 4), "› second");
    assert!(
        screen.terminal.backend().buffer()[(0, last - 5)]
            .style()
            .add_modifier
            .contains(Modifier::DIM),
        "dimmed: it is waiting, not part of the session"
    );
    // ... and the box and the status line are where they always are: the
    // queue is inserted, not drawn over anything.
    assert!(row(&screen, last - 3).starts_with('─'), "the box's top");
    assert_eq!(row(&screen, last), "m-1 · cache 0.0% · hit 0 · miss 0");

    // Run one: the queue gives a row back, and what ran is drawn as the
    // transcript's own line -- the same line the queue was showing, in the
    // place the session keeps it, and no longer dimmed.
    screen.state.submit("first");
    assert_eq!(screen.state.dequeue().as_deref(), Some("first"));
    screen.draw().unwrap();
    assert_eq!(row(&screen, 0), "› first", "the transcript has it now");
    assert_eq!(row(&screen, last - 5), "", "the row it gave back");
    assert_eq!(
        row(&screen, last - 4),
        "› second",
        "only what is still waiting is in the queue"
    );
    assert_eq!(row(&screen, last), "m-1 · cache 0.0% · hit 0 · miss 0");
    let buf = screen.terminal.backend().buffer();
    assert!(
        !buf[(2, 0)].style().add_modifier.contains(Modifier::DIM),
        "the line that ran reads as the session's, not as something waiting"
    );
    assert!(
        buf[(2, last - 4)]
            .style()
            .add_modifier
            .contains(Modifier::DIM),
        "and the one behind it still waits"
    );
}

#[test]
fn the_queue_is_capped_and_keeps_its_end() {
    // A queue longer than its rows is drawn from its end, like the transcript:
    // the line that was just typed is the one being looked for, and the rest
    // are waiting behind it either way. What it is not drawing is counted on
    // one of the rows the cap allows, so that three rows of a longer queue do
    // not read as the whole queue.
    let mut state = State::default();
    for i in 0..(QUEUE_ROWS + 2) {
        state.enqueue(format!("line {i}"));
    }
    assert_eq!(state.queue_lines(40).len(), QUEUE_ROWS, "capped, in rows");
    assert_eq!(
        state
            .queue_lines(40)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>(),
        vec!["  … 3 more", "› line 3", "› line 4"]
    );
}

#[test]
fn a_queued_line_wider_than_the_screen_is_wrapped_not_clipped() {
    // Queued lines are wrapped like everything else that is committed to the
    // screen: a fixed-width path would cut the end off the command that is
    // about to be run.
    let mut state = State::default();
    let typed = "x".repeat(50);
    state.enqueue(typed.clone());
    let lines: Vec<String> = state
        .queue_lines(20)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(lines.len(), 3, "52 columns at 20 columns a row");
    assert!(lines.iter().all(|l| super::super::text::width(l) <= 20));
    assert_eq!(lines.concat(), format!("› {typed}"));
}

#[test]
fn the_standing_list_is_the_last_one_written_and_nothing_when_none_is() {
    // Nothing has been written: the block takes no rows at all, so a session that
    // never uses the tool sees exactly the screen it saw before there was one.
    let mut state = State::default();
    assert!(state.todo_lines(40).is_empty());
    assert_eq!(todo_rows(0), 0);

    state.show(written(serde_json::json!({"todos": [
        {"content": "Add the parse function", "status": "completed"},
        {"content": "Draw the cell", "status": "in_progress"}
    ]})));
    assert_eq!(
        state
            .todo_lines(40)
            .iter()
            .map(|l| l.to_string())
            .collect::<Vec<_>>(),
        vec![
            // A blank row first: without it the block reads as the tail of the
            // transcript, which is the one thing it must not be taken for.
            "",
            "· todos · 1/2 done",
            "✔ Add the parse function",
            "▸ Draw the cell",
        ]
    );

    // The whole list is sent every time, so a later write replaces the earlier
    // one: what stands is the last list written and not the run of them.
    state.show(written(serde_json::json!({"todos": []})));
    assert!(
        state.todo_lines(40).is_empty(),
        "a cleared list is not one to keep in view"
    );
}

#[test]
fn a_task_longer_than_the_screen_is_wrapped_not_clipped() {
    // The block is committed to the screen rather than written to a scrolling
    // stream, so it is wrapped like everything else that is: a fixed-width path
    // would cut the end off the one thing the reader is watching for.
    let mut state = State::default();
    let long = "x".repeat(50);
    state.show(written(
        serde_json::json!({"todos": [{"content": long, "status": "in_progress"}]}),
    ));
    let lines: Vec<String> = state.todo_lines(20).iter().map(|l| l.to_string()).collect();
    assert_eq!(
        lines.len(),
        2 + 3,
        "the two head rows, then 52 columns at 20"
    );
    assert!(lines.iter().all(|l| super::super::text::width(l) <= 20));
    // A wrapped task keeps its own column: only the first row carries the mark,
    // and the rows it wraps to line up under its words rather than back at the
    // left edge, where they would read as tasks of their own.
    assert_eq!(lines[2], format!("▸ {}", "x".repeat(18)));
    assert_eq!(lines[3], format!("  {}", "x".repeat(18)));
    assert_eq!(lines[4], format!("  {}", "x".repeat(50 - 36)));
}

#[test]
fn the_block_gives_up_its_rows_before_the_box_does() {
    // Both are pinned. The list is worth rows -- it is what the work is -- but the
    // box is where the session continues, so the block asks for its budget and the
    // box comes out of what is left.
    let mut screen = screen_for_test(40, 24);
    screen.state.transcript.push(Cell::Content("hello".into()));
    screen.draw().unwrap();
    // Nothing pinned: the box sits on the status line, its own three rows.
    assert_eq!(
        box_top(&screen),
        20,
        "no block, so the transcript runs to it"
    );

    screen.state.show(written(serde_json::json!({"todos": [
        {"content": "one"}, {"content": "two"}
    ]})));
    screen.draw().unwrap();
    // The block is drawn whole, in its own rows directly above the box, and the
    // transcript is the region that gave those rows up.
    let block: Vec<String> = screen
        .state
        .todo_lines(40)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(block.len(), 4, "the blank row, the title, and two tasks");
    let at = box_top(&screen) - block.len() as u16;
    for (i, line) in block.iter().enumerate() {
        assert_eq!(&row(&screen, at + i as u16), line, "row {i} of the block");
    }
    assert_eq!(row(&screen, at), "", "the blank row that sets it off");
    assert_eq!(row(&screen, at + 1), "\u{b7} todos \u{b7} 0/2 done");
    assert_eq!(row(&screen, at + 2), "\u{2610} one");
    assert_eq!(row(&screen, at + 3), "\u{2610} two");

    // A draft long enough to want the whole screen gives way to the block: the box
    // is the one of the two that can be bounded without losing the session, so the
    // block keeps every row it asked for. The block moves up with the box -- both
    // are pinned to the bottom -- which is what "directly above it" means.
    type_in(&mut screen.state, "one");
    for _ in 0..12 {
        screen.state.key(ctrl_j());
        type_in(&mut screen.state, "more");
    }
    screen.draw().unwrap();
    let tall = box_top(&screen);
    assert!(tall < at, "the taller box pushed the block up, to {tall}");
    for (i, line) in block.iter().enumerate() {
        let y = tall - block.len() as u16 + i as u16;
        assert_eq!(&row(&screen, y), line, "the block still stands, row {i}");
    }
}

/// The row the input box's top rule is drawn on, or the height when no rule was
/// drawn at all.
fn box_top(screen: &Screen<ratatui::backend::TestBackend>) -> u16 {
    let height = screen.terminal.backend().buffer().area.height;
    (0..height)
        .find(|y| row(screen, *y).starts_with('\u{2500}'))
        .unwrap_or(height)
}

#[test]
fn the_block_budgets_its_own_ends_before_its_tasks() {
    // The cap is in rows and the head is spent first, so a budget that was all head
    // would leave no room for the tasks the budget exists for. Tied to the constant
    // at compile time, because the arithmetic below only works out while it holds.
    const { assert!(TODO_HEADS < TODO_ROWS) };
    assert_eq!(todo_rows(usize::MAX), TODO_ROWS as u16, "capped");
    assert_eq!(todo_rows(TODO_ROWS - 1), (TODO_ROWS - 1) as u16);
}

/// The standing block's window: the task in hand is the one row it cannot lose,
/// and the counts are what give way when there is no room for them -- the picker's
/// bargain, for the same reason.
#[test]
fn the_todo_window_keeps_the_task_in_hand_on_the_screen() {
    for total in 1..14usize {
        for active in [None].into_iter().chain((0..total).map(Some)) {
            for room in 1..=6usize {
                let got = todo_window(total, active, room);
                let what = format!("{total} tasks, in hand {active:?}, room {room}");
                let drawn = got.last - got.first;
                // What is actually spent: the rows of the run, plus a row for each
                // count that is drawn. A count that would not fit is not drawn, and
                // then nothing is claimed -- which is the one case a cut window says
                // nothing about what it left out.
                let counted = usize::from(got.above > 0) + usize::from(got.below > 0);
                assert!(drawn >= 1, "{what}: something is drawn");
                assert!(got.last <= total, "{what}: past the end");
                assert!(drawn + counted <= room, "{what}: rows drawn");
                if counted > 0 {
                    assert_eq!(
                        (got.above, got.below),
                        (got.first, total - got.last),
                        "{what}: what the counts say"
                    );
                }
                // The row that cannot give way: with a task in hand it is drawn, as
                // long as the run it must sit in is not the whole list.
                if let Some(at) = active.filter(|_| total > room) {
                    assert!(got.first <= at && at < got.last, "{what}: in hand");
                }
                if total <= room {
                    // A list with room to spare is never cut and never counted.
                    assert_eq!(
                        (got.first, got.last, got.above, got.below),
                        (0, total, 0, 0),
                        "{what}: a list with room to spare"
                    );
                } else {
                    assert!(got.last - got.first < total, "{what}: a cut list");
                }
                if active.is_none() && total > room {
                    assert_eq!(got.first, 0, "{what}: no task in hand shows the plan");
                }
            }
        }
    }
}

#[test]
fn ctrl_j_makes_the_box_a_line_taller() {
    // The box is the draft's shape: the line Ctrl-J adds has a row to be typed
    // on, and the transcript gives one up for it.
    let mut screen = screen_for_test(40, 20);
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
    let mut screen = screen_for_test(40, 20);
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
    let shows = usize::from(box_rows(usize::MAX, height, 0)) - 2;
    let (draft, kept) = (shows + 4, shows - 2);
    let letters: Vec<char> = "abcdefghijklmnopqrstuvwxyz".chars().take(draft).collect();
    let mut screen = screen_for_test(40, height);
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
    let top = 1 + all_rows(&screen)
        .iter()
        .position(|r| r.starts_with('─'))
        .expect("the box is drawn");
    for (i, letter) in letters[..kept].iter().enumerate() {
        let drawn = body(&screen, (top + i) as u16);
        assert!(drawn.starts_with(*letter), "{drawn:?}");
    }
    assert!(
        row(&screen, (top + kept) as u16).starts_with('─'),
        "and nothing below the last line"
    );
}

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
    screen.state.transcript.push(Cell::tool_call(
        "Write",
        &format!(r#"{{"file_path":"a.txt","content":"{long}"}}"#),
    ));
    screen.draw().unwrap();
    let top = origin(&mut screen).y;
    assert_eq!(row(&screen, top + 1), format!("  + {}", "x".repeat(16)));
    assert_eq!(row(&screen, top + 2), format!("  {}", "x".repeat(14)));
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
    let live = rendered(&screen.state.lines(40));
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
    let drawn = rendered(&screen.state.lines(40));
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
    let live = screen.state.lines(40);
    screen.state.apply(Notice::FinishTurn);
    screen.draw().unwrap();
    assert_eq!(screen.state.lines(40), live, "the same lines either way");
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
    let (cancel, _cancelled) = watch::channel(false);
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
    screen.state.stream(Style::Plain, "half a line");
    screen.commit();
    assert!(screen.state.live.is_none(), "nothing left open");
    screen.state.stream(Style::Plain, " and the rest");
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

// ------------------------------------------------------------ activity ---

/// A state with a turn running, its clock and its stream pre-loaded.
fn working(elapsed: Duration, chars: usize, chars_per_token: f64) -> State {
    State {
        turn_running: true,
        turn_started: Some(Instant::now() - elapsed),
        streamed_chars: chars,
        chars_since_usage: chars,
        chars_per_token,
        ..State::default()
    }
}

#[test]
fn the_working_border_spins_counts_and_estimates() {
    // 12 345 ms in: frame 154 % 10 = 4, the fifth glyph; 4 000 characters at
    // a measured four to the token over 12.3 s rounds to 81 a second.
    let s = working(Duration::from_millis(12_345), 4000, 4.0);
    assert_eq!(s.activity_title(60), Some("⠼ 12s · ~81 token/s".to_owned()));
}

#[test]
fn the_estimate_waits_for_the_average_to_settle() {
    // 2 040 ms: frame 25, off a frame boundary so the clock's second read
    // cannot tip it
    let s = working(Duration::from_millis(2_040), 4000, 4.0);
    assert_eq!(s.activity_title(60), Some("⠴ 2s".to_owned()));
}

#[test]
fn a_silent_turn_estimates_nothing() {
    let s = working(Duration::from_millis(30_040), 0, 4.0);
    assert_eq!(s.activity_title(60), Some("⠴ 30s".to_owned()));
}

#[test]
fn a_narrow_border_drops_the_estimate_then_hides_the_indicator() {
    let s = working(Duration::from_millis(12_345), 4000, 4.0);
    let full = s.activity_title(usize::MAX).unwrap();
    let count = "⠼ 12s".to_owned();
    // one column short of the whole thing, the estimate goes whole
    assert_eq!(
        s.activity_title(crate::ui::text::width(&full) - 1),
        Some(count.clone())
    );
    // one column short of the count, nothing at all: a clipped spinner is
    // not an indicator
    assert_eq!(s.activity_title(crate::ui::text::width(&count) - 1), None);
}

#[test]
fn the_working_indicator_is_not_dim_on_a_dim_rule() {
    // While the model is quiet, the spinner on the box's rule is the only thing
    // on the screen that moves: it is chrome that has to be seen, so it is the
    // one thing on the box painted at full strength. A dim indicator on a dim
    // rule is the signal painted out of sight.
    let mut screen = screen_for_test(60, 20);
    screen.state = working(Duration::from_secs(3), 0, 4.0);
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

#[test]
fn calibration_learns_from_a_usage_notice() {
    let mut s = State {
        turn_running: true,
        turn_started: Some(Instant::now()),
        ..State::default()
    };
    s.stream(Style::Reasoning, &"x".repeat(800));
    s.apply(Notice::Usage(
        Usage {
            completion_tokens: 200,
            ..Usage::default()
        },
        Duration::ZERO,
    ));
    assert_eq!(s.chars_per_token, 4.0);
    // the counter is spent on the measurement: the next ratio starts clean
    assert_eq!(s.chars_since_usage, 0);
}

#[test]
fn a_question_takes_the_border_title_back() {
    let mut s = working(Duration::from_secs(12), 4000, 4.0);
    let (tx, _rx) = oneshot::channel();
    s.open_question(tx);
    assert_eq!(s.activity_title(60), None);
}

#[test]
fn the_border_shows_the_working_turn_and_then_does_not() {
    let mut screen = screen_for_test(60, 20);
    screen.state.begin_turn(Instant::now());
    screen.state.turn_started = Some(Instant::now() - Duration::from_secs(12));
    screen.state.streamed_chars = 4000;
    screen.state.chars_per_token = 4.0;
    screen.draw().unwrap();
    // the box's top border: the status line's row, the box's three, and no
    // queue above it
    let top = 20 - 1 - BOX_ROWS;
    let line = row(&screen, top);
    assert!(line.contains("token/s"), "{line:?}");
    screen.state.end_turn();
    screen.draw().unwrap();
    assert!(!row(&screen, top).contains("token/s"));
}

fn press(state: &mut State, code: KeyCode) -> Submitted {
    state.key(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
}

/// A mouse event of a given kind. Where the pointer is does not matter: this
/// front end reads the kind and nothing else.
fn mouse(kind: MouseEventKind) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column: 1,
        row: 1,
        modifiers: KeyModifiers::NONE,
    })
}

fn type_in(state: &mut State, text: &str) {
    for c in text.chars() {
        press(state, KeyCode::Char(c));
    }
}

/// Ctrl-J: the newline key, and so the one the box has to make room for.
fn ctrl_j() -> Event {
    Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL))
}

#[test]
fn a_slash_opens_the_picker_and_a_space_closes_it() {
    let mut screen = State::default();
    assert!(screen.picker.is_none(), "nothing typed, nothing to offer");
    type_in(&mut screen, "/res");
    let picker = screen.picker.as_ref().expect("a command is being named");
    assert_eq!(
        picker
            .choices
            .iter()
            .map(|c| c.label.as_str())
            .collect::<Vec<_>>(),
        vec!["/resume"]
    );
    // A space means the rest is an argument, not part of the name.
    type_in(&mut screen, " 2026");
    assert!(screen.picker.is_none());
}

#[test]
fn the_highlight_wraps_in_both_directions() {
    let mut screen = State::default();
    type_in(&mut screen, "/");
    let count = screen.picker.as_ref().unwrap().choices.len();
    assert!(count > 1, "the bare slash offers every command");
    assert_eq!(screen.picker.as_ref().unwrap().selected, 0);
    press(&mut screen, KeyCode::Up);
    assert_eq!(
        screen.picker.as_ref().unwrap().selected,
        count - 1,
        "up from the first reaches the last"
    );
    press(&mut screen, KeyCode::Down);
    assert_eq!(screen.picker.as_ref().unwrap().selected, 0);
}

#[test]
fn tab_puts_the_highlighted_command_in_the_box_without_running_it() {
    let mut screen = State::default();
    type_in(&mut screen, "/res");
    assert!(matches!(
        press(&mut screen, KeyCode::Tab),
        Submitted::Nothing
    ));
    assert_eq!(screen.text(), "/resume");
    assert!(screen.picker.is_none(), "it has been chosen");
    // And it is not submitted: an argument may still be wanted.
    assert!(!screen.history.iter().any(|h| h == "/resume"));
}

#[test]
fn escape_dismisses_the_command_picker_and_the_line_it_was_filtering() {
    let mut screen = State::default();
    type_in(&mut screen, "/s");
    press(&mut screen, KeyCode::Esc);
    assert!(screen.picker.is_none());
    assert_eq!(
        screen.text(),
        "",
        "the half-typed command goes with the list: a lone `/` left behind \
         would glue itself to the next word"
    );
    assert_eq!(
        screen.textarea.placeholder_text(),
        IDLE_PLACEHOLDER,
        "the box is back to inviting the next message"
    );
}

#[test]
fn escape_while_a_turn_runs_gives_the_queue_line_back() {
    let mut screen = State::default();
    screen.begin_turn(Instant::now());
    type_in(&mut screen, "/s");
    press(&mut screen, KeyCode::Esc);
    assert!(screen.picker.is_none());
    assert_eq!(
        screen.textarea.placeholder_text(),
        QUEUE_PLACEHOLDER,
        "the box still belongs to the turn that is running"
    );
}

/// A session as the picker sees it. Its path is never read: the front end is
/// handed the list, it does not go looking for it.
fn session_info(id: &str, messages: usize, preview: &str) -> session::SessionInfo {
    session::SessionInfo {
        id: id.to_owned(),
        path: std::path::PathBuf::from(format!("/nowhere/{id}.jsonl")),
        modified: 0,
        message_count: messages,
        preview: preview.to_owned(),
    }
}

#[test]
fn sessions_are_offered_newest_first_with_what_is_in_them() {
    let mut screen = State::default();
    assert!(screen.open_sessions(&[
        session_info("20260910-120000", 12, "what does this do?"),
        session_info("20260909-090000", 3, "hello"),
    ]));
    let picker = screen.picker.as_ref().unwrap();
    assert_eq!(picker.kind, Choosing::Session);
    let shown: Vec<_> = picker
        .choices
        .iter()
        .map(|c| (c.label.as_str(), c.detail.as_str()))
        .collect();
    assert_eq!(
        shown,
        vec![
            ("20260910-120000", "12 messages · what does this do?"),
            ("20260909-090000", "3 messages · hello"),
        ],
        "the order is the list's, and the detail is what is in the session"
    );
}

#[test]
fn choosing_a_session_submits_the_line_that_switches_to_it() {
    // The choice is not a completion: it is the line the plain prompt would
    // have been given, submitted, so switching itself has one implementation.
    let mut screen = State::default();
    screen.open_sessions(&[
        session_info("newest", 1, "hi"),
        session_info("older", 2, "yo"),
    ]);
    press(&mut screen, KeyCode::Down);
    assert!(matches!(
        press(&mut screen, KeyCode::Enter),
        Submitted::Line
    ));
    assert_eq!(screen.text(), "/resume older");
    assert!(screen.picker.is_none(), "it has been chosen");
}

#[test]
fn tab_takes_the_highlighted_session_too() {
    let mut screen = State::default();
    screen.open_sessions(&[session_info("only", 1, "hi")]);
    assert!(matches!(press(&mut screen, KeyCode::Tab), Submitted::Line));
    assert_eq!(screen.text(), "/resume only");
}

#[test]
fn typing_does_not_turn_a_session_list_into_a_command_search() {
    // It was asked for in full, and it stays until it is answered or
    // dismissed: recomputing matches under the user's fingers would replace
    // the list with something they did not ask for.
    let mut screen = State::default();
    screen.open_sessions(&[session_info("one", 1, "hi")]);
    type_in(&mut screen, "/he");
    let picker = screen.picker.as_ref().expect("still the session list");
    assert_eq!(picker.kind, Choosing::Session);
    assert_eq!(picker.choices.len(), 1);
    press(&mut screen, KeyCode::Esc);
    assert!(screen.picker.is_none(), "escape is how it is dismissed");
    assert_eq!(
        screen.text(),
        "/he",
        "a row list is not the line's: what was typed since it opened is a \
         draft, not a query, and dismissing the list has no claim on it"
    );
}

#[test]
fn nothing_to_offer_leaves_the_line_alone() {
    // With no sessions on disk the command has to reach the handler, which is
    // where both front ends say what an id is for.
    let mut screen = State::default();
    assert!(!screen.open_sessions(&[]));
    assert!(screen.picker.is_none());
}

#[test]
fn the_session_list_is_drawn_like_the_command_one() {
    let mut screen = screen_for_test(60, 20);
    screen
        .state
        .open_sessions(&[session_info("20260910-124721", 3, "what is in this file?")]);
    screen.draw().unwrap();
    let rows = all_rows(&screen);
    assert!(
        rows.iter()
            .any(|r| r.contains("20260910-124721")
                && r.contains("3 messages · what is in this file?")),
        "one row per session: {rows:?}"
    );
}

#[test]
fn a_provider_menu_reads_by_name_and_a_model_menu_by_id() {
    // The two menus read differently on purpose: a provider is picked by the
    // name a person knows it by, a model by the `<provider id>/<modelid>` the
    // flags, the session and the status line all carry.
    let mut screen = screen_for_test(70, 20);
    screen
        .state
        .open_choices(Choosing::Provider, choice_rows(config::provider_choices()));
    screen.draw().unwrap();
    let rows = all_rows(&screen);
    assert!(rows.iter().any(|r| r.contains("DeepSeek")), "{rows:?}");
    assert!(
        rows.iter().any(|r| r.contains("Z.AI Coding CN")),
        "{rows:?}"
    );
    assert!(
        !rows.iter().any(|r| r.contains("deepseek ")),
        "the menu does not show what is typed at: {rows:?}"
    );

    screen.state.open_choices(
        Choosing::Model,
        choice_rows(config::model_menu("deepseek/deepseek-flash")),
    );
    screen.draw().unwrap();
    let rows = all_rows(&screen);
    assert!(
        rows.iter().any(|r| r.contains("deepseek/deepseek-v4-pro")),
        "{rows:?}"
    );
    assert!(
        rows.iter().any(|r| r.contains("zai-coding-cn/glm-5.3")),
        "{rows:?}"
    );
}

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
    for i in 0..(history::MAX_ENTRIES + 5) {
        screen.remember(&format!("line {i}"));
    }
    assert_eq!(screen.history.len(), history::MAX_ENTRIES);
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
fn the_picker_keeps_its_highlight_on_the_screen() {
    // The regression: rows were drawn from the first choice down, so a menu
    // longer than the picker could be scrolled past its own end -- and `Enter`
    // then chose a row the screen never named. Nine sessions, the ninth
    // selected: the row under the highlight is the one that has to be drawn.
    let mut screen = screen_for_test(60, 20);
    let choices: Vec<Choice> = (0..9)
        .map(|i| Choice {
            label: format!("session-{i}"),
            argument: format!("session-{i}"),
            detail: format!("{i} messages"),
        })
        .collect();
    screen.state.open_choices(Choosing::Session, choices);
    for _ in 0..8 {
        screen.state.down();
    }
    screen.draw().unwrap();

    let buf = screen.terminal.backend().buffer();
    let highlighted: Vec<String> = (0..buf.area.height)
        .filter(|&y| (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::REVERSED)))
        .map(|y| row(&screen, y))
        .collect();
    let chosen: Vec<&String> = highlighted
        .iter()
        .filter(|r| r.contains("session-"))
        .collect();
    assert_eq!(
        chosen.len(),
        1,
        "one picker row is highlighted: {highlighted:?}"
    );
    assert!(chosen[0].contains("session-8"), "{:?}", chosen[0]);
    // And the rows the window is not showing are counted, not dropped: four
    // of the nine are above it.
    let drawn = all_rows(&screen);
    assert!(drawn.iter().any(|r| r.contains("… 4 more")), "{drawn:?}");
    // The count costs rows, so the block still fits what the transcript can
    // spare: five rows of menu and the count.
    assert_eq!(
        drawn.iter().filter(|r| r.contains("session-")).count(),
        5,
        "{drawn:?}"
    );
}

#[test]
fn the_queue_counts_the_rows_it_is_not_showing() {
    let mut screen = screen_for_test(60, 20);
    for line in ["/new", "/sessions", "/model"] {
        screen.state.queued.push_back(line.into());
    }
    // A queue with room to spare: every line, and nothing said about rows that
    // are not there.
    let drawn = rendered(&screen.state.queue_lines(60));
    assert_eq!(drawn.len(), 3, "{drawn:?}");
    assert!(
        drawn.iter().all(|(text, _)| !text.contains("more")),
        "{drawn:?}"
    );

    // One more line than the cap, and the row that does not fit is counted on
    // one of the rows the cap allows: the queue never costs more than three.
    screen.state.queued.push_back("/help".into());
    let drawn = rendered(&screen.state.queue_lines(60));
    assert_eq!(drawn.len(), QUEUE_ROWS, "{drawn:?}");
    assert!(drawn[0].0.contains("… 2 more"), "{drawn:?}");
    assert!(drawn[1].0.contains("/model"), "{drawn:?}");
    assert!(drawn[2].0.contains("/help"), "{drawn:?}");
}

#[test]
fn the_picker_is_drawn_over_the_live_area_with_one_row_highlighted() {
    let mut screen = screen_for_test(40, 20);
    type_in(&mut screen.state, "/");
    screen.draw().unwrap();
    let rows = all_rows(&screen);
    let shows = |needle: &str| {
        let at = rows.iter().position(|r| r.contains(needle));
        at.expect("the picker was drawn")
    };
    let help = shows("/help");
    let resume = shows("/resume");
    assert_eq!(resume, help + 3, "the whole list, in table order");
    // The first entry is highlighted, and only it.
    let selected = screen
        .terminal
        .backend()
        .buffer()
        .content
        .iter()
        .filter(|c| c.modifier.contains(Modifier::REVERSED))
        .count();
    assert!(selected > 0, "something is highlighted");
}

#[test]
fn the_transcript_gets_the_rows_the_pinned_region_leaves() {
    // Asked of the layout rather than worked out here. What a terminal too
    // short for the pinned region has to say about it is the whole point: the
    // transcript gets no rows at all there, and a window that believed the
    // arithmetic instead would be scrolling against a row that was never
    // drawn.
    let transcript = |height: u16, input: u16, queued: u16| {
        screen_rows(Rect::new(0, 0, 80, height), 0, input, queued)[0].height
    };
    assert_eq!(
        transcript(24, BOX_ROWS, 0),
        24 - PINNED_ROWS - BOX_ROWS,
        "the box has its borders and the status line its row"
    );
    assert_eq!(
        transcript(5, BOX_ROWS, 0),
        1,
        "one row, on the shortest terminal that can spare it"
    );
    assert_eq!(
        transcript(1, BOX_ROWS, 0),
        0,
        "and none at all when there is nothing to spare"
    );
    assert_eq!(transcript(1, 0, 0), 0, "the status line has the only row");
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
fn a_cramped_screen_windows_the_rows_the_layout_gave_it() {
    // Every height, however short: what the window is measured against has to
    // be the rows the transcript was actually drawn into. The two were worked
    // out separately, and below the height the pinned region needs they
    // disagreed -- so the window scrolled against a row that was never drawn,
    // and `drawn_rows` is what paging and staying put both read.
    for height in 1..8u16 {
        let mut screen = screen_for_test(40, height);
        screen.state.transcript.push(Cell::Content("one".into()));
        screen.state.transcript.push(Cell::Content("two".into()));
        screen.draw().unwrap();
        let area = origin(&mut screen);
        let input = screen.state.input_rows(height, 0);
        let queued = screen.state.queue_lines(40).len() as u16;
        assert_eq!(
            screen.state.drawn_rows,
            screen_rows(area, 0, input, queued)[0].height as usize,
            "a {height}-row terminal"
        );
    }
    // And the row it drew is the one the window asked for: the room the
    // layout gave it, taken from the end of the transcript. Five rows is the
    // shortest terminal with room for the box as well as a transcript row.
    let mut screen = screen_for_test(40, 5);
    screen.state.transcript.push(Cell::Content("one".into()));
    screen.state.transcript.push(Cell::Content("two".into()));
    screen.draw().unwrap();
    assert_eq!(row(&screen, 0), "two", "the last line of the transcript");
    assert!(row(&screen, 1).starts_with('─'), "then the box");
    assert!(row(&screen, 4).contains("cache"), "and the status line");
}

#[test]
fn a_draw_lays_out_only_what_arrived_since_the_last_one() {
    // What is pinned here is invisible on the screen -- a draw that wrapped
    // the whole session would draw the same picture -- and it is the whole
    // point of the layout being kept: a session is as long as the
    // conversation has been, and a fragment arrives many times a second.
    let mut screen = screen_for_test(40, 20);
    screen.state.transcript.push(Cell::Content("one".into()));
    screen.state.transcript.push(Cell::Content("two".into()));
    screen.state.laid_cells = 0;
    screen.draw().unwrap();
    assert_eq!(screen.state.laid_cells, 2, "both of them, once");
    screen.draw().unwrap();
    assert_eq!(screen.state.laid_cells, 2, "and not again");

    // A fragment of a running turn is not a cell yet: the block it is writing
    // is wrapped as it is drawn, and it becomes a cell -- one to lay out --
    // when it closes.
    screen.state.apply(Notice::Content("three".into()));
    screen.draw().unwrap();
    assert_eq!(screen.state.laid_cells, 2, "still the two cells");
    screen.state.apply(Notice::FinishTurn);
    screen.draw().unwrap();
    assert_eq!(screen.state.laid_cells, 3, "the block it left behind");

    // A resize re-lays the whole transcript: every line was wrapped to a
    // width, so a new width is a new layout of every cell there is.
    screen.terminal.backend_mut().resize(30, 20);
    screen.draw().unwrap();
    assert_eq!(
        screen.state.laid_cells, 6,
        "all three again, at the new width"
    );
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
    let whole: Vec<Line> = screen
        .state
        .transcript
        .iter()
        .flat_map(|cell| cell_lines(cell, width))
        .collect();
    assert_eq!(screen.state.lines(width), whole, "the whole transcript");
    assert_eq!(
        screen.state.window_lines(3, 9, &[], &[]),
        whole[3..9].to_vec(),
        "and a window in the middle of one cell"
    );
}

#[test]
fn everything_that_changes_the_screen_moves_the_revision() {
    let mut state = State::default();
    let mut moved = Vec::new();
    let mut step = |state: &State| moved.push(state.revision);
    step(&state);
    state.apply(Notice::Content("hello".into()));
    step(&state);
    state.key(Event::Key(KeyEvent::from(KeyCode::Char('h'))));
    step(&state);
    let (reply, _answer) = oneshot::channel();
    state.open_question(reply);
    step(&state);
    state.close_question();
    step(&state);
    let mut sorted = moved.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, moved, "every step moved it: {moved:?}");
}

/// A backend that counts the frames it is asked to paint, so that a draw the
/// screen decided to skip is something a test can observe rather than infer.
struct Counting {
    inner: ratatui::backend::TestBackend,
    frames: std::rc::Rc<std::cell::Cell<usize>>,
}

impl Backend for Counting {
    type Error = std::convert::Infallible;

    fn draw<'a, I>(&mut self, content: I) -> Result<(), std::convert::Infallible>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.frames.set(self.frames.get() + 1);
        self.inner.draw(content)
    }

    fn hide_cursor(&mut self) -> Result<(), std::convert::Infallible> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> Result<(), std::convert::Infallible> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(
        &mut self,
    ) -> Result<ratatui::layout::Position, std::convert::Infallible> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> Result<(), std::convert::Infallible> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> Result<(), std::convert::Infallible> {
        self.inner.clear()
    }

    fn clear_region(
        &mut self,
        clear_type: ratatui::backend::ClearType,
    ) -> Result<(), std::convert::Infallible> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> Result<ratatui::layout::Size, std::convert::Infallible> {
        self.inner.size()
    }

    fn window_size(&mut self) -> Result<ratatui::backend::WindowSize, std::convert::Infallible> {
        self.inner.window_size()
    }

    fn flush(&mut self) -> Result<(), std::convert::Infallible> {
        self.inner.flush()
    }
}

#[test]
fn an_unchanged_screen_is_not_drawn_again() {
    let frames = std::rc::Rc::new(std::cell::Cell::new(0));
    let backend = Counting {
        inner: ratatui::backend::TestBackend::new(40, 20),
        frames: frames.clone(),
    };
    let mut screen = Screen {
        terminal: fullscreen(backend).unwrap(),
        state: State::default(),
        tty: None,
        drawn: None,
    };
    screen.draw_if_changed().unwrap();
    assert_eq!(frames.get(), 1);
    screen.draw_if_changed().unwrap();
    assert_eq!(frames.get(), 1, "nothing changed, so nothing was drawn");
    // A keystroke changes what the box holds, so the next draw happens.
    screen
        .state
        .key(Event::Key(KeyEvent::from(KeyCode::Char('h'))));
    screen.draw_if_changed().unwrap();
    assert_eq!(frames.get(), 2);
    // So does a turn starting: the box's placeholder says so, which is a
    // change the revision has to carry.
    screen.state.begin_turn(Instant::now());
    screen.draw_if_changed().unwrap();
    assert_eq!(frames.get(), 3);
}

#[test]
fn closing_a_turn_is_worth_a_draw() {
    // The turn's end is the one thing that changes the screen without a
    // notice arriving, so it has to move the revision itself.
    let mut screen = screen_for_test(40, 20);
    screen.state.stream(Style::Plain, "half a line");
    screen.draw_if_changed().unwrap();
    let drawn = screen.state.revision;
    screen.commit();
    assert_ne!(screen.state.revision, drawn, "the draw cannot be skipped");
}

#[test]
fn both_front_ends_satisfy_the_handle_the_repl_needs() {
    // Compile-time guard: `repl::handle` is written against `Front`, and the
    // interactive front end must keep satisfying it alongside the plain one.
    fn assert_front<T: Front>() {}
    assert_front::<Notifier>();
    assert_front::<Renderer>();
}

#[test]
fn the_notifier_reaches_the_loop_through_the_channel() {
    // The `Ui` side must not touch the terminal: everything it is told has to
    // come out as a notice.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut n = Notifier { tx };
    n.content_delta("hi");
    assert!(matches!(rx.try_recv(), Ok(Notice::Content(s)) if s == "hi"));
}

#[test]
fn what_the_user_says_becomes_part_of_the_transcript() {
    // Replay reads the user's line out of the log, so a live turn has to put it
    // in the same place: the same session must not read two ways depending on
    // when it was looked at.
    let mut state = State::default();
    state.submit("look at src/main.rs");
    assert_eq!(state.transcript, vec![Cell::user("look at src/main.rs")]);
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
    let mut screen = screen_for_test(40, 20);
    screen.state.submit("look at src/main.rs");
    screen.state.transcript.push(Cell::Content("on it".into()));
    screen.draw().unwrap();
    let top = origin(&mut screen).y;
    assert_eq!(row(&screen, top), "› look at src/main.rs");
    assert_eq!(row(&screen, top + 1), "on it");
}

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

/// Type into the box the way the event loop delivers a key while a turn runs.
fn type_while_working(state: &mut State, text: &str, cancel: &watch::Sender<bool>) {
    for c in text.chars() {
        state.key_while_working(Event::Key(KeyEvent::from(KeyCode::Char(c))), cancel);
    }
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
fn the_box_says_what_enter_will_do() {
    // Three states, three invitations: the answer to a question, the line for
    // after the turn, and the next message. The box is where all three are
    // typed, so it is the only place that can say which one it is.
    let mut state = State::default();
    assert_eq!(state.textarea.placeholder_text(), IDLE_PLACEHOLDER);

    state.begin_turn(Instant::now());
    assert_eq!(state.textarea.placeholder_text(), QUEUE_PLACEHOLDER);

    let (reply, _answer) = oneshot::channel();
    state.open_question(reply);
    assert_eq!(state.textarea.placeholder_text(), ANSWER_PLACEHOLDER);

    state.close_question();
    assert_eq!(
        state.textarea.placeholder_text(),
        QUEUE_PLACEHOLDER,
        "the turn is still running"
    );

    state.end_turn();
    assert_eq!(state.textarea.placeholder_text(), IDLE_PLACEHOLDER);
}

#[test]
fn a_line_being_typed_is_held_aside_while_the_gate_is_open() {
    // The answer to "run it?" is a `y`, and a sentence that was already in the
    // box is not one. The gate takes the box for its answer and gives it back.
    let mut state = State::default();
    let (cancel, _cancelled) = watch::channel(false);
    state.begin_turn(Instant::now());
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
    let mut screen = screen_for_test(60, 20);
    let (cancel, _cancelled) = watch::channel(false);
    let (reply, answer) = oneshot::channel();
    screen.state.begin_turn(Instant::now());
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
    let mut screen = screen_for_test(60, 20);
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
    state.begin_turn(Instant::now());
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

#[test]
fn a_provider_or_model_row_is_submitted_as_the_line_it_stands_for() {
    // The pickers `/login`, `/model` and `/effort` open: the row is the
    // argument, and the command that takes it is the one the plain prompt would
    // be given, so choosing is implemented once for both front ends.
    let mut state = State::default();
    assert!(state.open_choices(
        Choosing::Provider,
        vec![
            Choice {
                label: "DeepSeek".into(),
                argument: "deepseek".into(),
                detail: "key stored".into(),
            },
            Choice {
                label: "Z.AI Coding CN".into(),
                argument: "zai-coding-cn".into(),
                detail: "no key".into(),
            },
        ]
    ));
    state.down();
    assert!(state.choose());
    assert_eq!(
        state.textarea.lines(),
        ["/login zai-coding-cn"],
        "the row is read by name and submitted by id"
    );

    assert!(state.open_choices(
        Choosing::Model,
        named_rows(vec![("zai-coding-cn/glm-5.3".into(), "current".into())])
    ));
    assert!(state.choose());
    assert_eq!(state.textarea.lines(), ["/model zai-coding-cn/glm-5.3"]);

    assert!(state.open_choices(
        Choosing::Effort,
        named_rows(vec![("max".into(), "current".into())])
    ));
    assert!(state.choose());
    assert_eq!(
        state.textarea.lines(),
        ["/effort max"],
        "a tier is named by itself"
    );
}

#[test]
fn an_offered_list_of_providers_survives_typing_like_a_session_list_does() {
    // It was asked for in full; recomputing completions over it would take
    // the list away the moment a letter was typed.
    let mut state = State::default();
    state.open_choices(
        Choosing::Provider,
        named_rows(vec![("Z.AI Coding CN".into(), "no key".into())]),
    );
    state.refresh_picker();
    assert_eq!(state.picker.as_ref().unwrap().kind, Choosing::Provider);
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

#[test]
fn zz_visual_review_dump() {
    let mut screen = screen_for_test(96, 30);
    screen.state.status.set_model("deepseek/deepseek-flash");
    screen.state.show(Cell::Notice(
        "caocli \u{b7} session 20260910-224129 (12 messages) \u{b7} deepseek/deepseek-flash".into(),
    ));
    screen
        .state
        .transcript
        .push(Cell::user("why is the build slow?"));
    screen.state.transcript.push(Cell::Reasoning(
        "The user asks about build time. I should look at the Cargo profile and maybe check if there are heavy dependencies. Let me start by reading Cargo.toml and then check the target directory size.".into(),
    ));
    screen.state.transcript.push(Cell::Content(
        "Two things usually dominate: an unoptimized dev profile and relinking every dependency on each edit. Let me look.".into(),
    ));
    screen.state.transcript.push(Cell::tool_call(
        "Bash",
        r#"{"command":"ls -la target/debug | head -20"}"#,
    ));
    screen.state.transcript.push(Cell::ToolResult(
        "total 4823136\ndrwxr-xr-x 12 user user 4096 ...\n".into(),
    ));
    screen.state.transcript.push(Cell::tool_call(
        "Edit",
        r#"{"file_path":"Cargo.toml","old_string":"[profile.dev]\ndebug = 2","new_string":"[profile.dev]\ndebug = 0"}"#,
    ));
    screen
        .state
        .transcript
        .push(Cell::ToolResult("edited Cargo.toml\n".into()));
    screen.state.transcript.push(Cell::Content(
        "Setting `debug = 0` alone is usually worth a third of the link time. The other half is the linker: with `lld` the final link stops being the long pole.".into(),
    ));
    screen.state.apply(Notice::Usage(
        Usage {
            prompt_tokens: 12480,
            total_tokens: 12980,
            completion_tokens: 500,
            prompt_cache_hit_tokens: 12000,
            prompt_cache_miss_tokens: 480,
            prompt_tokens_details: None,
        },
        Duration::from_secs(9),
    ));
    screen.state.transcript.push(Cell::Failure(
        "no API key for DeepSeek: run /login deepseek".into(),
    ));
    screen.state.transcript.push(Cell::Interrupted);
    screen
        .state
        .transcript
        .push(Cell::Notice("/help for commands".into()));
    screen.draw().unwrap();
    let buf = screen.terminal.backend().buffer();
    for y in 0..buf.area.height {
        let mut line = String::new();
        for x in 0..buf.area.width {
            let c = &buf[(x, y)];
            let m = c.style().add_modifier;
            let tag = if m.contains(Modifier::DIM) {
                "d"
            } else if c.style().fg == Some(Color::Yellow) {
                "Y"
            } else if c.style().fg == Some(Color::Green) {
                "G"
            } else if c.style().fg == Some(Color::Red) {
                "R"
            } else {
                " "
            };
            line.push_str(tag);
        }
        println!("STYLE {y:02} {line}");
        println!("TEXT  {y:02} |{}|", row(&screen, y));
    }
}

#[test]
fn a_menu_command_names_its_menu() {
    assert_eq!(menu_for("/resume"), Some(Menu::Sessions));
    assert_eq!(menu_for(" /login "), Some(Menu::Login));
    assert_eq!(menu_for("/model"), Some(Menu::Model));
    assert_eq!(menu_for("/effort"), Some(Menu::Effort));
}

#[test]
fn anything_else_runs_as_a_turn_and_not_as_a_menu() {
    assert_eq!(menu_for("say something"), None);
    assert_eq!(menu_for("/help"), None);
    // A menu command with an argument already has what it needs.
    assert_eq!(menu_for("/resume abc123"), None);
    assert_eq!(menu_for("/model glm-4.6"), None);
    assert_eq!(menu_for("/effort low"), None);
}

/// The cancel source this front end hands the turn is the one it built around
/// Ctrl-C arriving as a key (raw mode leaves no SIGINT to listen for), and what
/// it delivers through is a watch. A key that lands between two waits must not
/// be lost: while a turn runs, this loop is between waits most of the time.
#[tokio::test]
async fn a_cancel_between_two_waits_is_not_lost() {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut cancel = CtrlC(cancel_rx);

    // The first wait is polled once and dropped, which is what the end of a
    // select does with it.
    let waiting = cancel.wait();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), waiting)
            .await
            .is_err(),
        "nothing was cancelled yet"
    );

    // The key arrives while nothing is waiting for it.
    cancel_tx.send(true).unwrap();

    let waiting = cancel.wait();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), waiting)
            .await
            .is_ok(),
        "the cancel must still be seen by the next wait"
    );
}

// ------------------------------------------------------- the question panel ---

use crate::tools::ask::{Answer, Question};

/// One question with two options, the shape most of these tests use.
fn two_options(multi_select: bool) -> Question {
    Question {
        id: "auth".into(),
        header: "Auth".into(),
        question: "Which auth should we use?".into(),
        options: vec![
            crate::tools::ask::Choice {
                label: "JWT".into(),
                description: "one token for the API".into(),
            },
            crate::tools::ask::Choice {
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
    type_while_working(&mut state, "a sentence I was writing", &cancel);
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
    type_while_working(&mut state, "a sentence I was writing", &cancel);
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
    assert_eq!(state.placeholder(), super::panel::PANEL_PLACEHOLDER);
}

#[test]
fn the_panel_draws_the_question_its_options_and_the_keys() {
    let mut screen = screen_for_test(60, 20);
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
    let mut screen = screen_for_test(60, 20);
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

//! Tests for the pinned region of the screen -- the rows that are not the
//! transcript, that is to say the box, the queue above it, the standing todo
//! block, and the layout's arithmetic about how many rows each gets.
//!
//! "Pinned" is the contract: the box is the draft's shape and the status line
//! is the session's summary, and both must be on the screen however the
//! transcript grows. What changes is how the rows above them are split
//! between transcript, queue and the standing list.

use ratatui::layout::Rect;
use ratatui::style::Modifier;

use crate::ui::cell::Cell;

use super::super::input::IDLE_PLACEHOLDER;
use super::super::layout::{
    BOX_ROWS, PINNED_ROWS, TODO_HEADS, TODO_ROWS, screen_rows, todo_rows, todo_window,
};
use super::super::render;
use super::super::state::{Scroll, State};
use super::box_top;
use super::ctrl_j;
use super::row;
use super::screen_for_test;
use super::transcript_top;
use super::type_in;
use super::written;

#[test]
fn paging_to_either_end_stops_there() {
    let mut view = Scroll::default();
    let (total, height) = (100, 10);
    view.by(-1000, total, height);
    assert_eq!(view.first(total, height), 0, "the beginning of it");
    view.bottom();
    assert_eq!(view.first(total, height), 90, "and its end");
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
    assert_eq!(
        row(&screen, transcript_top(&screen, 1)),
        "› first",
        "the transcript has it now"
    );
    assert_eq!(
        transcript_top(&screen, 1),
        last - 5,
        "and the row it gave back is the transcript's own"
    );
    assert_eq!(
        row(&screen, last - 4),
        "› second",
        "only what is still waiting is in the queue"
    );
    assert_eq!(row(&screen, last), "m-1 · cache 0.0% · hit 0 · miss 0");
    let buf = screen.terminal.backend().buffer();
    assert!(
        !buf[(2, transcript_top(&screen, 1))]
            .style()
            .add_modifier
            .contains(Modifier::DIM),
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
    for i in 0..(super::super::render::QUEUE_ROWS + 2) {
        state.enqueue(format!("line {i}"));
    }
    assert_eq!(
        render::queue_lines(&state, 40).len(),
        super::super::render::QUEUE_ROWS,
        "capped, in rows"
    );
    assert_eq!(
        render::queue_lines(&state, 40)
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
    let lines: Vec<String> = render::queue_lines(&state, 20)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(lines.len(), 3, "52 columns at 20 columns a row");
    assert!(lines.iter().all(|l| crate::ui::text::width(l) <= 20));
    assert_eq!(lines.concat(), format!("› {typed}"));
}

#[test]
fn the_standing_list_is_the_last_one_written_and_nothing_when_none_is() {
    // Nothing has been written: the block takes no rows at all, so a session that
    // never uses the tool sees exactly the screen it saw before there was one.
    let mut state = State::default();
    assert!(render::todo_lines(&state, 40).is_empty());
    assert_eq!(todo_rows(0), 0);

    state.show(written(serde_json::json!({"todos": [
        {"content": "Add the parse function", "status": "completed"},
        {"content": "Draw the cell", "status": "in_progress"}
    ]})));
    assert_eq!(
        render::todo_lines(&state, 40)
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
        render::todo_lines(&state, 40).is_empty(),
        "a cleared list is not one to keep in view"
    );
}

#[test]
fn the_standing_list_is_drawn_by_the_block_and_not_by_the_cell_too() {
    // The same tasks twice on one screen spend the block's rows saying nothing:
    // the cell keeps the head -- the line the model was answered with -- and the
    // block, which is where the list stands, keeps the tasks. Every write folds
    // and not only the list that stands, because a cell is laid out once for one
    // width: a rule that changed with a later cell would rewrite the transcript
    // on the next resize rather than on the draw that made the list stand.
    let mut screen = screen_for_test(60, 20);
    screen
        .state
        .transcript
        .push(written(serde_json::json!({"todos": [
            {"content": "Read the failing test", "status": "completed"},
            {"content": "Fix the parser", "status": "in_progress"}
        ]})));
    screen.state.transcript.push(Cell::Content("on it".into()));
    screen.draw().unwrap();

    let top = transcript_top(&screen, 2);
    assert_eq!(row(&screen, top), "▸ TodoWrite 1/2 done");
    assert_eq!(row(&screen, top + 1), "on it");

    let boxed = box_top(&screen);
    assert_eq!(row(&screen, boxed - 1), "▸ Fix the parser");
    assert_eq!(row(&screen, boxed - 2), "✔ Read the failing test");
    assert_eq!(row(&screen, boxed - 3), "· todos · 1/2 done");

    let copies = super::all_rows(&screen)
        .iter()
        .filter(|r| r.contains("Read the failing test"))
        .count();
    assert_eq!(copies, 1, "the list is drawn once");
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
    let lines: Vec<String> = render::todo_lines(&state, 20)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(
        lines.len(),
        2 + 3,
        "the two head rows, then 52 columns at 20"
    );
    assert!(lines.iter().all(|l| crate::ui::text::width(l) <= 20));
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
    let block: Vec<String> = render::todo_lines(&screen.state, 40)
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
        let area = super::origin(&mut screen);
        let input = screen.state.input_rows(height, 0);
        let queued = render::queue_lines(&screen.state, 40).len() as u16;
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

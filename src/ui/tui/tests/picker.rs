//! Tests for the picker: the command picker that filters commands as the user
//! types, the session picker that offers the list when `/resume` has nothing
//! to resume to, the provider picker that `/login` opens, and the model and
//! effort pickers that `/model` and `/effort` open.
//!
//! Each picker has its own shape -- a session list is a list of rows that
//! stays put while the user types, a model list is named by id and a
//! provider list by the name a person knows it by -- and what they have in
//! common is the key bindings: arrows move the highlight, `Tab` or `Enter`
//! choose, and `Esc` dismisses.

use std::path::PathBuf;

use crossterm::event::KeyCode;
use ratatui::style::Modifier;

use crate::config;
use crate::session;

use super::super::input::{IDLE_PLACEHOLDER, Submitted};
use super::super::picker::{Choice, Choosing, choice_rows, named_rows};
use super::super::state::State;
use super::all_rows;
use super::press;
use super::row;
use super::screen_for_test;
use super::type_in;

/// A session as the picker sees it. Its path is never read: the front end is
/// handed the list, it does not go looking for it.
fn session_info(id: &str, messages: usize, preview: &str) -> session::SessionInfo {
    session::SessionInfo {
        id: id.to_owned(),
        path: PathBuf::from(format!("/nowhere/{id}.jsonl")),
        modified: 0,
        message_count: messages,
        preview: preview.to_owned(),
    }
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
    let drawn = super::rendered(&super::super::render::queue_lines(&screen.state, 60));
    assert_eq!(drawn.len(), 3, "{drawn:?}");
    assert!(
        drawn.iter().all(|(text, _)| !text.contains("more")),
        "{drawn:?}"
    );

    // One more line than the cap, and the row that does not fit is counted on
    // one of the rows the cap allows: the queue never costs more than three.
    screen.state.queued.push_back("/help".into());
    let drawn = super::rendered(&super::super::render::queue_lines(&screen.state, 60));
    assert_eq!(drawn.len(), super::super::render::QUEUE_ROWS, "{drawn:?}");
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

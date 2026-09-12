//! Tests for the working border: the spinner, the count of characters sent,
//! the estimate of how fast they are arriving, and how a running turn
//! calibrates that estimate against the usage notices the model sends back.
//!
//! The border is the only thing on the screen that moves while a turn runs:
//! it has to look like motion without taking the reader's eye off the
//! transcript, and the estimate it carries has to be one the reader can
//! believe.

use std::time::{Duration, Instant};

use ratatui::style::Modifier;
use tokio::sync::oneshot;

use crate::types::Usage;
use crate::ui::cell::Style;

use super::super::layout::BOX_ROWS;
use super::super::notice::Notice;
use super::super::render as render_mod;
use super::super::render::SPINNER;
use super::super::state::State;
use super::row;
use super::screen_for_test;
use super::working;

/// What the spinner says at the moment the title is built, for a turn that
/// has been running `elapsed` and has sent `chars` characters at an average
/// of `chars_per_token` to the token.
#[test]
fn the_working_border_spins_counts_and_estimates() {
    // 12 345 ms in: frame 154 % 10 = 4, the fifth glyph; 4 000 characters at
    // a measured four to the token over 12.3 s rounds to 81 a second.
    let s = working(Duration::from_millis(12_345), 4000, 4.0);
    assert_eq!(
        render_mod::activity_title(&s, 60),
        Some("⠼ 12s · ~81 token/s".to_owned())
    );
}

#[test]
fn the_estimate_waits_for_the_average_to_settle() {
    // 2 040 ms: frame 25, off a frame boundary so the clock's second read
    // cannot tip it
    let s = working(Duration::from_millis(2_040), 4000, 4.0);
    assert_eq!(render_mod::activity_title(&s, 60), Some("⠴ 2s".to_owned()));
}

#[test]
fn a_silent_turn_estimates_nothing() {
    let s = working(Duration::from_millis(30_040), 0, 4.0);
    assert_eq!(render_mod::activity_title(&s, 60), Some("⠴ 30s".to_owned()));
}

#[test]
fn a_narrow_border_drops_the_estimate_then_hides_the_indicator() {
    let s = working(Duration::from_millis(12_345), 4000, 4.0);
    let full = render_mod::activity_title(&s, usize::MAX).unwrap();
    let count = "⠼ 12s".to_owned();
    // one column short of the whole thing, the estimate goes whole
    assert_eq!(
        render_mod::activity_title(&s, crate::ui::text::width(&full) - 1),
        Some(count.clone())
    );
    // one column short of the count, nothing at all: a clipped spinner is
    // not an indicator
    assert_eq!(
        render_mod::activity_title(&s, crate::ui::text::width(&count) - 1),
        None
    );
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
fn a_tool_calls_arguments_join_the_calibration_window() {
    // The arguments are output the backend billed for, so their characters
    // count towards both the live estimate's numerator and the window the
    // next usage notice calibrates on — otherwise a call-heavy turn runs the
    // ratio towards zero tokens per character.
    let mut s = State {
        turn_running: true,
        turn_started: Some(Instant::now()),
        ..State::default()
    };
    s.apply(Notice::ToolStart {
        name: "Bash".into(),
        args: r#"{"command":"echo hi"}"#.into(),
    });
    let args_chars = r#"{"command":"echo hi"}"#.chars().count();
    assert_eq!(s.streamed_chars, args_chars);
    assert_eq!(s.chars_since_usage, args_chars);
    s.apply(Notice::Usage(
        Usage {
            completion_tokens: 10,
            ..Usage::default()
        },
        Duration::ZERO,
    ));
    assert_eq!(s.chars_per_token, args_chars as f64 / 10.0);
}

#[test]
fn a_question_takes_the_border_title_back() {
    let mut s = working(Duration::from_secs(12), 4000, 4.0);
    let (tx, _rx) = oneshot::channel();
    s.open_question(tx);
    assert_eq!(render_mod::activity_title(&s, 60), None);
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

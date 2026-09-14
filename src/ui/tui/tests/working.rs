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
use super::super::notice::MachineNotice;
use super::super::render as render_mod;
use super::super::render::{SPINNER_MS, spinner};
use super::super::state::State;
use super::row;
use super::screen_for_test;
use super::working;

/// The frame the spinner is on `elapsed` into a turn.
///
/// Computed rather than written out, because these tests are about the words
/// beside the spinner -- the phase, the elapsed seconds, the estimate -- and the
/// frame is a function of the clock. Pinning a glyph here would mean seven edits
/// the next time the frame set is retuned, none of them about what the test is
/// named for. What the frames *are* is [`crate::ui::glyphs`]'s business, and it
/// has tests of its own.
fn frame(elapsed: Duration) -> char {
    let frames = spinner();
    frames[(elapsed.as_millis() / SPINNER_MS) as usize % frames.len()]
}

/// What the spinner says at the moment the title is built, for a turn that
/// has been running `elapsed` and has sent `chars` characters at an average
/// of `chars_per_token` to the token.
#[test]
fn the_working_border_spins_counts_and_estimates() {
    // 12 345 ms in: frame 154 % 4 = 2, the third glyph; 4 000 characters at
    // a measured four to the token over 12.3 s rounds to 81 a second.
    let s = working(Duration::from_millis(12_345), 4000, 4.0);
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, 60),
        Some(format!(
            "{} working · 12s · ~81 token/s",
            frame(Duration::from_millis(12_345))
        ))
    );
}

#[test]
fn the_estimate_waits_for_the_average_to_settle() {
    // 2 040 ms: frame 25 % 4 = 1, off a frame boundary so the clock's second
    // read cannot tip it
    let s = working(Duration::from_millis(2_040), 4000, 4.0);
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, 60),
        Some(format!(
            "{} working · 2s",
            frame(Duration::from_millis(2_040))
        ))
    );
}

#[test]
fn a_silent_turn_estimates_nothing() {
    let s = working(Duration::from_millis(30_040), 0, 4.0);
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, 60),
        Some(format!(
            "{} working · 30s",
            frame(Duration::from_millis(30_040))
        ))
    );
}

#[test]
fn a_narrow_border_drops_the_estimate_then_hides_the_indicator() {
    let s = working(Duration::from_millis(12_345), 4000, 4.0);
    let full = render_mod::activity_title(&s.overlay, &s.turn, usize::MAX).unwrap();
    let count = format!("{} working · 12s", frame(Duration::from_millis(12_345)));
    // one column short of the whole thing, the estimate goes whole
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, crate::ui::text::width(&full) - 1),
        Some(count.clone())
    );
    // one column short of the count, nothing at all: a clipped spinner is
    // not an indicator
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, crate::ui::text::width(&count) - 1),
        None
    );
}

#[test]
fn the_working_indicator_is_signal_on_a_rule_of_geometry() {
    // While the model is quiet, the spinner on the box's rule is the only thing
    // on the screen that moves, so it is painted as the signal it is -- the
    // palette's attention colour -- while the rule it rides on carries the
    // geometry colour and nothing else. The two used to be told apart by a
    // modifier, which is the arrangement this test exists to keep out: a
    // modifier renders as anything between "slightly grey" and "nothing at all"
    // depending on the terminal, and an indicator the reader cannot see is an
    // indicator that is not there.
    let mut screen = screen_for_test(60, 20);
    screen.state = working(Duration::from_secs(3), 0, 4.0);
    screen.draw().unwrap();
    let rule = screen.terminal.backend().buffer().area.height - 1 - 3;
    let buf = screen.terminal.backend().buffer();
    let at = (0..buf.area.width)
        .find(|&x| spinner().contains(&buf[(x, rule)].symbol().chars().next().unwrap_or(' ')))
        .expect("the indicator is drawn on the box's top rule");
    let signal = crate::ui::theme::style_of(crate::ui::cell::Style::Yellow).fg;
    let geometry = crate::ui::theme::theme().rule_style().fg;
    assert_eq!(buf[(at, rule)].style().fg, signal, "the indicator is lit");
    assert_eq!(
        buf[(0, rule)].style().fg,
        geometry,
        "and the rule is chrome"
    );
    for x in [at, 0] {
        assert!(
            !buf[(x, rule)].style().add_modifier.contains(Modifier::DIM),
            "neither is dimmed: x={x}"
        );
    }
}

#[test]
fn calibration_learns_from_a_usage_notice() {
    let mut s = State {
        turn: crate::ui::tui::turn::Turn {
            running: true,
            started: Some(Instant::now()),
            ..crate::ui::tui::turn::Turn::default()
        },
        ..State::default()
    };
    s.stream(Style::Reasoning, &"x".repeat(800));
    s.apply(MachineNotice::Usage(
        Usage {
            completion_tokens: 200,
            ..Usage::default()
        },
        Duration::ZERO,
    ));
    assert_eq!(s.turn.chars_per_token, 4.0);
    // the counter is spent on the measurement: the next ratio starts clean
    assert_eq!(s.turn.chars_since_usage, 0);
}

#[test]
fn a_tool_calls_arguments_join_the_calibration_window() {
    // The arguments are output the backend billed for, so their characters
    // count towards both the live estimate's numerator and the window the
    // next usage notice calibrates on — otherwise a call-heavy turn runs the
    // ratio towards zero tokens per character.
    let mut s = State {
        turn: crate::ui::tui::turn::Turn {
            running: true,
            started: Some(Instant::now()),
            ..crate::ui::tui::turn::Turn::default()
        },
        ..State::default()
    };
    s.apply(MachineNotice::ToolStart {
        name: "Bash".into(),
        args: r#"{"command":"echo hi"}"#.into(),
    });
    let args_chars = r#"{"command":"echo hi"}"#.chars().count();
    assert_eq!(s.turn.streamed_chars, args_chars);
    assert_eq!(s.turn.chars_since_usage, args_chars);
    s.apply(MachineNotice::Usage(
        Usage {
            completion_tokens: 10,
            ..Usage::default()
        },
        Duration::ZERO,
    ));
    assert_eq!(s.turn.chars_per_token, args_chars as f64 / 10.0);
}

#[test]
fn a_question_takes_the_border_title_back() {
    let mut s = working(Duration::from_secs(12), 4000, 4.0);
    let (tx, _rx) = oneshot::channel();
    s.open_question(tx);
    assert_eq!(render_mod::activity_title(&s.overlay, &s.turn, 60), None);
}

/// While a Reasoning block is streaming, the border says "thinking" --
/// it names the phase the agent is in, not just that something is
/// happening. The spinner character is unchanged; the word changes.
#[test]
fn the_border_says_thinking_while_a_reasoning_block_streams() {
    let mut s = working(Duration::from_millis(2_500), 100, 4.0);
    // Open a Reasoning block: stream starts with the Reasoning style.
    s.apply(MachineNotice::Reasoning("hmm".into()));
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, 60),
        Some(format!(
            "{} thinking · 2s",
            frame(Duration::from_millis(2_500))
        )),
        "Reasoning in flight -> thinking"
    );
    // Close the Reasoning block by switching to Content: the phase
    // reverts to the generic `working` because the model is producing
    // body text now, not reasoning.
    s.end_block();
    s.apply(MachineNotice::Content("answer".into()));
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, 60),
        Some(format!(
            "{} working · 2s",
            frame(Duration::from_millis(2_500))
        )),
        "Content in flight -> working"
    );
}

/// While a tool call is in flight, the border says "running X" where X
/// is the tool verb. The result notice clears the verb and the label
/// reverts to whatever phase the model is in next.
#[test]
fn the_border_says_running_verb_while_a_tool_runs() {
    // Below SPEED_AFTER_SECS so the per-second estimate is not on the
    // border yet -- this test pins the phase label, not the speed
    // estimate (which has its own tests).
    let mut s = working(Duration::from_millis(2_500), 100, 4.0);
    s.apply(MachineNotice::ToolStart {
        name: "Bash".into(),
        args: "{}".into(),
    });
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, 60),
        Some(format!(
            "{} running Bash · 2s",
            frame(Duration::from_millis(2_500))
        )),
        "the verb on the border is the one the agent sent"
    );
    s.apply(MachineNotice::ToolResult("exit_code: 0".into()));
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, 60),
        Some(format!(
            "{} working · 2s",
            frame(Duration::from_millis(2_500))
        )),
        "settling the step drops the verb, the border says working"
    );
}

/// A Running step's verb carries through to `finalize_open_step` (the
/// path an interrupted turn follows), so an interrupted tool does not
/// leave the border pointing at a call that did not run.
#[test]
fn an_interrupted_step_clears_the_tool_verb() {
    let mut s = working(Duration::from_millis(2_500), 100, 4.0);
    s.apply(MachineNotice::ToolStart {
        name: "Bash".into(),
        args: "{}".into(),
    });
    s.apply(MachineNotice::Interrupted);
    assert_eq!(
        render_mod::activity_title(&s.overlay, &s.turn, 60),
        Some(format!(
            "{} working · 2s",
            frame(Duration::from_millis(2_500))
        )),
        "interrupted step -> the border's no longer running X"
    );
}

#[test]
fn the_border_shows_the_working_turn_and_then_does_not() {
    let mut screen = screen_for_test(60, 20);
    screen.state.begin_turn(Instant::now());
    screen.state.turn.started = Some(Instant::now() - Duration::from_secs(12));
    screen.state.turn.streamed_chars = 4000;
    screen.state.turn.chars_per_token = 4.0;
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

//! The bar pinned under the stream: where it stands, what it says, and what a
//! terminal that cannot host one leaves out.

use std::time::Duration;

use super::super::cell::Style;
use super::super::status_bar::StatusBar;
use super::*;

#[test]
fn status_bar_from_size_guards() {
    assert_eq!(StatusBar::from_size(None), None);
    assert_eq!(StatusBar::from_size(Some((2, 80))), None); // too few rows
    assert_eq!(StatusBar::from_size(Some((24, 1))), None); // too narrow
    assert_eq!(
        StatusBar::from_size(Some((24, 80))),
        Some(StatusBar { rows: 24, cols: 80 })
    );
}

#[test]
fn status_bar_detect_rejects_a_terminal_that_cannot_host_it() {
    assert_eq!(StatusBar::detect(&StandIn::new().tty(false)), None);
    assert_eq!(StatusBar::detect(&StandIn::new().dumb(true)), None);
    assert_eq!(StatusBar::detect(&StandIn::new().unmeasurable()), None);
    assert_eq!(StatusBar::detect(&StandIn::new().sized(2, 80)), None);
    assert_eq!(StatusBar::detect(&StandIn::new().sized(24, 1)), None);
    assert_eq!(
        StatusBar::detect(&StandIn::new().sized(24, 80)),
        Some(StatusBar { rows: 24, cols: 80 })
    );
}

/// Regression: the bar used to measure its label in chars, so every wide
/// character was undercharged by one column and the label spilled past the
/// column the bar deliberately leaves free (which is what keeps the write
/// from triggering autowrap on the last row).
#[test]
fn status_bar_render_charges_wide_chars_two_columns() {
    let bar = StatusBar { rows: 10, cols: 20 }; // width = 19
    let (mut r, buf) = with_buffer(false);
    // two ideographs + space + rocket = 7 columns but only 4 chars
    let label = "\u{6df1}\u{5ea6} \u{1f680}";
    assert_eq!(label.chars().count(), 4);
    assert_eq!(text::width(label), 7);
    let visible = text::truncate(label, bar.width());
    bar.render(r.out.as_mut(), visible, visible);
    let s = buf_of(&buf);
    // 19 - 7 = 12 columns of padding. Counting chars would have written 15
    // and pushed the label 3 columns past the right edge.
    assert!(
        s.contains(&format!("\x1b[2K{}{label}\x1b8", " ".repeat(12))),
        "{s:?}"
    );
}

#[test]
fn status_bar_setup_teardown_sequences() {
    let bar = StatusBar { rows: 10, cols: 40 };
    let (mut r, buf) = with_buffer(false);
    bar.setup(r.out.as_mut());
    bar.teardown(r.out.as_mut());
    let s = buf_of(&buf);
    assert!(s.contains("\x1b[10;1H\x1b[2K\x1b[1;9r\x1b[9;1H"), "{s:?}");
    assert!(s.contains("\x1b[r\x1b[10;1H\x1b[2K\r\n"), "{s:?}");
}

#[test]
fn status_bar_render_right_aligns_and_paints() {
    let bar = StatusBar { rows: 10, cols: 40 }; // width = 39
    let (mut r, buf) = with_buffer(true);
    let label = "cache 98.6% · hit 32384 · miss 461"; // 34 characters
    let painted = r.paint(Style::Dim, label);
    bar.render(r.out.as_mut(), label, &painted);
    let s = buf_of(&buf);
    // 39 - 34 = 5 spaces of left padding, right aligned
    assert!(
        s.contains(&format!(
            "\x1b7\x1b[10;1H\x1b[2K     \x1b[2m{label}\x1b[0m\x1b8"
        )),
        "{s:?}"
    );
}

#[test]
fn apply_status_bar_transitions() {
    let a = StatusBar { rows: 10, cols: 40 };
    let b = StatusBar { rows: 12, cols: 50 };

    // (None, None): no output
    let (mut r, buf) = with_buffer(false);
    r.apply_status_bar(None);
    assert!(buf_of(&buf).is_empty());

    // (None, Some): create the bar + first draw
    r.apply_status_bar(Some(a));
    let s = buf_of(&buf);
    assert!(s.contains("\x1b[1;9r"), "{s:?}");
    assert!(s.contains("cache 0.0% · hit 0 · miss 0"), "{s:?}");
    assert_eq!(r.bar, Some(a));

    // (Some, Some(same)): redraw only
    let before = buf_of(&buf).len();
    r.apply_status_bar(Some(a));
    assert!(buf_of(&buf)[before..].contains("\x1b[10;1H\x1b[2K"));

    // (Some, Some(different)): tear down the old one, build the new one
    let before = buf_of(&buf).len();
    r.apply_status_bar(Some(b));
    let tail = &buf_of(&buf)[before..];
    assert!(tail.contains("\x1b[r"), "{tail:?}");
    assert!(tail.contains("\x1b[1;11r"), "{tail:?}");
    assert_eq!(r.bar, Some(b));

    // (Some, None): tear the bar down
    let before = buf_of(&buf).len();
    r.apply_status_bar(None);
    assert!(buf_of(&buf)[before..].contains("\x1b[r"));
    assert_eq!(r.bar, None);
}

#[test]
fn usage_paints_session_cache_bar_and_reset_clears_it() {
    let bar = StatusBar { rows: 10, cols: 60 };
    let (mut r, buf) = with_buffer(false);
    r.apply_status_bar(Some(bar));
    r.usage(&usage_fixture(6, 4), Duration::ZERO);
    r.usage(&usage_fixture(12, 8), Duration::ZERO);
    let s = buf_of(&buf);
    assert!(
        s.contains("tokens: in 10/10 (hit 6/miss 4) · out 0"),
        "{s:?}"
    );
    // session accumulation: 18 hit / 12 miss = 60.0%
    assert!(s.contains("cache 60.0% · hit 18 · miss 12"), "{s:?}");

    r.reset_stats();
    let tail = &buf_of(&buf)[s.len()..];
    assert!(tail.contains("cache 0.0% · hit 0 · miss 0"), "{tail:?}");
    assert_eq!(r.stats(), CacheStats::default());
}

/// The status bar line the renderer draws at the given terminal width, with
/// a model and cache statistics already in place.
fn bar_line(cols: u16, model: &str) -> String {
    let (mut r, buf) = with_buffer(false);
    r.set_model(model);
    r.usage(&usage_fixture(6, 4), Duration::ZERO);
    // The bar is attached last, so the redraw it triggers is the first one
    // that has anything to draw.
    r.apply_status_bar(Some(StatusBar { rows: 10, cols }));
    buf_of(&buf)
}

/// Assert that the bar's content is exactly `label`: clipping it would
/// break the `\x1b8` cursor restore that immediately follows.
fn assert_bar_exactly(cols: u16, model: &str, label: &str) {
    let s = bar_line(cols, model);
    assert!(
        s.contains(&format!("{label}\x1b8")),
        "cols={cols} expected {label:?} in {s:?}"
    );
}

/// Progressive disclosure: a bar too narrow for everything drops whole
/// segments from the end instead of cutting a number in half.
#[test]
fn status_bar_drops_whole_segments_on_narrow_terminals() {
    let model = "deepseek-v4-flash";
    // 79 columns: everything fits
    assert_bar_exactly(
        80,
        model,
        "deepseek-v4-flash · cache 60.0% · hit 6 · miss 4",
    );
    // 34 columns: the counts go, the model and rate stay
    assert_bar_exactly(35, model, "deepseek-v4-flash · cache 60.0%");
    // 19 columns: only the model is left
    assert_bar_exactly(20, model, "deepseek-v4-flash");
}

/// When not even the shortest combination fits, it is clipped rather than
/// leaving the bar blank.
#[test]
fn status_bar_clips_the_shortest_segment_as_a_last_resort() {
    assert_bar_exactly(10, "deepseek-v4-flash", "deepseek-");
}

/// Variants are chosen by display width, not by char count: at 21 columns
/// the rate segment is 22 columns wide (18 chars), so it has to be dropped.
#[test]
fn status_bar_chooses_variants_by_display_width() {
    let wide_model = "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}"; // 8 columns, 4 chars
    assert_bar_exactly(
        25,
        wide_model,
        "\u{6df1}\u{5ea6}\u{6c42}\u{7d22} · cache 60.0%",
    );
    assert_bar_exactly(21, wide_model, wide_model);
}

#[test]
fn status_bar_shows_model_and_updates_on_switch() {
    let bar = StatusBar { rows: 10, cols: 80 };
    let (mut r, buf) = with_buffer(false);
    r.apply_status_bar(Some(bar));
    r.set_model("deepseek-v4-flash");
    r.usage(&usage_fixture(6, 4), Duration::ZERO);
    let s = buf_of(&buf);
    assert!(
        s.contains("deepseek-v4-flash · cache 60.0% · hit 6 · miss 4"),
        "{s:?}"
    );

    // switching models: after the redraw only the new model is left
    r.set_model("deepseek-v4-pro");
    let tail = &buf_of(&buf)[s.len()..];
    assert!(tail.contains("deepseek-v4-pro · cache 60.0%"), "{tail:?}");
    assert!(!tail.contains("deepseek-v4-flash"), "{tail:?}");

    // reset_stats clears only the cache stats and keeps the model
    let before = buf_of(&buf).len();
    r.reset_stats();
    let tail = &buf_of(&buf)[before..];
    assert!(
        tail.contains("deepseek-v4-pro · cache 0.0% · hit 0 · miss 0"),
        "{tail:?}"
    );
}

#[test]
fn refresh_status_bar_without_tty_is_noop_and_teardown_idempotent() {
    let (mut r, buf) = with_buffer(false);
    r.refresh_status_bar(); // the stand-in is not a terminal → not enabled
    assert!(buf_of(&buf).is_empty());
    r.teardown(); // idempotent when not enabled
    assert!(buf_of(&buf).is_empty());
}

/// The whole point of the terminal being a capability: what used to be reachable
/// only under a pty -- the bar coming up, the echo going off -- is a decision a
/// test can make, because it is a function of facts the test supplies.
#[test]
fn a_bar_comes_up_on_a_terminal_that_can_host_it_and_goes_down_on_teardown() {
    let (mut r, buf) = renderer_on(StandIn::new().sized(24, 80), true);
    r.refresh_status_bar();
    let s = buf_of(&buf);
    // the scroll region stops one line short of the bottom, and the bar's first
    // frame is drawn on the line it leaves
    assert!(s.contains("\x1b[1;23r"), "the scroll region: {s:?}");
    assert!(s.contains("cache 0.0% · hit 0 · miss 0"), "{s:?}");
    // a second refresh with the same size only redraws
    let before = s.len();
    r.refresh_status_bar();
    let tail = &buf_of(&buf)[before..];
    assert!(tail.contains("\x1b[24;1H"), "{tail:?}");
    assert!(!tail.contains("\x1b[1;23r"), "no second region: {tail:?}");

    r.teardown();
    let tail = &buf_of(&buf)[before..];
    assert!(tail.ends_with("\x1b[r\x1b[24;1H\x1b[2K\r\n"), "{tail:?}");
}

/// A terminal that says it cannot address the screen is one the bar stays off
/// for, whether it is dumb or has no rows to spare.
#[test]
fn a_terminal_that_cannot_host_the_bar_leaves_it_off() {
    for term in [
        StandIn::new().dumb(true),
        StandIn::new().unmeasurable(),
        StandIn::new().sized(2, 80),
    ] {
        let (mut r, buf) = renderer_on(term, true);
        r.refresh_status_bar();
        assert!(
            buf_of(&buf).is_empty(),
            "nothing may be written for a bar that is not enabled"
        );
    }
}

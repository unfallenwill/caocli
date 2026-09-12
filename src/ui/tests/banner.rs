//! What a session opens with: one measured line, in every terminal there is.

use super::super::banner::{banner, banner_at};
use super::*;

/// The whole line, as an 80-column terminal would show it: the terminal it is
/// most likely to be read on is the one the old line broke in half.
#[test]
fn the_banner_fits_the_terminal_it_is_going_into() {
    let full = "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands";
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", None),
        full
    );
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(100)),
        full,
        "a wide terminal keeps the hint"
    );
    // 80 columns: the hint is the first thing to go -- the box repeats it.
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(80)),
        "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro"
    );
    // Then the model, which the status line has anyway.
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(60)),
        "caocli · session 20260910-213122 (12 messages)"
    );
    // Then the count. Which session this is is the one thing nothing else
    // says, so the id is the last of the optional ones to go -- and what is
    // left is what fits a 40-column terminal whole.
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(40)),
        "caocli · session 20260910-213122"
    );
    // Below that, the line comes back whole to be clipped by whoever writes
    // it: an over-long line reads better than a line with nothing on it.
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(12)),
        "caocli · session 20260910-213122"
    );
}

#[test]
fn the_banner_counts_one_message_in_the_singular() {
    let one = banner_at("20260910-213122", 1, "m", Some(80));
    assert!(one.contains("(1 message)"), "{one:?}");
    assert!(banner_at("20260910-213122", 2, "m", Some(80)).contains("(2 messages)"));
    assert!(banner_at("20260910-213122", 0, "m", Some(80)).contains("(0 messages)"));
}

#[test]
fn the_banner_is_never_cut_mid_word() {
    // Every width, not just the round ones: what comes back is always one of
    // the segment joins -- never a piece of one -- so a word is never cut in
    // half however narrow the terminal is.
    let joins = [
        "caocli · session 20260910-213122",
        "caocli · session 20260910-213122 (12 messages)",
        "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro",
        "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands",
    ];
    for columns in 1..=120 {
        let line = banner_at(
            "20260910-213122",
            12,
            "deepseek/deepseek-v4-pro",
            Some(columns),
        );
        assert!(joins.contains(&line.as_str()), "{columns}: {line:?}");
        if text::width(&line) > columns {
            // Only the shortest form may overflow, and only to be clipped by
            // whoever writes it: an over-long line reads better than none.
            assert_eq!(line, joins[0], "{columns}: {line:?}");
        }
    }
}

#[test]
fn the_banner_measures_wide_characters_by_column() {
    // A CJK model id is two columns per glyph, so a budget counted in chars
    // would let through a segment that does not fit: the ideographs are 8
    // columns and only 4 characters.
    let cjk = "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}";
    let with_model = "caocli · session id (1 message) · \u{6df1}\u{5ea6}\u{6c42}\u{7d22}";
    assert_eq!(text::width(with_model), 42);
    assert_eq!(banner_at("id", 1, cjk, Some(42)), with_model);
    assert_eq!(
        banner_at("id", 1, cjk, Some(41)),
        "caocli · session id (1 message)",
        "one column short of the model: the model goes whole"
    );
}

#[test]
fn the_banner_keeps_everything_when_there_is_no_terminal() {
    // Output redirected to a file or a pipe: there is no width to fit, and a
    // file can hold the whole line. (A test's own stdout is a pipe, so this is
    // also the case the width is really asked for in.)
    assert_eq!(
        banner("20260910-213122", 12, "deepseek/deepseek-v4-pro"),
        "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands"
    );
}

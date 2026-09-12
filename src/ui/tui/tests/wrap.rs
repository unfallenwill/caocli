//! Tests for text wrapping.
//!
//! Wrapping is what turns a long span into the rows a fixed-width region can
//! hold. Nothing may be lost: a clipped line would be the one piece of the
//! answer the reader was looking for, and a wrong column would put the eye on
//! the wrong word.
//!
//! Two layers are covered here: the `wrap` helper turns a string into a list
//! of `(text, width)` pairs and is where most of the rules live; the
//! `wrapped_lines` helper carries spans (with their styles) through to a
//! list of `Line`s, which is what the TUI renders.

use crate::ui::cell::{Cell, Span, Style};
use crate::ui::text;

use super::super::layout::BOX_ROWS;
use super::super::paint::{MEASURE, wrapped_lines};
use super::all_rows;
use super::rendered;
use super::screen_for_test;
use super::transcript_rows;

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
        .map(|r| text::width(r.trim_end()))
        .max()
        .unwrap();
    assert!(
        widest <= MEASURE,
        "laid out to the measure: {widest} columns"
    );
    assert!(widest > MEASURE - 10, "and the measure is used: {widest}");
    let first = transcript
        .iter()
        .find(|row| !row.trim().is_empty())
        .expect("the text is drawn");
    assert!(first.starts_with("word word"), "the left edge is kept");
}

#[test]
fn a_wide_character_is_never_split_across_lines() {
    // Ten ideographs are twenty columns; at eight, each line holds four.
    let text = "\u{6df1}".repeat(10);
    let lines = wrap(&text, 8);
    assert_eq!(lines.len(), 3, "8 + 8 + 4 columns");
    assert_eq!(lines[2].1, 4);
    assert_eq!(text::width(&text), 20, "nothing was dropped");
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

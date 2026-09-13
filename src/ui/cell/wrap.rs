//! Wrap styled spans into the terminal lines they need at a fixed width.
//!
//! Both halves of the front end need this, for the same reason: nothing here may
//! be left to the terminal's own soft wrapping. The transcript is a region of a
//! fixed width, so an unwrapped line would be cut off at the edge,
//! and `insert_before` renders into a fixed-width buffer, where an over-long line
//! is silently cut off -- unlike the plain front end, where the terminal wraps
//! and nothing is lost.
//!
//! Written here rather than handed to the framework, which has nothing to hand
//! it to. The framework wraps inside the widget that draws: `Paragraph::wrap`
//! wraps into the area it is rendered into and keeps the result, so the lines
//! never come back -- `Paragraph::line_count` answers with a number, and asking
//! it before a render means wrapping everything twice to get one.
//! `reflow::WordWrapper`, which is what does the wrapping in there, is a private
//! module. What this returns is a value instead, and the same value serves all
//! three places the transcript is laid out: the window on screen, the plain
//! front end's fixed-width buffer, and the row count the window is computed
//! from.
//!
//! Lives in the cell layer rather than the painter because it is a pure
//! transformation of spans and the rules it owns are the cell's, not the
//! front end's: a word-break is a word-break whichever front end reads the
//! result, and the paint layer only knows about gutters and budgets.

use ratatui::text::{Line, Span as RSpan};

use super::Span;
use crate::ui::cell::style_of;
use crate::ui::text;

// `style_of` lives on the cell layer rather than in the painter because both
// this module and the painter reach for it: the painter for `cell_lines`, this
// module for the spans it has to dress while wrapping. The mapping is a
// property of a cell-layer concept (a `Style` label) and the framework, not of
// the painter.

/// Wrap styled spans into the terminal lines they need at `width` columns.
///
/// Progress is guaranteed even when a single character is wider than the whole
/// field: the first character is taken regardless, so a narrow terminal degrades
/// to a clipped wide glyph rather than looping forever.
pub(crate) fn wrapped_lines(spans: &[Span], width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut current: Vec<RSpan<'static>> = Vec::new();
    let mut used = 0usize;
    // Whether the line being built is still empty, and, if so, whether the width
    // is what emptied it. Without the second flag an explicit newline landing on a
    // line the width already ended would look like a blank line in the text.
    let mut fresh = true;
    let mut ended_by_width = false;

    for span in spans {
        let style = style_of(span.style);
        let mut rest: &str = span.text.as_ref();
        loop {
            let (piece, after) = match rest.find('\n') {
                Some(at) => (&rest[..at], Some(&rest[at + 1..])),
                None => (rest, None),
            };
            let mut piece = piece;
            while !piece.is_empty() {
                // Whitespace at the head of a line the width broke is the break, not
                // text: it is dropped and the line starts with the word after it. A
                // line that follows an explicit newline keeps its leading whitespace
                // -- that is the text's own indentation, and it is read.
                if fresh && ended_by_width {
                    piece = piece.trim_start_matches(char::is_whitespace);
                    if piece.is_empty() {
                        break;
                    }
                }
                let (head, rest, done) = break_line(piece, width - used, width);
                let (head, rest) = if head.is_empty() {
                    if used > 0 {
                        // The line has run out of columns and this text cannot
                        // start one yet.
                        lines.push(Line::from(std::mem::take(&mut current)));
                        used = 0;
                        fresh = true;
                        ended_by_width = true;
                        continue;
                    }
                    // Even alone it does not fit: take the character anyway, and
                    // cut it where it falls -- there is no space to break at.
                    let ch = piece.chars().next().expect("piece is not empty");
                    (&piece[..ch.len_utf8()], &piece[ch.len_utf8()..])
                } else {
                    (head, rest)
                };
                current.push(RSpan::styled(head.to_owned(), style));
                used += text::width(head);
                fresh = false;
                ended_by_width = false;
                piece = rest;
                // A break at a space ends the line there, however many columns it
                // left unused: the next word belongs on the next line.
                if done || used >= width {
                    lines.push(Line::from(std::mem::take(&mut current)));
                    used = 0;
                    fresh = true;
                    ended_by_width = true;
                }
            }
            match after {
                Some(after) => {
                    // An explicit break ends the line -- and means a blank one
                    // when nothing was written since the last break.
                    if !fresh {
                        lines.push(Line::from(std::mem::take(&mut current)));
                        used = 0;
                        fresh = true;
                    } else if !ended_by_width {
                        lines.push(Line::from(""));
                    }
                    ended_by_width = false;
                    rest = after;
                }
                None => break,
            }
        }
    }
    if !fresh {
        lines.push(Line::from(current));
    }
    lines
}

/// Where a line ends inside `piece`: what stays, what the next line starts with,
/// and whether the line is finished.
///
/// `room` is the columns left in the line being built, `width` the columns a line
/// has. A break is taken at a space when there is one to take -- after the last
/// word that fits, or before the word that does not -- so that a wrapped line reads
/// as a line instead of as two halves of a word. The space a break is taken at is
/// the break and not text: it is dropped, and the next line starts with the word
/// after it.
///
/// Two cases have no space to break at, and both are cut at the column, which is
/// the only place a word is ever cut:
///
/// - the word is wider than a whole line, so moving it down would waste what is
///   left of this one without saving the cut -- filling the line first costs a row
///   fewer, and not one column is lost;
/// - there is no space in what fits, because it is all one word already.
///
/// The flag says whether the line is finished: a break at a space finishes it at
/// once, while text that fits leaves it open for whatever comes next.
fn break_line(piece: &str, room: usize, width: usize) -> (&str, &str, bool) {
    let fits = text::truncate(piece, room);
    if fits.len() == piece.len() {
        return (fits, "", false);
    }
    let after = &piece[fits.len()..];
    // A space being the first thing that did not fit means the cut is already at a
    // word boundary: the line ends after the word that fit. The whitespace the break
    // is taken at is the break and not text, so none of it is carried to the line.
    let next = after.trim_start_matches(char::is_whitespace);
    if next.len() < after.len() {
        return (fits.trim_end_matches(char::is_whitespace), next, true);
    }
    // Otherwise the cut landed inside a word, and how wide that word is decides
    // whether moving it down is worth a row. The word begins on this line and ends
    // on the next one: it is what is left of it here, plus what did not fit, up to
    // the space after it.
    let start = fits.rfind(char::is_whitespace).map_or(0, |at| at + 1);
    let tail = &after[..after.find(char::is_whitespace).unwrap_or(after.len())];
    let whole = text::width(&fits[start..]) + text::width(tail);
    let cut = fits[..start].trim_end_matches(char::is_whitespace);
    if whole > width || cut.is_empty() {
        // Cut where it falls -- and whitespace it fell on is a break like any other,
        // so none of it is left dangling at the end of the line either.
        return (fits.trim_end_matches(char::is_whitespace), after, false);
    }
    // The word moves down whole: the next line starts at the word itself, which is
    // this piece from just past the space the break is taken at.
    (cut, &piece[start..], true)
}

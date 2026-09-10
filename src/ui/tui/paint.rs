//! The painter: cells and styled spans turned into the terminal lines the
//! screen is drawn from.
//!
//! One layer, because the transcript on screen, the window over it and the
//! session log folded back into cells are three places the same cells are laid
//! out, and a session that reads differently in any of them is a session that
//! was not really one transcript. Everything here is a pure function of its
//! arguments: nothing owns state, touches the terminal, or reads the clock.

use ratatui::style::{Color, Modifier, Style as RStyle};
use ratatui::text::{Line, Span as RSpan};

use crate::ui::cell::{Cell, Span, Style};
use crate::ui::text;

/// The widest a line of the transcript is laid out, however wide the terminal is.
///
/// A line of prose is read by running the eye back to its start, and past a certain
/// width that return trip costs more than the columns it saved: on a 200-column
/// terminal, an answer set to the full width is a line the reader has to hunt the
/// start of. 100 columns is about as wide as a line of monospaced text stays
/// comfortable, and it is wider than the 80-column terminal most of this is read
/// on -- so the measure only ever shortens a line on the screens that need it.
pub(super) const MEASURE: usize = 100;

/// The columns text is laid out in inside a region `width` wide: as wide as the
/// region, and no wider than [`MEASURE`].
pub(super) fn measure(width: usize) -> usize {
    width.min(MEASURE)
}

/// Our style, as ratatui sees it. This is what [`Style`] being data buys: the
/// mapping happens once per front end, instead of at every call site.
pub(super) fn style_of(style: Style) -> RStyle {
    match style {
        Style::Plain => RStyle::new(),
        Style::Dim => RStyle::new().add_modifier(Modifier::DIM),
        // Dim, the same as the line above, and deliberately: thinking is told
        // apart from a tool result by the rule in its gutter, which is a
        // difference in the layout and so one that does not depend on a terminal
        // honoring SGR 2 or on a theme having a readable idea of what dim is.
        Style::Reasoning => RStyle::new().add_modifier(Modifier::DIM),
        // Painted styles are bold as well as colored, and bold is the half that
        // does not depend on the terminal's theme: the color is a palette slot the
        // theme chose for a background this code cannot see, while the weight reads
        // on a light background and a dark one alike. See `Style::code` for the
        // same rule in the plain front end.
        Style::Yellow => RStyle::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        Style::Green => RStyle::new().fg(Color::Green).add_modifier(Modifier::BOLD),
        Style::Red => RStyle::new().fg(Color::Red).add_modifier(Modifier::BOLD),
    }
}

/// The row that says how many rows a window is not showing, at the end it was cut
/// at.
///
/// A cut window that says nothing is a window that lies about how much there is:
/// six rows of a menu read as the whole menu, three rows of a queue as the whole
/// queue. Dim, like everything else that is not the thing being chosen; `lead` is
/// the marker column the row it stands in for would carry, so that the count lines
/// up with what it counts.
pub(super) fn more_line(lead: &str, n: usize) -> Line<'static> {
    Line::styled(format!("{lead}… {n} more"), style_of(Style::Dim))
}

/// How many lines of a think the transcript keeps before it says how many are
/// behind it.
///
/// Thinking is the one block that grows without bound -- at `max` effort a
/// hundred lines is an ordinary turn -- and the one block nobody is still
/// reading by the time the answer lands. Kept whole it evicts the answer from
/// the window that pins to the newest line; folded to its head and a count it
/// costs a dozen rows instead of a screenful. The full text stays in the
/// session log, which is where the durable copy lives either way.
pub(super) const THINKING_LINES: usize = 12;

/// The lines one cell occupies at `width`.
///
/// One function, because the transcript on screen, the window over it and the
/// session log folded back into cells are three places the same cells are laid
/// out, and a session that reads differently in any of them is a session that was
/// not really one transcript.
///
/// One cell at a time, because a cell is what a draw can keep: it is the unit the
/// session's output arrives in and it does not change once it is pushed, so it is
/// also the unit [`State`] lays out and remembers.
pub(super) fn cell_lines(cell: &Cell, width: usize) -> Vec<Line<'static>> {
    // Never wider than the measure, however wide the region is: the gutter is
    // inside it, so the answer and the machinery around it end in the same column.
    let width = measure(width);
    let Some(gutter) = cell.gutter() else {
        // The answer: the one cell that starts at the left edge.
        return wrapped_lines(&cell.spans(), width);
    };
    // Wrapped into what the gutter leaves, so a line of a set-in cell carries as
    // much as a line of the answer rather than two columns more.
    let mut lines: Vec<Line<'static>> =
        wrapped_lines(&cell.spans(), width.saturating_sub(gutter.width()))
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                // The marker opens the cell, and the lines after it continue in the
                // same columns: that is what keeps a wrapped block -- and with it the
                // left edge the whole layout is read down -- on one line.
                let lead = if i == 0 { gutter.head } else { gutter.rest };
                let mut spans = vec![RSpan::styled(lead, style_of(gutter.style))];
                spans.extend(line.spans);
                Line::from(spans)
            })
            .collect();
    // A long think folds to its head and a count. The rule lives in this layer
    // rather than in the cell because it is a budget of the screen, like the
    // wrapping width is: the plain front end has no screen to keep one on, and
    // streams the block as it arrives. It reaches the live block through this
    // same call, which is what keeps a folded think from jumping open the
    // moment it closes: the block that is filed away and the block that was
    // watched have to lay out to the same lines.
    if matches!(cell, Cell::Reasoning(_)) && lines.len() > THINKING_LINES {
        let hidden = lines.len() - THINKING_LINES;
        lines.truncate(THINKING_LINES);
        lines.push(Line::styled(
            format!("{}… {hidden} more line(s)", gutter.head),
            style_of(gutter.style),
        ));
    }
    lines
}

/// The cell a streaming block is: what a run of deltas becomes once it stops
/// arriving.
///
/// One function because a block laid out while it streams and the cell the same
/// block is filed away as have to be the same value. [`State::live_lines`] lays
/// one out and [`State::end_block`] stores the other, and a second copy of this
/// match would be a second answer to what a fragment in a style means -- which is
/// the answer a resumed session is replayed through.
pub(super) fn live_cell(style: Style, text: &str) -> Cell {
    match style {
        Style::Reasoning => Cell::Reasoning(text.to_owned()),
        _ => Cell::Content(text.to_owned()),
    }
}

/// Wrap styled spans into the terminal lines they need at `width` columns.
///
/// Both halves of the front end need this, for the same reason: nothing here may
/// be left to the terminal's own soft wrapping. The transcript is a region of a
/// fixed width, so an unwrapped line would be cut off at the edge,
/// and `insert_before` renders into a fixed-width buffer, where an over-long line
/// is silently cut off -- unlike the plain front end, where the terminal wraps
/// and nothing is lost.
///
/// Written here rather than handed to the framework, which has nothing to hand it
/// to. The framework wraps inside the widget that draws: `Paragraph::wrap` wraps
/// into the area it is rendered into and keeps the result, so the lines never come
/// back -- `Paragraph::line_count` answers with a number, and asking it before a
/// render means wrapping everything twice to get one. `reflow::WordWrapper`, which
/// is what does the wrapping in there, is a private module. What this returns is a
/// value instead, and the same value serves all three places the transcript is
/// laid out: the window on screen, the plain front end's fixed-width buffer, and
/// the row count the window is computed from.
///
/// Progress is guaranteed even when a single character is wider than the whole
/// field: the first character is taken regardless, so a narrow terminal degrades
/// to a clipped wide glyph rather than looping forever.
pub(super) fn wrapped_lines(spans: &[Span], width: usize) -> Vec<Line<'static>> {
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
        let mut rest = span.text.as_str();
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
pub(super) fn break_line(piece: &str, room: usize, width: usize) -> (&str, &str, bool) {
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

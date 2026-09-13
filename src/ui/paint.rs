//! The painter: cells, spans and screen elements turned into the terminal
//! lines the screen is drawn from.
//!
//! One layer for everything a cell becomes -- the transcript on screen, the
//! window over it, the session log folded back into cells, the standing task
//! list pinned under the transcript, the queue waiting to run, and the
//! question panel standing over the box. A session that reads differently in
//! any of them is a session that was not really one transcript, and that is
//! what a single painter buys.
//!
//! Three things are kept apart on purpose:
//!
//! - [`crate::ui::cell`] holds what a cell *is*: terminal-agnostic data, with
//!   a [`Style`] that is a label rather than an escape sequence and a
//!   [`Gutter`] that names its own columns. The painter reaches for both;
//!   the cell never reaches back for the painter.
//! - This module holds how a cell is *drawn*: the pure functions that take
//!   cells, spans and widths, and produce the `ratatui` [`Line`]s the screen
//!   hands to its backend. Nothing here owns state, touches the terminal, or
//!   reads the clock.
//! - [`crate::ui::tui`] holds *what* is drawn from: the screen, the
//!   transcript, the input box and the state a notice or a key changes. The
//!   painter answers `&State` questions; it does not ask them.
//!
//! The plain front end writes its own bytes (see [`crate::ui::renderer`]),
//! because leaving wrapping to the terminal is the right thing for a stream
//! that has no fixed-width region to fit in. The two front ends share the
//! cell layer; they do not share this one -- the painter is the TUI's, with
//! no backend-agnostic detour through a `RenderedSpan` the plain front end
//! would only translate back.

use ratatui::style::{Color, Modifier, Style as RStyle};
use ratatui::text::{Line, Span as RSpan};

use crate::tools::ask::Question;
use crate::tools::todo::{self, Todo};
use crate::ui::cell::{self, Cell, Gutter, Span, Style};
use crate::ui::text;
use crate::ui::tui::layout;

/// The widest a line of the transcript is laid out, however wide the terminal is.
///
/// A line of prose is read by running the eye back to its start, and past a certain
/// width that return trip costs more than the columns it saved: on a 200-column
/// terminal, an answer set to the full width is a line the reader has to hunt the
/// start of. 100 columns is about as wide as a line of monospaced text stays
/// comfortable, and it is wider than the 80-column terminal most of this is read
/// on -- so the measure only ever shortens a line on the screens that need it.
pub(crate) const MEASURE: usize = 100;

/// The columns text is laid out in inside a region `width` wide: as wide as the
/// region, and no wider than [`MEASURE`].
pub(crate) fn measure(width: usize) -> usize {
    width.min(MEASURE)
}

/// Our style, as ratatui sees it. This is what [`Style`] being data buys: the
/// mapping happens once per front end, instead of at every call site.
///
/// The dim/bold half comes from [`Style::modifiers`] -- the same method the
/// plain front end uses, so the two front ends cannot disagree on what a
/// `Reasoning` line looks like. The colour is this front end's own, as a
/// `Color` the framework hands to the theme: a palette the theme chose for a
/// background this code cannot see.
pub(crate) fn style_of(style: Style) -> RStyle {
    let mut s = RStyle::new();
    let m = style.modifiers();
    if m.dim {
        s = s.add_modifier(Modifier::DIM);
    }
    if m.bold {
        s = s.add_modifier(Modifier::BOLD);
    }
    let color = match style {
        Style::Yellow => Some(Color::Yellow),
        Style::Green => Some(Color::Green),
        Style::Red => Some(Color::Red),
        _ => None,
    };
    if let Some(c) = color {
        s = s.fg(c);
    }
    s
}

/// The row that says how much of the transcript the window is not showing, drawn
/// on the edge it was cut at.
///
/// The transcript is the one block with no cap of its own: it is as long as the
/// session has been, and a window over it is cut at both ends. The two counts are
/// the same shape, so each says which end it is at rather than leaving the reader
/// to work it out from where the row landed -- a reader who has just scrolled is
/// reading one row, not the shape of the region.
///
/// The marker is the transcript's own two columns, so the count starts in the
/// column the cells' text does and cannot be mistaken for a cell.
///
/// The count covers the row the report stands on as well as the lines beyond it:
/// the report is one row of the transcript that is not drawn, and a count that
/// left its own row out would be a line short. So the smallest either end can
/// say is two lines, and there is no singular form here.
pub(crate) fn edge_line(above: bool, n: usize) -> Line<'static> {
    let side = if above { "above" } else { "below" };
    Line::styled(format!("\u{22ee} {n} lines {side}"), style_of(Style::Dim))
}

/// The row that says how many rows a window is not showing, at the end it was cut
/// at.
///
/// A cut window that says nothing is a window that lies about how much there is:
/// six rows of a menu read as the whole menu, three rows of a queue as the whole
/// queue. Dim, like everything else that is not the thing being chosen; `lead` is
/// the marker column the row it stands in for would carry, so that the count lines
/// up with what it counts.
///
/// The glyph is the vertical ellipsis `⋮` -- the same one [`edge_line`] uses to
/// count rows hidden above or below the window over the transcript. The picker
/// menu, the standing task list and the queued lines are three more vertical
/// lists with hidden items; sharing the glyph means a reader who has learned
/// one of them has learned all of them.
pub(crate) fn more_line(lead: &str, n: usize) -> Line<'static> {
    Line::styled(format!("{lead}⋮ {n} more"), style_of(Style::Dim))
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
pub(crate) const THINKING_LINES: usize = 12;

/// The spans a cell is drawn from, given what the screen shows elsewhere.
///
/// One case so far: the tasks of a `TodoWrite` call. The list they belong to
/// stands in the pinned block for as long as it is the list that is true, and the
/// same list twice on one screen spends the block's rows on saying nothing -- so
/// the transcript keeps the head, which is the line the model was answered with,
/// and the block keeps the tasks. Every `TodoWrite` cell folds, and not only the
/// list that stands: a cell is laid out once for one width and never revisited,
/// so a rule that changed with a later cell would rewrite history on the next
/// resize rather than on the draw that made the list stand.
fn spans_of(cell: &Cell) -> Vec<Span> {
    match cell {
        Cell::Todo(todos) => cell::todo_head_spans(todos),
        _ => cell.spans(),
    }
}

/// The lines one cell occupies at `width`.
///
/// One function, because the transcript on screen, the window over it and the
/// session log folded back into cells are three places the same cells are laid
/// out, and a session that reads differently in any of them is a session that was
/// not really one transcript.
///
/// One cell at a time, because a cell is what a draw can keep: it is the unit the
/// session's output arrives in and it does not change once it is pushed, so it is
/// also the unit [`State`](crate::ui::tui::state::State) lays out and remembers.
pub(crate) fn cell_lines(cell: &Cell, width: usize) -> Vec<Line<'static>> {
    // Never wider than the measure, however wide the region is: the gutter is
    // inside it, so the answer and the machinery around it end in the same column.
    let width = measure(width);
    let Some(gutter) = cell.gutter() else {
        // The answer: the one cell that starts at the left edge.
        return wrapped_lines(&spans_of(cell), width);
    };
    // Wrapped into what the gutter leaves, so a line of a set-in cell carries as
    // much as a line of the answer rather than two columns more.
    let mut lines = wrapped_under(&spans_of(cell), width, gutter);
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
            format!("{}{hidden} more lines", gutter.head),
            style_of(gutter.style),
        ));
    }
    lines
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

/// The rule that keeps a wrapped block on one left edge, and the reason it lives
/// here rather than in the cell: only the layer that does the wrapping knows where
/// the lines fall. A cell reaches it through [`cell_lines`], and the standing task
/// list through it directly -- a block that is pinned is not a cell, but a task
/// whose wrapped rows came back to column zero would still read as a task of its
/// own.
fn wrapped_under(spans: &[Span], width: usize, gutter: Gutter) -> Vec<Line<'static>> {
    wrapped_lines(spans, width.saturating_sub(gutter.width()))
        .into_iter()
        .enumerate()
        .map(|(i, line)| {
            let lead = if i == 0 { gutter.head } else { gutter.rest };
            let mut spans = vec![RSpan::styled(lead, style_of(gutter.style))];
            spans.extend(line.spans);
            Line::from(spans)
        })
        .collect()
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

// ============================================================ standing tasks =

/// The heading of the standing task list: what the block is, and how far it has
/// got.
///
/// Dim, and marked with the same `·` a note carries: the block is pinned under
/// the transcript for the whole of a turn, and its one job when nothing has
/// changed is to not be read. The tasks below it carry the attention.
///
/// The count is [`todo::summary`], the same string the cell's head and the model's
/// result carry, so the three cannot say different things about one list.
pub(crate) fn todo_title(todos: &[Todo]) -> Line<'static> {
    Line::styled(
        format!("· todo · {}", todo::summary(todos)),
        style_of(Style::Dim),
    )
}

/// The standing task list as it is drawn: a blank line first so the block cannot
/// read as the tail of the transcript above it, then the title, then a window of
/// the tasks. A list that does not fit in the block folds to a count, the way
/// the window over the transcript folds when it is scrolled.
///
/// The tasks drawn are the window [`layout::todo_window`] picks, so the task in
/// hand is one of them: a block pinned to the head would hide exactly the row
/// being worked on.
pub(crate) fn standing_todo_lines(todos: &[Todo], width: usize) -> Vec<Line<'static>> {
    let width = measure(width);
    let room = layout::TODO_ROWS.saturating_sub(layout::TODO_HEADS);
    let active = todos
        .iter()
        .position(|todo| todo.status == todo::Status::InProgress);
    let window = layout::todo_window(todos.len(), active, room);
    // The blank row first, so that the block cannot be read as the tail of the
    // transcript above it, then the title.
    let mut lines = vec![Line::default(), todo_title(todos)];
    if window.above > 0 {
        lines.push(more_line("  ", window.above));
    }
    for todo in &todos[window.first..window.last] {
        // Through the cell's own wrapping, with the task's own gutter: a task
        // is one row until its words run out of columns, and the rows it then
        // takes are its own -- the ones behind it keep theirs.
        lines.extend(wrapped_under(
            &cell::todo_line_spans(todo),
            width,
            cell::todo_gutter(todo),
        ));
    }
    if window.below > 0 {
        lines.push(more_line("  ", window.below));
    }
    lines
}

// ================================================================ queue ======

/// The queued lines, laid out, capped at [`layout::QUEUE_ROWS`].
///
/// The cap is a budget of the screen, not the queue: a queue longer than that
/// -- more lines, or longer ones -- costs the transcript those rows and no
/// more. What the end of the window keeps is the newest line, which is the
/// one just typed and the one being waited for; what it is not showing is
/// counted rather than dropped, the rule the picker keeps to as well, so
/// that a queue running past the cap does not read as a queue of three. The
/// count is drawn on one of the rows the cap allows rather than on a row of
/// its own: the cap is what the transcript is paying.
pub(crate) fn queued_lines(queued: &[String], width: usize) -> Vec<Line<'static>> {
    let width = measure(width);
    let mut lines = Vec::new();
    for line in queued {
        lines.extend(wrapped_lines(
            &[Span::new(Style::Dim, format!("› {line}"))],
            width,
        ));
    }
    if lines.len() <= layout::QUEUE_ROWS {
        return lines;
    }
    let hidden = lines.len() - (layout::QUEUE_ROWS - 1);
    let mut window = vec![more_line("  ", hidden)];
    window.extend(lines.split_off(hidden));
    window
}

// ============================================================ question panel =

/// The columns that open a row of the question panel: where the cursor is, and
/// which options are chosen. One marker per question kind, so that a row is
/// never saying two things in one column -- a cursor sitting on a chosen option
/// still shows both.
const PANEL_CURSOR: &str = "❯ ";
const PANEL_CHOSEN: &str = "✓ ";
const PANEL_BLANK: &str = "  ";

/// Lines the panel shows under the options. The keys are the whole of what a
/// reader has to learn here, and the panel is where they are learnt.
const PANEL_MOVE: &str = "↑/↓ move";
const PANEL_TOGGLE: &str = "space toggles";
const PANEL_TYPE: &str = "type to answer in your own words";
const PANEL_CONFIRM: &str = "Enter confirms";
const PANEL_SKIP: &str = "Esc skips";

/// The question tool's panel as it is drawn: which question of how many, the
/// question itself, its options with the cursor on one of them, and the keys.
///
/// Wrapped, never clipped: it is drawn over the transcript, so an over-long
/// line would be cut at the edge of the region instead of folded, which is the
/// one thing the terminal cannot be left to fix.
///
/// `questions` is the full list, `at` the index of the question being answered,
/// `cursor` the option the cursor is on, `chosen` the labels picked so far per
/// question. The panel state itself is the caller's -- the painter only reads
/// what the question, the cursor and the chosen labels say.
pub(crate) fn panel_lines(
    questions: &[Question],
    at: usize,
    cursor: usize,
    chosen: &[Vec<String>],
    width: usize,
) -> Vec<Line<'static>> {
    let question = &questions[at];
    let width = measure(width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !question.header.is_empty() || questions.len() > 1 {
        let mut heading = String::new();
        if !question.header.is_empty() {
            heading.push_str(&question.header);
            heading.push_str(" · ");
        }
        heading.push_str(&format!("question {} of {}", at + 1, questions.len()));
        lines.extend(wrapped_lines(&[Span::new(Style::Dim, heading)], width));
    }
    let mut ask = vec![Span::new(Style::Yellow, question.question.clone())];
    if question.multi_select {
        ask.push(Span::new(Style::Dim, " · choose any"));
    }
    lines.extend(wrapped_lines(&ask, width));
    // Every option is drawn: the call allows four of them, so there is
    // nothing here to window -- and an option nobody can see is an option
    // nobody can choose.
    for option_at in 0..question.options.len() {
        lines.extend(option_lines(
            question,
            &chosen[at],
            option_at,
            cursor,
            width,
        ));
    }
    lines.extend(wrapped_lines(
        &[Span::new(Style::Dim, footer(question))],
        width,
    ));
    lines
}

/// One option as the panel draws it, the cursor's row set in reverse.
fn option_lines(
    question: &Question,
    chosen: &[String],
    at: usize,
    cursor: usize,
    width: usize,
) -> Vec<Line<'static>> {
    let option = &question.options[at];
    let cursor_marker = if at == cursor {
        PANEL_CURSOR
    } else {
        PANEL_BLANK
    };
    let mut spans = Vec::new();
    if question.multi_select {
        let is_chosen = chosen.contains(&option.label);
        spans.push(Span::new(
            Style::Dim,
            format!(
                "{cursor_marker}{}",
                if is_chosen { PANEL_CHOSEN } else { PANEL_BLANK }
            ),
        ));
    } else {
        spans.push(Span::new(Style::Dim, cursor_marker));
    }
    spans.push(Span::new(Style::Dim, format!("{}. ", at + 1)));
    spans.push(Span::new(Style::Plain, option.label.clone()));
    if !option.description.is_empty() {
        spans.push(Span::new(Style::Dim, format!(" — {}", option.description)));
    }
    let mut lines = wrapped_lines(&spans, width);
    if at == cursor {
        // The row the next key acts on is the one reversed, the way the picker
        // marks the row `Enter` would take.
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
    }
    lines
}

/// What the panel says the keys do, under the options it is offering.
fn footer(question: &Question) -> String {
    if question.options.is_empty() {
        return format!("{PANEL_TYPE} · {PANEL_CONFIRM} · {PANEL_SKIP}");
    }
    let mut keys = vec![PANEL_MOVE];
    if question.multi_select {
        keys.push(PANEL_TOGGLE);
    }
    format!(
        "{} · {PANEL_TYPE} · {PANEL_CONFIRM} · {PANEL_SKIP}",
        keys.join(" · ")
    )
}

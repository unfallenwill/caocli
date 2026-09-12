//! The view layer: turning [`State`] into the ratatui lines the screen draws.
//!
//! Every function here reads [`State`] (sometimes mutably, to maintain the
//! laid cache) and produces a `Line`, a `Block`, a `String`, or a `Vec<Line>`
//! that the screen hands to ratatui. The split from [`State`] is on purpose:
//! state holds what is true of the session, render holds what is drawn from
//! it, and the two no longer share methods.
//!
//! The one bit of state render does mutate is the laid cache (`laid`,
//! `laid_width`, `laid_cells`). The invariant it keeps is that `laid` is a
//! laid-out prefix of `transcript`: every entry of `laid` corresponds to a
//! cell in `transcript[0..laid.len()]`, and the two move together. State
//! pushes cells but does not touch `laid` -- only render extends it, and only
//! up to the length `transcript` has reached.
//!
//! `Screen::draw_at` is the only caller that takes a mutable reference to
//! `State` through here; everywhere else render is `&State`.

use std::time::Instant;

use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::Line;
use ratatui::widgets::Block;

use crate::tools::todo::{self, Todo};
use crate::ui::cell::{self, Span, Style};
use crate::ui::tui::layout::{self, BOX_BORDERS};
use crate::ui::tui::paint::{
    cell_lines, live_cell, measure, more_line, style_of, wrapped_lines, wrapped_under,
};
use crate::ui::tui::state::State;

/// The frames the working spinner cycles through while a turn runs, one per
/// [`SPINNER_MS`]. Braille raising-dots: the convention terminal spinners have
/// settled on, and narrow enough to sit inside a border without crowding it.
pub(super) const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// How long one spinner frame holds, in milliseconds. Twelve-ish frames a
/// second: fast enough to read as motion, slow enough that a frame is one
/// redraw and no more.
pub(super) const SPINNER_MS: u128 = 80;

/// The live speed estimate appears once the turn has run this many seconds:
/// before that the cumulative average swings too much to be worth reading.
pub(super) const SPEED_AFTER_SECS: u64 = 3;

/// The rows the queue is allowed to take from the transcript.
///
/// Three lines: enough to read the head and the count without reading so much
/// that the answer is pushed off the screen. A queue longer than that -- more
/// lines, or longer ones -- costs the transcript those rows and no more.
pub(super) const QUEUE_ROWS: usize = 3;

// --------------------------------------------------------------------- laid -

/// Lay the transcript out at `width`, keeping what is already laid.
///
/// Only the cells that are not laid yet are wrapped, which for a running turn
/// is the one block that has just closed. The alternative is wrapping the
/// whole session for every fragment that arrives, and a session is as long as
/// the conversation has been.
///
/// The cache is a prefix of the transcript: `laid.len() <= transcript.len()`,
/// and a cell that was laid is laid at the most recent `width`. A different
/// `width` invalidates every entry, since wrapping depends on width.
pub(super) fn ensure_laid(state: &mut State, width: usize) {
    // A different width is a different wrapping of every line there is.
    if state.laid_width != Some(width) {
        state.laid.clear();
        state.laid_width = Some(width);
    }
    // Never more cells than the transcript has. Only a test can take one away,
    // and lines of a cell that is gone are worse than laying one out twice.
    state.laid.truncate(state.transcript.len());
    let from = state.laid.len();
    for cell in &state.transcript[from..] {
        #[cfg(test)]
        {
            state.laid_cells += 1;
        }
        state.laid.push(cell_lines(cell, width));
    }
}

/// The rows the laid cells take: what a window over the transcript is measured
/// in. A count rather than a copy of the lines, so the part of a long session
/// that is off the top costs a draw nothing.
pub(super) fn laid_rows(state: &State) -> usize {
    state.laid.iter().map(Vec::len).sum()
}

/// The rows `[first, last)`, at the width the cells were laid at.
///
/// Only the cells that overlap the window are touched, and of those only the
/// lines inside it. `live` and `question` come after the cells, in that order:
/// they are the two blocks that are not laid out with them, because they
/// change with every fragment and every keystroke.
pub(super) fn window_lines(
    state: &State,
    first: usize,
    last: usize,
    live: &[Line<'static>],
    question: &[Line<'static>],
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(last.saturating_sub(first));
    let mut at = 0;
    for segment in state.laid.iter().map(Vec::as_slice).chain([live, question]) {
        if at >= last {
            break;
        }
        let end = at + segment.len();
        if end > first {
            let lo = first.saturating_sub(at);
            let hi = (last - at).min(segment.len());
            out.extend_from_slice(&segment[lo..hi]);
        }
        at = end;
    }
    out
}

// ------------------------------------------------------------------ blocks -

/// The block still being streamed, laid out, or nothing when no turn is writing
/// one.
///
/// Laid out through the same [`cell_lines`] a filed cell goes through, and not
/// merely because the two look alike: a live block that took no gutter would
/// jump two columns left the moment the block closed, which is the one reading
/// position a reader is sitting on when the model stops typing.
pub(super) fn live_lines(state: &State, width: usize) -> Vec<Line<'static>> {
    match &state.live {
        Some((style, text)) => cell_lines(&live_cell(*style, text), width),
        None => Vec::new(),
    }
}

/// The question standing over the box, laid out, or nothing when none is open.
///
/// The question is what the answer is about, and the call in it can be a long
/// command: one that is clipped gives the user nothing to decide with. It is a
/// cell like any other, so it carries the marker its kind carries, and the
/// answer typed into the box below it starts in the column its own text does.
pub(super) fn question_lines(state: &State, width: usize) -> Vec<Line<'static>> {
    match &state.question {
        Some(question) => cell_lines(question, width),
        None => Vec::new(),
    }
}

/// The heading of the standing task list: what the block is, and how far it has
/// got.
///
/// Dim, and marked with the same `·` a note carries: the block is pinned under
/// the transcript for the whole of a turn, and its one job when nothing has
/// changed is to not be read. The tasks below it carry the attention.
///
/// The count is [`todo::summary`], the same string the cell's head and the model's
/// result carry, so the three cannot say different things about one list.
pub(super) fn todo_title(todos: &[Todo]) -> Line<'static> {
    Line::styled(
        format!("· todos · {}", todo::summary(todos)),
        style_of(Style::Dim),
    )
}

/// The standing task list, laid out, or nothing when no call has written one
/// or the last one cleared it.
///
/// A fold over the transcript and not a copy of it: what is standing is the
/// last list a call wrote, which is a cell like any other, so a resumed session
/// keeps exactly the list in view that the session watched live had. There is
/// nothing here to keep in step with anything, which is the whole reason the
/// tool writes the list into the log instead of holding it somewhere.
///
/// Each task is wrapped on its own: the block has a fixed number of rows to
/// give, and a task long enough to wrap must cost its own rows rather than
/// push the tasks behind it out of the block. The tasks drawn are the window
/// [`layout::todo_window`] picks, so the task in hand is one of them.
pub(super) fn todo_lines(state: &State, width: usize) -> Vec<Line<'static>> {
    let Some(todos) = cell::standing_todos(&state.transcript) else {
        return Vec::new();
    };
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

/// The queued lines, laid out, capped at [`QUEUE_ROWS`].
///
/// The cap is a budget of the screen, not the queue: a queue longer than that
/// -- more lines, or longer ones -- costs the transcript those rows and no
/// more. What the end of the window keeps is the newest line, which is the
/// one just typed and the one being waited for; what it is not showing is
/// counted rather than dropped, the rule the picker keeps to as well, so
/// that a queue running past the cap does not read as a queue of three. The
/// count is drawn on one of the rows the cap allows rather than on a row of
/// its own: the cap is what the transcript is paying.
pub(super) fn queue_lines(state: &State, width: usize) -> Vec<Line<'static>> {
    let width = measure(width);
    let mut lines = Vec::new();
    for line in &state.queued {
        lines.extend(wrapped_lines(
            &[Span::new(Style::Dim, format!("› {line}"))],
            width,
        ));
    }
    if lines.len() <= QUEUE_ROWS {
        return lines;
    }
    let hidden = lines.len() - (QUEUE_ROWS - 1);
    let mut window = vec![more_line("  ", hidden)];
    window.extend(lines.split_off(hidden));
    window
}

/// The whole transcript as lines, in draw order: the session's finished cells,
/// then the block still streaming, then a pending question.
///
/// A draw wants the window, not the whole of it, but a test that asks what the
/// transcript says has to be able to read all of it -- and it reads it through
/// the same pieces the draw uses, which is what keeps the two from being two
/// renderers.
#[cfg(test)]
pub(super) fn lines(state: &mut State, width: usize) -> Vec<Line<'static>> {
    ensure_laid(state, width);
    let live = live_lines(state, width);
    let question = question_lines(state, width);
    let total = laid_rows(state) + live.len() + question.len();
    window_lines(state, 0, total, &live, &question)
}

// ----------------------------------------------------------- status / box -

/// The pinned status line: the session summary, always. What a turn is doing
/// is the transcript's to say -- the cells it produces -- and the summary is
/// what you read when you are about to type rather than while you wait.
///
/// One column is left free so the write cannot trigger autowrap.
pub(super) fn status_line(state: &State, width: usize) -> Line<'static> {
    Line::from(state.status.line(width.saturating_sub(1)))
}

/// The working indicator the input box's top border carries while a turn
/// runs: a spinner, the seconds it has run, and -- once the turn has lasted
/// long enough for the average to settle -- an estimated tokens-per-second.
///
/// None when there is nothing to say: no turn, or a question standing over the
/// box (the gate's own words own that border's neighbourhood), or a border too
/// narrow even for the spinner and the count. A narrow border drops the
/// estimate whole first and hides the indicator entirely second -- never a
/// clipped number, the same rule the status line keeps to.
pub(super) fn activity_title(state: &State, width: usize) -> Option<String> {
    activity_title_at(state, Instant::now(), width)
}

/// The indicator's words at `now`, without reading the clock.
fn activity_title_at(state: &State, now: Instant, width: usize) -> Option<String> {
    if state.reply.is_some() {
        return None;
    }
    let started = state.turn_started?;
    let elapsed = now - started;
    let frame = SPINNER[(elapsed.as_millis() / SPINNER_MS) as usize % SPINNER.len()];
    let count = format!("{frame} {}s", elapsed.as_secs());
    let mut title = count.clone();
    if elapsed.as_secs() >= SPEED_AFTER_SECS && state.streamed_chars > 0 {
        // The measured ratio turns characters into tokens; the tilde keeps
        // the estimate honest about being one.
        let per_second =
            state.streamed_chars as f64 / state.chars_per_token / elapsed.as_secs_f64();
        let per_second = per_second.round().max(1.0) as u64;
        title = format!("{count} · ~{per_second} token/s");
    }
    if crate::ui::text::width(&title) <= width {
        return Some(title);
    }
    (crate::ui::text::width(&count) <= width).then_some(count)
}

/// The box's rules: the two lines that close it in, and the working indicator
/// the top of them carries while a turn runs.
///
/// The rules are the front end's to draw rather than the editor's, and that is
/// what buys the draft its column: a block on the editor is rendered into the
/// area the editor is rendered into, so insetting the editor to clear the
/// marker would pull both rules in with it and leave the box two columns short
/// of the edges the transcript is read against.
///
/// Built per draw, which costs one small struct: the title is a clock and a
/// spinner, so there is nothing here worth remembering, and nothing that can go
/// stale when the editor is replaced whole by a submitted or a cleared line.
pub(super) fn box_rule(state: &State, width: usize) -> Block<'static> {
    let mut block = Block::default()
        .borders(BOX_BORDERS)
        .border_style(RStyle::new().add_modifier(Modifier::DIM));
    if let Some(title) = activity_title(state, width.saturating_sub(2)) {
        // Not dim, unlike the rule it sits on: it is the one thing on this box
        // that moves, and the only sign that a turn is still running when the
        // model has gone quiet. A dim indicator on a dim border is the signal
        // painted out of sight.
        //
        // The dim is taken *off* rather than left unset: a title is written on
        // the border's own cells, and the border's style is patched into them
        // before the title is, so a title that says nothing about weight is a
        // dim title. Only a style that removes the modifier can undo that.
        let lit = RStyle::new().remove_modifier(Modifier::DIM);
        block = block.title_top(Line::styled(title, lit).right_aligned());
    }
    block
}

/// Bump the revision when the spinner frame or the second counter has
/// advanced, so the border keeps moving while nothing else arrives and no
/// redraw is spent when it has nothing new to show.
pub(super) fn tick_activity(state: &mut State, now: Instant) {
    let Some(title) = activity_title_at(state, now, usize::MAX) else {
        return;
    };
    if Some(&title) != state.ticked_activity.as_ref() {
        state.ticked_activity = Some(title);
        state.revision += 1;
    }
}

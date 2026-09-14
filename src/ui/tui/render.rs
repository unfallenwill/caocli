//! The view layer: turning [`View`], [`Turn`] and [`Overlay`] into the ratatui
//! lines the screen draws.
//!
//! Every function here takes only the slices of state it actually reads or
//! writes -- its parameter list is its dependency list. Pure cell-to-line
//! rendering lives one layer down in [`crate::ui::paint`]; what is here is the
//! state-aware composition -- which cells go on screen, which lines of them
//! fit the window, and what the box's border says while a turn runs.
//!
//! The split is on purpose: state holds what is true of the session, render
//! holds what is drawn from it, and the painter holds what a cell becomes --
//! three layers, each with one job. The slices this module asks for are the
//! surface area a function is allowed to touch: adding a field to [`State`] is
//! not a change here, because none of these functions can reach it.
//!
//! The one bit of state render mutates is the laid cache (`laid`,
//! `laid_width`, `laid_cells`). The invariant it keeps is that `laid` is a
//! laid-out prefix of `transcript`: every entry of `laid` corresponds to a
//! cell in `transcript[0..laid.len()]`, and the two move together. The
//! transcript pushes cells but does not touch `laid` -- only [`ensure_laid`]
//! extends it, and only up to the length the transcript has reached.
//!
//! [`View`]: crate::ui::tui::view::View
//! [`Turn`]: crate::ui::tui::turn::Turn
//! [`Overlay`]: crate::ui::tui::overlay::Overlay

use std::time::Instant;

use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::Line;
use ratatui::widgets::Block;

use crate::ui::cell;
use crate::ui::paint::{cell_lines, standing_todo_lines};
use crate::ui::tui::layout::BOX_BORDERS;
use crate::ui::tui::overlay::Overlay;
use crate::ui::tui::thought::ThoughtBlock;
use crate::ui::tui::turn::Turn;
use crate::ui::tui::view::View;

/// The frames the working spinner cycles through while a turn runs, one per
/// [`SPINNER_MS`]. Four half-circles rotating around a centre: the convention
/// terminal spinners settled on in the late 1990s, and narrow enough to sit
/// inside a border without crowding it.
pub(super) const SPINNER: [char; 4] = ['◐', '◓', '◑', '◒'];

/// How long one spinner frame holds, in milliseconds. Twelve-ish frames a
/// second: fast enough to read as motion, slow enough that a frame is one
/// redraw and no more.
pub(super) const SPINNER_MS: u128 = 80;

/// The live speed estimate appears once the turn has run this many seconds:
/// before that the cumulative average swings too much to be worth reading.
pub(super) const SPEED_AFTER_SECS: u64 = 3;

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
pub(super) fn ensure_laid(view: &mut View, width: usize) {
    if view.laid_width != Some(width) {
        view.laid.clear();
        view.laid_width = Some(width);
    }
    // Never more cells than the transcript has. Only a test can take one away,
    // and lines of a cell that is gone are worse than laying one out twice.
    view.laid.truncate(view.transcript.len());
    let from = view.laid.len();
    for cell in &view.transcript[from..] {
        #[cfg(test)]
        {
            view.laid_cells += 1;
        }
        view.laid.push(cell_lines(cell, width));
    }
}

/// The rows the laid cells take: what a window over the transcript is measured
/// in. A count rather than a copy of the lines, so the part of a long session
/// that is off the top costs a draw nothing.
pub(super) fn laid_rows(view: &View) -> usize {
    view.laid.iter().map(Vec::len).sum()
}

/// The rows `[first, last)`, at the width the cells were laid at.
///
/// Only the cells that overlap the window are touched, and of those only the
/// lines inside it. `live` and `question` come after the cells, in that order:
/// they are the two blocks that are not laid out with them, because they
/// change with every fragment and every keystroke.
pub(super) fn window_lines(
    view: &View,
    first: usize,
    last: usize,
    live: &[Line<'static>],
    question: &[Line<'static>],
) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(last.saturating_sub(first));
    let mut at = 0;
    for segment in view.laid.iter().map(Vec::as_slice).chain([live, question]) {
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
pub(super) fn live_lines(view: &View, width: usize) -> Vec<Line<'static>> {
    match view.stream.current() {
        Some((style, text)) => cell_lines(&style.stream_cell(text.to_owned()), width),
        None => Vec::new(),
    }
}

/// The question standing over the box, laid out, or nothing when none is open.
///
/// The question is what the answer is about, and the call in it can be a long
/// command: one that is clipped gives the user nothing to decide with. It is a
/// cell like any other, so it carries the marker its kind carries, and the
/// answer typed into the box below it starts in the column its own text does.
pub(super) fn question_lines(view: &View, width: usize) -> Vec<Line<'static>> {
    match &view.question {
        Some(question) => cell_lines(question, width),
        None => Vec::new(),
    }
}

/// The standing task list as the screen draws it: nothing when no call has
/// written one or the last one cleared it.
///
/// A fold over the transcript and not a copy of it: what is standing is the
/// last list a call wrote, which is a cell like any other, so a resumed
/// session keeps exactly the list in view that the session watched live had.
/// The drawing itself lives in [`crate::ui::paint::standing_todo_lines`]; this
/// layer only finds the list.
pub(super) fn todo_lines(view: &View, width: usize) -> Vec<Line<'static>> {
    let Some(todos) = cell::standing_todos(&view.transcript) else {
        return Vec::new();
    };
    standing_todo_lines(todos, width)
}

/// Render the expanded body of a thought block: the cells the
/// thought "owns", laid out flat, full window width.
///
/// Cells come from `thought.snapshot` — the frozen copy taken when
/// the user pressed Ctrl-O. Step cells render with their children
/// visible (a Done step's output is exactly the detail the user
/// opened the expanded view to read); Reasoning cells render the
/// same way they do on the transcript.
///
/// `gap` is the space between two consecutive cells in the body —
/// the same blank line the transcript puts between cells, kept
/// consistent so a body that begins with the same cells the
/// transcript ended with reads as the same picture.
#[allow(dead_code)] // wired up by the thinking widget in a follow-up commit
pub(super) fn expanded_thought_lines(
    snapshot: &[crate::ui::cell::Cell],
    width: usize,
    gap: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for (i, cell) in snapshot.iter().enumerate() {
        if i > 0 && gap {
            out.push(Line::raw(""));
        }
        out.extend(crate::ui::paint::expanded_cell_lines(cell, width));
    }
    out
}

/// The queued lines as the screen draws them. The drawing itself lives in
/// [`crate::ui::paint::queued_lines`]; this layer only hands the queue over.
pub(super) fn queue_lines(turn: &Turn, width: usize) -> Vec<Line<'static>> {
    let queued: Vec<String> = turn.queued.iter().cloned().collect();
    crate::ui::paint::queued_lines(&queued, width)
}

/// The whole transcript as lines, in draw order: the session's finished cells,
/// then the block still streaming, then a pending question.
///
/// A draw wants the window, not the whole of it, but a test that asks what the
/// transcript says has to be able to read all of it -- and it reads it through
/// the same pieces the draw uses, which is what keeps the two from being two
/// renderers.
#[cfg(test)]
pub(super) fn lines(view: &mut View, width: usize) -> Vec<Line<'static>> {
    ensure_laid(view, width);
    let live = live_lines(view, width);
    let question = question_lines(view, width);
    let total = laid_rows(view) + live.len() + question.len();
    window_lines(view, 0, total, &live, &question)
}

// ----------------------------------------------------------- status / box -

/// The pinned status line: the session summary, always. What a turn is doing
/// is the transcript's to say -- the cells it produces -- and the summary is
/// what you read when you are about to type rather than while you wait.
///
/// One column is left free so the write cannot trigger autowrap.
pub(super) fn status_line(view: &View, width: usize) -> Line<'static> {
    Line::from(view.status.line(width.saturating_sub(1)))
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
pub(super) fn activity_title(
    overlay: &Overlay,
    turn: &Turn,
    thought: Option<&ThoughtBlock>,
    width: usize,
) -> Option<String> {
    activity_title_at(overlay, turn, thought, Instant::now(), width)
}

/// The indicator's words at `now`, without reading the clock.
///
/// Phase-aware: the label says what the agent is doing, not just that
/// something is happening. The phases the border can name are
/// `thinking` (a thought block is open, no tool running), `running X`
/// (a tool that keeps the thought open is in flight), and `working`
/// (the model is producing content or the turn has nothing yet). The
/// spinner glyph and the elapsed seconds stay on every phase; the
/// only thing that changes is the word between them.
///
/// Approval gate keeps the border clear (gate owns it); no turn keeps
/// the border empty.
pub(super) fn activity_title_at(
    overlay: &Overlay,
    turn: &Turn,
    thought: Option<&ThoughtBlock>,
    now: Instant,
    width: usize,
) -> Option<String> {
    if overlay.reply.is_some() {
        return None;
    }
    // The clock the spinner and the seconds read from. The thought's
    // own clock when one is open (this is the per-block "how long
    // has this section been running"); the turn's clock when no
    // thought is open — a side-effectful tool has just closed the
    // previous one, the new one has not started, and the turn's
    // timer is the only one still running.
    let started = thought.and_then(|t| t.started_at).or(turn.started)?;
    let elapsed = now - started;
    let frame = SPINNER[(elapsed.as_millis() / SPINNER_MS) as usize % SPINNER.len()];
    // The phase comes from what the thought (or the running tool) is
    // doing right now. A tool running inside an open thought is the
    // "running X" shape; a tool that closed the thought (write,
    // edit, side-effectful bash) still shows on the border, but as
    // "running X" without an enclosing thought -- the turn is
    // still alive, the agent is still working, and saying nothing
    // would be a regression from the previous border.
    let phase = if let Some(verb) = turn.current_tool_verb.as_deref() {
        format!("running {verb}")
    } else if thought.is_some() {
        // The thought is open and no tool is running: the agent is
        // either streaming reasoning or just sitting between steps.
        // Either way, "thinking" is the closer word, and the thought
        // body's expanded view is where the distinction lives.
        "thinking".to_owned()
    } else {
        "working".to_owned()
    };
    let count = format!("{frame} {phase} · {}s", elapsed.as_secs());
    let mut title = count.clone();
    if elapsed.as_secs() >= SPEED_AFTER_SECS && turn.streamed_chars > 0 {
        // The measured ratio turns characters into tokens; the tilde keeps
        // the estimate honest about being one.
        let per_second = turn.streamed_chars as f64 / turn.chars_per_token / elapsed.as_secs_f64();
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
pub(super) fn box_rule(
    overlay: &Overlay,
    turn: &Turn,
    thought: Option<&ThoughtBlock>,
    width: usize,
) -> Block<'static> {
    let mut block = Block::default()
        .borders(BOX_BORDERS)
        .border_style(RStyle::new().add_modifier(Modifier::DIM));
    if let Some(title) = activity_title(overlay, turn, thought, width.saturating_sub(2)) {
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

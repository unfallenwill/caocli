//! The banner: the line a session opens with, and the only thing the plain front
//! end writes before the first turn.
//!
//! Kept apart from the renderer because it is measured rather than painted: the
//! layout is a pure function of a width (so every terminal width is a test), and
//! only the outermost call asks the terminal for one.

use super::cell;
use super::terminal::RealTerminal;
use super::terminal::Terminal;
use super::text;

/// The line a session opens with: what this is, which session it is, how much is
/// in it, and the model it is talking to.
///
/// Sized to the terminal it is going into. Whole segments are dropped from the
/// right when the line does not fit -- the rule the status line keeps to -- so
/// nothing is ever cut in the middle of a word: a summary that wraps reads as two
/// half-sentences, and on an 80-column terminal that is exactly what the line
/// used to do to the session's own path.
///
/// The path is not here at all. It is `~/.caocli/sessions/<id>.jsonl`, so it says
/// nothing the id does not, and it was the longest thing on the line; `--list`
/// prints it for anyone who wants the file itself.
pub(crate) fn banner(id: &str, messages: usize, model: &str) -> String {
    // No terminal, no width to fit: a file or a pipe can hold the whole line.
    banner_at(id, messages, model, available_columns())
}

/// The same, measured against a width given rather than one asked for, so that
/// what it does with an 80-column terminal can be a test.
pub(super) fn banner_at(id: &str, messages: usize, model: &str, columns: Option<usize>) -> String {
    // The segments in the order they are dropped from the right: the hint first
    // (the box says it too), then the model (the status line has it), then the
    // count (the id already says which session this is). Each carries the
    // separator that introduces it, because the count is set off from the session
    // it counts rather than joined to it the way the top-level segments are.
    let parts: [(&str, String); 5] = [
        ("", "caocli".to_owned()),
        (" · ", format!("session {id}")),
        (
            " ",
            match messages {
                1 => "(1 message)".to_owned(),
                n => format!("({n} messages)"),
            },
        ),
        (" · ", model.to_owned()),
        (" · ", "/help for commands".to_owned()),
    ];
    let line = |keep: usize| {
        parts[..keep]
            .iter()
            .map(|(sep, text)| format!("{sep}{text}"))
            .collect::<String>()
    };
    // The longest run of segments that fits, and never fewer than the two that
    // say what this is and which session it is: a line too short even for those
    // is left whole for the front end to clip, rather than emptied here.
    let mut keep = parts.len();
    if let Some(columns) = columns {
        while keep > 2 && text::width(&line(keep)) > columns {
            keep -= 1;
        }
    }
    line(keep)
}

/// The columns a transcript line has to write in: the terminal's width, less the
/// columns every line that is not an answer is set in from the left edge.
fn available_columns() -> Option<usize> {
    RealTerminal
        .size()
        .map(|(_, cols)| (cols as usize).saturating_sub(cell::MARKER_COLUMNS))
}

//! The streaming block: a run of text in one [`Style`] that becomes one cell
//! when it closes.
//!
//! A turn is a stream of fragments in two or three styles: reasoning, body
//! text, and (when a tool runs) the command's own output. The same block is
//! open until a fragment in a different style arrives or the turn ends, and
//! what is being streamed is the only kind of text the transcript keeps as
//! one cell per run. This is the data layer for that machinery: it tracks the
//! open block and turns it into a [`Cell`] when asked.
//!
//! Why both front ends share it: the rule "a fragment in a different style
//! closes the open block" and the answer to "what cell is a block in style X"
//! are the same in both. The plain front end writes the bytes as the stream
//! moves; the TUI keeps a [`Cell::Reasoning`], [`Cell::Content`] or
//! [`Cell::ToolOutput`] of the running block in its transcript. Either way,
//! the same Stream underneath produces the same answer to `close`.

use super::{Cell, Style};

/// One block being streamed, with the text accumulated so far.
///
/// The pair is `Option` because the stream is closed at the end of a turn, and
/// because a turn that has not yet started has nothing open.
#[derive(Debug, Default, Clone)]
pub struct Stream {
    open: Option<(Style, String)>,
}

impl Stream {
    /// An empty stream: no block open. The default.
    pub const fn new() -> Self {
        Self { open: None }
    }

    /// The block currently streaming, if any: the style and a borrowed view of
    /// the text accumulated so far. `None` is a stream that is closed or has
    /// not yet opened one.
    pub fn current(&self) -> Option<(Style, &str)> {
        self.open.as_ref().map(|(s, t)| (*s, t.as_str()))
    }

    /// Append `text` to the block in `style`, opening one if the open block
    /// is in a different style. The return value is the [`Cell`] the previous
    /// block became when it had to be closed, or `None` when no block had to
    /// close (a fresh stream or the same style continuing).
    ///
    /// Empty `text` is allowed and is what the plain front end uses to open a
    /// block without writing any bytes through it yet: the call still closes a
    /// previous block in a different style, which is what keeps the open /
    /// close alternation correct.
    pub fn append(&mut self, style: Style, text: &str) -> Option<Cell> {
        if self.open.as_ref().is_some_and(|(open, _)| *open == style) {
            self.open.as_mut().expect("open").1.push_str(text);
            return None;
        }
        let closed = self.close();
        self.open = Some((style, String::from(text)));
        closed
    }

    /// Close whatever is open, returning the cell it became. `None` is a
    /// stream that had no open block to close.
    pub fn close(&mut self) -> Option<Cell> {
        self.open
            .take()
            .map(|(style, text)| style.stream_cell(text))
    }
}

impl Style {
    /// The cell a stream in this style becomes when it closes.
    ///
    /// Two of the six styles are the ones a stream can be in: reasoning
    /// becomes [`Cell::Reasoning`], every other style -- which only the body
    /// text is -- becomes [`Cell::Content`]. The other four are not
    /// streaming styles and would be a bug to call this on; they fall through
    /// to `Content` so the non-streaming callers that reach this through a
    /// `Style` get a defined answer rather than a panic.
    ///
    /// Tool output no longer flows through the stream: it goes to the
    /// children of the running [`Cell::Step`] instead. A dim fragment in
    /// the stream is no longer a reachable state and falls through to
    /// `Content`; if it ever shows up, it would be a bug worth surfacing.
    pub fn stream_cell(self, text: String) -> Cell {
        match self {
            Style::Reasoning => Cell::Reasoning(text),
            _ => Cell::Content(text),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The contract that holds both front ends to the same stream shape:
    /// same-style fragments are one block, a different style closes the
    /// previous one and opens the next, and an empty stream closes to nothing.
    #[test]
    fn same_style_fragments_become_one_block() {
        let mut s = Stream::new();
        assert_eq!(s.append(Style::Reasoning, "a"), None);
        assert_eq!(s.append(Style::Reasoning, "b"), None);
        assert_eq!(s.append(Style::Reasoning, "c"), None);
        assert_eq!(s.current(), Some((Style::Reasoning, "abc")));
        // A close returns the cell the open block became.
        assert_eq!(s.close(), Some(Cell::Reasoning("abc".into())));
        assert_eq!(s.current(), None);
        assert_eq!(s.close(), None, "nothing left to close");
    }

    /// A different-style fragment closes the previous block and returns the
    /// cell it became, which is what both front ends push into the transcript
    /// when the running turn has to file a finished block before opening the
    /// next one.
    #[test]
    fn a_different_style_closes_the_previous_block() {
        let mut s = Stream::new();
        assert_eq!(s.append(Style::Reasoning, "hmm"), None);
        // Body text is its own cell: the reasoning block closes as it.
        assert_eq!(
            s.append(Style::Plain, "answer"),
            Some(Cell::Reasoning("hmm".into()))
        );
        assert_eq!(s.current(), Some((Style::Plain, "answer")));
    }

    /// The plain front end's `open_block` opens a style without writing any
    /// text through it: the Stream has to honour that and not invent a
    /// previous block to close.
    #[test]
    fn opening_a_style_with_empty_text_is_a_no_op_when_already_open() {
        let mut s = Stream::new();
        assert_eq!(s.append(Style::Reasoning, ""), None);
        // Same call a second time is still no-op: the open block did not
        // change, so no previous block has to close.
        assert_eq!(s.append(Style::Reasoning, ""), None);
        assert_eq!(s.current(), Some((Style::Reasoning, "")));
        assert_eq!(s.close(), Some(Cell::Reasoning(String::new())));
    }
}

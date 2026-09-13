//! The shape of a backend that renders cells.
//!
//! One trait, two implementations: the plain front end writes SGR bytes to a
//! `Write`, the TUI front end collects spans to feed through
//! [`crate::ui::paint::wrapped_lines`]. The cell does not know which is behind
//! it, and never has to: the gap rule, the gutter, the line end are the cell's
//! business, and the bytes or lines that come out are the sink's.
//!
//! A sink that needs its own state carries it on the impl block; the trait has
//! no shared state and the cell holds none of its own. The order the cell
//! calls the methods in is fixed -- begin, gap, span..., line_end, end -- so a
//! sink that wants to do its own wrap can collect spans in `span` and do the
//! work in `end_cell`, and a sink that writes bytes as they arrive can write
//! them in `span` and close the style in `end_cell`.

use super::{Cell, Gutter, Style};

/// A backend a cell renders itself through.
pub trait CellSink {
    /// Begin the cell. `gutter` is the cell's gutter (if any); a sink that
    /// wants the continuation columns reads them from here, and a sink that
    /// has no use for them ignores the argument.
    fn begin_cell(&mut self, gutter: Option<&Gutter>);

    /// A blank line between cells. The plain sink writes `"\n"`; the TUI's
    /// layout decides gap insertion between cells on its own and ignores this.
    fn gap(&mut self);

    /// One styled span. A `'\n'` in `text` is a line break inside the cell:
    /// the sink uses the gutter's continuation columns for the next line.
    /// The plain sink writes the bytes; the TUI sink collects them.
    fn span(&mut self, style: Style, text: &str);

    /// The cell's trailing line break. The plain sink writes `"\n"`; the TUI
    /// builds a `Vec<Line>` and has no trailing newline to write.
    fn line_end(&mut self);

    /// End the cell. The plain sink closes any open style here, so the next
    /// cell starts unstyled; the TUI sink does no work of its own.
    fn end_cell(&mut self);
}

/// The finish information a cell rendering returns to its caller: the cell's
/// text-block status (the spacing rule keys on it) and whether it ended its
/// line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellFinish {
    pub is_text_block: bool,
    pub ends_line: bool,
}

impl Cell {
    /// Render this cell through `sink`: the gap, the gutter's head, the
    /// styled spans, and the line end, in the order they happen on screen.
    ///
    /// `prev_block` is whether the cell the caller wrote last was a text
    /// block (the spacing rule keys on it). The cell decides whether to gap
    /// from this and from its own shape; the sink does what `gap` says.
    ///
    /// Returns the [`CellFinish`] the caller needs to keep the spacing rule
    /// running: whether the cell it just rendered was a text block, and
    /// whether it ended its line.
    pub fn render(&self, prev_block: bool, sink: &mut dyn CellSink) -> CellFinish {
        sink.begin_cell(self.gutter().as_ref());
        if self.gap_after(prev_block) {
            sink.gap();
        }
        if let Some(gutter) = self.gutter() {
            sink.span(gutter.style, gutter.head);
        }
        for s in self.spans() {
            sink.span(s.style, &s.text);
        }
        let finish = CellFinish {
            is_text_block: self.is_text_block(),
            ends_line: self.ends_line(),
        };
        // `end_cell` first, so a sink that closes a style does it before the
        // line end is written: the newline belongs to the line the cell
        // occupied, not the line after it, and a style that was open across
        // the newline is one that would colour the blank line below.
        sink.end_cell();
        if finish.ends_line {
            sink.line_end();
        }
        finish
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Every event a sink receives, in the order it receives them. The
    /// `render` order is part of the contract: a sink that writes bytes
    /// depends on `end_cell` arriving before `line_end`, and a sink that
    /// collects spans depends on the gutter's head arriving before its own
    /// spans. Tests below pin both.
    #[derive(Debug, PartialEq, Eq)]
    enum Event {
        Begin(Option<&'static str>),
        Gap,
        Span { style: Style, text: String },
        LineEnd,
        End,
    }

    /// A sink that records every call into a `RefCell<Vec<Event>>`, so a
    /// test can inspect the order without writing a real backend.
    struct Recording(RefCell<Vec<Event>>);

    impl Recording {
        fn new() -> Self {
            Self(RefCell::new(Vec::new()))
        }
        fn events(&self) -> Vec<Event> {
            self.0.borrow().iter().map(clone_event).collect()
        }
    }

    fn clone_event(e: &Event) -> Event {
        match e {
            Event::Begin(r) => Event::Begin(*r),
            Event::Gap => Event::Gap,
            Event::Span { style, text } => Event::Span {
                style: *style,
                text: text.clone(),
            },
            Event::LineEnd => Event::LineEnd,
            Event::End => Event::End,
        }
    }

    impl CellSink for Recording {
        fn begin_cell(&mut self, gutter: Option<&Gutter>) {
            self.0
                .borrow_mut()
                .push(Event::Begin(gutter.map(|g| g.rest)));
        }
        fn gap(&mut self) {
            self.0.borrow_mut().push(Event::Gap);
        }
        fn span(&mut self, style: Style, text: &str) {
            self.0.borrow_mut().push(Event::Span {
                style,
                text: text.to_owned(),
            });
        }
        fn line_end(&mut self) {
            self.0.borrow_mut().push(Event::LineEnd);
        }
        fn end_cell(&mut self) {
            self.0.borrow_mut().push(Event::End);
        }
    }

    /// A guttered cell's render: begin, no gap (no rule says one), the gutter
    /// head as the first span, then the cell's own spans, then end, then
    /// line end.
    #[test]
    fn a_guttered_cell_calls_in_the_order_the_screen_sees_them() {
        let cell = Cell::Notice("hi".into());
        let mut sink = Recording::new();
        let finish = cell.render(false, &mut sink);

        assert_eq!(
            sink.events(),
            vec![
                Event::Begin(Some("  ")),
                Event::Span {
                    style: Style::Dim,
                    text: "* ".into()
                },
                Event::Span {
                    style: Style::Dim,
                    text: "hi".into()
                },
                Event::End,
                Event::LineEnd,
            ]
        );
        assert!(!finish.is_text_block);
        assert!(finish.ends_line);
    }

    /// The answer is the one cell without a gutter: render does not emit a
    /// gutter head span, and still emits end + line end.
    #[test]
    fn a_cell_without_a_gutter_skips_the_head_span() {
        let cell = Cell::Content("answer".into());
        let mut sink = Recording::new();
        let finish = cell.render(false, &mut sink);

        assert_eq!(
            sink.events(),
            vec![
                Event::Begin(None),
                Event::Span {
                    style: Style::Plain,
                    text: "answer".into()
                },
                Event::End,
                Event::LineEnd,
            ]
        );
        assert!(finish.is_text_block);
        assert!(finish.ends_line);
    }

    /// A step opens tight against a text block: the gap is the cell's
    /// own gutter, not a blank line the spacing rule inserts.
    #[test]
    fn a_step_after_a_block_is_tight() {
        let cell = Cell::from_tool_call("Bash", "{}");
        let mut sink = Recording::new();
        cell.render(true, &mut sink);

        assert!(
            !sink.events().iter().any(|e| matches!(e, Event::Gap)),
            "a step's header is its own line, no padding from the spacing rule"
        );
    }

    /// The approval gate is the one cell that leaves its line open: end is
    /// still called, line end is not.
    #[test]
    fn the_approval_cell_omits_the_line_end() {
        let cell = Cell::approval("Bash", "{}");
        let mut sink = Recording::new();
        let finish = cell.render(false, &mut sink);

        assert!(!sink.events().contains(&Event::LineEnd));
        assert!(sink.events().contains(&Event::End));
        assert!(!finish.ends_line);
    }

    /// `end_cell` arrives before `line_end`: a sink that closes a style in
    /// `end_cell` does it before the newline, so the newline is unstyled.
    #[test]
    fn end_cell_comes_before_line_end() {
        let cell = Cell::Notice("hi".into());
        let mut sink = Recording::new();
        cell.render(false, &mut sink);

        let events = sink.events();
        let end = events.iter().position(|e| matches!(e, Event::End));
        let line_end = events.iter().position(|e| matches!(e, Event::LineEnd));
        assert!(end.is_some() && line_end.is_some() && end.unwrap() < line_end.unwrap());
    }
}

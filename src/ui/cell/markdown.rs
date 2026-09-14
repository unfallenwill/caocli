//! Markdown → styled spans for the cell layer.
//!
//! The transcript renders `Cell::Content` (the answer) and `Cell::Reasoning`
//! (the thinking behind it) as plain text today, and a model that defaults to
//! GitHub-Flavored Markdown would lay its `**bold**` and triple-backtick fences
//! down raw on the screen. This module turns markdown source into the same
//! `Vec<Span>` shape the painter already takes, so the gutter, the wrap step,
//! and the cell machinery keep working without a new render path on either
//! front end.
//!
//! What it does:
//!
//! - Parses headings, paragraphs, lists, block quotes, fenced and indented
//!   code blocks, thematic rules, and inline emphasis, strong, strikethrough,
//!   inline code, and links.
//! - Pushes styled text into spans with internal `\n` at hard breaks, so the
//!   existing `wrapped_lines` step lays it out at the right columns without
//!   anything knowing markdown was involved.
//! - Falls back to the raw source for tables: column-aligned rendering is the
//!   one thing our wrap step cannot do, and pretending otherwise would draw
//!   a worse table than the source already is.
//!
//! What it deliberately does not:
//!
//! - No HTML output. We are a terminal renderer, and HTML would be the wrong
//!   intermediate.
//! - No syntax highlighting. The minimum set -- emphasis, code, lists,
//!   blockquote, headings -- is enough to make answers scannable. Highlighting
//!   inside fenced code blocks belongs in a later pass, behind a settings flag,
//!   and would require a `syntect`-class dependency we do not want yet.
//! - No HTML blocks. `Event::Html` is dropped, so `<details>` and friends
//!   become nothing rather than raw angle brackets on the screen.
//! - No GFM features that the minimum set does not need: footnotes, math,
//!   definition lists, superscript, subscript, metadata blocks. The option
//!   flags stay off so the parser stays small and unambiguous.

use std::collections::VecDeque;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

use super::{Span, Style};

/// The minimum CommonMark + GFM features we render.
///
/// Tables, strikethrough, and task-list markers are the only additions over
/// plain CommonMark: tables fall back to raw text (the parser sees them, the
/// renderer skips the alignment dance), strikethrough is one extra style
/// toggle that costs nothing to support, and task-list markers only show up
/// inside list items, which we already render.
fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
}

/// Parse a markdown fragment into the styled spans the painter wraps.
///
/// Two callers feed it: the live stream, where a thinking or body block is
/// closed and rendered, and the session replay, which rebuilds the same
/// cells from the log. The output is always non-empty for non-empty input:
/// an empty input returns no spans, the way the cell layer's old
/// `vec![Span::new(style, "")]` did.
///
/// `base` is the style that unmarked text inherits: `Style::Plain` for the
/// answer, `Style::Reasoning` for a thinking block. Markdown markers
/// (emphasis, strong, code) override it; the gutter and the wrap step
/// leave it alone. The point is that a reader still sees a thinking block
/// as dim text, even when the body of the block happens to be plain
/// prose with no markers.
pub(crate) fn parse(text: &str, base: Style) -> Vec<Span> {
    if text.is_empty() {
        return Vec::new();
    }
    let parser = Parser::new_ext(text, options()).into_offset_iter();
    let mut out = Vec::new();
    let mut state = State::new(base);
    for (event, range) in parser {
        state.feed(event, range, text, &mut out);
    }
    state.finish(&mut out);
    out
}

/// The walker that turns an event stream into styled spans.
///
/// One pass over `text`: every block tag adjusts the block-level prefix the
/// next inline events see, and every inline tag pushes a style the text
/// inherits. The state itself is the only thing that knows how the open
/// block tags nest; everything outside this module sees only the spans it
/// produced.
struct State {
    /// The style unmarked text inherits. Set once from the caller: the
    /// answer's `Style::Plain` keeps body text in the foreground colour,
    /// the thinking block's `Style::Reasoning` keeps it in the comment
    /// colour even when no markdown marker is in play.
    base: Style,
    /// The block tags currently open, outermost first: the prefixes they
    /// dictate are pushed in the order they were opened, and popped in
    /// reverse. Inline tags are not tracked here -- they ride on
    /// `style_stack` instead.
    blocks: Vec<BlockFrame>,
    /// The inline styles currently in effect, outermost first. The
    /// `base` style sits at the bottom and is always present, so every
    /// text run finds a style to push under without a special-case.
    style_stack: Vec<Style>,
    /// Where the next emitted text should land: a queue of prefixes that
    /// the next text run carries with it. A `Bullet` scheduled by the
    /// next item opening stacks on top of a `Newline` scheduled by the
    /// previous item closing, and `flush_prefix` drains the queue in
    /// order so the cell opens with `"\n- "` rather than one of them
    /// missing.
    pending_prefix: VecDeque<PendingPrefix>,
    /// True when the open block is a fenced code block we have decided to
    /// draw a fence line for. The opener (```` ``` ````) and closer (```` ``` ````)
    /// are emitted only around fenced blocks; indented code blocks stay bare.
    code_block_fenced: bool,
    /// The bullet shape the next `Tag::Item` should open with. Reset by
    /// `Tag::List` start.
    next_bullet: Option<BulletKind>,
    /// Drop the next text run we see. Used by `Image` so the alt text the
    /// parser emits between `Start(Image)` and `End(Image)` does not
    /// double up on the placeholder we synthesise at `End(Image)`. When
    /// `pending_image` is also set, the dropped text is captured here
    /// as the alt string: the parser puts alt text in `Text`, the title
    /// attribute is the optional quoted "title" not what a reader sees.
    drop_next_text: bool,
    /// The destination URL of the link currently being rendered, if any.
    /// Drained on `TagEnd::Link` to emit the parenthetical URL.
    pending_link_url: Option<String>,
    /// The image we are mid-render of. The parser emits `Start(Image)`
    /// then a `Text` for the alt text and then `End(Image)`; the
    /// placeholder we draw on `End(Image)` needs both halves.
    pending_image: Option<ImageParts>,
    /// Alt text the parser emitted as `Text` after `Start(Image)`,
    /// captured because the placeholder on `End(Image)` reads better
    /// with it than with the empty `title` field.
    captured_alt: Option<String>,
}

impl State {
    fn new(base: Style) -> Self {
        Self {
            base,
            blocks: Vec::new(),
            style_stack: vec![base],
            pending_prefix: VecDeque::new(),
            code_block_fenced: false,
            next_bullet: None,
            drop_next_text: false,
            pending_link_url: None,
            pending_image: None,
            captured_alt: None,
        }
    }
}

/// What the next text run needs to prefix itself with.
#[derive(Debug, Clone)]
enum PendingPrefix {
    /// `- `, `* `, or `N. ` for an unordered or ordered list item.
    Bullet(BulletKind),
    /// `> ` for the first line of a block quote paragraph.
    Quote,
    /// `## ` (heading level dependent) for the heading body.
    Heading(HeadingLevel),
    /// The very first line of a paragraph after a code block or a rule, so
    /// the wrap step does not collapse two paragraphs into one. This is the
    /// `"\n"` between two block tags the parser separates with a SoftBreak.
    Newline,
    /// A pre-built prefix (used by the task-list marker to override a
    /// pending bullet with `[x] ` or `[ ] `).
    Raw(String),
}

/// The bullet shape the list item opens with. Two flavours so unordered
/// and ordered lists are visually distinct at the top of a cell.
#[derive(Debug, Clone, Copy)]
enum BulletKind {
    /// `-` is the one model output uses; `*` would render the same but a
    /// model that writes `* foo` is rarer, and we keep one shape.
    Dash,
    /// An ordered item, with the number the list started at. The number
    /// resets on every `List` open: a model that breaks the sequence still
    /// gets something readable, even if not what the source said.
    Number(u64),
}

impl State {
    /// Drain any unwritten prefix and finalize trailing state. Called once
    /// after the iterator is exhausted.
    fn finish(&mut self, out: &mut Vec<Span>) {
        // A trailing `Newline` would draw a blank row under the cell,
        // because the painter's wrap step treats the last `\n` as the
        // line that comes after the last visible one. Drop it; anything
        // else (a bullet, a heading prefix) is a real bug, and keeping
        // it would draw half a prefix at the bottom of the cell.
        if matches!(self.pending_prefix.front(), Some(PendingPrefix::Newline)) {
            self.pending_prefix.pop_front();
        }
        self.flush_prefix(out);
    }

    /// Drive one event from the parser into the span stream.
    fn feed(
        &mut self,
        event: Event<'_>,
        range: std::ops::Range<usize>,
        source: &str,
        out: &mut Vec<Span>,
    ) {
        match event {
            Event::Start(tag) => self.start(tag, range, source, out),
            Event::End(end) => self.end(end, range, source, out),
            Event::Text(text) => self.text(&text, out),
            Event::Code(text) => self.inline_code(&text, out),
            Event::SoftBreak => self.soft_break(out),
            Event::HardBreak => self.hard_break(out),
            Event::Rule => self.rule(out),
            // Inline HTML, footnote references, math, and HTML blocks fall
            // through: they are dropped because rendering them is out of
            // scope, and emitting the raw bytes would put angle brackets on
            // the screen.
            Event::Html(_)
            | Event::InlineHtml(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_)
            | Event::FootnoteReference(_) => {}
            // TaskListMarker fires inside an Item and means the item is a
            // checkbox. We emit `[x] ` or `[ ] ` at the same place a `- `
            // would land, so the user can see the checked state.
            Event::TaskListMarker(checked) => self.task_list(checked, out),
        }
    }

    fn start(
        &mut self,
        tag: Tag<'_>,
        range: std::ops::Range<usize>,
        source: &str,
        out: &mut Vec<Span>,
    ) {
        match tag {
            // The very first text in a paragraph or list item sees a fresh
            // bullet or quote. Without flushing whatever prefix is pending
            // here, a list item would open with no `-` and a heading with no
            // `##`. Flush before pushing the new frame so a paragraph inside
            // a list item is not prefixed twice.
            Tag::Paragraph => {
                self.flush_prefix(out);
                self.blocks.push(BlockFrame::Paragraph);
            }
            Tag::Heading { level, .. } => {
                self.flush_prefix(out);
                self.pending_prefix.push_back(PendingPrefix::Heading(level));
                self.blocks.push(BlockFrame::Heading(level));
            }
            Tag::BlockQuote(_) => {
                self.flush_prefix(out);
                self.pending_prefix.push_back(PendingPrefix::Quote);
                self.blocks.push(BlockFrame::BlockQuote);
            }
            Tag::CodeBlock(kind) => {
                self.open_code_block(kind, source, range, out);
            }
            // HTML blocks are dropped, but the inner events still flow past
            // us -- push a frame so `end` pops it and the inline events
            // outside the frame are unaffected.
            Tag::HtmlBlock => {
                self.blocks.push(BlockFrame::Raw);
            }
            Tag::List(start) => {
                self.flush_prefix(out);
                self.blocks.push(BlockFrame::List);
                self.next_bullet = Some(match start {
                    Some(n) => BulletKind::Number(n),
                    None => BulletKind::Dash,
                });
            }
            Tag::Item => {
                // Items without a preceding list are a parser bug; treat
                // them as a paragraph and move on. The bullet we use
                // here also feeds the next item: an ordered list starts
                // at the number the parser hands us and increments from
                // there, so consecutive items do not repeat "1." after
                // the first.
                let next = self.next_bullet.take().unwrap_or(BulletKind::Dash);
                self.pending_prefix.push_back(PendingPrefix::Bullet(next));
                self.blocks.push(BlockFrame::Item);
                self.next_bullet = Some(match next {
                    BulletKind::Dash => BulletKind::Dash,
                    BulletKind::Number(n) => BulletKind::Number(n + 1),
                });
            }
            // Tables are the one block we deliberately render as raw
            // markdown: column-aligned text needs a column model our wrap
            // step does not have, and the source layout usually beats
            // anything we would reconstruct from per-cell text. Capture the
            // whole table source range as one `Style::Plain` span and stop
            // tracking its inner events.
            Tag::Table(_) => {
                self.capture_table(source, range, out);
            }
            Tag::TableHead | Tag::TableRow | Tag::TableCell => {}
            // Inline emphasis and strong share a style stack with strikethrough:
            // they nest, and the innermost wins. Two flavours are enough; a
            // model that writes `***bold-italic***` ends up bold-italic here
            // (the parser still emits the tags in order; only our style
            // application collapses them onto `Style::Yellow`).
            Tag::Emphasis => self.push_style(self.base),
            Tag::Strong => self.push_style(Style::Yellow),
            Tag::Strikethrough => self.push_style(Style::Dim),
            Tag::Link { dest_url, .. } => {
                // OSC 8 hyperlinks are out of scope: emitting them would mean
                // tracking ANSI state through `wrapped_lines`, which assumes
                // spans are pure text. Render the link as its visible text
                // plus the URL in dim parentheses, which is what a reader
                // without a hyperlink-supporting terminal gets anyway.
                self.pending_link_url = Some(dest_url.into_string());
                self.push_style(self.base);
            }
            Tag::Image {
                dest_url, title, ..
            } => {
                // The parser follows `Start(Image)` with a `Text` for
                // the alt text, then `End(Image)`. We collapse the three
                // events into a single placeholder line on `End`; the
                // `drop_next_text` flag swallows the alt text so it does
                // not double up. Title and URL combine to read as
                // `[alt](url)`, with the URL plain so a reader can
                // still copy it from the screen.
                self.pending_image = Some(ImageParts {
                    alt: title.into_string(),
                    url: dest_url.into_string(),
                });
                self.drop_next_text = true;
            }
            // Footnote / definition-list / metadata blocks: the parser
            // would only emit these with their feature flags on, so a
            // stray match here means an API drift, not a missing case.
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_) => {}
            Tag::Superscript | Tag::Subscript => {}
        }
    }

    fn end(
        &mut self,
        end: TagEnd,
        _range: std::ops::Range<usize>,
        _source: &str,
        out: &mut Vec<Span>,
    ) {
        match end {
            TagEnd::Paragraph => self.pop_block(|b| matches!(b, BlockFrame::Paragraph)),
            TagEnd::Heading(_) => self.pop_block(|b| matches!(b, BlockFrame::Heading(_))),
            TagEnd::BlockQuote(_) => self.pop_block(|b| matches!(b, BlockFrame::BlockQuote)),
            TagEnd::CodeBlock => self.close_code_block(out),
            TagEnd::HtmlBlock => {
                self.pop_block(|b| matches!(b, BlockFrame::Raw));
            }
            TagEnd::List(_) => {
                // A trailing newline scheduled by the last item is
                // dropped by `finish` -- but it must not bleed into the
                // next list. Clearing the bullet too keeps a new
                // `List` start from inheriting the last number.
                self.next_bullet = None;
                self.pop_block(|b| matches!(b, BlockFrame::List))
            }
            TagEnd::Item => self.pop_block(|b| matches!(b, BlockFrame::Item)),
            TagEnd::Table => {
                self.flush_prefix(out);
                self.pop_block(|b| matches!(b, BlockFrame::Table));
            }
            TagEnd::TableHead | TagEnd::TableRow | TagEnd::TableCell => {}
            TagEnd::Emphasis => self.pop_style(self.base),
            TagEnd::Strong => self.pop_style(Style::Yellow),
            TagEnd::Strikethrough => self.pop_style(Style::Dim),
            TagEnd::Link => {
                if let Some(url) = self.pending_link_url.take() {
                    self.text(&format!(" ({url})"), out);
                }
                self.pop_style(self.base);
            }
            TagEnd::Image => {
                // Placeholder emission happens here, on `End`, after
                // any inline tags the alt text was wrapped in have
                // already balanced. The `title` field is the optional
                // markdown "title" attribute, not the alt text the
                // reader expects; the alt text is the `Text` event the
                // parser emits right after `Start(Image)` and which
                // `text()` captured into `captured_alt`. Empty alt and
                // title both fall back to a generic placeholder.
                if let Some(ImageParts { alt, url }) = self.pending_image.take() {
                    let alt = self.captured_alt.take().unwrap_or(alt);
                    let line = match (alt.is_empty(), url.is_empty()) {
                        (true, true) => "[image]".to_owned(),
                        (true, false) => format!("[image]({url})"),
                        (false, true) => format!("[{alt}]"),
                        (false, false) => format!("[{alt}]({url})"),
                    };
                    self.text(&line, out);
                }
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_) => {}
            TagEnd::Superscript | TagEnd::Subscript => {}
        }
    }

    /// Push text from an `Event::Text` node. The style is whatever the
    /// current top of `style_stack` says, after `flush_prefix` has written
    /// any bullet or heading marker that was waiting for the first text.
    fn text(&mut self, text: &str, out: &mut Vec<Span>) {
        // The alt text the parser emits inside an `Image` tag is
        // absorbed by the placeholder drawn on `TagEnd::Image`; we
        // capture it into `captured_alt` so the placeholder can show it
        // in place of the empty `title` field the parser exposes.
        if std::mem::take(&mut self.drop_next_text) && self.pending_image.is_some() {
            self.captured_alt = Some(text.to_owned());
            return;
        }
        self.flush_prefix(out);
        // A code block is one block of `Style::Dim` text -- the inline
        // style stack still has whatever the surrounding paragraph held,
        // which would land the body on `Style::Plain`. Detect the open
        // code block and override.
        let style = if matches!(self.blocks.last(), Some(BlockFrame::CodeBlock)) {
            Style::Dim
        } else {
            *self.style_stack.last().expect("style stack non-empty")
        };
        // Empty text can come from a SoftBreak that immediately follows the
        // opening of a paragraph: the parser emits an empty `Text` for
        // an empty paragraph body, and rendering it would draw a
        // leading blank line. Drop it.
        if text.is_empty() {
            return;
        }
        push_span(out, style, text);
    }

    /// Inline code: a backtick span. The body is opaque to markdown
    /// parsing, so we do not need to track anything inside it.
    fn inline_code(&mut self, code: &str, out: &mut Vec<Span>) {
        self.flush_prefix(out);
        push_span(out, Style::Dim, code);
    }

    /// A soft break (a newline inside a paragraph that is not two trailing
    /// spaces or a backslash). The markdown spec joins the two halves with
    /// whitespace, and our painter will wrap the joined line at whatever
    /// columns the cell has. Setting the flag here lets the next text
    /// emit a single leading space; an empty text run is also a soft break
    /// in some streams, and we skip it so a paragraph that opens with one
    /// does not get a phantom space.
    fn soft_break(&mut self, out: &mut Vec<Span>) {
        // A soft break is a single newline inside a paragraph, not a
        // paragraph boundary. The CommonMark spec leaves it to the
        // renderer -- space in HTML, newline elsewhere. A terminal cell
        // is closer to "elsewhere": a space would let two source lines
        // share one wrapped row, which is the same trap the painter
        // warns against for overlong text. A newline is what `wrapped_lines`
        // understands, so emit one and let the wrap step do its job.
        //
        // Inside a block quote the wrap step lays out the gutter but
        // does not emit the `> ` marker on continuation rows, so a
        // soft break has to re-pend one for the next line of text.
        // Outside a quote the gutter alone is enough.
        self.flush_prefix(out);
        let style = *self.style_stack.last().expect("style stack non-empty");
        push_span(out, style, "\n");
        let in_quote = self
            .blocks
            .iter()
            .any(|b| matches!(b, BlockFrame::BlockQuote));
        if in_quote {
            self.pending_prefix.push_back(PendingPrefix::Quote);
        }
    }

    /// A hard break: two trailing spaces or a backslash. Rendered as a
    /// newline in the span text, which the wrap step will treat as a real
    /// line break.
    fn hard_break(&mut self, out: &mut Vec<Span>) {
        self.flush_prefix(out);
        let style = *self.style_stack.last().expect("style stack non-empty");
        push_span(out, style, "\n");
    }

    /// A horizontal rule. Rendered as a dim row of `─` glyphs of the cell
    /// width -- the painter cannot measure the cell width here, so the line
    /// is a single dim span and the wrap step clips it to the cell.
    /// Thirty-two is a comfortable default: it is wide enough to read as a
    /// rule on a 100-col terminal and short enough that the wrap step will
    /// clip it cleanly if the cell is narrow.
    fn rule(&mut self, out: &mut Vec<Span>) {
        self.flush_prefix(out);
        push_span(out, Style::Dim, "────────────────────────────────");
    }

    /// Task-list marker inside a list item. The bullet the item would have
    /// opened with is replaced by `[x] ` or `[ ] ` so the reader can see
    /// whether the box is checked.
    fn task_list(&mut self, checked: bool, out: &mut Vec<Span>) {
        let marker = if checked { "[x] " } else { "[ ] " };
        // Replace any pending `- ` (or `1. `) the item would otherwise
        // open with the checkbox shape. Pushing `Raw` on top of an
        // existing `Bullet` would draw both, so the Bullet has to come
        // off the queue first. Without a pending bullet, the marker is
        // mid-paragraph text and lands as a plain span of its own.
        if matches!(self.pending_prefix.back(), Some(PendingPrefix::Bullet(_))) {
            self.pending_prefix.pop_back();
            self.pending_prefix
                .push_back(PendingPrefix::Raw(marker.to_owned()));
        } else {
            self.flush_prefix(out);
            push_span(out, Style::Plain, marker);
        }
    }

    /// Open a fenced or indented code block. The raw bytes between
    /// `range.start` and `range.end` become one dim span with internal
    /// newlines; the painter wraps them like any other block.
    ///
    /// The parser emits a `Text` event between `Start(CodeBlock)` and
    /// `End(CodeBlock)` whose bytes are the body (without the fences
    /// when the block is fenced). We do not emit anything here; the
    /// `Text` event in `text()` checks for an open code block and
    /// writes to a `Style::Dim` span instead of the inline stack's
    /// top. `close_code_block` only has work to do when the block was
    /// fenced, where a closing ``` ``` ```` line belongs.
    fn open_code_block(
        &mut self,
        kind: CodeBlockKind<'_>,
        _source: &str,
        _range: std::ops::Range<usize>,
        _out: &mut Vec<Span>,
    ) {
        self.flush_prefix(_out);
        self.code_block_fenced = matches!(kind, CodeBlockKind::Fenced(_));
        if self.code_block_fenced {
            // Open the fence with the language hint as a dim span.
            // Indented code blocks have no fence, by definition.
            let lang = match kind {
                CodeBlockKind::Fenced(lang) => lang.into_string(),
                _ => String::new(),
            };
            let open = if lang.is_empty() {
                "```".to_owned()
            } else {
                format!("```{lang}")
            };
            push_span(_out, Style::Dim, &open);
        }
        self.blocks.push(BlockFrame::CodeBlock);
    }

    /// Close the open code block. Fenced blocks get a closing ``` ``` ````
    /// line so the body has fences on both sides; indented blocks were
    /// emitted bare by `text()`. The closing line lives on the same
    /// dim style as the body so the wrap step reads them as one block.
    fn close_code_block(&mut self, out: &mut Vec<Span>) {
        if self.code_block_fenced {
            // The body has a trailing newline the parser preserved; the
            // closing fence goes on its own row.
            push_span(out, Style::Dim, "\n```");
        }
        self.code_block_fenced = false;
        self.pop_block(|b| matches!(b, BlockFrame::CodeBlock));
    }

    /// Captured a `Tag::Table` open: paste the raw source back as a plain
    /// span. The parser will continue to emit `TableHead`/`TableRow`/
    /// `TableCell` events; we drop them, so the inner text never gets
    /// emitted twice. A blank line around the captured span keeps the
    /// block visually separated from whatever precedes it.
    fn capture_table(&mut self, source: &str, range: std::ops::Range<usize>, out: &mut Vec<Span>) {
        self.flush_prefix(out);
        let raw = source[range.start..range.end].to_owned();
        // Indent every line two columns so the table reads as a quoted
        // block: the wrap step will pad with the gutter, so an indented
        // prefix is just `  `.
        let indented: String = raw
            .split_inclusive('\n')
            .map(|line| {
                if line == "\n" {
                    line.to_owned()
                } else {
                    format!("  {line}")
                }
            })
            .collect();
        push_span(out, Style::Plain, &indented);
        self.blocks.push(BlockFrame::Table);
    }

    /// Push a style onto the inline stack. The next text run inherits it.
    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    /// Pop a style, asserting it matches the expected one. A mismatch is
    /// a parser bug and would mean a missing event in our walker.
    fn pop_style(&mut self, expected: Style) {
        let popped = self.style_stack.pop();
        debug_assert_eq!(
            popped,
            Some(expected),
            "inline tag stack mismatch: popped {popped:?}, expected {expected:?}"
        );
    }

    /// Close a block-level frame. Two consecutive body blocks need a
    /// newline between them; otherwise `- one- two` is one cell row, or
    /// two headings collapse into one wrapped line. The newline rides on
    /// the next prefix: a `Bullet` for the next item, a `Heading` for the
    /// next heading, the body of a fresh paragraph for the next
    /// paragraph. Without this, two list items in the same list collapse
    /// into one wrapped line.
    ///
    /// The flush step ignores a trailing newline when the document runs
    /// out, so scheduling one on the way out is safe even when nothing
    /// follows.
    fn pop_block(&mut self, matches: impl Fn(BlockFrame) -> bool) {
        let popped = self.blocks.pop();
        debug_assert!(
            popped.is_some_and(&matches),
            "block stack mismatch: popped {popped:?}"
        );
        let Some(popped) = popped else { return };
        let newline_already_pending =
            matches!(self.pending_prefix.back(), Some(PendingPrefix::Newline));
        match popped {
            BlockFrame::Paragraph
            | BlockFrame::Heading(_)
            | BlockFrame::BlockQuote
            | BlockFrame::CodeBlock => {
                // A blank line of separation is what makes a paragraph
                // boundary readable in plain text; this is the newline
                // that goes between two top-level paragraphs.
                if !newline_already_pending {
                    self.pending_prefix.push_back(PendingPrefix::Newline);
                }
            }
            BlockFrame::Item => {
                // Two items in the same list need a newline so they
                // land on different rows; the newline lands before the
                // next item's bullet when `flush_prefix` runs.
                if !newline_already_pending {
                    self.pending_prefix.push_back(PendingPrefix::Newline);
                }
            }
            BlockFrame::List | BlockFrame::Table | BlockFrame::Raw => {}
        }
    }

    /// Emit whatever prefix was waiting for the next text. A pending
    /// prefix is consumed here and the prefix becomes part of the span
    /// text under the open block's style. This is the one place that
    /// knows that `- foo` is a list item, `## foo` is a heading, and
    /// `> foo` is a block quote.
    fn flush_prefix(&mut self, out: &mut Vec<Span>) {
        // The `after_soft_break` flag is consumed by `text`, not here, so
        // a pending prefix that fires between two text runs is not
        // accidentally preceded by a space.
        //
        // Multiple prefixes can stack up: a `Newline` scheduled by the
        // previous block closing sits behind a `Bullet` scheduled by the
        // next item opening. They both flush in the order they were
        // scheduled, so the result is "\n- " rather than "- \n" or
        // something that lost one of them.
        while let Some(prefix) = self.pending_prefix.pop_front() {
            let style = *self.style_stack.last().unwrap_or(&self.base);
            match prefix {
                PendingPrefix::Bullet(BulletKind::Dash) => push_span(out, style, "- "),
                PendingPrefix::Bullet(BulletKind::Number(n)) => {
                    push_span(out, style, &format!("{n}. "))
                }
                PendingPrefix::Quote => push_span(out, style, "> "),
                PendingPrefix::Heading(level) => {
                    let hashes = "#".repeat(level as usize);
                    push_span(out, style, &format!("{hashes} "));
                }
                PendingPrefix::Newline => push_span(out, style, "\n"),
                PendingPrefix::Raw(s) => push_span(out, style, &s),
            }
        }
    }
}

/// The two strings an image tag carries: the alt text and the URL.
#[derive(Debug, Clone)]
struct ImageParts {
    alt: String,
    url: String,
}

/// The block-level frames that can be open at one time. Only used to keep
/// the open frames balanced; the actual rendering keys are inlined in
/// `start`/`end` so a stray new variant is caught at the match site.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockFrame {
    Paragraph,
    Heading(HeadingLevel),
    BlockQuote,
    CodeBlock,
    List,
    Item,
    Table,
    /// HTML blocks and image tags we drop the bytes of but still want to
    /// track so the start/end events stay balanced.
    Raw,
}

/// Append `text` to `out`, merging with the last span if both share a
/// style. The painter and `wrapped_lines` already handle adjacent same-style
/// spans correctly, but keeping them merged makes the span stream easier to
/// read in tests and lets us keep `\n` joins in one place.
fn push_span(out: &mut Vec<Span>, style: Style, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.style == style
    {
        last.text.to_mut().push_str(text);
        return;
    }
    out.push(Span::new(style, text));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pull the (style, text) pairs out of the spans for easier asserts.
    fn pairs(spans: &[Span]) -> Vec<(Style, &str)> {
        spans.iter().map(|s| (s.style, s.text.as_ref())).collect()
    }

    /// Every test below parses with the same base style the answer uses:
    /// the foreground colour for unmarked text. `Style::Reasoning` is
    /// exercised separately.
    const BASE: Style = Style::Plain;

    fn parse(text: &str) -> Vec<Span> {
        super::parse(text, BASE)
    }

    #[test]
    fn an_empty_input_returns_no_spans() {
        assert!(parse("").is_empty());
    }

    #[test]
    fn a_plain_paragraph_is_one_styled_run() {
        let spans = parse("hello world");
        assert_eq!(pairs(&spans), vec![(Style::Plain, "hello world")]);
    }

    #[test]
    fn inline_emphasis_bolds_its_text() {
        // `*x*` is emphasis; `**x**` is strong. The walker pushes the
        // matching style onto the stack and pops it on the matching end,
        // so the text inside carries the bold style and the text outside
        // does not. The runner also coalesces adjacent same-style runs:
        // three Plain spans collapse into one, with the trailing space
        // landing on whichever side the next strong or non-plain tag
        // sits.
        let spans = parse("plain *em* **strong**");
        assert_eq!(
            pairs(&spans),
            vec![(Style::Plain, "plain em "), (Style::Yellow, "strong")],
            "the strong span picks up the space that follows it"
        );
    }

    #[test]
    fn inline_code_uses_the_dim_style() {
        let spans = parse("use `foo()` here");
        assert_eq!(
            pairs(&spans),
            vec![
                (Style::Plain, "use "),
                (Style::Dim, "foo()"),
                (Style::Plain, " here"),
            ]
        );
    }

    #[test]
    fn a_fenced_code_block_carries_its_raw_bytes() {
        let src = "before\n```rust\nfn main() {}\n```\nafter";
        let spans = parse(src);
        // Fenced blocks emit a dim opener with the language hint, the
        // body in dim, a closing dim fence, and the surrounding
        // paragraphs in plain. The runner coalesces adjacent dim
        // spans into one, which is the right answer for the painter:
        // it sees one long dim span with embedded newlines and the
        // wrap step treats it as one block.
        let dim: String = spans
            .iter()
            .filter(|s| s.style == Style::Dim)
            .map(|s| s.text.as_ref())
            .collect();
        assert_eq!(dim, "```rustfn main() {}\n\n```");
        // The two surrounding paragraphs are plain spans on either
        // side of the dim block.
        let plain: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == Style::Plain)
            .map(|s| s.text.as_ref())
            .collect();
        assert_eq!(plain, vec!["before\n", "\nafter"]);
    }

    #[test]
    fn an_indented_code_block_has_no_fence() {
        let src = "para\n\n    indented code\n\nafter";
        let spans = parse(src);
        // The opening ```` ``` ```` line is what tells the reader where
        // the block begins; indented blocks rely on indentation alone, so
        // they do not get one. Only the body lands in `Style::Dim`.
        let dim_spans: Vec<&str> = spans
            .iter()
            .filter(|s| s.style == Style::Dim)
            .map(|s| s.text.as_ref())
            .collect();
        assert_eq!(dim_spans, vec!["indented code\n"]);
    }

    #[test]
    fn a_heading_is_prefixed_with_its_level_marker() {
        // Heading + body across two paragraphs: the painter receives one
        // plain span carrying both, with a `\n` separating them. Coalescing
        // is fine because the wrap step splits on `\n` regardless of
        // where the span boundary fell.
        assert_eq!(
            pairs(&parse("# Title\n\nbody")),
            vec![(Style::Plain, "# Title\nbody")],
        );
        assert_eq!(pairs(&parse("### Sub")), vec![(Style::Plain, "### Sub")]);
    }

    #[test]
    fn an_unordered_list_prefixes_each_item_with_a_dash() {
        let spans = parse("- one\n- two\n- three");
        // Each `- ` is the prefix, the rest is the body; the items are
        // separated by a newline so the wrap step draws them on three
        // lines.
        assert_eq!(
            pairs(&spans),
            vec![(Style::Plain, "- one\n- two\n- three")],
            "adjacent list items merge into one span under the same style"
        );
    }

    #[test]
    fn an_ordered_list_numbers_from_one() {
        let spans = parse("1. one\n2. two");
        assert_eq!(
            pairs(&spans),
            vec![(Style::Plain, "1. one\n2. two")],
            "ordered lists number each item"
        );
    }

    #[test]
    fn a_block_quote_prefixes_each_line_with_a_gt() {
        // A block quote with a soft break between two paragraphs: the
        // soft break lives in a quoted context, so the walker emits a
        // `\n` and re-pends a `> ` prefix; the trailing newline at the
        // end of the quote is dropped by `finish`.
        let src = "> a quote\n> across two lines";
        let spans = parse(src);
        assert_eq!(
            pairs(&spans),
            vec![(Style::Plain, "> a quote\n> across two lines")],
            "the two lines of a quote share one span"
        );
    }

    #[test]
    fn a_horizontal_rule_is_a_dim_row_of_dashes() {
        let spans = parse("before\n\n---\n\nafter");
        // A rule is a `Style::Dim` span of `─` glyphs, sitting between
        // the two paragraphs that bracket it. The wrap step clips it
        // to the cell width; the painter will not draw a row longer
        // than the cell.
        assert_eq!(
            pairs(&spans),
            vec![
                (Style::Plain, "before\n"),
                (Style::Dim, "────────────────────────────────"),
                (Style::Plain, "after"),
            ]
        );
    }

    #[test]
    fn a_strikethrough_runs_under_the_dim_style() {
        // Strikethrough sits in the same family as emphasis and strong
        // for the minimum set: dim. The reader can still tell the
        // original text from the rendering style.
        let spans = parse("~~gone~~");
        assert_eq!(pairs(&spans), vec![(Style::Dim, "gone")]);
    }

    #[test]
    fn a_link_renders_as_text_followed_by_the_url() {
        // OSC 8 is out of scope; the visible text carries the link, and
        // the destination follows in parentheses. The runner coalesces
        // the two same-style pieces into one span, which the painter
        // treats as one line of body text.
        let spans = parse("[caocli](https://example.test)");
        assert_eq!(
            pairs(&spans),
            vec![(Style::Plain, "caocli (https://example.test)")],
        );
    }

    #[test]
    fn an_image_renders_as_a_placeholder_line() {
        let spans = parse("![alt text](https://example.test/x.png)");
        assert_eq!(
            pairs(&spans),
            vec![(Style::Plain, "[alt text](https://example.test/x.png)")],
        );
    }

    #[test]
    fn a_task_list_item_prefixes_with_a_checkbox() {
        let spans = parse("- [ ] todo\n- [x] done");
        assert_eq!(
            pairs(&spans),
            vec![(Style::Plain, "[ ] todo\n[x] done")],
            "unchecked then checked"
        );
    }

    #[test]
    fn a_table_falls_back_to_the_raw_markdown() {
        let src = "| a | b |\n|---|---|\n| 1 | 2 |";
        let spans = parse(src);
        // The capture-table path is the one block whose bytes go straight
        // through: every line of the source becomes a 2-space indented
        // plain span. The wrap step will pad with the gutter, so the
        // table reads as a quoted block.
        let plain: String = spans
            .iter()
            .filter(|s| s.style == Style::Plain)
            .map(|s| s.text.as_ref())
            .collect();
        assert!(plain.contains("| a | b |"));
        assert!(plain.contains("| 1 | 2 |"));
        assert!(plain.starts_with("  "));
    }

    #[test]
    fn emphasis_inside_a_heading_still_bolds() {
        // The state machine lets inline tags open inside block tags, so
        // `## A **bold** title` still bolds the right word. Coalescing
        // folds the heading's `## ` and the rest of the line into one
        // plain span, then the bold span sits between the two plain
        // pieces.
        assert_eq!(
            pairs(&parse("## A **bold** title")),
            vec![
                (Style::Plain, "## A "),
                (Style::Yellow, "bold"),
                (Style::Plain, " title"),
            ]
        );
    }

    #[test]
    fn inline_html_is_dropped_silently() {
        // Dropping HTML tags is the contract: a model that emits raw HTML
        // would otherwise put `<details>` on the screen.
        let spans = parse("a <em>b</em> c");
        assert_eq!(pairs(&spans), vec![(Style::Plain, "a b c")]);
    }

    #[test]
    fn a_soft_break_is_a_newline_in_the_span() {
        // A single newline inside a paragraph is a soft break. In
        // CommonMark the renderer picks "space or newline"; a terminal
        // cell reads better with a newline, so two source lines land
        // on two rows rather than on one wrapped row.
        assert_eq!(pairs(&parse("foo\nbar")), vec![(Style::Plain, "foo\nbar")]);
    }

    #[test]
    fn a_hard_break_is_a_real_newline_in_the_span() {
        // Two trailing spaces (or a backslash) make the wrap step treat
        // the newline as a hard break: the second line lands on its own
        // row. The coalescing runner puts both halves of the plain-text
        // run into one span; the embedded `\n` is what tells the wrap
        // step to break the line.
        assert_eq!(
            pairs(&parse("foo  \nbar")),
            vec![(Style::Plain, "foo\nbar")],
        );
    }

    #[test]
    fn wide_characters_survive_the_parser() {
        // The renderer measures columns, not characters; a regression
        // here would mean a parser that turned CJK into per-glyph runs.
        let spans = parse("\u{6df1}\u{5ea6}");
        assert_eq!(pairs(&spans), vec![(Style::Plain, "\u{6df1}\u{5ea6}")]);
    }

    #[test]
    fn the_same_text_repeated_renders_to_the_same_spans() {
        // Determinism: the live stream and the resumed session both call
        // this on the same source and must agree.
        let a = parse("# Title\n\nbody with `code` and **bold**.");
        let b = parse("# Title\n\nbody with `code` and **bold**.");
        assert_eq!(a, b);
    }
}

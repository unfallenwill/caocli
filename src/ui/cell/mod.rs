//! Transcript cells: the renderer's output modelled as values.
//!
//! A cell renders itself to styled spans and does nothing else -- no I/O, no
//! mutable state, no knowledge of the terminal. Live streaming and session
//! replay both produce cells and hand them to the same painter, so a session
//! that is resumed looks exactly like the one that was watched live.
//!
//! The data here is deliberately terminal-agnostic. [`Style`] is a semantic
//! label rather than an escape sequence, on purpose: the plain front end
//! renders it to ANSI bytes and the TUI front end renders it to ratatui
//! styles, both off the palette in [`crate::ui::theme`]. Anything that would
//! only matter to one of them -- an SGR code, a ratatui `Color` -- lives on
//! that front end's side of the seam, not on the cell's; the palette is the
//! one thing on this side of it that both ends are told to agree on.

use std::borrow::Cow;
use std::time::Duration;

use crate::image::Note;
use crate::tools::ask::Question;
use crate::tools::todo::{self, Todo};
use crate::types::Usage;

use super::text;

mod call;
pub(super) mod layout;
mod markdown;
pub(super) mod question;
mod replay;
pub(super) mod sink;
mod step;
mod stream;
mod todos;
pub(super) mod wrap;

// Internally: the helpers a [`Cell`] builds itself from.
use call::hint;
use question::question_spans;
use todos::todo_spans;

// Externally: re-exported at `cell::*` because that is what the rest of the
// crate reaches for. The producers live in their own module next to the
// helper that needs them; the re-export is what keeps `cell::` working as
// the entry point for the data layer.
//
// The two that are not producers are the palette's: a `Style` becomes a colour
// on the way out, in one place both front ends read, and the re-export keeps
// that call site spelled the way it reads -- `style_of(style)` and the
// `style_code` half that the plain front end's writer asks the palette for.
pub(crate) use crate::ui::theme::{style_code, style_of};
pub use replay::from_messages;
pub use sink::CellSink;
// `StepStatus` is re-exported for tests and downstream callers; the cell
// layer's own match arms reach for it through the `step::*` path.
#[allow(unused_imports)]
pub use step::{Step, StepStatus};
pub use stream::Stream;
pub use todos::{standing_todos, todo_gutter, todo_head_spans, todo_line_spans};

/// A text style, held as data: which of the session's lines this is, not what
/// colour it is painted in. The colour lives in [`crate::ui::theme`], one
/// palette both front ends read -- the plain front end's writer spells it out
/// as SGR, the TUI hands it to `ratatui`, and neither chooses one of its own.
///
/// The visual rules shared by both front ends -- "is this line secondary" and
/// "does it carry weight" -- live on this type as [`Style::is_dim`] and
/// [`Style::is_bold`]. What colour each of them is painted in is
/// [`crate::ui::theme`]'s: the palette is one value both front ends read, and
/// the half that does not depend on it is the one the methods expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Unstyled text: body text.
    Plain,
    /// Secondary text: tool results, notices.
    Dim,
    /// The thinking behind an answer.
    ///
    /// A style of its own rather than `Plain` because the block machinery keys on
    /// it -- a fragment in a different style is a different block -- and not
    /// because it looks different: it renders dim, exactly as [`Style::Dim`] does.
    /// What tells thinking apart from a tool result is the rule in its gutter,
    /// which is a difference in the layout rather than in the colors, and so one
    /// that survives both a terminal that ignores SGR 2 and a reader who has
    /// turned color off.
    Reasoning,
    /// Attention text: tool calls, interruptions.
    Yellow,
    /// Text a change adds.
    Green,
    /// Failures, written to stderr.
    Red,
}

impl Style {
    /// Whether this style is secondary text: the lines that stand behind the
    /// session rather than in it.
    ///
    /// The one rule both front ends agree on without looking at the terminal.
    /// What it means for the painting is the palette's: Dracula paints these in
    /// its `comment` colour, and the line a tool result is set on is told apart
    /// from the answer by a colour and by the layout -- the marker in its gutter
    /// -- neither of which depends on a terminal honoring SGR 2.
    ///
    /// Kept public alongside the cell layer's contract: a future backend that
    /// cannot ask the palette directly (a third front end, a static export)
    /// reads the two rules from here.
    #[allow(dead_code)]
    pub fn is_dim(self) -> bool {
        matches!(self, Style::Dim | Style::Reasoning)
    }

    /// Whether this style carries weight as well as a colour.
    ///
    /// Weight reads on both a light background and a dark one, on a terminal
    /// that can render it. [`crate::ui::theme::style_code`] and
    /// [`crate::ui::theme::style_of`] apply the same rule, which is what this
    /// method is the one place for.
    ///
    /// Kept public for the same reason [`Style::is_dim`] is: the cell layer's
    /// contract, not the palette's. Today's backends ask the palette directly.
    #[allow(dead_code)]
    pub fn is_bold(self) -> bool {
        matches!(self, Style::Yellow | Style::Green | Style::Red)
    }

    /// The dim/bold modifiers this style carries, in the shape a backend
    /// applies: the cell layer's answer to "secondary?" and "weight?", for a
    /// backend that wants both as data rather than as two predicates.
    ///
    /// The palette in effect does not need it -- it paints each style its own
    /// colour, and carries weight from [`Style::is_bold`] directly -- so this is
    /// the shape the next backend reads, not the one
    /// [`crate::ui::theme`] happens to.
    #[allow(dead_code)]
    pub fn modifiers(self) -> Modifiers {
        Modifiers {
            dim: self.is_dim(),
            bold: self.is_bold(),
        }
    }
}

/// The dim/bold flags a backend applies to a span: what `Style::modifiers`
/// returns, decoupled from the enum so each front end can pattern-match on a
/// struct of booleans rather than re-derive the same answer from `is_dim` and
/// `is_bold` separately. Named for the rules they carry -- "secondary text",
/// "carries weight" -- rather than for one backend's way of expressing them,
/// which is the palette's to choose.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub dim: bool,
    pub bold: bool,
}

/// A styled run of text.
///
/// The text is a `Cow<'static, str>` so a literal marker can be borrowed and
/// a constructed string owned, the way the cell layer wants to express
/// them. The borrow form is what a future borrow-preserving wrap step would
/// carry through; today the painter copies every span it wraps, so the
/// distinction is dormant. The shape is what matters: the cell layer can
/// now say which text is a literal and which it had to build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub style: Style,
    pub text: Cow<'static, str>,
}

impl Span {
    pub fn new(style: Style, text: impl Into<String>) -> Self {
        Self {
            style,
            text: Cow::Owned(text.into()),
        }
    }
}

/// One line of a change, as a tool call shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffKind,
    pub text: String,
}

/// What a line of a change is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffKind {
    /// A line the call takes out.
    Removed,
    /// A line the call puts in.
    Added,
    /// Lines the cell does not show, and how many there are.
    Omitted(usize),
    /// A line neither taken out nor put in: kept around the change so the reader
    /// can see what surrounds it. Painted with a single leading space and the
    /// plain color, not the call's yellow, because the line is not the call's
    /// doing.
    Context,
    /// The header that opens a hunk: `@@ -old_start,old_count +new_start,new_count @@`.
    ///
    /// The four numbers are 1-based, the way unified diffs always are. A hunk
    /// with no old lines (a pure addition) is `@@ -0,0 +N,M @@`; a hunk with
    /// no new lines (a pure removal) is `@@ -N,M +0,0 @@`. The text field of the
    /// `DiffLine` is empty for a header -- the rendered text is the four numbers
    /// -- and the kind is what carries them.
    Hunk {
        old_start: usize,
        old_count: usize,
        new_start: usize,
        new_count: usize,
    },
}

/// One unit of transcript output.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// A user line, and the images that came with it.
    ///
    /// A line typed at the prompt is echoed by the front end that read it -- the
    /// plain one's line editor writes what was typed, the one that owns the
    /// screen places it in its transcript. A line carrying an image is never
    /// typed: `/image` builds the message, and the cell for it comes from the
    /// same replay a resumed session goes through, so the two read alike.
    User {
        text: String,
        /// One line each, under the text: what a reader needs is that there was
        /// an image and which one, never the bytes, which are in the session log
        /// and go to the backend.
        images: Vec<Note>,
    },
    /// A thinking block.
    Reasoning(String),
    /// A body-text block.
    Content(String),
    /// A tool call's full lifecycle: born running, settles on the result.
    ///
    /// A [`Step`] is one row in the ledger. While running, the tool's own
    /// output streams in as children. On `ToolResult`, the verdict line is
    /// filled in and the step is settled: a successful step is quiet (one
    /// line -- the verdict), a failed step is loud (verdict + children).
    ///
    /// Replaces what used to be three separate cells -- `ToolCall`, the
    /// stream of `ToolOutput` cells, and `ToolResult` -- because a tool
    /// call is one thing with a lifecycle, not three things the renderer
    /// happens to lay out next to each other.
    Step(Step),
    /// The question tool's call: what is being asked, and what there is to
    /// choose from.
    ///
    /// A cell of its own rather than a [`Cell::Step`] carrying the raw
    /// arguments, because these are the words the user has to read -- and because
    /// they are read back from the call's own arguments, so a resumed session
    /// shows the questions exactly as the live one did.
    Question(Vec<Question>),
    /// The todo tool's call: the plan for the work in hand.
    ///
    /// A cell of its own rather than a [`Cell::Step`] carrying the raw
    /// arguments, for the reason the question tool's is: the list is the thing
    /// the reader is meant to read, and it is read back from the call's own
    /// arguments, so a resumed session shows the list exactly as it was written
    /// and not as a copy of it that has to be kept somewhere.
    Todo(Vec<Todo>),
    /// A dim informational line. Used for results that are not part of a
    /// [`Cell::Step`] (the answer to a [`Cell::Question`], the update
    /// confirmation for a [`Cell::Todo`]) and for one-off notes.
    Notice(String),
    /// A failure. The plain front end writes these to the error stream; a front
    /// end owning the screen has to place them in its own output instead.
    Failure(String),
    /// The turn was cancelled.
    Interrupted,
    /// Token usage for one sub-request, with the wall time the stream took.
    Usage { usage: Usage, stream: Duration },
    /// The approval gate's question. It deliberately does not end its line: the
    /// answer is typed on the same one.
    Approval { name: String, hint: String },
}

/// The columns a cell opens in.
///
/// The answer is the only cell that starts in column zero. It is what a reader is
/// here for, and a left edge that one kind of line and nothing else touches is a
/// left edge the eye can run down to find every answer on the screen. Everything
/// else -- the thinking, the calls, the changes, the results, the notices -- is
/// set two columns past it, where it reads as the machinery around the answer
/// rather than as part of it.
///
/// The marker is columns of its own rather than a prefix on the text, which is why
/// it is not in [`Cell::spans`]: a block that wraps has to keep one left edge, and
/// only the layer that does the wrapping knows where the lines fall. A front end
/// that lets the terminal wrap -- the plain one -- can therefore set a block's
/// first line in and leave the rest to the terminal, but it cannot indent a line
/// it never sees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gutter {
    /// What opens the cell: a marker saying which kind of line this is, or two
    /// blanks for a line that is only being set off.
    pub head: &'static str,
    /// What a wrapped continuation opens in. Always as wide as `head`, so every
    /// line of a cell starts in the same column.
    pub rest: &'static str,
    /// The style both are written in.
    pub style: Style,
}

impl Gutter {
    const fn new(head: &'static str, rest: &'static str, style: Style) -> Self {
        Self { head, rest, style }
    }

    /// The columns the gutter takes. `head` and `rest` are the same width, so this
    /// is the inset of every line of the cell.
    pub fn width(&self) -> usize {
        text::width(self.head)
    }
}

/// The marker a line of the user's own carries, in the transcript and in the box
/// while the same line is still being typed.
///
/// One marker in one column in both places, which is what makes a line read the same
/// either side of the moment it is submitted: the box draws this itself rather than
/// carrying it in the placeholder, so it does not move when the typing starts and
/// does not go away when the hint does.
pub const USER_MARKER: &str = "› ";

/// The columns every gutter takes.
///
/// The gutters below are written out rather than built from this, so that each one
/// reads as what it is; the tests pin them all back to it, and the box -- which has to
/// start a draft in the same column the transcript starts a line -- asks for it rather
/// than for a number of its own.
pub const MARKER_COLUMNS: usize = 2;

impl Cell {
    /// A user line with nothing attached to it: the shape every line but an
    /// image's is.
    pub fn user(text: impl Into<String>) -> Self {
        Cell::User {
            text: text.into(),
            images: Vec::new(),
        }
    }

    /// A tool call about to run: produces an open [`Cell::Step`] for regular
    /// tools, or the dedicated rich cell for the question and todo tools.
    ///
    /// The question tool's arguments are what is shown, and the todo tool's
    /// arguments are the plan that is meant to be read -- a step carrying
    /// the raw JSON would be a copy of the wire format that a person has
    /// to parse. Anything else falls back to an open step, which is what an
    /// unreadable call ends up as.
    ///
    /// Lives on the cell layer rather than at the call sites because the
    /// dispatch is a property of the cells the call becomes, not of
    /// whichever front end is rendering: the plain front end and the TUI
    /// both end up with the same Question, Todo, or open Step when given
    /// the same arguments, and the fallback is the same.
    pub fn from_tool_call(name: &str, args: &str) -> Self {
        if name == crate::tools::ask::ASK_NAME
            && let Ok(questions) = crate::tools::ask::parse(args)
        {
            return Cell::Question(questions);
        }
        if name == crate::tools::TODO_NAME
            && let Ok(todos) = todo::parse(args)
        {
            return Cell::Todo(todos);
        }
        Cell::Step(Step::open(name, args))
    }

    /// The approval gate's question for a tool call.
    pub fn approval(name: &str, args: &str) -> Self {
        Cell::Approval {
            name: name.to_owned(),
            hint: hint(args),
        }
    }

    /// The columns this cell is set in, or `None` for the answer, which is the one
    /// cell that starts at the left edge.
    ///
    /// The markers are a small vocabulary and it is the whole of what a reader has
    /// to learn here: `›` is something they said, `▸` is something about to happen,
    /// `┆` is the model thinking, `·` is a result or a note. A line with no marker
    /// at all is the answer.
    pub fn gutter(&self) -> Option<Gutter> {
        match self {
            // The one cell on the left edge.
            Cell::Content(_) => None,
            Cell::User { .. } => Some(Gutter::new(USER_MARKER, "  ", Style::Dim)),
            // The thinking keeps its rule on every line: it is what makes a long
            // think read as one asided block rather than as a run of loose text. The
            // rule is painted in the thinking's own style, not a second one: it is
            // part of the block, and one block is one style run.
            Cell::Reasoning(_) => Some(Gutter::new("┆ ", "┆ ", Style::Reasoning)),
            // A step opens with the marker the status chose: `▸` for something
            // about to happen, `✔` for something that finished well, `✘` for
            // something that did not. The marker's style carries the verdict's
            // weight -- a green check and a red cross read differently even
            // without colour -- so a terminal that ignores SGR still shows the
            // step's outcome.
            Cell::Step(step) => Some(Gutter::new(
                step.status.marker(),
                "  ",
                match step.status {
                    step::StepStatus::Running => Style::Yellow,
                    step::StepStatus::Done => Style::Green,
                    step::StepStatus::Failed | step::StepStatus::Denied => Style::Red,
                },
            )),
            Cell::Approval { .. } | Cell::Question(_) | Cell::Todo(_) => {
                Some(Gutter::new("▸ ", "  ", Style::Yellow))
            }
            // A failed step's auto-expanded children take the same dim
            // gutter as their settled result would, so the failure reads as
            // one block from the verdict line down through the output that
            // caused it.
            Cell::Notice(_) => Some(Gutter::new("* ", "  ", Style::Dim)),
            Cell::Failure(_) => Some(Gutter::new("  ", "  ", Style::Red)),
            Cell::Interrupted => Some(Gutter::new("  ", "  ", Style::Yellow)),
            // A usage line says "tokens:" in its first word, but the gutter
            // carries its own mark: three short rules for "summary / stats /
            // numbers", so a reader with colour turned off still sees the
            // line as the kind of line it is.
            Cell::Usage { .. } => Some(Gutter::new("≡ ", "  ", Style::Dim)),
        }
    }

    /// The cell's styled spans, without the columns its [`Cell::gutter`] sets it
    /// in, the separating blank line, or the line ending the painter adds.
    ///
    /// This is the compact form: settled-Done steps hide their children,
    /// failures auto-expand. The Ctrl-O verbose toggle re-renders with
    /// [`Cell::spans_with`]`(true)`, which surfaces settled-Done children
    /// without changing the layout of the running or failed ones.
    pub fn spans(&self) -> Vec<Span> {
        self.spans_with(false)
    }

    /// The cell's spans at `verbose`. Only `Step` changes shape between
    /// compact and verbose: every other cell has the same spans in both
    /// modes, and the parameter is here so the call site reads
    /// uniformly.
    pub fn spans_with(&self, verbose: bool) -> Vec<Span> {
        match self {
            Cell::User { text, images } => {
                let mut spans = Vec::with_capacity(images.len() + 1);
                if !text.is_empty() {
                    spans.push(Span::new(Style::Plain, text.as_str()));
                }
                // An image is a line of the same cell, set in under the words it
                // came with: dim, like the rest of what is about the message
                // rather than being it. With no words it is the cell's first
                // line, and there is nothing to break away from.
                //
                // Two or more images are one line of their own, with the count
                // and the total bytes rather than one entry per image: a user
                // who attaches five pictures gets one line, not five, and the
                // per-image format is left to the message the backend sees --
                // it is in the data URL, so it survives a resume.
                let lead = if spans.is_empty() { "" } else { "\n" };
                if images.len() > 1 {
                    let total: usize = images.iter().map(|i| i.bytes).sum();
                    spans.push(Span::new(
                        Style::Dim,
                        format!("{lead}[{} images · {total} bytes]", images.len()),
                    ));
                } else {
                    for image in images {
                        spans.push(Span::new(
                            Style::Dim,
                            format!("{lead}{}", image_line(image)),
                        ));
                    }
                }
                spans
            }
            Cell::Reasoning(text) => markdown::parse(text, Style::Reasoning),
            Cell::Content(text) => markdown::parse(text, Style::Plain),
            Cell::Step(step) => step.spans_with(verbose),
            Cell::Notice(text) => vec![Span::new(Style::Dim, text.as_str())],
            Cell::Failure(text) => vec![Span::new(Style::Red, format!("error: {text}"))],
            Cell::Interrupted => vec![Span::new(Style::Yellow, "⏹ interrupted (Ctrl-C)")],
            Cell::Usage { usage, stream } => {
                vec![Span::new(Style::Dim, usage_line(usage, *stream))]
            }
            Cell::Approval { name, hint } => vec![Span::new(
                Style::Yellow,
                format!("{name} {hint} · run it? y/N "),
            )],
            Cell::Question(questions) => question_spans(questions),
            Cell::Todo(todos) => todo_spans(todos),
        }
    }

    /// Whether a blank line separates this cell from the one before it.
    ///
    /// `prev_is_text_block` is all the painter has to remember about the cell it
    /// wrote last; the transcript itself lives in the session log.
    pub fn gap_after(&self, prev_is_text_block: bool) -> bool {
        match self {
            // Thinking and the answer are separate visual blocks, but a block
            // that opens a turn -- or follows a tool line -- starts tight
            // against it, so a turn is not padded with blank lines.
            Cell::Reasoning(_) | Cell::Content(_) => prev_is_text_block,
            // A step is announced on a line of its own (the header), but the
            // children that follow it are part of the same cell -- the cell's
            // own spans join them with `\n` -- so a step gap_after is false:
            // the gap before the step's header is its gutter's job, not the
            // spacing rule's.
            Cell::Step(_) => false,
            // A replayed user line is set off from whatever preceded it.
            Cell::User { .. } | Cell::Question(_) | Cell::Todo(_) | Cell::Approval { .. } => true,
            _ => false,
        }
    }

    /// Whether this cell is a text block, which is what the spacing rule keys
    /// on.
    pub fn is_text_block(&self) -> bool {
        matches!(self, Cell::Reasoning(_) | Cell::Content(_))
    }

    /// Whether the cell ends its line.
    pub fn ends_line(&self) -> bool {
        !matches!(self, Cell::Approval { .. })
    }
}

/// The line an attached image contributes to the user's cell: what it is and how
/// much of it there is, which is as much as the message says.
fn image_line(image: &Note) -> String {
    format!("[image {} · {} bytes]", image.format, image.bytes)
}

/// The per-sub-request usage line.
///
/// The stream's wall time turns the reported completion tokens into a tokens-per-
/// second figure, appended when it is worth showing: a stream must have run at
/// least a second (below that the quotient is noise the test suite would pin at
/// absurd heights) and must have produced tokens at all.
///
/// The line lays out as `in X/Y · cache hit/miss · out Z [· N token/s]`: a
/// flat sequence joined by the same "·" the rest of the UI uses as a
/// separator, so the eye learns one rule for "what kind of thing comes next".
/// The cache segment names its own sub-fields rather than wrapping them in
/// parentheses, which read as a parenthesis rather than as a separator against
/// the joins around them.
fn usage_line(usage: &Usage, stream: Duration) -> String {
    let cache = match usage.cache() {
        Some(c) => format!("hit {}/miss {}", c.hit, c.miss),
        None => "cache —".to_string(),
    };
    let mut line = format!(
        "tokens: in {}/{} · {cache} · out {}",
        usage.prompt_tokens, usage.total_tokens, usage.completion_tokens
    );
    if usage.completion_tokens > 0 && stream >= Duration::from_secs(1) {
        let per_second = usage.completion_tokens as f64 / stream.as_secs_f64();
        line.push_str(&format!(" · {} token/s", per_second.round() as u64));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only a text block behind another text block is spaced apart.
    #[test]
    fn gap_opens_tight_and_spaces_blocks() {
        let reasoning = Cell::Reasoning("t".into());
        let content = Cell::Content("a".into());
        // a block opening a turn starts tight
        assert!(!reasoning.gap_after(false));
        // thinking followed by an answer is spaced
        assert!(content.gap_after(true));
        // a later block after a tool line starts tight again
        assert!(!reasoning.gap_after(false));
    }

    #[test]
    fn gap_is_declared_by_the_non_block_cells() {
        let call = Cell::from_tool_call("Bash", "{}");
        // A step's header is its own line; the gap (if any) is the
        // spacing rule's job, and it is none for a step -- the cells
        // that come before are the ones whose gap_after says so.
        assert!(
            !call.gap_after(false),
            "a step opens tight against its predecessor"
        );
        assert!(Cell::user("hi").gap_after(false));
        assert!(!Cell::Interrupted.gap_after(true));
        assert!(!Cell::Notice("n".into()).gap_after(false));
        assert!(!Cell::Step(Step::settled("Bash", "{}", "r")).gap_after(false));
    }

    #[test]
    fn only_the_approval_question_leaves_its_line_open() {
        assert!(!Cell::approval("Bash", "{}").ends_line());
        assert!(Cell::from_tool_call("Bash", "{}").ends_line());
        assert!(Cell::Content("x".into()).ends_line());
    }

    #[test]
    fn user_line_marks_itself_and_hands_over_to_body_text() {
        // The marker is the gutter's, not the text's, so the body is the line as it
        // was said: a front end that writes the gutter itself cannot end up with a
        // second copy of the marker inside the text it is marking.
        let cell = Cell::user("hello");
        assert_eq!(cell.spans(), vec![Span::new(Style::Plain, "hello")]);
        let gutter = cell.gutter().expect("a user line is set in");
        assert_eq!(gutter.head, "› ");
        assert_eq!(gutter.rest, "  ", "and it wraps to the column it opened in");
        assert_eq!(gutter.style, Style::Dim);
    }

    #[test]
    fn an_attached_image_is_a_line_of_the_users_own_cell() {
        let cell = Cell::User {
            text: "what is this?".into(),
            images: vec![Note {
                format: "png".into(),
                bytes: 49152,
            }],
        };
        assert_eq!(
            cell.spans(),
            vec![
                Span::new(Style::Plain, "what is this?"),
                // A line of the same cell, dim, and the cell's own break: the
                // layer that wraps sets it in the columns the words are in.
                Span::new(Style::Dim, "\n[image png · 49152 bytes]"),
            ]
        );
        // One cell, so the spacing rule sees one thing to set off.
        assert!(cell.gap_after(false));
        assert!(!cell.is_text_block());
    }

    #[test]
    fn multiple_images_collapse_to_one_line_with_count_and_total_bytes() {
        // The format breakdown is one entry per image -- that turns a five-image
        // message into five lines of "what it is", which is five rows the
        // reader has to skip past before the answer. One line, count and total
        // bytes, is what a user with several pictures actually wants to see:
        // the format itself is in the data URL the backend sees, and a resumed
        // session replays the same bytes it sent the first time.
        let cell = Cell::User {
            text: "compare these".into(),
            images: vec![
                Note {
                    format: "png".into(),
                    bytes: 49152,
                },
                Note {
                    format: "jpeg".into(),
                    bytes: 30720,
                },
                Note {
                    format: "webp".into(),
                    bytes: 16384,
                },
            ],
        };
        assert_eq!(
            cell.spans(),
            vec![
                Span::new(Style::Plain, "compare these"),
                Span::new(Style::Dim, "\n[3 images · 96256 bytes]"),
            ]
        );
    }

    #[test]
    fn a_tool_call_with_context_and_hunk_lines_renders_them() {
        // The new variants of `DiffKind` are how a real unified diff is
        // painted: context lines have a single leading space and the plain
        // style, hunk headers name the line ranges in dim. The test pins the
        // shape of both, since both front ends go through `spans()`.
        let mut step = Step::open(
            "Edit",
            r#"{"file_path":"a.rs","old_string":"fn a() {\n    1\n}","new_string":"fn a() {\n    2\n}"}"#,
        );
        step.diff = vec![
            DiffLine {
                kind: DiffKind::Hunk {
                    old_start: 1,
                    old_count: 3,
                    new_start: 1,
                    new_count: 3,
                },
                text: String::new(),
            },
            DiffLine {
                kind: DiffKind::Context,
                text: "fn a() {".into(),
            },
            DiffLine {
                kind: DiffKind::Removed,
                text: "    1".into(),
            },
            DiffLine {
                kind: DiffKind::Added,
                text: "    2".into(),
            },
        ];
        let cell = Cell::Step(step);
        assert_eq!(
            cell.spans(),
            vec![
                Span::new(Style::Yellow, "Edit a.rs"),
                Span::new(Style::Dim, "\n@@ -1,3 +1,3 @@"),
                Span::new(Style::Plain, "\n fn a() {"),
                Span::new(Style::Red, "\n-     1"),
                Span::new(Style::Green, "\n+     2"),
            ]
        );
    }

    #[test]
    fn a_message_that_is_only_images_has_no_empty_first_line() {
        let cell = Cell::User {
            text: String::new(),
            images: vec![
                Note {
                    format: "jpeg".into(),
                    bytes: 3,
                },
                Note {
                    format: "webp".into(),
                    bytes: 4,
                },
            ],
        };
        // The aggregate line carries the cell's break itself, so a message
        // whose only content is images opens in the same column the answer
        // would -- never with a blank line the reader would have to skip.
        assert_eq!(
            cell.spans(),
            vec![Span::new(Style::Dim, "[2 images · 7 bytes]")]
        );
    }

    #[test]
    fn tool_cells_carry_the_extracted_hint() {
        let step = match Cell::from_tool_call("Bash", r#"{"command":"ls"}"#) {
            Cell::Step(s) => s,
            other => panic!("expected a tool call, got {other:?}"),
        };
        assert_eq!(step.verb, "Bash");
        assert_eq!(step.subject, "ls");
        assert!(step.diff.is_empty(), "a command has no lines to show");
        let cell = Cell::from_tool_call("Bash", r#"{"command":"ls"}"#);
        assert_eq!(cell.spans(), vec![Span::new(Style::Yellow, "Bash ls")]);
        assert_eq!(cell.gutter().unwrap().head, "▸ ");
        let cell = Cell::approval("Bash", r#"{"command":"rm -rf /"}"#);
        assert_eq!(
            cell.spans(),
            vec![Span::new(Style::Yellow, "Bash rm -rf / · run it? y/N ")]
        );
        assert_eq!(
            cell.gutter().unwrap().head,
            "▸ ",
            "the gate is a call, and is marked as one"
        );
    }

    /// A tool call's streaming output: the children of a running step,
    /// each line dim, each on its own row under the header.
    #[test]
    fn tool_output_renders_the_lines_it_was_given() {
        let mut step = Step::open("Bash", r#"{"command":"ls"}"#);
        step.push_output("Compiling foo\nwarning: unused\n");
        let cell = Cell::Step(step);
        assert_eq!(
            cell.spans(),
            vec![
                Span::new(Style::Yellow, "Bash ls"),
                Span::new(Style::Dim, "\nCompiling foo"),
                Span::new(Style::Dim, "\nwarning: unused"),
            ]
        );
        // Marked as a step, set in like one. The gutter depends on
        // status, not on whether children are present.
        assert_eq!(cell.gutter().unwrap().head, "▸ ");
        assert!(cell.ends_line());
        assert!(!cell.is_text_block());
        assert!(!cell.gap_after(true));
    }

    #[test]
    fn tool_result_renders_first_line_and_size_only() {
        let result = "exit_code: 3\n--- stdout ---\nSECRET_BODY";
        let cell = Cell::Step(Step::settled("Bash", r#"{"command":"ls"}"#, result));
        let spans = cell.spans();
        // A Bash result with a non-zero exit code is a failure: the
        // settled step carries the verdict in red, with the verb and
        // subject attached so the reader can scan the row.
        assert_eq!(spans, vec![Span::new(Style::Red, "Bash ls · exit_code: 3")]);
        // An empty result: no recognized prefix, the default summary
        // (first line and byte count) is what falls out. An empty result
        // is not a failure, so the settled step is green.
        let cell = Cell::Step(Step::settled("Bash", r#"{"command":"ls"}"#, ""));
        let spans = cell.spans();
        assert_eq!(spans, vec![Span::new(Style::Green, "Bash ls ·  · 0 bytes")]);
    }

    /// An `error:` result is the one case where the style changes: failures
    /// are red, so a reader can tell success from failure at a glance.
    #[test]
    fn a_tool_result_starting_with_error_is_red() {
        let cell = Cell::Step(Step::settled(
            "Read",
            r#"{"file_path":"/nope"}"#,
            "error: file not found",
        ));
        let spans = cell.spans();
        assert_eq!(
            spans,
            vec![Span::new(Style::Red, "Read /nope · error: file not found")]
        );
        // The gutter follows the content: a red marker in front of a red
        // message reads as one failure, not two things.
        let gutter = cell.gutter().unwrap();
        assert_eq!(gutter.style, Style::Red);
        assert_eq!(gutter.head, "✘ ");
    }

    #[test]
    fn usage_line_reports_cache_when_the_provider_does() {
        let mut u = Usage {
            prompt_tokens: 20,
            total_tokens: 28,
            completion_tokens: 8,
            ..Usage::default()
        };
        // no cache fields recognized: an unknown rate is never shown as 0%
        let spans = Cell::Usage {
            usage: u.clone(),
            stream: Duration::ZERO,
        }
        .spans();
        assert_eq!(
            spans,
            vec![Span::new(Style::Dim, "tokens: in 20/28 · cache — · out 8")]
        );
        u.prompt_cache_hit_tokens = 12;
        u.prompt_cache_miss_tokens = 8;
        let spans = Cell::Usage {
            usage: u,
            stream: Duration::ZERO,
        }
        .spans();
        assert_eq!(
            spans,
            vec![Span::new(
                Style::Dim,
                "tokens: in 20/28 · hit 12/miss 8 · out 8"
            )]
        );
    }

    #[test]
    fn a_stream_that_ran_a_second_reports_its_speed() {
        let u = Usage {
            prompt_tokens: 20,
            total_tokens: 532,
            completion_tokens: 512,
            ..Usage::default()
        };
        // 512 tokens over 1.25 s rounds to 410
        let spans = Cell::Usage {
            usage: u,
            stream: Duration::from_millis(1250),
        }
        .spans();
        assert_eq!(
            spans,
            vec![Span::new(
                Style::Dim,
                "tokens: in 20/532 · cache — · out 512 · 410 token/s"
            )]
        );
    }

    #[test]
    fn a_fast_or_tokenless_stream_reports_no_speed() {
        let u = Usage {
            prompt_tokens: 20,
            total_tokens: 532,
            completion_tokens: 512,
            ..Usage::default()
        };
        // under a second the quotient is noise, not a speed
        let spans = Cell::Usage {
            usage: u.clone(),
            stream: Duration::from_millis(999),
        }
        .spans();
        assert_eq!(spans[0].text, "tokens: in 20/532 · cache — · out 512");
        // nothing streamed: no tokens to divide by
        let u = Usage {
            prompt_tokens: 20,
            total_tokens: 20,
            completion_tokens: 0,
            ..Usage::default()
        };
        let spans = Cell::Usage {
            usage: u,
            stream: Duration::from_secs(9),
        }
        .spans();
        assert_eq!(spans[0].text, "tokens: in 20/20 · cache — · out 0");
    }

    /// One of every kind of cell, so a rule about cells can be asked of all of them.
    fn one_of_each() -> Vec<Cell> {
        vec![
            Cell::user("hi"),
            Cell::Reasoning("hmm".into()),
            Cell::Content("answer".into()),
            Cell::from_tool_call("Bash", r#"{"command":"ls"}"#),
            Cell::from_tool_call(
                "AskUserQuestion",
                r#"{"questions":[{"id":"a","question":"Which?"}]}"#,
            ),
            Cell::from_tool_call("TodoWrite", r#"{"todos":[{"content":"Run the gates"}]}"#),
            Cell::Step(Step::settled("Bash", r#"{"command":"ls"}"#, "ok")),
            Cell::Notice("noted".into()),
            Cell::Failure("broken".into()),
            Cell::Interrupted,
            Cell::Usage {
                usage: Usage::default(),
                stream: Duration::ZERO,
            },
            Cell::approval("Bash", r#"{"command":"ls"}"#),
        ]
    }

    /// The contract the whole layout is read down: one cell on the left edge, and
    /// everything else in columns past it -- the same columns on every line of a
    /// cell, which is what keeps a wrapped block on one left edge.
    #[test]
    fn the_answer_is_the_only_cell_without_a_gutter() {
        for cell in one_of_each() {
            let gutter = cell.gutter();
            if matches!(cell, Cell::Content(_)) {
                assert!(gutter.is_none(), "{cell:?} is the answer");
                continue;
            }
            let gutter = gutter.unwrap_or_else(|| panic!("{cell:?} is set in"));
            assert_eq!(
                gutter.width(),
                text::width(gutter.rest),
                "{cell:?} wraps to the column it opens in"
            );
            assert!(gutter.width() > 0, "{cell:?} is set in, not flush");
        }
    }

    /// Every cell that is set in is set in by the same number of columns, so that one
    /// of them can answer for all of them -- a front end that has to write the gutter
    /// itself can ask any cell how wide it is.
    #[test]
    fn every_gutter_is_the_width_of_every_other() {
        let cells = one_of_each();
        let mut widths = cells
            .iter()
            .filter_map(|cell| cell.gutter())
            .map(|gutter| gutter.width());
        let first = widths.next().expect("some cell is set in");
        assert!(
            widths.all(|width| width == first),
            "the markers are not all one width"
        );
        assert_eq!(first, MARKER_COLUMNS, "and this is the width they all are");
    }

    /// The box draws the user's marker itself, from this constant, so a line being
    /// typed is marked in the column the same line is marked in once it is submitted.
    #[test]
    fn the_user_marker_is_one_marker_wide() {
        assert_eq!(Cell::user("x").gutter().unwrap().head, USER_MARKER);
        assert_eq!(text::width(USER_MARKER), MARKER_COLUMNS);
    }

    /// The dim/bold rules both front ends build on. The contract is what keeps
    /// the plain SGR mapping and the TUI's ratatui mapping in step: a `Style`
    /// that is `is_dim` is secondary text in both, and one that is `is_bold`
    /// carries weight in both. Adding a new coloured style to one place without
    /// the other would be a one-line oversight; this test is what makes it two.
    #[test]
    fn the_dim_and_bold_rules_are_what_both_front_ends_agree_on() {
        // Dim and reasoning are the two styles that read as a line about the
        // answer, not as the answer itself: secondary text in both front ends,
        // and the only styles that are.
        for style in [Style::Dim, Style::Reasoning] {
            assert!(style.is_dim(), "{style:?} is dim");
            assert!(!style.is_bold(), "{style:?} carries no weight of its own");
        }
        // Body text is plain in both, and the rest of the styles carry weight
        // as well as a colour -- weight being the half that survives a reader
        // who has turned colour off.
        let plain = Style::Plain;
        assert!(!plain.is_dim(), "{plain:?} is not dim");
        assert!(!plain.is_bold(), "{plain:?} carries no weight of its own");
        for style in [Style::Yellow, Style::Green, Style::Red] {
            assert!(!style.is_dim(), "{style:?} is not dim");
            assert!(style.is_bold(), "{style:?} is bold");
        }
    }

    /// The modifiers struct is the same shape the two `is_*` methods answer,
    /// so a backend that asks for `modifiers()` cannot drift from one that
    /// asks for `is_dim()`/`is_bold()` separately.
    #[test]
    fn modifiers_agrees_with_is_dim_and_is_bold() {
        for style in [
            Style::Plain,
            Style::Dim,
            Style::Reasoning,
            Style::Yellow,
            Style::Green,
            Style::Red,
        ] {
            let m = style.modifiers();
            assert_eq!(m.dim, style.is_dim(), "{style:?} dim");
            assert_eq!(m.bold, style.is_bold(), "{style:?} bold");
        }
        // The empty default is what a plain line carries.
        assert_eq!(Style::Plain.modifiers(), Modifiers::default());
    }
}

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
//! styles, both from the same enum. Anything that would only matter to one
//! of them -- an SGR code, a ratatui `Color` -- lives on that front end's
//! side of the seam, not on the cell's.

use crate::image::Note;
use crate::tools::ask::Question;
use crate::tools::todo::{self, Todo};
use crate::types::Usage;

use std::time::Duration;

use super::text;

mod call;
pub(super) mod question;
mod replay;
mod todos;

// Internally: the helpers a [`Cell`] builds itself from.
use call::{diff_lines, hint};
use question::question_spans;
use todos::todo_spans;

// Externally: re-exported at `cell::*` because that is what the rest of the
// crate reaches for. The producers live in their own module next to the
// helper that needs them; the re-export is what keeps `cell::` working as
// the entry point for the data layer.
pub use replay::from_messages;
pub use todos::{standing_todos, todo_gutter, todo_head_spans, todo_line_spans};

/// A text style, held as data. Whether it becomes an escape sequence is decided
/// by the front end that renders it, which is why there is no escape-sequence
/// method on this type -- a `Style::Yellow` here is the same value the plain
/// front end's writer and the TUI's `style_of` start from, and the bytes it
/// becomes are decided in only one place each.
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

/// A styled run of text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub style: Style,
    pub text: String,
}

impl Span {
    pub fn new(style: Style, text: impl Into<String>) -> Self {
        Self {
            style,
            text: text.into(),
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
    /// A tool call about to run, with the change it makes if it makes one.
    ToolCall {
        name: String,
        hint: String,
        diff: Vec<DiffLine>,
    },
    /// The question tool's call: what is being asked, and what there is to
    /// choose from.
    ///
    /// A cell of its own rather than a [`Cell::ToolCall`] carrying the raw
    /// arguments, because these are the words the user has to read -- and because
    /// they are read back from the call's own arguments, so a resumed session
    /// shows the questions exactly as the live one did.
    Question(Vec<Question>),
    /// The todo tool's call: the plan for the work in hand.
    ///
    /// A cell of its own rather than a [`Cell::ToolCall`] carrying the raw
    /// arguments, for the reason the question tool's is: the list is the thing
    /// the reader is meant to read, and it is read back from the call's own
    /// arguments, so a resumed session shows the list exactly as it was written
    /// and not as a copy of it that has to be kept somewhere.
    Todo(Vec<Todo>),
    /// A tool result. Only a summary is ever rendered, never the full text.
    ToolResult(String),
    /// A running command's own output, as it arrived.
    ///
    /// The one cell no session log can rebuild: those bytes are the terminal's, the
    /// log keeps a result and not a view of one, and a resumed session therefore
    /// shows the result where a watched one showed this and then the result. It is
    /// marked as a result because that is what it is -- the result, arriving before
    /// the call is over -- and a reader tells the two apart by what they say: a
    /// stream of the command's lines, then the line the call answered with.
    ToolOutput(String),
    /// A dim informational line.
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

    /// A tool call, with the argument summary derived from the raw arguments.
    ///
    /// The question tool is the one call whose arguments are what is shown: its
    /// cell is the questions themselves, because a clipped copy of the wire
    /// format is not something a person can answer. The todo tool is the other:
    /// its arguments are a list a person is meant to read, and a reader who has
    /// to parse the wire format to find the plan has not been shown a plan. An
    /// unreadable call falls back to the raw line -- the interpreter answers it
    /// with the parse failure, and the transcript shows what it was that could
    /// not be read.
    pub fn tool_call(name: &str, args: &str) -> Self {
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
        Cell::ToolCall {
            name: name.to_owned(),
            hint: hint(args),
            diff: diff_lines(name, args),
        }
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
            Cell::ToolCall { .. } | Cell::Approval { .. } | Cell::Question(_) | Cell::Todo(_) => {
                Some(Gutter::new("▸ ", "  ", Style::Yellow))
            }
            // The result and the output of the command it is the result of: one
            // marker, because one is the other arriving early. A result that
            // is a failure is red, both in the content and in the marker --
            // a dim `·` followed by a red message would be jarring, and the
            // marker is what tells the eye which line of the transcript is
            // the result in the first place.
            Cell::ToolResult(text) if text.starts_with("error:") => {
                Some(Gutter::new("· ", "  ", Style::Red))
            }
            Cell::ToolResult(_) | Cell::ToolOutput(_) => Some(Gutter::new("· ", "  ", Style::Dim)),
            Cell::Notice(_) => Some(Gutter::new("  ", "  ", Style::Dim)),
            Cell::Failure(_) => Some(Gutter::new("  ", "  ", Style::Red)),
            Cell::Interrupted => Some(Gutter::new("  ", "  ", Style::Yellow)),
            Cell::Usage { .. } => Some(Gutter::new("  ", "  ", Style::Dim)),
        }
    }

    /// The cell's styled spans, without the columns its [`Cell::gutter`] sets it
    /// in, the separating blank line, or the line ending the painter adds.
    pub fn spans(&self) -> Vec<Span> {
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
                for image in images {
                    let lead = if spans.is_empty() { "" } else { "\n" };
                    spans.push(Span::new(
                        Style::Dim,
                        format!("{lead}{}", image_line(image)),
                    ));
                }
                spans
            }
            Cell::Reasoning(text) => vec![Span::new(Style::Reasoning, text.as_str())],
            Cell::Content(text) => vec![Span::new(Style::Plain, text.as_str())],
            Cell::ToolCall { name, hint, diff } => {
                let mut spans = vec![Span::new(Style::Yellow, format!("{name} {hint}"))];
                // A line of the change is a line of the cell, so it lands in the
                // gutter's continuation columns and lines up under the call.
                for line in diff {
                    spans.push(match &line.kind {
                        DiffKind::Removed => Span::new(Style::Red, format!("\n- {}", line.text)),
                        DiffKind::Added => Span::new(Style::Green, format!("\n+ {}", line.text)),
                        DiffKind::Omitted(n) => {
                            Span::new(Style::Dim, format!("\n… {n} more line(s)"))
                        }
                        // Context: the same column the gutter's continuation
                        // opens in, one space wide of indent so it does not
                        // read as a removed line.
                        DiffKind::Context => Span::new(Style::Plain, format!("\n {}", line.text)),
                        // Hunk header: the line that says where in the file the
                        // change is, dim so it does not compete with the lines
                        // it heads.
                        DiffKind::Hunk {
                            old_start,
                            old_count,
                            new_start,
                            new_count,
                        } => Span::new(
                            Style::Dim,
                            format!("\n@@ -{old_start},{old_count} +{new_start},{new_count} @@"),
                        ),
                    });
                }
                spans
            }
            Cell::ToolResult(result) => {
                let (style, text) = call::result_summary(result);
                vec![Span::new(style, text)]
            }
            Cell::ToolOutput(text) => vec![Span::new(Style::Dim, text.as_str())],
            Cell::Notice(text) => vec![Span::new(Style::Dim, text.as_str())],
            Cell::Failure(text) => vec![Span::new(Style::Red, format!("error: {text}"))],
            Cell::Interrupted => vec![Span::new(Style::Yellow, "⏹ interrupted (Ctrl-C)")],
            Cell::Usage { usage, stream } => {
                vec![Span::new(Style::Dim, usage_line(usage, *stream))]
            }
            Cell::Approval { name, hint } => vec![Span::new(
                Style::Yellow,
                format!("{name} {hint} — run it? [y/N] "),
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
            // A tool call is announced on a line of its own, and a replayed user
            // line is set off from whatever preceded it.
            Cell::ToolCall { .. } | Cell::User { .. } | Cell::Question(_) | Cell::Todo(_) => true,
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
fn usage_line(usage: &Usage, stream: Duration) -> String {
    let cache = match usage.cache() {
        Some(c) => format!("hit {}/miss {}", c.hit, c.miss),
        None => "cache —".to_string(),
    };
    let mut line = format!(
        "tokens: in {}/{} ({cache}) · out {}",
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
        let call = Cell::tool_call("Bash", "{}");
        assert!(call.gap_after(false), "a tool call gets its own line");
        assert!(Cell::user("hi").gap_after(false));
        assert!(!Cell::Interrupted.gap_after(true));
        assert!(!Cell::Notice("n".into()).gap_after(false));
        assert!(!Cell::ToolResult("r".into()).gap_after(false));
    }

    #[test]
    fn only_the_approval_question_leaves_its_line_open() {
        assert!(!Cell::approval("Bash", "{}").ends_line());
        assert!(Cell::tool_call("Bash", "{}").ends_line());
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
    fn a_tool_call_with_context_and_hunk_lines_renders_them() {
        // The new variants of `DiffKind` are how a real unified diff is
        // painted: context lines have a single leading space and the plain
        // style, hunk headers name the line ranges in dim. The test pins the
        // shape of both, since both front ends go through `spans()`.
        let cell = Cell::ToolCall {
            name: "Edit".into(),
            hint: "a.rs".into(),
            diff: vec![
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
            ],
        };
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
        assert_eq!(
            cell.spans(),
            vec![
                Span::new(Style::Dim, "[image jpeg · 3 bytes]"),
                Span::new(Style::Dim, "\n[image webp · 4 bytes]"),
            ]
        );
    }

    #[test]
    fn tool_cells_carry_the_extracted_hint() {
        match Cell::tool_call("Bash", r#"{"command":"ls"}"#) {
            Cell::ToolCall { name, hint, diff } => {
                assert_eq!(name, "Bash");
                assert_eq!(hint, "ls");
                assert!(diff.is_empty(), "a command has no lines to show");
            }
            other => panic!("expected a tool call, got {other:?}"),
        }
        let cell = Cell::tool_call("Bash", r#"{"command":"ls"}"#);
        assert_eq!(cell.spans(), vec![Span::new(Style::Yellow, "Bash ls")]);
        assert_eq!(cell.gutter().unwrap().head, "▸ ");
        let cell = Cell::approval("Bash", r#"{"command":"rm -rf /"}"#);
        assert_eq!(
            cell.spans(),
            vec![Span::new(Style::Yellow, "Bash rm -rf / — run it? [y/N] ")]
        );
        assert_eq!(
            cell.gutter().unwrap().head,
            "▸ ",
            "the gate is a call, and is marked as one"
        );
    }

    /// A command's own output is kept whole, unlike the result of the call: it is
    /// what the command printed, and a reader watching a build is reading it.
    #[test]
    fn tool_output_renders_the_lines_it_was_given() {
        let cell = Cell::ToolOutput("Compiling foo\nwarning: unused\n".into());
        assert_eq!(
            cell.spans(),
            vec![Span::new(Style::Dim, "Compiling foo\nwarning: unused\n")]
        );
        // Marked as a result, since it is one arriving early, and set in like one.
        assert_eq!(cell.gutter(), Cell::ToolResult(String::new()).gutter());
        assert!(cell.ends_line());
        assert!(!cell.is_text_block());
        assert!(!cell.gap_after(true));
    }

    #[test]
    fn tool_result_renders_first_line_and_size_only() {
        let result = "exit_code: 3\n--- stdout ---\nSECRET_BODY";
        let spans = Cell::ToolResult(result.into()).spans();
        // A Bash result: the exit code, and only the exit code. The
        // transcript's rule is that a tool result's summary is metadata,
        // never content -- and the first line of stdout is content.
        assert_eq!(spans, vec![Span::new(Style::Dim, "exit_code: 3")]);
        // An empty result: no recognized prefix, the default summary
        // (first line and byte count) is what falls out.
        let spans = Cell::ToolResult(String::new()).spans();
        assert_eq!(spans, vec![Span::new(Style::Dim, " · 0 bytes")]);
    }

    /// An `error:` result is the one case where the style changes: failures
    /// are red, so a reader can tell success from failure at a glance.
    #[test]
    fn a_tool_result_starting_with_error_is_red() {
        let spans = Cell::ToolResult("error: file not found".into()).spans();
        assert_eq!(spans, vec![Span::new(Style::Red, "error: file not found")]);
        // The gutter follows the content: a dim marker in front of a red
        // message would be a line that fights itself, and the marker is
        // the only thing that says "this is a tool result".
        let gutter = Cell::ToolResult("error: file not found".into())
            .gutter()
            .unwrap();
        assert_eq!(gutter.style, Style::Red);
        assert_eq!(gutter.head, "· ");
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
            vec![Span::new(Style::Dim, "tokens: in 20/28 (cache —) · out 8")]
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
                "tokens: in 20/28 (hit 12/miss 8) · out 8"
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
                "tokens: in 20/532 (cache —) · out 512 · 410 token/s"
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
        assert_eq!(spans[0].text, "tokens: in 20/532 (cache —) · out 512");
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
        assert_eq!(spans[0].text, "tokens: in 20/20 (cache —) · out 0");
    }

    /// One of every kind of cell, so a rule about cells can be asked of all of them.
    fn one_of_each() -> Vec<Cell> {
        vec![
            Cell::user("hi"),
            Cell::Reasoning("hmm".into()),
            Cell::Content("answer".into()),
            Cell::tool_call("Bash", r#"{"command":"ls"}"#),
            Cell::tool_call(
                "AskUserQuestion",
                r#"{"questions":[{"id":"a","question":"Which?"}]}"#,
            ),
            Cell::tool_call("TodoWrite", r#"{"todos":[{"content":"Run the gates"}]}"#),
            Cell::ToolResult("ok".into()),
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
}

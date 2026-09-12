//! Transcript cells: the renderer's output modelled as values.
//!
//! A cell renders itself to styled spans and does nothing else -- no I/O, no
//! mutable state, no knowledge of the terminal. Live streaming and session
//! replay both produce cells and hand them to the same painter, so a session
//! that is resumed looks exactly like the one that was watched live.

use crate::image::{self, Note};
use crate::tools::ask::Question;
use crate::tools::todo::{self, Status, Todo};
use crate::types::{Message, Role, Usage};

use std::time::Duration;

use super::text;

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffKind {
    /// A line the call takes out.
    Removed,
    /// A line the call puts in.
    Added,
    /// Lines the cell does not show, and how many there are.
    Omitted(usize),
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
            // marker, because one is the other arriving early.
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
                    spans.push(match line.kind {
                        DiffKind::Removed => Span::new(Style::Red, format!("\n- {}", line.text)),
                        DiffKind::Added => Span::new(Style::Green, format!("\n+ {}", line.text)),
                        DiffKind::Omitted(n) => {
                            Span::new(Style::Dim, format!("\n… {n} more line(s)"))
                        }
                    });
                }
                spans
            }
            Cell::ToolResult(result) => vec![Span::new(Style::Dim, summary(result))],
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

/// Flatten history messages into cells.
///
/// This is the replay half of the renderer: a resumed session goes through the
/// same cells as a live one, so the two cannot drift apart. Tool messages
/// contribute only a summary, so replaying never re-dumps a tool's output.
pub fn from_messages(messages: &[Message]) -> Vec<Cell> {
    let mut cells = Vec::new();
    for m in messages {
        match m.role {
            Role::User => {
                if let Some(c) = &m.content {
                    let text = c.text();
                    // An injected instructions message is machinery in the
                    // shape of a user message — the only shape that can be
                    // appended after a tool window — and folds into the same
                    // notice the live turn emitted, never into a line shown
                    // to the reader as their own.
                    match crate::agents_md::injected_dir(&text) {
                        Some(dir) => cells.push(Cell::Notice(crate::agents_md::notice_text(dir))),
                        None => cells.push(Cell::User {
                            text,
                            images: c
                                .images()
                                .iter()
                                .filter_map(|url| image::note(url))
                                .collect(),
                        }),
                    }
                }
            }
            Role::Assistant => {
                if let Some(r) = &m.reasoning_content
                    && !r.is_empty()
                {
                    cells.push(Cell::Reasoning(r.clone()));
                }
                if let Some(text) = m.text()
                    && !text.is_empty()
                {
                    cells.push(Cell::Content(text));
                }
                for call in m.tool_calls.iter().flatten() {
                    cells.push(Cell::tool_call(
                        &call.function.name,
                        &call.function.arguments,
                    ));
                }
            }
            Role::Tool => {
                if let Some(text) = m.text() {
                    cells.push(Cell::ToolResult(text));
                }
            }
            // The system prompt is a compile-time constant and is never part of
            // a session log, so there is nothing to replay for it.
            Role::System => {}
        }
    }
    cells
}

/// The argument summary shown for a tool call: the interesting argument when the
/// call carries one, otherwise the raw arguments clipped to one line's worth of
/// columns.
///
/// A glob call is the one whose interesting arguments are not `command` or
/// `file_path`: the pattern is the question and the directory is where it is
/// asked, and both are shown, because a pattern asked of the wrong tree is the
/// one thing about the call a reader can catch before it runs.
fn hint(args: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return text::truncate(args, HINT_COLUMNS).to_owned();
    };
    if let Some(named) = v
        .get("command")
        .or_else(|| v.get("file_path"))
        .and_then(|c| c.as_str())
    {
        return named.to_owned();
    }
    if let Some(pattern) = v.get("pattern").and_then(|p| p.as_str()) {
        return match v.get("path").and_then(|p| p.as_str()) {
            Some(dir) => format!("{pattern} in {dir}"),
            None => pattern.to_owned(),
        };
    }
    text::truncate(args, HINT_COLUMNS).to_owned()
}

/// Columns of raw arguments kept when they cannot be summarized by name.
const HINT_COLUMNS: usize = 80;

/// How many lines of a change a tool call shows before it says how many are left.
///
/// A call is announced before it runs, and an edit can be hundreds of lines: what
/// the announcement is for is seeing what is about to happen, not reading the
/// whole file. The rest is one line, so a long change costs a screenful at most.
const DIFF_LINES: usize = 12;

/// The change a call makes, for the calls that make one.
///
/// The arguments are the wire format of a tool call, which is also what replay
/// reads out of the session log, so live and replayed turns show the same lines
/// without a second source for either one.
fn diff_lines(name: &str, args: &str) -> Vec<DiffLine> {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(args) else {
        return Vec::new();
    };
    let side = |key: &str, kind: DiffKind| -> Vec<DiffLine> {
        args.get(key)
            .and_then(|s| s.as_str())
            .map(|text| {
                text.lines()
                    .map(|line| DiffLine {
                        kind,
                        text: line.to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut lines = match name {
        // The order a diff is read in: what goes, then what arrives.
        "Edit" => {
            let mut lines = side("old_string", DiffKind::Removed);
            lines.extend(side("new_string", DiffKind::Added));
            lines
        }
        "Write" => side("content", DiffKind::Added),
        _ => return Vec::new(),
    };
    if lines.len() > DIFF_LINES {
        let omitted = lines.len() - DIFF_LINES;
        lines.truncate(DIFF_LINES);
        lines.push(DiffLine {
            kind: DiffKind::Omitted(omitted),
            text: String::new(),
        });
    }
    lines
}

/// The line an attached image contributes to the user's cell: what it is and how
/// much of it there is, which is as much as the message says.
fn image_line(image: &Note) -> String {
    format!("[image {} · {} bytes]", image.format, image.bytes)
}

/// The todo tool's cell: how far the list has got, then the list.
///
/// The head is the same line the model is answered with, so what the reader is
/// shown and what the model was told cannot disagree about the same list.
fn todo_spans(todos: &[Todo]) -> Vec<Span> {
    let mut spans = vec![Span::new(
        Style::Yellow,
        format!("{} {}", crate::tools::TODO_NAME, todo::summary(todos)),
    )];
    // The tasks are lines of this one block, so the mark is written in front of
    // each of them rather than set in a gutter of its own: the block already
    // carries the cell's gutter, and a second one inside it would be columns the
    // list does not have to spend.
    for todo in todos {
        spans.push(Span::new(
            todo_style(todo.status),
            format!("\n{}", todo_head(todo.status)),
        ));
        spans.extend(todo_line_spans(todo));
    }
    spans
}

/// What opens a task's line: the mark, and the blank that sets its words apart
/// from it.
fn todo_head(status: Status) -> &'static str {
    match status {
        Status::Pending => "☐ ",
        Status::InProgress => "▸ ",
        Status::Completed => "✔ ",
    }
}

/// What a task's line is painted in.
///
/// The task in hand is the one painted with attention and the finished ones are
/// faded; what is not started yet is left plain, because it is the list's own
/// reading matter rather than a note about the list.
fn todo_style(status: Status) -> Style {
    match status {
        Status::Pending => Style::Plain,
        Status::InProgress => Style::Yellow,
        Status::Completed => Style::Dim,
    }
}

/// The columns a task's row opens in, and what its wrapped rows open in: the mark
/// and a blank, then two blanks under them.
///
/// The same [`Gutter`] a cell carries, for a row that is not a cell. A task, not a
/// list: a task long enough to wrap must keep its own left edge, or its second row
/// comes back to column zero and reads as a task of its own.
pub fn todo_gutter(todo: &Todo) -> Gutter {
    Gutter {
        head: todo_head(todo.status),
        rest: "  ",
        style: todo_style(todo.status),
    }
}

/// One task's words, without the mark they are set after: what the cell's line and
/// the standing block's row are both made of.
pub fn todo_line_spans(todo: &Todo) -> Vec<Span> {
    vec![Span::new(todo_style(todo.status), todo.content.clone())]
}

/// The list a transcript leaves standing: the last one a call wrote, which is the
/// one that is still true.
///
/// A fold over the cells rather than a second piece of state to keep in step --
/// and the same fold a resumed session goes through, since replay produces the
/// same cells, so a session read back from the log keeps exactly the list in view
/// that the session watched live had.
///
/// `None` when no call has written a list, and when the last one written cleared
/// it: an empty list is how a list is taken down, and a list that has been taken
/// down is not one to keep in view.
pub fn standing_todos(cells: &[Cell]) -> Option<&[Todo]> {
    let todos = cells.iter().rev().find_map(|cell| match cell {
        Cell::Todo(todos) => Some(todos.as_slice()),
        _ => None,
    })?;
    (!todos.is_empty()).then_some(todos)
}

/// One-line summary of a tool result: its first line and how much text came
/// back.
/// The question tool's cell: what is being asked, and what there is to choose
/// from.
///
/// One block per question, in the order they were asked, with the options
/// numbered: the number is what a front end reading a typed answer matches, and
/// what the panel's own keys jump to. The numbers and the descriptions are dim
/// because the label is what the answer is made of; the question itself is
/// painted like the call it is, since that is what it is.
fn question_spans(questions: &[Question]) -> Vec<Span> {
    let mut spans = Vec::new();
    for (i, question) in questions.iter().enumerate() {
        let lead = if i == 0 { "" } else { "\n\n" };
        let heading = if question.header.is_empty() {
            question.question.clone()
        } else {
            format!("{}: {}", question.header, question.question)
        };
        spans.push(Span::new(Style::Yellow, format!("{lead}{heading}")));
        if question.multi_select {
            spans.push(Span::new(Style::Dim, " · choose any"));
        }
        if question.options.is_empty() {
            spans.push(Span::new(Style::Dim, "\n  answer in your own words"));
            continue;
        }
        for (n, option) in question.options.iter().enumerate() {
            spans.push(Span::new(Style::Dim, format!("\n  {}. ", n + 1)));
            spans.push(Span::new(Style::Plain, option.label.clone()));
            if !option.description.is_empty() {
                spans.push(Span::new(Style::Dim, format!(" — {}", option.description)));
            }
        }
    }
    spans
}

/// The summary shown for a tool result: its first line and how much there is of
/// it. A result is often a screenful of a file or a command's output, and what the
/// transcript is for is the shape of the turn, not the contents of every result.
fn summary(result: &str) -> String {
    format!(
        "{} · {} bytes",
        result.lines().next().unwrap_or(""),
        result.len()
    )
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
    use crate::types::{ToolCall, ToolCallFunction};

    fn assistant(
        reasoning: Option<&str>,
        content: Option<&str>,
        calls: Option<Vec<ToolCall>>,
    ) -> Message {
        Message {
            role: Role::Assistant,
            content: content.map(Into::into),
            reasoning_content: reasoning.map(str::to_owned),
            tool_calls: calls,
            tool_call_id: None,
            thinking: None,
        }
    }

    fn call(name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: "call_1".into(),
            r#type: "function".into(),
            function: ToolCallFunction {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

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
    fn replay_reads_the_users_images_out_of_the_message() {
        let message = Message::user_with_images(
            "what is this?",
            vec!["data:image/png;base64,Zm9vYmFy".into()],
        );
        assert_eq!(
            from_messages(&[message]),
            vec![Cell::User {
                text: "what is this?".into(),
                images: vec![Note {
                    format: "png".into(),
                    bytes: 6,
                }],
            }]
        );
        // A part the transcript cannot read contributes no line rather than a
        // made-up one; the bytes still go to the backend.
        let foreign = Message::user_with_images("hi", vec!["https://example.com/cat.png".into()]);
        assert_eq!(from_messages(&[foreign]), vec![Cell::user("hi")]);
    }

    #[test]
    fn hint_prefers_command_then_file_path() {
        assert_eq!(hint(r#"{"command":"ls -la"}"#), "ls -la");
        assert_eq!(hint(r#"{"file_path":"/a/b.txt"}"#), "/a/b.txt");
        // command wins when both are present
        assert_eq!(hint(r#"{"command":"ls","file_path":"/a"}"#), "ls");
    }

    /// A glob's line is what it asks for, and of where: the pattern alone when
    /// the call takes the working directory, both when it names one.
    #[test]
    fn hint_reads_a_glob_as_the_question_it_is() {
        assert_eq!(hint(r#"{"pattern":"**/*.rs"}"#), "**/*.rs");
        assert_eq!(
            hint(r#"{"pattern":"**/*.rs","path":"/a/src"}"#),
            "**/*.rs in /a/src"
        );
    }

    #[test]
    fn hint_falls_back_to_clipped_raw_arguments() {
        assert_eq!(hint("not json at all"), "not json at all");
        assert_eq!(hint(r#"{"other":"x"}"#), r#"{"other":"x"}"#);
        // a long raw argument is clipped to one line's worth of columns
        let long = "x".repeat(HINT_COLUMNS + 40);
        assert_eq!(hint(&long).chars().count(), HINT_COLUMNS);
    }

    #[test]
    fn hint_clips_wide_characters_by_column() {
        // 60 ideographs are 120 columns but only 60 chars; the clip keeps 40
        let wide = "\u{6df1}".repeat(60);
        let got = hint(&wide);
        assert_eq!(text::width(&got), HINT_COLUMNS);
        assert_eq!(got.chars().count(), HINT_COLUMNS / 2);
    }

    /// The arguments an edit arrives with, as the wire format spells them.
    fn edit(old: &str, new: &str) -> String {
        serde_json::json!({
            "file_path": "src/main.rs",
            "old_string": old,
            "new_string": new,
        })
        .to_string()
    }

    #[test]
    fn an_edit_shows_what_leaves_and_what_arrives() {
        let lines = diff_lines("Edit", &edit("let a = 1;\nlet b = 2;", "let a = 3;"));
        assert_eq!(
            lines,
            vec![
                DiffLine {
                    kind: DiffKind::Removed,
                    text: "let a = 1;".into(),
                },
                DiffLine {
                    kind: DiffKind::Removed,
                    text: "let b = 2;".into(),
                },
                DiffLine {
                    kind: DiffKind::Added,
                    text: "let a = 3;".into(),
                },
            ],
            "what goes is shown before what arrives"
        );
    }

    #[test]
    fn a_deletion_shows_only_what_goes() {
        let lines = diff_lines("Edit", &edit("gone", ""));
        assert_eq!(
            lines,
            vec![DiffLine {
                kind: DiffKind::Removed,
                text: "gone".into(),
            }]
        );
    }

    #[test]
    fn a_write_shows_what_it_puts_there() {
        let args = serde_json::json!({ "file_path": "a.txt", "content": "one\ntwo\n" }).to_string();
        let lines = diff_lines("Write", &args);
        assert_eq!(lines.len(), 2, "a trailing newline does not start a line");
        assert!(lines.iter().all(|l| l.kind == DiffKind::Added));
    }

    #[test]
    fn a_long_change_ends_in_a_count_of_the_rest() {
        let many = (0..DIFF_LINES + 5)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = diff_lines("Write", &serde_json::json!({ "content": many }).to_string());
        assert_eq!(lines.len(), DIFF_LINES + 1, "the lines and the count");
        assert_eq!(lines.last().unwrap().kind, DiffKind::Omitted(5));
    }

    #[test]
    fn only_a_change_has_lines_to_show() {
        assert!(diff_lines("Bash", r#"{"command":"ls"}"#).is_empty());
        assert!(diff_lines("Read", r#"{"file_path":"x"}"#).is_empty());
        assert!(diff_lines("Edit", "not json at all").is_empty());
        assert!(
            diff_lines("Edit", r#"{"file_path":"x"}"#).is_empty(),
            "the strings are what makes it a change"
        );
    }

    #[test]
    fn the_rendered_call_is_its_header_then_its_lines() {
        let cell = Cell::tool_call("Edit", &edit("old", "new"));
        assert_eq!(cell.gutter().unwrap().head, "▸ ");
        assert_eq!(
            cell.spans(),
            vec![
                Span::new(Style::Yellow, "Edit src/main.rs"),
                Span::new(Style::Red, "\n- old"),
                Span::new(Style::Green, "\n+ new"),
            ],
            "each line of the change is a line of the cell, set in with it"
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
        assert_eq!(
            spans,
            vec![Span::new(Style::Dim, "exit_code: 3 · 39 bytes")]
        );
        // an empty result still reports its (zero) size
        let spans = Cell::ToolResult(String::new()).spans();
        assert_eq!(spans, vec![Span::new(Style::Dim, " · 0 bytes")]);
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

    #[test]
    fn from_messages_orders_reasoning_content_then_calls() {
        let cells = from_messages(&[
            Message::user("take a look"),
            assistant(
                Some("let me think"),
                Some("running it"),
                Some(vec![call("Bash", r#"{"command":"ls -la"}"#)]),
            ),
            Message::tool("call_1", "exit_code: 0\n--- stdout ---\nSECRET_BODY"),
        ]);
        assert_eq!(
            cells,
            vec![
                Cell::user("take a look"),
                Cell::Reasoning("let me think".into()),
                Cell::Content("running it".into()),
                Cell::tool_call("Bash", r#"{"command":"ls -la"}"#),
                Cell::ToolResult("exit_code: 0\n--- stdout ---\nSECRET_BODY".into()),
            ]
        );
    }

    #[test]
    fn from_messages_skips_what_is_not_replayed() {
        // system messages carry the fixed prompt and are never in a log
        assert!(from_messages(&[Message::system("must not appear")]).is_empty());
        // empty assistant fields contribute nothing
        assert!(from_messages(&[assistant(Some(""), Some(""), None)]).is_empty());
        assert!(from_messages(&[]).is_empty());
    }

    /// An injected instructions message is machinery in the shape of a user
    /// message: the replay folds it into the same notice the live turn
    /// emitted, never into a line shown to the reader as their own.
    #[test]
    fn from_messages_folds_an_injected_message_into_a_notice() {
        let message = Message::user(format!(
            "{}{}\n\nthe directory's rules",
            crate::agents_md::INJECT_LEAD,
            "/workspace/sub"
        ));
        assert_eq!(
            from_messages(&[message]),
            vec![Cell::Notice(crate::agents_md::notice_text(
                "/workspace/sub"
            ))]
        );
    }

    #[test]
    fn a_user_line_is_never_taken_for_an_injected_message() {
        let cells = from_messages(&[Message::user("Project instructions are a good idea")]);
        assert!(matches!(cells[0], Cell::User { .. }));
    }

    /// The question tool's cell is read back out of the call's own arguments, so
    /// a resumed session shows the questions exactly as the live one did -- the
    /// options and all, which is what a reader needs to make sense of the answer
    /// line that follows.
    #[test]
    fn replay_of_a_question_call_is_the_questions_again() {
        let args =
            r#"{"questions":[{"id":"auth","question":"Which auth?","options":[{"label":"JWT"}]}]}"#;
        let cells = from_messages(&[
            assistant(None, None, Some(vec![call("AskUserQuestion", args)])),
            Message::tool("call_1", "auth: JWT"),
        ]);
        assert_eq!(
            cells,
            vec![
                Cell::tool_call("AskUserQuestion", args),
                Cell::ToolResult("auth: JWT".into()),
            ]
        );
        assert!(matches!(cells[0], Cell::Question(_)));
    }

    #[test]
    fn the_question_tool_shows_its_questions_and_not_its_arguments() {
        let cell = Cell::tool_call(
            "AskUserQuestion",
            &serde_json::json!({"questions": [{
                "id": "auth",
                "header": "Auth",
                "question": "Which auth should we use?",
                "options": [
                    {"label": "JWT", "description": "one token for the API"},
                    {"label": "Session cookie"}
                ],
                "multi_select": true
            }]})
            .to_string(),
        );
        let Cell::Question(questions) = &cell else {
            panic!("the questions are the cell, not {cell:?}");
        };
        assert_eq!(questions.len(), 1);
        assert_eq!(
            cell.spans(),
            vec![
                Span::new(Style::Yellow, "Auth: Which auth should we use?"),
                Span::new(Style::Dim, " · choose any"),
                Span::new(Style::Dim, "\n  1. "),
                Span::new(Style::Plain, "JWT"),
                Span::new(Style::Dim, " — one token for the API"),
                // An option the model gave no description for is its label
                // alone: no separator with nothing behind it.
                Span::new(Style::Dim, "\n  2. "),
                Span::new(Style::Plain, "Session cookie"),
            ]
        );
    }

    #[test]
    fn a_question_with_nothing_to_pick_from_says_so() {
        let cell = Cell::tool_call(
            "AskUserQuestion",
            r#"{"questions":[{"id":"a","question":"Which?"}]}"#,
        );
        assert_eq!(
            cell.spans(),
            vec![
                Span::new(Style::Yellow, "Which?"),
                Span::new(Style::Dim, "\n  answer in your own words"),
            ]
        );
    }

    #[test]
    fn arguments_the_question_tool_cannot_read_are_still_a_call() {
        // The interpreter answers this one with the parse failure; the transcript
        // shows the line that could not be read.
        let cell = Cell::tool_call("AskUserQuestion", r#"{"questions":"nonsense"}"#);
        assert!(matches!(cell, Cell::ToolCall { .. }), "was {cell:?}");
    }

    /// The todo tool's cell is read back out of the call's own arguments, so a
    /// resumed session shows the list exactly as it was written -- which is also
    /// what lets the list be a fold of the log rather than a second thing to keep
    /// in step with it.
    #[test]
    fn replay_of_a_todo_call_is_the_same_list() {
        let args = r#"{"todos":[{"content":"Run the gates","status":"in_progress"}]}"#;
        let cells = from_messages(&[
            assistant(None, None, Some(vec![call("TodoWrite", args)])),
            Message::tool("call_1", "todo list updated (0/1 done)"),
        ]);
        assert_eq!(
            cells,
            vec![
                Cell::tool_call("TodoWrite", args),
                Cell::ToolResult("todo list updated (0/1 done)".into()),
            ]
        );
        assert!(matches!(cells[0], Cell::Todo(_)));
    }

    #[test]
    fn the_todo_tool_shows_the_list_and_not_its_arguments() {
        let cell = Cell::tool_call(
            "TodoWrite",
            &serde_json::json!({"todos": [
                {"content": "Add the parse function", "status": "completed"},
                {"content": "Draw the cell", "status": "in_progress"},
                {"content": "Run the gates"}
            ]})
            .to_string(),
        );
        let Cell::Todo(todos) = &cell else {
            panic!("the list is the cell, not {cell:?}");
        };
        assert_eq!(todos.len(), 3);
        assert_eq!(
            cell.spans(),
            vec![
                // The head is the same string the model was answered with.
                Span::new(Style::Yellow, "TodoWrite 1/3 done"),
                // A task is its mark and its words, both in the style of its state.
                Span::new(Style::Dim, "\n✔ "),
                Span::new(Style::Dim, "Add the parse function"),
                Span::new(Style::Yellow, "\n▸ "),
                Span::new(Style::Yellow, "Draw the cell"),
                // Not started yet is plain: it is the list's reading matter, not a
                // note about the list.
                Span::new(Style::Plain, "\n☐ "),
                Span::new(Style::Plain, "Run the gates"),
            ]
        );
    }

    #[test]
    fn a_cleared_list_is_a_cell_with_nothing_under_it() {
        let cell = Cell::tool_call("TodoWrite", r#"{"todos":[]}"#);
        assert_eq!(
            cell.spans(),
            vec![Span::new(Style::Yellow, "TodoWrite list cleared")]
        );
    }

    /// Every mark takes one column and the three states are told apart by them.
    ///
    /// The column is the point: a mark that measured two would push its own line's
    /// words out of the column every other task's words are in, and the column of
    /// words is the only thing that makes a list of twenty lines scannable. The
    /// gutter's two columns are pinned to the rest of the vocabulary's, so that a
    /// task's wrapped rows continue where its own words are rather than a column
    /// either side of them.
    #[test]
    fn every_task_mark_is_one_column_and_the_states_are_distinct() {
        let heads = [Status::Pending, Status::InProgress, Status::Completed].map(todo_head);
        for head in heads {
            assert_eq!(text::width(head.trim_end()), 1, "{head:?} takes one column");
            assert_eq!(text::width(head), MARKER_COLUMNS, "{head:?} and its blank");
        }
        let states: std::collections::HashSet<&str> =
            heads.map(str::trim_end).into_iter().collect();
        assert_eq!(states.len(), 3, "the three states read differently");
        for status in [Status::Pending, Status::InProgress, Status::Completed] {
            let gutter = todo_gutter(&Todo {
                content: "x".into(),
                status,
            });
            assert_eq!(gutter.width(), MARKER_COLUMNS);
            assert_eq!(text::width(gutter.rest), MARKER_COLUMNS);
        }
    }

    #[test]
    fn arguments_the_todo_tool_cannot_read_are_still_a_call() {
        // The model is answered with the parse failure; the transcript shows the
        // call that could not be read rather than a list nobody wrote.
        let cell = Cell::tool_call("TodoWrite", r#"{"todos":"a plan"}"#);
        assert!(matches!(cell, Cell::ToolCall { .. }), "was {cell:?}");
    }

    fn written(todos: serde_json::Value) -> Cell {
        Cell::tool_call("TodoWrite", &todos.to_string())
    }

    /// The list that stands is a fold of the transcript -- the last list a call
    /// wrote, which is the one still true. There is no second copy of it to keep
    /// in step, which is the whole reason the tool writes into the log instead of
    /// holding the list anywhere.
    #[test]
    fn the_list_that_stands_is_the_last_one_written() {
        let cells = vec![
            Cell::Content("thinking about it".into()),
            written(serde_json::json!({"todos": [{"content": "a"}]})),
            Cell::ToolResult("todo list updated (0/1 done)".into()),
            // The second write replaces the first rather than following it: what
            // the model sends is the whole list, not a change to it.
            written(serde_json::json!({"todos": [
                {"content": "a", "status": "completed"},
                {"content": "b", "status": "in_progress"}
            ]})),
        ];
        let standing = standing_todos(&cells).expect("a list was written");
        assert_eq!(standing.len(), 2);
        assert_eq!(standing[1].content, "b");
        assert_eq!(standing[1].status, Status::InProgress);
    }

    #[test]
    fn a_list_that_was_cleared_is_not_one_to_keep_in_view() {
        // An empty list is how a list is taken down, and the screen has to stop
        // pinning it: a block that outlived the work it described is a block the
        // reader learns to ignore.
        let cleared = vec![
            written(serde_json::json!({"todos": [{"content": "a"}]})),
            written(serde_json::json!({"todos": []})),
        ];
        assert_eq!(standing_todos(&cleared), None);
        // And no list at all is the same answer: nothing to pin.
        assert_eq!(standing_todos(&[Cell::Content("just talk".into())]), None);
        assert_eq!(standing_todos(&[]), None);
    }

    /// A call that could not be read is a [`Cell::ToolCall`] and not a list, so the
    /// fold steps over it: the list that stands is still the last one that was
    /// actually written.
    #[test]
    fn a_call_that_could_not_be_read_leaves_the_standing_list_alone() {
        let cells = vec![
            written(serde_json::json!({"todos": [{"content": "a"}]})),
            Cell::tool_call("TodoWrite", r#"{"todos":"a plan"}"#),
        ];
        let standing = standing_todos(&cells).expect("the first write still stands");
        assert_eq!(standing.len(), 1);
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

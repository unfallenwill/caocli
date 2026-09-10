//! Transcript cells: the renderer's output modelled as values.
//!
//! A cell renders itself to styled spans and does nothing else -- no I/O, no
//! mutable state, no knowledge of the terminal. Live streaming and session
//! replay both produce cells and hand them to the same painter, so a session
//! that is resumed looks exactly like the one that was watched live.

use crate::types::{Message, Role, Usage};

use std::time::Duration;

use super::text;

/// A text style, held as data. Whether it becomes an escape sequence is decided
/// once, at paint time, so no cell has to know whether colors are enabled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Style {
    /// Unstyled text: body text.
    Plain,
    /// Secondary text: tool results, notices.
    Dim,
    /// The thinking behind an answer.
    ///
    /// Its own style rather than `Dim`, because faint text is not a distinction
    /// every terminal makes -- SGR 2 is ignored by some, and on those a reader
    /// cannot tell thinking from an answer -- while a ground is.
    ///
    /// Only the line-drawing front end fills that ground across the width: it has
    /// the row in hand and redraws it, where the plain one writes a line as it
    /// arrives and has nothing to paint the blanks with afterwards.
    Reasoning,
    /// Attention text: tool calls, interruptions.
    Yellow,
    /// Text a change adds.
    Green,
    /// Failures, written to stderr.
    Red,
}

impl Style {
    /// The escape sequence that opens this style.
    ///
    /// [`Style::Reasoning`] sets a foreground as well as a ground, and that is
    /// deliberate: a fixed dark ground under the terminal's own foreground is
    /// unreadable on a light theme, so the pair is chosen together rather than
    /// inherited.
    pub fn code(self) -> &'static str {
        match self {
            Style::Plain => "",
            Style::Dim => "\x1b[2m",
            Style::Reasoning => "\x1b[38;5;245;48;5;236m",
            Style::Yellow => "\x1b[33m",
            Style::Green => "\x1b[32m",
            Style::Red => "\x1b[31m",
        }
    }
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
    /// A user line. Only replay produces one: live input is echoed by the line
    /// editor's own prompt, not by the renderer.
    User(String),
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
    /// A tool result. Only a summary is ever rendered, never the full text.
    ToolResult(String),
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

impl Cell {
    /// A tool call, with the argument summary derived from the raw arguments.
    pub fn tool_call(name: &str, args: &str) -> Self {
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

    /// The cell's styled spans, without the separating blank line or the line
    /// ending the painter adds.
    pub fn spans(&self) -> Vec<Span> {
        match self {
            Cell::User(text) => vec![
                Span::new(Style::Dim, "› "),
                Span::new(Style::Plain, text.as_str()),
            ],
            Cell::Reasoning(text) => vec![Span::new(Style::Reasoning, text.as_str())],
            Cell::Content(text) => vec![Span::new(Style::Plain, text.as_str())],
            Cell::ToolCall { name, hint, diff } => {
                let mut spans = vec![Span::new(Style::Yellow, format!("▸ {name} {hint}"))];
                for line in diff {
                    spans.push(match line.kind {
                        DiffKind::Removed => Span::new(Style::Red, format!("\n  - {}", line.text)),
                        DiffKind::Added => Span::new(Style::Green, format!("\n  + {}", line.text)),
                        DiffKind::Omitted(n) => {
                            Span::new(Style::Dim, format!("\n  … {n} more line(s)"))
                        }
                    });
                }
                spans
            }
            Cell::ToolResult(result) => vec![Span::new(Style::Dim, summary(result))],
            Cell::Notice(text) => vec![Span::new(Style::Dim, text.as_str())],
            Cell::Failure(text) => vec![Span::new(Style::Red, format!("error: {text}"))],
            Cell::Interrupted => vec![Span::new(Style::Yellow, "⏹ interrupted (Ctrl-C)")],
            Cell::Usage { usage, stream } => {
                vec![Span::new(Style::Dim, usage_line(usage, *stream))]
            }
            Cell::Approval { name, hint } => vec![Span::new(
                Style::Yellow,
                format!("▸ {name} {hint} — run it? [y/N] "),
            )],
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
            Cell::ToolCall { .. } | Cell::User(_) => true,
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
                    cells.push(Cell::User(c.clone()));
                }
            }
            Role::Assistant => {
                if let Some(r) = &m.reasoning_content
                    && !r.is_empty()
                {
                    cells.push(Cell::Reasoning(r.clone()));
                }
                if let Some(c) = &m.content
                    && !c.is_empty()
                {
                    cells.push(Cell::Content(c.clone()));
                }
                for call in m.tool_calls.iter().flatten() {
                    cells.push(Cell::tool_call(
                        &call.function.name,
                        &call.function.arguments,
                    ));
                }
            }
            Role::Tool => {
                if let Some(c) = &m.content {
                    cells.push(Cell::ToolResult(c.clone()));
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
fn hint(args: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| {
            v.get("command")
                .or_else(|| v.get("file_path"))
                .and_then(|c| c.as_str())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| text::truncate(args, HINT_COLUMNS).to_owned())
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

/// One-line summary of a tool result: its first line and how much text came
/// back.
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
fn usage_line(u: &Usage, stream: Duration) -> String {
    let cache = match u.cache() {
        Some(c) => format!("hit {}/miss {}", c.hit, c.miss),
        None => "cache —".to_string(),
    };
    let mut line = format!(
        "tokens: in {}/{} ({cache}) · out {}",
        u.prompt_tokens, u.total_tokens, u.completion_tokens
    );
    if u.completion_tokens > 0 && stream >= Duration::from_secs(1) {
        let per_second = u.completion_tokens as f64 / stream.as_secs_f64();
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
            content: content.map(str::to_owned),
            reasoning_content: reasoning.map(str::to_owned),
            tool_calls: calls,
            tool_call_id: None,
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
        assert!(Cell::User("hi".into()).gap_after(false));
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
    fn user_line_marks_itself_then_hands_over_to_body_text() {
        let spans = Cell::User("hello".into()).spans();
        assert_eq!(spans[0], Span::new(Style::Dim, "› "));
        assert_eq!(spans[1], Span::new(Style::Plain, "hello"));
    }

    #[test]
    fn hint_prefers_command_then_file_path() {
        assert_eq!(hint(r#"{"command":"ls -la"}"#), "ls -la");
        assert_eq!(hint(r#"{"file_path":"/a/b.txt"}"#), "/a/b.txt");
        // command wins when both are present
        assert_eq!(hint(r#"{"command":"ls","file_path":"/a"}"#), "ls");
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
        let spans = Cell::tool_call("Edit", &edit("old", "new")).spans();
        assert_eq!(
            spans,
            vec![
                Span::new(Style::Yellow, "▸ Edit src/main.rs"),
                Span::new(Style::Red, "\n  - old"),
                Span::new(Style::Green, "\n  + new"),
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
        let spans = Cell::tool_call("Bash", r#"{"command":"ls"}"#).spans();
        assert_eq!(spans, vec![Span::new(Style::Yellow, "▸ Bash ls")]);
        let spans = Cell::approval("Bash", r#"{"command":"rm -rf /"}"#).spans();
        assert_eq!(
            spans,
            vec![Span::new(Style::Yellow, "▸ Bash rm -rf / — run it? [y/N] ")]
        );
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
                Cell::User("take a look".into()),
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

    #[test]
    fn styles_map_to_their_escape_sequences() {
        assert_eq!(Style::Plain.code(), "");
        assert_eq!(Style::Dim.code(), "\x1b[2m");
        // A ground as well as a weight: faint text is a distinction some terminals
        // ignore, and the ground is the one they cannot.
        assert_eq!(Style::Reasoning.code(), "\x1b[38;5;245;48;5;236m");
        assert_eq!(Style::Yellow.code(), "\x1b[33m");
        assert_eq!(Style::Red.code(), "\x1b[31m");
    }
}

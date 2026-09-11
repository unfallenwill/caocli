//! The tests for the plain front end.

use std::io::Write;
use std::time::Duration;

use super::banner::banner_at;
use super::cell::Style;
use super::status_bar::StatusBar;
use super::terminal::{Echo, Terminal};
use super::*;
use crate::types::{Message, Role, Usage};
use status::CacheStats;

pub(super) struct SharedBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A terminal that answers what a test tells it to rather than what the
/// process's own does.
///
/// The facts are the terminal's; the decisions built on them -- is the bar
/// enabled, is the size usable, does the echo go off before the question is
/// asked -- are this front end's, and these are what let a unit test make them.
pub(super) struct StandIn {
    size: Option<(u16, u16)>,
    tty: bool,
    dumb: bool,
    color: bool,
    /// Whether the echo is off right now. The guard turns it back on.
    echo: std::rc::Rc<std::cell::Cell<bool>>,
    /// What `read_line` answers, one per call; empty is an end of input.
    lines: std::cell::RefCell<std::collections::VecDeque<String>>,
}

/// A read on whether the echo is off, held after the stand-in is moved away.
pub(super) struct EchoHandle(std::rc::Rc<std::cell::Cell<bool>>);

impl EchoHandle {
    pub(super) fn is_off(&self) -> bool {
        self.0.get()
    }
}

/// Put back on drop, the way the real terminal's echo is restored.
struct Silence(std::rc::Rc<std::cell::Cell<bool>>);

impl Echo for Silence {}

impl Drop for Silence {
    fn drop(&mut self) {
        self.0.set(false);
    }
}

impl StandIn {
    /// A 24x80 terminal that can host the bar.
    pub(super) fn new() -> Self {
        Self {
            size: Some((24, 80)),
            tty: true,
            dumb: false,
            color: true,
            echo: std::rc::Rc::new(std::cell::Cell::new(false)),
            lines: std::cell::RefCell::new(std::collections::VecDeque::new()),
        }
    }

    /// The lines this terminal answers with, one per call.
    pub(super) fn answering(self, lines: &[&str]) -> Self {
        let queue = lines.iter().map(|l| (*l).to_owned()).collect();
        *self.lines.borrow_mut() = queue;
        self
    }

    /// The echo's current state, readable after the stand-in has been moved into
    /// a renderer: what a test about a secret has to look at while the answer is
    /// being read.
    pub(super) fn echo_handle(&self) -> EchoHandle {
        EchoHandle(self.echo.clone())
    }

    pub(super) fn tty(mut self, tty: bool) -> Self {
        self.tty = tty;
        self
    }

    pub(super) fn dumb(mut self, dumb: bool) -> Self {
        self.dumb = dumb;
        self
    }

    pub(super) fn sized(mut self, rows: u16, cols: u16) -> Self {
        self.size = Some((rows, cols));
        self
    }

    /// A terminal that cannot be measured at all.
    pub(super) fn unmeasurable(mut self) -> Self {
        self.size = None;
        self
    }
}

impl Terminal for StandIn {
    fn size(&self) -> Option<(u16, u16)> {
        self.size
    }

    fn is_tty(&self) -> bool {
        self.tty
    }

    fn is_dumb(&self) -> bool {
        self.dumb
    }

    fn echo_off(&self) -> Option<Box<dyn Echo>> {
        self.echo.set(true);
        Some(Box::new(Silence(self.echo.clone())))
    }

    fn read_line(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<String>> + '_>> {
        let line = self.lines.borrow_mut().pop_front();
        Box::pin(async move { line })
    }

    /// Answered from the stand-in rather than the environment, so a test says
    /// what the terminal wants instead of setting a variable the whole process
    /// shares.
    fn wants_color(&self) -> bool {
        self.color
    }
}
/// The whole line, as an 80-column terminal would show it: the terminal it is
/// most likely to be read on is the one the old line broke in half.
#[test]
fn the_banner_fits_the_terminal_it_is_going_into() {
    let full = "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands";
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", None),
        full
    );
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(100)),
        full,
        "a wide terminal keeps the hint"
    );
    // 80 columns: the hint is the first thing to go -- the box repeats it.
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(80)),
        "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro"
    );
    // Then the model, which the status line has anyway.
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(60)),
        "caocli · session 20260910-213122 (12 messages)"
    );
    // Then the count. Which session this is is the one thing nothing else
    // says, so the id is the last of the optional ones to go -- and what is
    // left is what fits a 40-column terminal whole.
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(40)),
        "caocli · session 20260910-213122"
    );
    // Below that, the line comes back whole to be clipped by whoever writes
    // it: an over-long line reads better than a line with nothing on it.
    assert_eq!(
        banner_at("20260910-213122", 12, "deepseek/deepseek-v4-pro", Some(12)),
        "caocli · session 20260910-213122"
    );
}

#[test]
fn the_banner_counts_one_message_in_the_singular() {
    let one = banner_at("20260910-213122", 1, "m", Some(80));
    assert!(one.contains("(1 message)"), "{one:?}");
    assert!(banner_at("20260910-213122", 2, "m", Some(80)).contains("(2 messages)"));
    assert!(banner_at("20260910-213122", 0, "m", Some(80)).contains("(0 messages)"));
}

#[test]
fn the_banner_is_never_cut_mid_word() {
    // Every width, not just the round ones: what comes back is always one of
    // the segment joins -- never a piece of one -- so a word is never cut in
    // half however narrow the terminal is.
    let joins = [
        "caocli · session 20260910-213122",
        "caocli · session 20260910-213122 (12 messages)",
        "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro",
        "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands",
    ];
    for columns in 1..=120 {
        let line = banner_at(
            "20260910-213122",
            12,
            "deepseek/deepseek-v4-pro",
            Some(columns),
        );
        assert!(joins.contains(&line.as_str()), "{columns}: {line:?}");
        if text::width(&line) > columns {
            // Only the shortest form may overflow, and only to be clipped by
            // whoever writes it: an over-long line reads better than none.
            assert_eq!(line, joins[0], "{columns}: {line:?}");
        }
    }
}

#[test]
fn the_banner_measures_wide_characters_by_column() {
    // A CJK model id is two columns per glyph, so a budget counted in chars
    // would let through a segment that does not fit: the ideographs are 8
    // columns and only 4 characters.
    let cjk = "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}";
    let with_model = "caocli · session id (1 message) · \u{6df1}\u{5ea6}\u{6c42}\u{7d22}";
    assert_eq!(text::width(with_model), 42);
    assert_eq!(banner_at("id", 1, cjk, Some(42)), with_model);
    assert_eq!(
        banner_at("id", 1, cjk, Some(41)),
        "caocli · session id (1 message)",
        "one column short of the model: the model goes whole"
    );
}

#[test]
fn the_banner_keeps_everything_when_there_is_no_terminal() {
    // Output redirected to a file or a pipe: there is no width to fit, and a
    // file can hold the whole line. (A test's own stdout is a pipe, so this is
    // also the case the width is really asked for in.)
    assert_eq!(
        banner("20260910-213122", 12, "deepseek/deepseek-v4-pro"),
        "caocli · session 20260910-213122 (12 messages) · deepseek/deepseek-v4-pro · /help for commands"
    );
}

#[test]
fn reasoning_then_content_are_separate_blocks() {
    let (mut r, buf) = with_buffer(true);
    r.reasoning_delta("thinking...");
    r.content_delta("answer");
    r.finish_turn();
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "\x1b[2m┆ thinking...\x1b[0m\n\nanswer\x1b[0m\n"
    );
}

#[test]
fn content_then_reasoning_second_subturn_separated() {
    let (mut r, buf) = with_buffer(true);
    r.content_delta("partial");
    r.reasoning_delta("more thinking");
    r.finish_turn();
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "partial\x1b[0m\n\n\x1b[2m┆ more thinking\x1b[0m\n"
    );
}

#[test]
fn no_color_still_separates_blocks() {
    let (mut r, buf) = with_buffer(false);
    r.reasoning_delta("thought");
    r.content_delta("text");
    r.finish_turn();
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "┆ thought\n\ntext\n"
    );
}

#[test]
fn content_only_has_no_leading_separator() {
    let (mut r, buf) = with_buffer(true);
    r.content_delta("hi");
    r.finish_turn();
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "hi\x1b[0m\n"
    );
}

#[test]
fn reasoning_only_block_closes_cleanly() {
    let (mut r, buf) = with_buffer(true);
    r.reasoning_delta("hmm");
    r.finish_turn();
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "\x1b[2m┆ hmm\x1b[0m\n"
    );
}

#[test]
fn same_mode_deltas_do_not_reopen_block() {
    let (mut r, buf) = with_buffer(true);
    r.reasoning_delta("a");
    r.reasoning_delta("b"); // still Reasoning: no repeated color code
    r.content_delta("x");
    r.content_delta("y"); // still Content: no new block
    r.finish_turn();
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "\x1b[2m┆ ab\x1b[0m\n\nxy\x1b[0m\n"
    );
}

#[test]
fn tool_start_extracts_command_hint() {
    let (mut r, buf) = with_buffer(true);
    r.tool_start("Bash", r#"{"command":"ls -la"}"#);
    r.tool_start("Read", r#"{"file_path":"/a/b.txt"}"#);
    r.tool_start("Write", "not json at all");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("▸ Bash ls -la"), "{s}");
    assert!(s.contains("▸ Read /a/b.txt"), "{s}");
    assert!(s.contains("▸ Write not json at all"), "{s}"); // bad JSON falls back to raw text
}

/// A running command's output is written as it arrives, in its own block: the
/// marker opens it once and every line after a break inside a chunk is set in to the
/// same column, since this front end is the one that sees those breaks.
#[test]
fn a_running_commands_output_is_written_as_it_arrives() {
    let (mut r, buf) = with_buffer(true);
    r.tool_start("Bash", r#"{"command":"echo one; echo two"}"#);
    r.tool_output("one\n");
    r.tool_output("two");
    r.tool_result("exit_code: 0\n--- stdout ---\none\ntwo");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("▸ Bash echo one; echo two"), "{s:?}");
    assert!(s.contains("\x1b[2m· one\n  two\x1b[0m\n"), "{s:?}");
    assert!(s.contains("· exit_code: 0"), "{s:?}");
}

/// Nothing is written for a command that printed nothing: an empty chunk is not a
/// line, and a block opened on one would leave the marker behind on its own.
#[test]
fn an_empty_chunk_opens_no_block() {
    let (mut r, buf) = with_buffer(true);
    r.tool_output("");
    r.tool_result("exit_code: 0");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert_eq!(s, "\x1b[2m· exit_code: 0 · 12 bytes\x1b[0m\n");
}

#[test]
fn replay_renders_history_compactly_with_colors() {
    use crate::types::{ToolCall, ToolCallFunction};
    let (mut r, buf) = with_buffer(true);
    r.replay(&[
        Message::user("take a look"),
        Message {
            role: Role::Assistant,
            content: Some("running it".into()),
            reasoning_content: Some("let me think".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "Bash".into(),
                    arguments: r#"{"command":"ls -la"}"#.into(),
                },
            }]),
            tool_call_id: None,
        },
        Message::tool("call_1", "exit_code: 0\n--- stdout ---\nSECRET_BODY"),
        Message::system("must not appear"),
    ]);
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("\x1b[2m› \x1b[0mtake a look"), "{s}");
    assert!(
        s.contains("\x1b[2m┆ let me think\x1b[0m"),
        "thinking is set in behind its own rule: {s}"
    );
    assert!(s.contains("running it"), "{s}");
    assert!(s.contains("▸ Bash ls -la"), "tool calls are yellow: {s}");
    // tool messages only get a summary, never the full text
    assert!(s.contains("exit_code: 0 · 39 bytes"), "{s}");
    assert!(
        !s.contains("SECRET_BODY"),
        "tool output must not be replayed: {s}"
    );
    assert!(
        !s.contains("must not appear"),
        "system messages are not replayed: {s}"
    );
}

#[test]
fn replay_shows_an_attached_image_beside_the_line_it_came_with() {
    let (mut r, buf) = with_buffer(true);
    r.replay(&[Message::user_with_images(
        "what is this?",
        vec!["data:image/png;base64,Zm9vYmFy".into()],
    )]);
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("\x1b[2m› \x1b[0mwhat is this?"), "{s}");
    // The image is a line of the same cell: set in the columns the words are in,
    // and never its bytes, which go to the backend and stay in the log.
    assert!(s.contains("\x1b[2m\n  [image png · 6 bytes]\x1b[0m"), "{s}");
    assert!(!s.contains("Zm9vYmFy"), "the bytes are not shown: {s}");
}

#[test]
fn replay_empty_history_emits_only_blank_line() {
    let (mut r, buf) = with_buffer(false);
    r.replay(&[]);
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "\n"
    );
}

#[test]
fn replay_skips_empty_assistant_fields() {
    let (mut r, buf) = with_buffer(false);
    r.replay(&[Message {
        role: Role::Assistant,
        content: Some("".into()),
        reasoning_content: Some(String::new()),
        tool_calls: None,
        tool_call_id: None,
    }]);
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "\n"
    );
}

/// The point of the cell model: a turn is laid out identically whether it is
/// watched live or replayed from the log, because both paths paint the same
/// cells.
///
/// Before the cells existed, `replay` carried its own copy of the layout
/// rules and silently disagreed with the live renderer: live put a blank line
/// between thinking and the answer, replay did not.
#[test]
fn live_and_replay_lay_out_a_turn_identically() {
    use crate::types::{Message, ToolCall, ToolCallFunction};

    // The order the agent drives the renderer in: a streaming round first,
    // then the tool calls it asked for.
    let (mut live, live_buf) = with_buffer(true);
    live.reasoning_delta("let me think");
    live.content_delta("running it");
    live.finish_turn();
    live.tool_start("Bash", r#"{"command":"ls -la"}"#);
    live.tool_result("exit_code: 0\n--- stdout ---\nBODY");

    let (mut replayed, replay_buf) = with_buffer(true);
    replayed.replay(&[
        Message {
            role: Role::Assistant,
            content: Some("running it".into()),
            reasoning_content: Some("let me think".into()),
            tool_calls: Some(vec![ToolCall {
                id: "call_1".into(),
                r#type: "function".into(),
                function: ToolCallFunction {
                    name: "Bash".into(),
                    arguments: r#"{"command":"ls -la"}"#.into(),
                },
            }]),
            tool_call_id: None,
        },
        Message::tool("call_1", "exit_code: 0\n--- stdout ---\nBODY"),
    ]);

    // Replay additionally separates the history from the prompt after it.
    assert_eq!(buf_of(&replay_buf), format!("{}\n", buf_of(&live_buf)));
}

/// The plain front end's whole vocabulary in one scripted session, frozen
/// byte for byte.
///
/// Every later change to this module has to keep this green: moving the parts
/// into their own files is a move, not a rewrite, and the existing assertions
/// are about pieces rather than about the stream they add up to. Driven in the
/// order `main.rs` and the agent drive it -- the bar before the banner, a
/// resumed history, a turn with thinking and an answer, a call and its result,
/// the usage line, a notice, the approval question, a cancellation, then the
/// bar coming down as the terminal goes back.
#[test]
fn the_plain_front_ends_stream_is_frozen() {
    let (mut r, buf) = with_buffer(true);
    r.set_model("deepseek-v4-pro");
    r.set_effort("max");
    r.apply_status_bar(Some(StatusBar { rows: 24, cols: 80 }));
    r.info("caocli · session 20260910-213122 (2 messages) · deepseek-v4-pro");
    r.replay(&[
        Message::user("take a look"),
        Message {
            role: Role::Assistant,
            content: Some("running it".into()),
            reasoning_content: Some("let me think".into()),
            tool_calls: None,
            tool_call_id: None,
        },
    ]);
    r.reasoning_delta("weigh");
    r.reasoning_delta(" it");
    r.content_delta("here");
    r.content_delta(" goes");
    r.finish_turn();
    r.tool_start("Bash", r#"{"command":"ls -la"}"#);
    r.tool_result("exit_code: 0\n--- stdout ---\nBODY");
    r.usage(&usage_fixture(6, 4), Duration::from_millis(1500));
    r.approval_requested("Write", r#"{"file_path":"/tmp/x"}"#);
    r.interrupted();
    r.teardown();

    // Frozen against the bytes this front end writes today: a move that
    // changes any of them is a rewrite, not a move.
    let expected = concat!(
        // the bar comes up on a 24x80 terminal: scroll region 1..23, cursor on 23,
        "\x1b[24;1H\x1b[2K\x1b[1;23r\x1b[23;1H",
        "\x1b7\x1b[24;1H\x1b[2K                     ",
        "\x1b[2mdeepseek-v4-pro · effort max · cache 0.0% · hit 0 · miss 0\x1b[0m\x1b8",
        // the banner is an info cell
        "\x1b[2m  caocli · session 20260910-213122 (2 messages) · deepseek-v4-pro\x1b[0m\n",
        "\n",
        "\x1b[2m› \x1b[0mtake a look\x1b[0m\n",
        "\x1b[2m┆ let me think\x1b[0m\n",
        "\n",
        "running it\x1b[0m\n",
        "\n",
        "\n",
        // a resumed history, then a live turn: thinking, then the answer
        "\x1b[2m┆ weigh it\x1b[0m\n",
        "\n",
        "here goes\x1b[0m\n",
        "\n",
        // a call, its result, and the usage line
        "\x1b[1;33m▸ Bash ls -la\x1b[0m\n",
        "\x1b[2m· exit_code: 0 · 32 bytes\x1b[0m\n",
        "\x1b[2m  tokens: in 10/10 (hit 6/miss 4) · out 0\x1b[0m\n",
        // the bar picks up the usage the line just recorded, then the gate asks
        "\x1b7\x1b[24;1H\x1b[2K                    ",
        "\x1b[2mdeepseek-v4-pro · effort max · cache 60.0% · hit 6 · miss 4\x1b[0m\x1b8",
        "\x1b[1;33m▸ Write /tmp/x — run it? [y/N] \x1b[0m",
        "\x1b[1;33m  ⏹ interrupted (Ctrl-C)\x1b[0m\n",
        // and the bar goes down as the terminal is handed back
        "\x1b[r\x1b[24;1H\x1b[2K\r\n",
    );

    assert_eq!(
        buf_of(&buf),
        expected,
        "the plain front end's byte stream changed"
    );
}

#[test]
fn interrupted_closes_block_and_prints_notice() {
    let (mut r, buf) = with_buffer(true);
    r.reasoning_delta("thinking");
    r.interrupted();
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("interrupted"), "{s}");
    assert!(s.ends_with("\x1b[0m\n"), "color reset closes the line: {s}");
}

#[test]
fn approval_requested_asks_without_newline() {
    let (mut r, buf) = with_buffer(false);
    r.approval_requested("Bash", r#"{"command":"rm -rf /"}"#);
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("▸ Bash rm -rf /"), "{s}");
    assert!(
        s.ends_with("[y/N] "),
        "ends on the y/N prompt without a newline: {s:?}"
    );
}

#[test]
fn tool_result_shows_exit_line_and_bytes() {
    let (mut r, buf) = with_buffer(false);
    r.tool_result("exit_code: 3\n--- stdout ---\nhello");
    r.tool_result("");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("exit_code: 3 · 33 bytes"), "{s}");
    assert!(s.contains(" · 0 bytes"), "{s}"); // empty result
}

#[test]
fn usage_and_info_render_dims() {
    let (mut r, buf) = with_buffer(false);
    r.usage(
        &Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
            prompt_cache_hit_tokens: 6,
            prompt_cache_miss_tokens: 4,
            ..Default::default()
        },
        Duration::ZERO,
    );
    r.info("session abc");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("tokens: in 10/15 (hit 6/miss 4) · out 5"), "{s}");
    assert!(s.contains("session abc"), "{s}");
}

#[test]
fn color_variants_render_codes_and_plain() {
    // color=true: info/usage take paint's colored branch; error goes red
    let (mut r, buf) = with_buffer(true);
    r.info("ok");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert_eq!(s, "\x1b[2m  ok\x1b[0m\n");
    r.error("boom"); // eprintln, does not write to buf; only checks it does not
    // panic and that the red paint call is covered
}

#[test]
fn no_color_paint_returns_plain_text() {
    let (mut r, buf) = with_buffer(false);
    r.info("plain");
    assert_eq!(
        String::from_utf8(buf.lock().unwrap().clone()).unwrap(),
        "  plain\n"
    );
}

/// A block watched live and the same block replayed after a resume come out as
/// the same bytes.
///
/// This is what the marker belonging to the cell buys. A streamed block writes it
/// through the block's own open style; a replayed cell paints it as the gutter of
/// the cell it opens. The two agree because a gutter's style is the style of the
/// cell it sets in -- and they would stop agreeing the day one of them drifted,
/// which is a comparison of escape sequences that no reader makes by eye.
#[test]
fn a_streamed_block_and_the_same_block_replayed_are_the_same_bytes() {
    let (mut live, live_buf) = with_buffer(true);
    live.reasoning_delta("thinking");
    live.content_delta("answer");
    live.finish_turn();

    let (mut replayed, replay_buf) = with_buffer(true);
    replayed.replay(&[Message {
        role: Role::Assistant,
        content: Some("answer".into()),
        reasoning_content: Some("thinking".into()),
        tool_calls: None,
        tool_call_id: None,
    }]);

    // The replay's own trailing separator is the one thing that is not the same:
    // it is what sets the history off from the prompt that follows it.
    assert_eq!(buf_of(&live_buf).trim_end(), buf_of(&replay_buf).trim_end());
}

/// A line break inside a cell is a line of that cell, not a line the terminal
/// wrapped, so it is set in with the rest of them.
///
/// The two are easy to confuse and they are not the same thing: what the terminal
/// wraps is beyond either front end's reach, and what a cell breaks itself cannot be
/// left out without a change's lines landing in the column the answers are in.
#[test]
fn a_cells_own_line_breaks_are_set_in_too() {
    let (mut r, buf) = with_buffer(false);
    r.tool_start(
        "Edit",
        r#"{"file_path":"a.txt","old_string":"one","new_string":"two"}"#,
    );
    let drawn = buf_of(&buf);
    assert!(
        drawn.contains("▸ Edit a.txt\n  - one\n  + two"),
        "a change is set in under its call: {drawn:?}"
    );

    let (mut r, buf) = with_buffer(false);
    r.replay(&[Message::user("alpha\nbeta")]);
    let drawn = buf_of(&buf);
    assert!(
        drawn.contains("› alpha\n  beta"),
        "a draft that was sent with a line in it reads back the same way: {drawn:?}"
    );
}

fn usage_fixture(hit: u64, miss: u64) -> Usage {
    Usage {
        prompt_tokens: hit + miss,
        completion_tokens: 0,
        total_tokens: hit + miss,
        prompt_cache_hit_tokens: hit,
        prompt_cache_miss_tokens: miss,
        ..Default::default()
    }
}

fn buf_of(buf: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> String {
    String::from_utf8(buf.lock().unwrap().clone()).unwrap()
}

#[test]
fn status_bar_from_size_guards() {
    assert_eq!(StatusBar::from_size(None), None);
    assert_eq!(StatusBar::from_size(Some((2, 80))), None); // too few rows
    assert_eq!(StatusBar::from_size(Some((24, 1))), None); // too narrow
    assert_eq!(
        StatusBar::from_size(Some((24, 80))),
        Some(StatusBar { rows: 24, cols: 80 })
    );
}

#[test]
fn status_bar_detect_rejects_a_terminal_that_cannot_host_it() {
    assert_eq!(StatusBar::detect(&StandIn::new().tty(false)), None);
    assert_eq!(StatusBar::detect(&StandIn::new().dumb(true)), None);
    assert_eq!(StatusBar::detect(&StandIn::new().unmeasurable()), None);
    assert_eq!(StatusBar::detect(&StandIn::new().sized(2, 80)), None);
    assert_eq!(StatusBar::detect(&StandIn::new().sized(24, 1)), None);
    assert_eq!(
        StatusBar::detect(&StandIn::new().sized(24, 80)),
        Some(StatusBar { rows: 24, cols: 80 })
    );
}

/// Regression: the bar used to measure its label in chars, so every wide
/// character was undercharged by one column and the label spilled past the
/// column the bar deliberately leaves free (which is what keeps the write
/// from triggering autowrap on the last row).
#[test]
fn status_bar_render_charges_wide_chars_two_columns() {
    let bar = StatusBar { rows: 10, cols: 20 }; // width = 19
    let (mut r, buf) = with_buffer(false);
    // two ideographs + space + rocket = 7 columns but only 4 chars
    let label = "\u{6df1}\u{5ea6} \u{1f680}";
    assert_eq!(label.chars().count(), 4);
    assert_eq!(text::width(label), 7);
    let visible = text::truncate(label, bar.width());
    bar.render(r.out.as_mut(), visible, visible);
    let s = buf_of(&buf);
    // 19 - 7 = 12 columns of padding. Counting chars would have written 15
    // and pushed the label 3 columns past the right edge.
    assert!(
        s.contains(&format!("\x1b[2K{}{label}\x1b8", " ".repeat(12))),
        "{s:?}"
    );
}

#[test]
fn status_bar_setup_teardown_sequences() {
    let bar = StatusBar { rows: 10, cols: 40 };
    let (mut r, buf) = with_buffer(false);
    bar.setup(r.out.as_mut());
    bar.teardown(r.out.as_mut());
    let s = buf_of(&buf);
    assert!(s.contains("\x1b[10;1H\x1b[2K\x1b[1;9r\x1b[9;1H"), "{s:?}");
    assert!(s.contains("\x1b[r\x1b[10;1H\x1b[2K\r\n"), "{s:?}");
}

#[test]
fn status_bar_render_right_aligns_and_paints() {
    let bar = StatusBar { rows: 10, cols: 40 }; // width = 39
    let (mut r, buf) = with_buffer(true);
    let label = "cache 98.6% · hit 32384 · miss 461"; // 34 characters
    let painted = r.paint(Style::Dim, label);
    bar.render(r.out.as_mut(), label, &painted);
    let s = buf_of(&buf);
    // 39 - 34 = 5 spaces of left padding, right aligned
    assert!(
        s.contains(&format!(
            "\x1b7\x1b[10;1H\x1b[2K     \x1b[2m{label}\x1b[0m\x1b8"
        )),
        "{s:?}"
    );
}

#[test]
fn apply_status_bar_transitions() {
    let a = StatusBar { rows: 10, cols: 40 };
    let b = StatusBar { rows: 12, cols: 50 };

    // (None, None): no output
    let (mut r, buf) = with_buffer(false);
    r.apply_status_bar(None);
    assert!(buf_of(&buf).is_empty());

    // (None, Some): create the bar + first draw
    r.apply_status_bar(Some(a));
    let s = buf_of(&buf);
    assert!(s.contains("\x1b[1;9r"), "{s:?}");
    assert!(s.contains("cache 0.0% · hit 0 · miss 0"), "{s:?}");
    assert_eq!(r.bar, Some(a));

    // (Some, Some(same)): redraw only
    let before = buf_of(&buf).len();
    r.apply_status_bar(Some(a));
    assert!(buf_of(&buf)[before..].contains("\x1b[10;1H\x1b[2K"));

    // (Some, Some(different)): tear down the old one, build the new one
    let before = buf_of(&buf).len();
    r.apply_status_bar(Some(b));
    let tail = &buf_of(&buf)[before..];
    assert!(tail.contains("\x1b[r"), "{tail:?}");
    assert!(tail.contains("\x1b[1;11r"), "{tail:?}");
    assert_eq!(r.bar, Some(b));

    // (Some, None): tear the bar down
    let before = buf_of(&buf).len();
    r.apply_status_bar(None);
    assert!(buf_of(&buf)[before..].contains("\x1b[r"));
    assert_eq!(r.bar, None);
}

#[test]
fn usage_paints_session_cache_bar_and_reset_clears_it() {
    let bar = StatusBar { rows: 10, cols: 60 };
    let (mut r, buf) = with_buffer(false);
    r.apply_status_bar(Some(bar));
    r.usage(&usage_fixture(6, 4), Duration::ZERO);
    r.usage(&usage_fixture(12, 8), Duration::ZERO);
    let s = buf_of(&buf);
    assert!(
        s.contains("tokens: in 10/10 (hit 6/miss 4) · out 0"),
        "{s:?}"
    );
    // session accumulation: 18 hit / 12 miss = 60.0%
    assert!(s.contains("cache 60.0% · hit 18 · miss 12"), "{s:?}");

    r.reset_stats();
    let tail = &buf_of(&buf)[s.len()..];
    assert!(tail.contains("cache 0.0% · hit 0 · miss 0"), "{tail:?}");
    assert_eq!(r.stats(), CacheStats::default());
}

/// The status bar line the renderer draws at the given terminal width, with
/// a model and cache statistics already in place.
fn bar_line(cols: u16, model: &str) -> String {
    let (mut r, buf) = with_buffer(false);
    r.set_model(model);
    r.usage(&usage_fixture(6, 4), Duration::ZERO);
    // The bar is attached last, so the redraw it triggers is the first one
    // that has anything to draw.
    r.apply_status_bar(Some(StatusBar { rows: 10, cols }));
    buf_of(&buf)
}

/// Assert that the bar's content is exactly `label`: clipping it would
/// break the `\x1b8` cursor restore that immediately follows.
fn assert_bar_exactly(cols: u16, model: &str, label: &str) {
    let s = bar_line(cols, model);
    assert!(
        s.contains(&format!("{label}\x1b8")),
        "cols={cols} expected {label:?} in {s:?}"
    );
}

/// Progressive disclosure: a bar too narrow for everything drops whole
/// segments from the end instead of cutting a number in half.
#[test]
fn status_bar_drops_whole_segments_on_narrow_terminals() {
    let model = "deepseek-v4-flash";
    // 79 columns: everything fits
    assert_bar_exactly(
        80,
        model,
        "deepseek-v4-flash · cache 60.0% · hit 6 · miss 4",
    );
    // 34 columns: the counts go, the model and rate stay
    assert_bar_exactly(35, model, "deepseek-v4-flash · cache 60.0%");
    // 19 columns: only the model is left
    assert_bar_exactly(20, model, "deepseek-v4-flash");
}

/// When not even the shortest combination fits, it is clipped rather than
/// leaving the bar blank.
#[test]
fn status_bar_clips_the_shortest_segment_as_a_last_resort() {
    assert_bar_exactly(10, "deepseek-v4-flash", "deepseek-");
}

/// Variants are chosen by display width, not by char count: at 21 columns
/// the rate segment is 22 columns wide (18 chars), so it has to be dropped.
#[test]
fn status_bar_chooses_variants_by_display_width() {
    let wide_model = "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}"; // 8 columns, 4 chars
    assert_bar_exactly(
        25,
        wide_model,
        "\u{6df1}\u{5ea6}\u{6c42}\u{7d22} · cache 60.0%",
    );
    assert_bar_exactly(21, wide_model, wide_model);
}

#[test]
fn status_bar_shows_model_and_updates_on_switch() {
    let bar = StatusBar { rows: 10, cols: 80 };
    let (mut r, buf) = with_buffer(false);
    r.apply_status_bar(Some(bar));
    r.set_model("deepseek-v4-flash");
    r.usage(&usage_fixture(6, 4), Duration::ZERO);
    let s = buf_of(&buf);
    assert!(
        s.contains("deepseek-v4-flash · cache 60.0% · hit 6 · miss 4"),
        "{s:?}"
    );

    // switching models: after the redraw only the new model is left
    r.set_model("deepseek-v4-pro");
    let tail = &buf_of(&buf)[s.len()..];
    assert!(tail.contains("deepseek-v4-pro · cache 60.0%"), "{tail:?}");
    assert!(!tail.contains("deepseek-v4-flash"), "{tail:?}");

    // reset_stats clears only the cache stats and keeps the model
    let before = buf_of(&buf).len();
    r.reset_stats();
    let tail = &buf_of(&buf)[before..];
    assert!(
        tail.contains("deepseek-v4-pro · cache 0.0% · hit 0 · miss 0"),
        "{tail:?}"
    );
}

#[test]
fn refresh_status_bar_without_tty_is_noop_and_teardown_idempotent() {
    let (mut r, buf) = with_buffer(false);
    r.refresh_status_bar(); // the stand-in is not a terminal → not enabled
    assert!(buf_of(&buf).is_empty());
    r.teardown(); // idempotent when not enabled
    assert!(buf_of(&buf).is_empty());
}

/// A renderer writing into a buffer, on a stand-in that is not a terminal: the
/// shape every test here starts from, since none of them is about a terminal
/// unless it says so.
fn with_buffer(color: bool) -> (Renderer, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    renderer_on(StandIn::new().tty(false), color)
}

/// A renderer writing into a buffer, on a terminal the test describes.
fn renderer_on(
    term: StandIn,
    color: bool,
) -> (Renderer, std::sync::Arc<std::sync::Mutex<Vec<u8>>>) {
    let buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    (
        Renderer::on(Box::new(SharedBuf(buf.clone())), color, Box::new(term)),
        buf,
    )
}

/// The whole point of the terminal being a capability: what used to be reachable
/// only under a pty -- the bar coming up, the echo going off -- is a decision a
/// test can make, because it is a function of facts the test supplies.
#[test]
fn a_bar_comes_up_on_a_terminal_that_can_host_it_and_goes_down_on_teardown() {
    let (mut r, buf) = renderer_on(StandIn::new().sized(24, 80), true);
    r.refresh_status_bar();
    let s = buf_of(&buf);
    // the scroll region stops one line short of the bottom, and the bar's first
    // frame is drawn on the line it leaves
    assert!(s.contains("\x1b[1;23r"), "the scroll region: {s:?}");
    assert!(s.contains("cache 0.0% · hit 0 · miss 0"), "{s:?}");
    // a second refresh with the same size only redraws
    let before = s.len();
    r.refresh_status_bar();
    let tail = &buf_of(&buf)[before..];
    assert!(tail.contains("\x1b[24;1H"), "{tail:?}");
    assert!(!tail.contains("\x1b[1;23r"), "no second region: {tail:?}");

    r.teardown();
    let tail = &buf_of(&buf)[before..];
    assert!(tail.ends_with("\x1b[r\x1b[24;1H\x1b[2K\r\n"), "{tail:?}");
}

/// A terminal that says it cannot address the screen is one the bar stays off
/// for, whether it is dumb or has no rows to spare.
#[test]
fn a_terminal_that_cannot_host_the_bar_leaves_it_off() {
    for term in [
        StandIn::new().dumb(true),
        StandIn::new().unmeasurable(),
        StandIn::new().sized(2, 80),
    ] {
        let (mut r, buf) = renderer_on(term, true);
        r.refresh_status_bar();
        assert!(
            buf_of(&buf).is_empty(),
            "nothing may be written for a bar that is not enabled"
        );
    }
}

/// The secret is the one question the plain front end asks, and the echo is the
/// part of it a unit test could never see: it goes off before the question is
/// written, and comes back on when the answer does.
#[tokio::test]
async fn the_echo_is_off_while_the_secret_is_awaited_and_back_on_after() {
    let term = StandIn::new().answering(&["sk-secret"]);
    let echo = term.echo_handle();
    let (mut r, buf) = renderer_on(term, false);

    let asked = r.ask_secret("the API key");
    // The question is on the screen before the answer is waited for: an answer
    // nobody knows is being asked for is not an answer.
    assert!(buf_of(&buf).contains("the API key"), "{}", buf_of(&buf));
    assert!(
        echo.is_off(),
        "the echo is off before the first await point"
    );

    assert_eq!(asked.await.as_deref(), Some("sk-secret"));
    assert!(
        !echo.is_off(),
        "and back on once the answer has arrived: a terminal left quiet stops showing what is typed into it"
    );
}

/// No input left is not a failure: it is the same `None` a cancellation is, and
/// the echo still goes back on.
#[tokio::test]
async fn a_secret_with_no_input_left_answers_nothing() {
    let term = StandIn::new();
    let echo = term.echo_handle();
    let (mut r, _buf) = renderer_on(term, false);

    assert_eq!(r.ask_secret("the API key").await, None);
    assert!(!echo.is_off());
}

//! The stream: what the machine's notifications look like when they are written,
//! and what a resumed session's history looks like when it is replayed through
//! the same painter.

use std::time::Duration;

use crate::types::{Message, Role};

use super::super::status_bar::StatusBar;
use super::*;

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
    // Nothing is left on the line the last chunk ended: a chunk that ends with a
    // break ends the block, and the cell after it follows on the next line.
    assert!(!s.contains("  \n"), "no line of nothing but columns: {s:?}");
}

/// A chunk that ends with a line break leaves the block standing at the start of a
/// line, and the chunk after it is set in like the line it continues -- which is the
/// one thing this front end has to remember between chunks.
#[test]
fn a_chunk_after_a_break_continues_the_line_it_started() {
    let (mut r, buf) = with_buffer(true);
    r.tool_output("one\n");
    r.tool_output("two\n");
    r.tool_output("three");
    r.tool_result("exit_code: 0");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(s.contains("\x1b[2m· one\n  two\n  three\x1b[0m\n"), "{s:?}");
}

/// Nothing is written for a command that printed nothing: an empty chunk is not a
/// line, and a block opened on one would leave the marker behind on its own.
#[test]
fn an_empty_chunk_opens_no_block() {
    let (mut r, buf) = with_buffer(true);
    r.tool_output("");
    r.tool_result("exit_code: 0");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    // A Bash result without stdout is just the exit code -- no "· N bytes"
    // suffix, because the suffix was a measure of the result, not of
    // anything a reader needs to know.
    assert_eq!(s, "\x1b[2m· exit_code: 0\x1b[0m\n");
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
            thinking: None,
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
    // tool messages only get a summary, never the full text. A Bash
    // summary is just the exit code -- the first line of stdout is
    // content, and the transcript's rule is that the summary is metadata.
    assert!(s.contains("exit_code: 0"), "{s}");
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
        thinking: None,
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
            thinking: None,
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
            thinking: None,
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
        "\x1b[2m· exit_code: 0\x1b[0m\n",
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
fn tool_result_shows_exit_code_only() {
    let (mut r, buf) = with_buffer(false);
    r.tool_result("exit_code: 3\n--- stdout ---\nhello");
    r.tool_result("");
    let s = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    // A Bash result: just the exit code, no first line of stdout, no
    // byte count. The transcript's rule is that the summary is metadata.
    assert!(s.contains("exit_code: 3"), "{s}");
    assert!(!s.contains("hello"), "the stdout body must not leak: {s}");
    // An empty result: no recognized prefix, the default summary
    // (first line and byte count) is what falls out.
    assert!(s.contains(" · 0 bytes"), "{s}");
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
        thinking: None,
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
        "a change is set in under its call, in the columns the call's lines are: {drawn:?}"
    );

    let (mut r, buf) = with_buffer(false);
    r.replay(&[Message::user("alpha\nbeta")]);
    let drawn = buf_of(&buf);
    assert!(
        drawn.contains("› alpha\n  beta"),
        "a draft that was sent with a line in it reads back the same way: {drawn:?}"
    );
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

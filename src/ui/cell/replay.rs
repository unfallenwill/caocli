//! Fold a session log back into cells: the replay that makes a resumed session
//! read like the one that was watched live.
//!
//! An assistant message with tool calls is paired with the tool messages that
//! follow it: each pair becomes one [`Cell::Step`] (for regular tools) or one
//! rich cell followed by a [`Cell::Notice`] (for the question and todo tools,
//! whose arguments are themselves the cell). The pairing is what makes live
//! and replay produce the same cells from the same wire log -- the test in
//! `step.rs` pins this property.
//!
//! The injection message that carries project instructions folds into the
//! same notice the live turn emitted -- it is machinery in the shape of a
//! user message, and reading it back as a user line would be the front end
//! lying about who said what.

use crate::image;
use crate::types::{Message, Role};

use super::Cell;
use super::step::Step;

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
                // The CoT is one slot regardless of wire; the rendering
                // wants the text, and `Cot::as_text` is the one place
                // that knows how to flatten blocks / items into a string.
                if let Some(cot) = &m.cot
                    && let Some(r) = cot.as_text()
                    && !r.is_empty()
                {
                    cells.push(Cell::Reasoning(r));
                }
                if let Some(text) = m.text()
                    && !text.is_empty()
                {
                    cells.push(Cell::Content(text));
                }
                // Each tool call opens a step (or its dedicated rich cell
                // for the question / todo tools). The matching tool message
                // settles it on the next iteration; the walk is in order
                // because the wire log is in order.
                if let Some(calls) = &m.tool_calls {
                    for call in calls {
                        let name = &call.function.name;
                        let args = &call.function.arguments;
                        cells.push(Cell::from_tool_call(name, args));
                    }
                }
            }
            Role::Tool => {
                // Settle the last cell if it is an open step; otherwise the
                // tool message answers a question or todo (which keep their
                // own rich cell) and we file its text as a notice.
                let text = m.text().unwrap_or_default();
                settle_or_notice(&mut cells, &text);
            }
            // The system prompt is a compile-time constant and is never part of
            // a session log, so there is nothing to replay for it.
            Role::System => {}
        }
    }
    cells
}

/// Pair a `Role::Tool` message with the preceding tool call: settle an open
/// step in place if one is at the end of the transcript, or append a notice
/// (the answer to a question, the update confirmation for a todo).
fn settle_or_notice(cells: &mut Vec<Cell>, text: &str) {
    if let Some(Cell::Step(step)) = cells.last_mut()
        && !step.status.is_settled()
    {
        step.settle(text);
        return;
    }
    // No open step at the end: the preceding cell is a Question, a Todo,
    // or something else. The wire log keeps the result text, and the
    // reader benefits from seeing it, so it goes in as a dim notice after
    // whatever rich cell is there.
    cells.push(Cell::Notice(text.to_owned()));
}

// The replay step constructor is reachable through this module for tests
// that want to build a settled step by hand. `Step` itself is the public
// surface; this keeps the helper available without exposing it on `Cell`.
#[allow(dead_code)]
pub(crate) fn settled_step(
    verb: &str,
    args: &str,
    children: Vec<String>,
    verdict: String,
    failed: bool,
) -> Step {
    Step::replayed(verb, args, children, verdict, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolCallFunction};
    use crate::ui::cell::StepStatus;

    fn assistant(
        reasoning: Option<&str>,
        content: Option<&str>,
        calls: Option<Vec<ToolCall>>,
    ) -> Message {
        Message {
            role: Role::Assistant,
            content: content.map(Into::into),
            cot: reasoning.map(|text| crate::types::Cot::OpenAiText {
                text: text.to_owned(),
            }),
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
        // User + Reasoning + Content + Step (settled Bash). The result text
        // settles the step in place; no fifth cell.
        assert_eq!(cells.len(), 4);
        assert!(matches!(cells[0], Cell::User { .. }));
        assert!(matches!(cells[1], Cell::Reasoning(_)));
        assert!(matches!(cells[2], Cell::Content(_)));
        assert!(
            matches!(cells[3], Cell::Step(ref s) if s.verb == "Bash" && s.status == StepStatus::Done)
        );
    }

    #[test]
    fn a_bash_failure_in_replay_is_a_settled_failed_step() {
        let cells = from_messages(&[
            Message::user("test"),
            assistant(
                None,
                None,
                Some(vec![call("Bash", r#"{"command":"false"}"#)]),
            ),
            Message::tool("call_1", "exit_code: 1\n--- stdout ---\nfoo"),
        ]);
        // cells[0] is the User cell; the Step is cells[1].
        match &cells[1] {
            Cell::Step(s) => {
                assert_eq!(s.status, StepStatus::Failed);
                assert_eq!(s.verdict, "exit_code: 1");
            }
            other => panic!("expected a step, got {other:?}"),
        }
    }

    #[test]
    fn a_question_replay_is_the_rich_cell_followed_by_the_answer() {
        let args = r#"{"questions":[{"id":"a","header":"x","question":"y","options":[],"multi_select":false}]}"#;
        let cells = from_messages(&[
            assistant(None, None, Some(vec![call("AskUserQuestion", args)])),
            Message::tool("call_1", "answer: first option"),
        ]);
        assert!(matches!(cells[0], Cell::Question(_)));
        match &cells[1] {
            Cell::Notice(text) => assert_eq!(text, "answer: first option"),
            other => panic!("expected a notice, got {other:?}"),
        }
    }

    #[test]
    fn a_todo_replay_is_the_list_followed_by_the_update_line() {
        let args = r#"{"todos":[{"content":"Run the gates","status":"in_progress"}]}"#;
        let cells = from_messages(&[
            assistant(None, None, Some(vec![call("TodoWrite", args)])),
            Message::tool("call_1", "todo list updated (0/1 done)"),
        ]);
        assert!(matches!(cells[0], Cell::Todo(_)));
        match &cells[1] {
            Cell::Notice(text) => assert_eq!(text, "todo list updated (0/1 done)"),
            other => panic!("expected a notice, got {other:?}"),
        }
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
    fn replay_reads_the_users_images_out_of_the_message() {
        let message = Message::user_with_images(
            "what is this?",
            vec!["data:image/png;base64,Zm9vYmFy".into()],
        );
        assert_eq!(
            from_messages(&[message]),
            vec![Cell::User {
                text: "what is this?".into(),
                images: vec![crate::image::Note {
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

    /// A user line is never taken for an injected message.
    #[test]
    fn a_user_line_is_never_taken_for_an_injected_message() {
        let cells = from_messages(&[Message::user("Project instructions are a good idea")]);
        assert!(matches!(cells[0], Cell::User { .. }));
    }
}

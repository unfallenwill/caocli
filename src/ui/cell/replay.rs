//! Fold a session log back into cells: the replay that makes a resumed session
//! read like the one that was watched live.
//!
//! Tool messages contribute only a summary, so replaying never re-dumps a
//! tool's output. The injection message that carries project instructions
//! folds into the same notice the live turn emitted -- it is machinery in
//! the shape of a user message, and reading it back as a user line would be
//! the front end lying about who said what.

use crate::image;
use crate::types::{Message, Role};

use super::Cell;

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

    #[test]
    fn a_user_line_is_never_taken_for_an_injected_message() {
        let cells = from_messages(&[Message::user("Project instructions are a good idea")]);
        assert!(matches!(cells[0], Cell::User { .. }));
    }
}

//! The todo tool's cell and the standing list: how the plan is shown, and how
//! the list that is still true is recovered from the transcript.
//!
//! Two halves share this module because they share the same vocabulary -- the
//! mark on a task, the style of a state, the line a task's words are written
//! on. The cell is one cell's worth of spans; `standing_todos` is the fold
//! over the whole transcript that picks the last list a call wrote.

use crate::tools::todo::{self, Status, Todo};
use crate::ui::cell::{Cell, Gutter, Span, Style};

/// What opens a task's line: the mark, and the blank that sets its words apart
/// from it.
pub(super) fn todo_head(status: Status) -> &'static str {
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
pub(super) fn todo_style(status: Status) -> Style {
    match status {
        Status::Pending => Style::Plain,
        Status::InProgress => Style::Yellow,
        Status::Completed => Style::Dim,
    }
}

/// The todo tool's cell: how far the list has got, then the list.
///
/// The head is the same line the model is answered with, so what the reader is
/// shown and what the model was told cannot disagree about the same list.
pub(super) fn todo_spans(todos: &[Todo]) -> Vec<Span> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolCallFunction};

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

    fn assistant(
        reasoning: Option<&str>,
        content: Option<&str>,
        calls: Option<Vec<ToolCall>>,
    ) -> crate::types::Message {
        crate::types::Message {
            role: crate::types::Role::Assistant,
            content: content.map(Into::into),
            reasoning_content: reasoning.map(str::to_owned),
            tool_calls: calls,
            tool_call_id: None,
            thinking: None,
        }
    }

    /// The todo tool's cell is read back out of the call's own arguments, so a
    /// resumed session shows the list exactly as it was written -- which is also
    /// what lets the list be a fold of the log rather than a second thing to keep
    /// in step with it.
    #[test]
    fn replay_of_a_todo_call_is_the_same_list() {
        let args = r#"{"todos":[{"content":"Run the gates","status":"in_progress"}]}"#;
        let cells = super::super::replay::from_messages(&[
            assistant(None, None, Some(vec![call("TodoWrite", args)])),
            crate::types::Message::tool("call_1", "todo list updated (0/1 done)"),
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
            assert_eq!(
                crate::ui::text::width(head.trim_end()),
                1,
                "{head:?} takes one column"
            );
            assert_eq!(
                crate::ui::text::width(head),
                crate::ui::cell::MARKER_COLUMNS,
                "{head:?} and its blank"
            );
        }
        let states: std::collections::HashSet<&str> =
            heads.map(str::trim_end).into_iter().collect();
        assert_eq!(states.len(), 3, "the three states read differently");
        for status in [Status::Pending, Status::InProgress, Status::Completed] {
            let gutter = todo_gutter(&Todo {
                content: "x".into(),
                status,
            });
            assert_eq!(gutter.width(), crate::ui::cell::MARKER_COLUMNS);
            assert_eq!(
                crate::ui::text::width(gutter.rest),
                crate::ui::cell::MARKER_COLUMNS
            );
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
}

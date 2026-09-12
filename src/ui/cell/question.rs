//! The question tool's cell: how the questions become spans.
//!
//! The question cell is read back out of the call's own arguments, which is
//! what keeps a resumed session and a watched one reading the same. What this
//! module is for is the formatting: the heading, the option list, the dim
//! marker for "choose any", and the case where a question gives the user
//! nothing to pick from.

use crate::tools::ask::Question;
use crate::ui::cell::{Span, Style};

/// The question tool's cell: what is being asked, and what there is to choose
/// from.
///
/// One block per question, in the order they were asked, with the options
/// numbered: the number is what a front end reading a typed answer matches, and
/// what the panel's own keys jump to. The numbers and the descriptions are dim
/// because the label is what the answer is made of; the question itself is
/// painted like the call it is, since that is what it is.
pub(super) fn question_spans(questions: &[Question]) -> Vec<Span> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolCallFunction};
    use crate::ui::cell::Cell;

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

    /// The question tool's cell is read back out of the call's own arguments, so
    /// a resumed session shows the questions exactly as the live one did -- the
    /// options and all, which is what a reader needs to make sense of the answer
    /// line that follows.
    #[test]
    fn replay_of_a_question_call_is_the_questions_again() {
        let args =
            r#"{"questions":[{"id":"auth","question":"Which auth?","options":[{"label":"JWT"}]}]}"#;
        let cells = super::super::replay::from_messages(&[
            crate::types::Message {
                role: crate::types::Role::Assistant,
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![call("AskUserQuestion", args)]),
                tool_call_id: None,
                thinking: None,
            },
            crate::types::Message::tool("call_1", "auth: JWT"),
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
}

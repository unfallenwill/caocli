//! The todo tool: the plan for the work in hand, written where a person can see
//! it.
//!
//! The list is **not state**, and nothing here keeps any: it is the call's own
//! arguments. The model sends the whole list every time it moves a line along,
//! so the list standing at any moment is the arguments of the last such call --
//! which is what lets a resumed session read it back out of the log and show
//! exactly what the session that was watched live showed. There is no second
//! copy to keep in step, and that is also why the call has no side effect at
//! all: writing the list down *is* the result.
//!
//! Unlike the question tool this one is executed here rather than by the front
//! end: it is one string in and one string out like any other tool, and what the
//! answer says does not depend on anybody being at the front end. The front ends
//! render it -- a cell in the transcript, and a block kept in view while the turn
//! runs -- out of these same arguments.

use serde_json::{Value, json};

use super::{checked, parse_args, required_string};
use crate::types::{FunctionDef, ToolDef};

/// The tool's name on the wire.
pub const TODO_NAME: &str = "TodoWrite";

/// How many tasks one list may hold.
///
/// A cap rather than a suggestion: the list is drawn where a person is meant to
/// read it while the work runs, and a list long enough to be a document is one
/// nobody reads at all.
///
/// Declared to the model as well as enforced here. The schema is what it reads
/// before it plans a list; this parser is what it meets only after it has
/// written one too long. One constant feeds both, so a cap moved for one is a
/// cap moved for the other.
pub const MAX_ITEMS: usize = 20;

/// Where a task stands.
///
/// Three states rather than a pair of booleans, because they are the three a
/// reader looks for: not started, being worked on, done. Which one a task is in
/// is the whole of what moves while the turn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Not started.
    Pending,
    /// Being worked on. The model is asked for one at a time: two tasks in hand
    /// is a plan that has stopped saying what is happening.
    InProgress,
    /// Done.
    Completed,
}

impl Status {
    fn parse(s: &str) -> Result<Self, String> {
        match s {
            "pending" => Ok(Status::Pending),
            "in_progress" => Ok(Status::InProgress),
            "completed" => Ok(Status::Completed),
            other => Err(format!(
                "status must be one of pending, in_progress, completed (got {other:?})"
            )),
        }
    }
}

/// One task: what it is, and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Todo {
    /// What the task is, in the imperative -- the model's own words, shown to the
    /// user unaltered.
    pub content: String,
    pub status: Status,
}

pub fn definition() -> ToolDef {
    ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: TODO_NAME.into(),
            description: Some(
                "Record the plan for the work in hand, and keep it up to date as you go. Use it \
                 for work that takes several steps; a question answered in one line does not \
                 need a list. The whole list is sent every time and replaces the one before it, \
                 so each call is the state of the work rather than a change to it: send every \
                 task, not only the one that moved. The list is shown to the user and stays in \
                 view while the turn runs, so keep the items short, keep at most one task \
                 in_progress, and mark a task completed as soon as it is done rather than in a \
                 batch at the end. Send an empty list when nothing is left to track. Writing \
                 the list is not doing the work: it is how the user sees what is being done."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "The whole list, in the order the work is done. Send an empty array to clear it.",
                        "maxItems": MAX_ITEMS,
                        "items": {
                            "type": "object",
                            "additionalProperties": true,
                            "properties": {
                                "content": {
                                    "type": "string",
                                    "description": "What the task is, in the imperative: \"Add the parse function\"."
                                },
                                "status": {
                                    "type": "string",
                                    "enum": ["pending", "in_progress", "completed"],
                                    "description": "Where the task stands. Defaults to pending."
                                }
                            },
                            "required": ["content"]
                        }
                    }
                },
                "required": ["todos"]
            })),
        },
    }
}

/// Parse and check a call's arguments.
///
/// Every failure is text: it becomes the tool's result and the model corrects
/// itself, the way it does for a bad `old_string`. A call that cannot be read is
/// never a hard error -- and never a list put in front of the user, because a
/// list nobody can read is not a plan.
///
/// An empty list is not a failure: it is how the model says there is nothing left
/// to track, and it is what takes the list off the screen.
pub fn parse(args_json: &str) -> Result<Vec<Todo>, String> {
    let args = parse_args(args_json)?;
    let raw = args
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| "error: missing required argument todos (array)".to_owned())?;
    if raw.len() > MAX_ITEMS {
        return Err(format!(
            "error: at most {MAX_ITEMS} todos per call (got {})",
            raw.len()
        ));
    }
    raw.iter()
        .enumerate()
        .map(|(i, item)| one_todo(item, i))
        .collect()
}

/// One task out of the array, with its path in the error text.
fn one_todo(item: &Value, index: usize) -> Result<Todo, String> {
    let path = |field: &str| format!("todos[{index}].{field}");
    let content = checked(required_string(item, "content"), &path("content"))?;
    let status = match item.get("status") {
        // A task the model did not place is one that has not been started: the
        // default is the state almost every item is in when it is first written.
        None | Some(Value::Null) => Status::Pending,
        Some(Value::String(s)) => {
            Status::parse(s).map_err(|e| format!("error: {}: {e}", path("status")))?
        }
        Some(_) => {
            return Err(format!(
                "error: {}: status must be a string",
                path("status")
            ));
        }
    };
    Ok(Todo { content, status })
}

/// Run the call: the arguments are read, and the result says the list was taken
/// down. Deterministic text, like every other thing that goes into the log.
pub fn execute(args_json: &str) -> String {
    match parse(args_json) {
        Ok(todos) => result_text(&todos),
        Err(e) => e,
    }
}

/// The tool result for a list the model just wrote.
///
/// The counts and not the list: the model wrote the list and can read it back
/// from its own call, and what it cannot otherwise know is whether the call
/// landed at all. With one exception: the asking is done in prose the model may
/// skim, so a call that leaves two tasks in hand is answered with the count it
/// has no reason to take of its own marks.
pub fn result_text(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "todo list cleared".to_owned();
    }
    // [`summary`] and not a second way of counting the same tasks, so that what
    // the model is told and what the user is shown cannot disagree.
    let mut state = summary(todos);
    // Silent while the rule holds, so that it stays a signal rather than a
    // suffix on every call -- and out of [`summary`], which is a head line for a
    // person who has the list itself on their screen, a mark per task.
    let in_hand = todos
        .iter()
        .filter(|t| t.status == Status::InProgress)
        .count();
    if in_hand > 1 {
        state.push_str(&format!("; {in_hand} in progress"));
    }
    format!("todo list updated ({state})")
}

/// How far the list has got, in the few columns a head line can spare for it.
///
/// One line for both surfaces that show the list -- the cell in the transcript
/// and the block kept in view -- so that the two cannot say different things
/// about the same list.
pub fn summary(todos: &[Todo]) -> String {
    if todos.is_empty() {
        return "list cleared".to_owned();
    }
    let done = todos
        .iter()
        .filter(|t| t.status == Status::Completed)
        .count();
    format!("{done}/{} done", todos.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: Value) -> String {
        json.to_string()
    }

    fn one(json: Value) -> Result<Vec<Todo>, String> {
        parse(&args(json))
    }

    fn todo(content: &str, status: Status) -> Todo {
        Todo {
            content: content.into(),
            status,
        }
    }

    #[test]
    fn a_list_round_trips_through_a_parse() {
        let todos = one(json!({"todos": [
            {"content": "Add the parse function", "status": "completed"},
            {"content": "Draw the cell", "status": "in_progress"},
            {"content": "Run the gates"}
        ]}))
        .unwrap();
        assert_eq!(
            todos,
            vec![
                todo("Add the parse function", Status::Completed),
                todo("Draw the cell", Status::InProgress),
                // A task with no status is one not started yet.
                todo("Run the gates", Status::Pending),
            ]
        );
    }

    #[test]
    fn an_empty_list_is_how_a_list_is_cleared() {
        assert_eq!(one(json!({"todos": []})).unwrap(), Vec::new());
        assert_eq!(result_text(&[]), "todo list cleared");
        assert_eq!(summary(&[]), "list cleared");
    }

    #[test]
    fn the_result_says_the_call_landed_and_no_more() {
        let todos = vec![
            todo("a", Status::Completed),
            todo("b", Status::InProgress),
            todo("c", Status::Pending),
        ];
        // The model wrote the list and can read it back; what it cannot know
        // otherwise is whether the call landed. The same words the user sees,
        // because it is the same function -- and one task in hand is the rule
        // kept, so there is nothing here to correct.
        assert_eq!(result_text(&todos), "todo list updated (1/3 done)");
        assert_eq!(summary(&todos), "1/3 done");
        assert!(result_text(&todos).contains(&summary(&todos)));
    }

    #[test]
    fn only_two_tasks_in_hand_are_counted_back_at_the_model() {
        fn list(statuses: &[Status]) -> Vec<Todo> {
            statuses.iter().map(|s| todo("t", *s)).collect()
        }

        // Nothing in hand, and the one in hand the tool asks for: both are the
        // plain count, with nothing for the model to correct.
        let at_most_one: [&[Status]; 4] = [
            &[Status::Completed, Status::Pending],
            &[Status::Completed, Status::InProgress],
            &[Status::InProgress],
            &[Status::Pending],
        ];
        for statuses in at_most_one {
            let text = result_text(&list(statuses));
            assert!(!text.contains("in progress"), "{text}");
        }

        // Two is the rule broken, and the one thing the model has no reason to
        // read off its own call.
        let several: [(&[Status], &str); 2] = [
            (
                &[Status::InProgress; 2],
                "todo list updated (0/2 done; 2 in progress)",
            ),
            (
                &[Status::InProgress; 3],
                "todo list updated (0/3 done; 3 in progress)",
            ),
        ];
        for (statuses, expected) in several {
            let todos = list(statuses);
            assert_eq!(result_text(&todos), expected);
            // And the head line a person reads is untouched by any of it: the
            // list is on their screen, a mark per task, and how many are in hand
            // is not something they asked to be told.
            assert_eq!(summary(&todos), format!("0/{} done", statuses.len()));
        }

        // A list with nothing left to track is still just cleared.
        assert_eq!(result_text(&[]), "todo list cleared");
    }

    #[test]
    fn an_unreadable_call_is_text_for_the_model_to_correct() {
        let cases: Vec<(Value, &str)> = vec![
            (json!({}), "error: missing required argument todos (array)"),
            (
                json!({"todos": "plan"}),
                "error: missing required argument todos (array)",
            ),
            (
                json!({"todos": [{"status": "pending"}]}),
                "error: todos[0].content: missing required argument content (string)",
            ),
            (
                json!({"todos": [{"content": "   "}]}),
                "error: todos[0].content: content must not be empty",
            ),
            (
                json!({"todos": [{"content": 7}]}),
                "error: todos[0].content: content must be a string",
            ),
            (
                json!({"todos": [{"content": "a", "status": "doing"}]}),
                "error: todos[0].status: status must be one of pending, in_progress, completed (got \"doing\")",
            ),
            (
                json!({"todos": [{"content": "a", "status": true}]}),
                "error: todos[0].status: status must be a string",
            ),
            (
                // The second item, so the index in the path is the one that is wrong.
                json!({"todos": [{"content": "a"}, {"content": "b", "status": 1}]}),
                "error: todos[1].status: status must be a string",
            ),
        ];
        for (bad, expected) in cases {
            let got = one(bad.clone()).unwrap_err();
            assert!(
                got.starts_with(expected),
                "{bad} should be {expected:?}, was {got:?}"
            );
        }
        assert!(
            one(json!({"todos": (0..=MAX_ITEMS).map(|i| json!({"content": format!("t{i}")})).collect::<Vec<_>>()})).unwrap_err()
                .starts_with(&format!("error: at most {MAX_ITEMS} todos per call (got 21)"))
        );
        // Arguments that are not JSON at all are the one failure that says
        // nothing about the list: the tool never got to read any.
        assert!(
            parse("not json")
                .unwrap_err()
                .starts_with("error: arguments are not valid JSON")
        );
    }

    #[test]
    fn execute_answers_a_bad_call_with_text_rather_than_failing() {
        // The same shape as every other tool: a failure the model can read and
        // correct, never a hard error that ends the turn.
        assert!(execute("not json").starts_with("error: "));
        assert_eq!(
            execute(&args(json!({"todos": [{"content": "Run the gates"}]}))),
            "todo list updated (0/1 done)"
        );
    }

    /// The schema is what the model reads before it decides how to plan, and it
    /// is part of the request prefix: a reworded description is a different
    /// prefix and a cache miss for every session that ran before it. Pinned so
    /// that rewording it is a decision rather than an accident.
    #[test]
    fn the_schema_the_model_reads_is_frozen() {
        let parameters = definition().function.parameters.unwrap();
        assert_eq!(parameters["required"], json!(["todos"]));
        // The cap is in the schema and in the parser, both out of [`MAX_ITEMS`]:
        // what the model is told before it plans a list is the same number that
        // is enforced once it has written one.
        assert_eq!(
            parameters["properties"]["todos"]["maxItems"],
            json!(MAX_ITEMS)
        );
        assert_eq!(
            parameters["properties"]["todos"]["items"]["required"],
            json!(["content"])
        );
        assert_eq!(
            parameters["properties"]["todos"]["items"]["properties"]["status"]["enum"],
            json!(["pending", "in_progress", "completed"])
        );
        assert_eq!(
            definition().function.name,
            "TodoWrite",
            "the name is the model's whole handle on this tool"
        );
    }

    #[test]
    fn the_three_words_the_schema_offers_are_the_three_that_parse() {
        // The vocabulary is closed: the schema offers exactly these three, and a
        // fourth the model invents is text it can correct rather than a task
        // silently in no state at all.
        for (word, status) in [
            ("pending", Status::Pending),
            ("in_progress", Status::InProgress),
            ("completed", Status::Completed),
        ] {
            let todos = one(json!({"todos": [{"content": "a", "status": word}]})).unwrap();
            assert_eq!(todos[0].status, status, "{word}");
        }
        assert!(Status::parse("Pending").is_err(), "the words are exact");
        assert!(Status::parse("").is_err());
        assert!(Status::parse("done").is_err());
    }
}

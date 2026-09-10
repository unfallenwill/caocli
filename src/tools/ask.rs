//! The question tool: the one call whose result comes from the user rather than
//! from the machine.
//!
//! The tool is defined here and its arguments are parsed here, but it is *not*
//! executed here: every other tool is one string in and one string out, while a
//! question has to be answered by whoever is sitting at the front end. The
//! interpreter asks through the front end's `Ask` channel and writes the answer
//! back as the tool's result.

use std::collections::HashSet;

use serde_json::{Value, json};

use super::parse_args;
use crate::types::{FunctionDef, ToolDef};

/// The tool's name on the wire. Named for what it does rather than for the
/// question, because the model reads the name first: this is the one call that
/// stops the turn until a person answers.
pub const ASK_NAME: &str = "AskUserQuestion";

/// How many questions one call may ask. A question stops the turn, and a form
/// long enough to scroll is one a user answers carelessly; the model is asked
/// instead to spend its questions in one round rather than to spend them one at
/// a time.
pub const MAX_QUESTIONS: usize = 4;

/// How many options one question may offer. Beyond four the choices stop being
/// a choice and become a list to read.
pub const MAX_OPTIONS: usize = 4;

/// What the result says for a question the user left unanswered. Deterministic
/// text, like every other thing this tool writes into the log.
pub const NO_ANSWER: &str = "(no answer)";

/// One option: what the user picks, and the sentence they read while picking.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub label: String,
    pub description: String,
}

/// One question as the model asked it.
///
/// `options` empty means the question is answered in the user's own words: the
/// model does not have to guess the alternatives to be able to ask.
#[derive(Debug, Clone, PartialEq)]
pub struct Question {
    /// The stable id the answer is reported under.
    pub id: String,
    /// A short heading, empty when the model gave none.
    pub header: String,
    /// The question itself.
    pub question: String,
    /// What there is to choose from, empty for a free-text answer.
    pub options: Vec<Choice>,
    /// Whether more than one option may be picked.
    pub multi_select: bool,
}

/// One question's answer: the labels the user picked, in the order they were
/// offered, or the words they typed when there was nothing to pick from. Empty
/// when the question was left unanswered.
#[derive(Debug, Clone, PartialEq)]
pub struct Answer {
    pub id: String,
    pub labels: Vec<String>,
}

pub fn definition() -> ToolDef {
    ToolDef {
        r#type: "function".into(),
        function: FunctionDef {
            name: ASK_NAME.into(),
            description: Some(
                "Ask the user a question and wait for the answer. Use it when a choice is the \
                 user's to make -- which of several approaches to take, which file is meant, \
                 whether to go ahead -- and when guessing would waste the work. Asking stops \
                 the turn until they answer, so ask everything in one call rather than in \
                 several, and do not ask what you can find out yourself. Each question carries \
                 a stable id, the words to ask, and up to four options; an option is a label \
                 (what they pick) plus one sentence of description (what they read). Put the \
                 option you recommend first and end its label with \"(Recommended)\". A \
                 question with no options is answered in the user's own words. The result \
                 reports the chosen labels per question id."
                    .into(),
            ),
            parameters: Some(json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "Questions to ask the user before continuing.",
                        "items": {
                            "type": "object",
                            "additionalProperties": true,
                            "properties": {
                                "id": {
                                    "type": "string",
                                    "description": "Stable id for this question; echoed in the answer."
                                },
                                "question": {
                                    "type": "string",
                                    "description": "The specific question to ask the user."
                                },
                                "header": {
                                    "type": "string",
                                    "description": "Optional short heading for the question, such as \"Confirm\" or \"Choose Mode\"."
                                },
                                "options": {
                                    "type": "array",
                                    "description": "Optional choices to show the user. If you recommend one, put it first and append \"(Recommended)\" to that label.",
                                    "items": {
                                        "type": "object",
                                        "additionalProperties": true,
                                        "properties": {
                                            "label": {
                                                "type": "string",
                                                "description": "Short user-facing option label."
                                            },
                                            "description": {
                                                "type": "string",
                                                "description": "One sentence explaining the tradeoff or impact."
                                            }
                                        },
                                        "required": ["label"]
                                    }
                                },
                                "multi_select": {
                                    "type": "boolean",
                                    "description": "Whether the user may select more than one option. Defaults to false."
                                }
                            },
                            "required": ["id", "question"]
                        }
                    }
                },
                "required": ["questions"]
            })),
        },
    }
}

/// Parse and check a call's arguments.
///
/// Every failure is text: it becomes the tool's result and the model corrects
/// itself, the way it does for a bad `old_string`. A call that cannot be read is
/// never a hard error and never reaches the user as a question -- an unreadable
/// question is one nobody can answer.
pub fn parse(args_json: &str) -> Result<Vec<Question>, String> {
    let args = parse_args(args_json)?;
    let raw = args
        .get("questions")
        .and_then(Value::as_array)
        .ok_or_else(|| "error: missing required argument questions (array)".to_owned())?;
    if raw.is_empty() {
        return Err("error: questions must not be empty".to_owned());
    }
    if raw.len() > MAX_QUESTIONS {
        return Err(format!(
            "error: at most {MAX_QUESTIONS} questions per call (got {})",
            raw.len()
        ));
    }
    let mut ids: HashSet<String> = HashSet::new();
    let mut questions = Vec::with_capacity(raw.len());
    for (i, item) in raw.iter().enumerate() {
        let question = one_question(item, i, &mut ids)?;
        questions.push(question);
    }
    Ok(questions)
}

/// One question out of the array, with its path in the error text.
fn one_question(item: &Value, index: usize, ids: &mut HashSet<String>) -> Result<Question, String> {
    let path = |field: &str| format!("questions[{index}].{field}");
    let id = checked(required_string(item, "id"), &path("id"))?;
    if !ids.insert(id.clone()) {
        return Err(format!("error: {}: duplicate id {id:?}", path("id")));
    }
    let question = checked(required_string(item, "question"), &path("question"))?;
    let header = checked(optional_string(item, "header"), &path("header"))?;
    let multi_select = checked(optional_bool(item, "multi_select"), &path("multi_select"))?;
    let options = match item.get("options") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(raw)) => {
            if raw.len() > MAX_OPTIONS {
                return Err(format!(
                    "error: {}: at most {MAX_OPTIONS} options per question (got {})",
                    path("options"),
                    raw.len()
                ));
            }
            raw.iter()
                .enumerate()
                .map(|(j, option)| one_option(option, index, j))
                .collect::<Result<Vec<_>, _>>()?
        }
        Some(_) => {
            return Err(format!(
                "error: {}: options must be an array",
                path("options")
            ));
        }
    };
    Ok(Question {
        id,
        header,
        question,
        options,
        multi_select,
    })
}

/// One option out of a question's array.
fn one_option(option: &Value, question: usize, index: usize) -> Result<Choice, String> {
    let path = |field: &str| format!("questions[{question}].options[{index}].{field}");
    let label = checked(required_string(option, "label"), &path("label"))?;
    let description = checked(optional_string(option, "description"), &path("description"))?;
    Ok(Choice { label, description })
}

/// A required string that is there and not blank.
fn required_string(v: &Value, key: &str) -> Result<String, String> {
    match v.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(format!("{key} must not be empty")),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("missing required argument {key} (string)")),
    }
}

/// An optional string: absent, null and empty all mean "not given".
fn optional_string(v: &Value, key: &str) -> Result<String, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(String::new()),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// An optional boolean, false when not given.
fn optional_bool(v: &Value, key: &str) -> Result<bool, String> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("{key} must be a boolean")),
    }
}

/// Prefix a failure with the path it was found at.
fn checked<T>(value: Result<T, String>, path: &str) -> Result<T, String> {
    value.map_err(|e| format!("error: {path}: {e}"))
}

/// The tool result for a call the user answered: one line per question, in the
/// order they were asked, as `<id>: <chosen labels>`.
///
/// The id is the model's own, so the answer says which question it belongs to
/// without repeating the words. A question left unanswered says so rather than
/// being left out: a line that is missing reads as a question that was never
/// asked, and the model would ask it again. The answer lines are the call's
/// result, so they are also what a resumed session shows for it.
pub fn answer_text(questions: &[Question], answers: &[Answer]) -> String {
    questions
        .iter()
        .map(|question| {
            let chosen = answers
                .iter()
                .find(|a| a.id == question.id)
                .map(|a| a.labels.as_slice())
                .unwrap_or_default();
            let text = if chosen.is_empty() {
                NO_ANSWER.to_owned()
            } else {
                // One line per question is what makes the result readable as a
                // whole, so a label typed over several lines is folded into one.
                chosen
                    .iter()
                    .map(|label| label.replace('\n', " "))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!("{}: {text}", question.id)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: Value) -> String {
        json.to_string()
    }

    fn one(json: Value) -> Result<Vec<Question>, String> {
        parse(&args(json))
    }

    /// The schema is what the model reads before it decides how to ask, and it is
    /// part of the request prefix: a reworded description is a different prefix
    /// and a cache miss for every session that ran before it. Pinned so that
    /// rewording it is a decision rather than an accident.
    #[test]
    fn the_schema_the_model_reads_is_frozen() {
        let parameters = definition().function.parameters.unwrap();
        assert_eq!(parameters["required"], json!(["questions"]));
        let item = &parameters["properties"]["questions"]["items"];
        assert_eq!(item["required"], json!(["id", "question"]));
        assert_eq!(item["additionalProperties"], json!(true));
        assert_eq!(
            item["properties"]["multi_select"]["description"],
            json!("Whether the user may select more than one option. Defaults to false.")
        );
        assert_eq!(
            item["properties"]["options"]["items"]["required"],
            json!(["label"])
        );
        assert_eq!(definition().function.name, ASK_NAME);
    }

    #[test]
    fn a_call_parses_into_the_questions_it_asked() {
        let questions = one(json!({"questions": [{
            "id": "auth",
            "header": "Auth",
            "question": "Which auth should we use?",
            "options": [
                {"label": "JWT (Recommended)", "description": "one token for the API"},
                {"label": "Session cookie"}
            ],
            "multi_select": true
        }]}))
        .unwrap();
        assert_eq!(
            questions,
            vec![Question {
                id: "auth".into(),
                header: "Auth".into(),
                question: "Which auth should we use?".into(),
                options: vec![
                    Choice {
                        label: "JWT (Recommended)".into(),
                        description: "one token for the API".into()
                    },
                    // A description the model left out reads as no description,
                    // not as a missing field to complain about.
                    Choice {
                        label: "Session cookie".into(),
                        description: String::new()
                    },
                ],
                multi_select: true,
            }]
        );
    }

    #[test]
    fn the_optional_fields_have_their_defaults() {
        let questions = one(json!({"questions": [{"id": "a", "question": "Which?"}]})).unwrap();
        assert_eq!(
            questions[0],
            Question {
                id: "a".into(),
                header: String::new(),
                question: "Which?".into(),
                options: Vec::new(),
                multi_select: false,
            }
        );
    }

    #[test]
    fn a_question_with_no_options_is_answered_in_words() {
        // The empty array is the same as no array at all: either way there is
        // nothing to pick from.
        let with_empty =
            one(json!({"questions": [{"id": "a", "question": "q", "options": []}]})).unwrap();
        assert!(with_empty[0].options.is_empty());
    }

    #[test]
    fn an_unreadable_call_is_text_for_the_model_to_correct() {
        let cases: Vec<(Value, &str)> = vec![
            (
                json!({}),
                "error: missing required argument questions (array)",
            ),
            (
                json!({"questions": []}),
                "error: questions must not be empty",
            ),
            (
                json!({"questions": "auth"}),
                "error: missing required argument questions (array)",
            ),
            (
                json!({"questions": [{"id": "a", "question": "q"}, {"id": "a", "question": "q"}]}),
                "error: questions[1].id: duplicate id \"a\"",
            ),
            (
                json!({"questions": [{"question": "q"}]}),
                "error: questions[0].id: missing required argument id (string)",
            ),
            (
                json!({"questions": [{"id": "  ", "question": "q"}]}),
                "error: questions[0].id: id must not be empty",
            ),
            (
                json!({"questions": [{"id": "a"}]}),
                "error: questions[0].question: missing required argument question (string)",
            ),
            (
                json!({"questions": [{"id": "a", "question": "q", "header": 7}]}),
                "error: questions[0].header: header must be a string",
            ),
            (
                json!({"questions": [{"id": "a", "question": "q", "multi_select": "yes"}]}),
                "error: questions[0].multi_select: multi_select must be a boolean",
            ),
            (
                json!({"questions": [{"id": "a", "question": "q", "options": {}}]}),
                "error: questions[0].options: options must be an array",
            ),
            (
                json!({"questions": [{"id": "a", "question": "q", "options": [{"description": "d"}]}]}),
                "error: questions[0].options[0].label: missing required argument label (string)",
            ),
            (
                json!({"questions": [{"id": "a", "question": "q", "options": [{"label": " "}]}]}),
                "error: questions[0].options[0].label: label must not be empty",
            ),
            (
                json!({"questions": [{"id": "a", "question": "q", "options": [{"label": "l", "description": 3}]}]}),
                "error: questions[0].options[0].description: description must be a string",
            ),
            (
                json!({"questions": [
                    {"id": "a", "question": "q"}, {"id": "b", "question": "q"},
                    {"id": "c", "question": "q"}, {"id": "d", "question": "q"},
                    {"id": "e", "question": "q"}
                ]}),
                "error: at most 4 questions per call (got 5)",
            ),
            (
                json!({"questions": [{"id": "a", "question": "q", "options": [
                    {"label": "1"}, {"label": "2"}, {"label": "3"}, {"label": "4"}, {"label": "5"}
                ]}]}),
                "error: questions[0].options: at most 4 options per question (got 5)",
            ),
        ];
        for (bad, expected) in cases {
            let got = parse(&args(bad.clone())).unwrap_err();
            assert!(
                got.starts_with(expected),
                "{bad} should be {expected:?}, was {got:?}"
            );
        }
        // Arguments that are not JSON at all are the one failure that says
        // nothing about the questions: the tool never got to read any.
        assert!(
            parse("not json")
                .unwrap_err()
                .starts_with("error: arguments are not valid JSON")
        );
    }

    #[test]
    fn the_answer_names_every_question_by_its_id() {
        let questions = one(json!({"questions": [
            {"id": "auth", "question": "Which auth?", "multi_select": true},
            {"id": "store", "question": "Where?"},
            {"id": "note", "question": "Anything else?"}
        ]}))
        .unwrap();
        let answers = vec![
            Answer {
                id: "auth".into(),
                labels: vec!["JWT".into(), "Session cookie".into()],
            },
            Answer {
                id: "store".into(),
                labels: vec!["Postgres".into()],
            },
            // `note` was left unanswered, and saying so is what keeps the model
            // from reading a missing line as a question never asked.
            Answer {
                id: "note".into(),
                labels: Vec::new(),
            },
        ];
        assert_eq!(
            answer_text(&questions, &answers),
            "auth: JWT, Session cookie\nstore: Postgres\nnote: (no answer)"
        );
    }

    #[test]
    fn an_answer_with_no_question_is_not_lost() {
        // The front end answers in the order it was asked, but the result is
        // looked up by id: an answer to a question that is not in this call
        // cannot be placed, and every question still gets its line.
        let questions = one(json!({"questions": [{"id": "a", "question": "q"}]})).unwrap();
        let answers = vec![Answer {
            id: "b".into(),
            labels: vec!["x".into()],
        }];
        assert_eq!(answer_text(&questions, &answers), "a: (no answer)");
    }

    #[test]
    fn a_multi_line_answer_stays_on_one_line() {
        // Free text is typed by a person and can hold a newline; the result is
        // read a line per question.
        let questions = one(json!({"questions": [{"id": "a", "question": "q"}]})).unwrap();
        let answers = vec![Answer {
            id: "a".into(),
            labels: vec!["first\nsecond".into()],
        }];
        assert_eq!(answer_text(&questions, &answers), "a: first second");
    }
}

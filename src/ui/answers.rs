//! How the plain front end answers the machine's questions.
//!
//! It has no event loop of its own: the terminal it sits in is not in raw mode, so
//! Ctrl-C arrives as SIGINT and the approval answer arrives on stdin. Both are
//! built here rather than by the interpreter, because which answer source exists
//! is a property of the front end that is running — the one that owns the screen
//! answers all of them from its own loop instead.

use std::future::Future;
use std::io::Write;
use std::pin::Pin;

use anyhow::Result;

use crate::tools::ask::{Answer, Question};
use crate::types::ToolCall;

use super::contract::{Approve, Ask, Cancel, Verdict};

/// The real SIGINT listener. It subscribes to tokio's watch at construction time
/// (no poll needed), so a signal arriving at any moment from the start of the
/// turn to its end cannot be lost to a "no listener" gap.
pub struct Sigint(
    #[cfg(unix)] tokio::signal::unix::Signal,
    #[cfg(not(unix))] tokio::signal::windows::CtrlC,
);

impl Sigint {
    /// Subscribe to SIGINT. The caller constructs one per turn, before the first
    /// await point, so no signal can arrive before the subscription exists.
    pub fn new() -> Result<Self> {
        #[cfg(unix)]
        let listener = Self(tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::interrupt(),
        )?);
        #[cfg(not(unix))]
        let listener = Self(tokio::signal::windows::ctrl_c()?);
        Ok(listener)
    }
}

impl Cancel for Sigint {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async {
            self.0.recv().await;
        })
    }
}

/// One line of stdin per question, denial by default: the plain front end's
/// answer source.
pub struct StdinApproval;

impl Approve for StdinApproval {
    fn approve(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = Verdict> + '_>> {
        Box::pin(async {
            // The blocking read is wrapped in spawn_blocking: the global stdin
            // buffer is shared across calls, so surplus type-ahead is not lost.
            // (Cost: on cancellation a blocked thread lingers and swallows the
            // first line typed afterwards -- a known trade-off.)
            let yes = tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                let bytes_read = std::io::stdin().read_line(&mut line);
                let line = line.trim();
                bytes_read.map(|count| count > 0).unwrap_or(false)
                    && (line.eq_ignore_ascii_case("y") || line.starts_with('y'))
            })
            .await
            .unwrap_or(false);
            if yes {
                Verdict::Allowed
            } else {
                Verdict::Denied
            }
        })
    }
}

/// The question tool, answered from stdin: one line per question, read under the
/// question the transcript is already showing.
///
/// A line is a selection when it reads as one -- a number, or an option's own
/// label, or several of either separated by commas for a question that allows
/// more than one -- and the user's own words otherwise, which is the only way a
/// question with no options can be answered at all. An empty line leaves that
/// question blank; an end of input leaves the rest of them blank too, which is
/// the answer a question gets when there is nobody at the keyboard.
pub struct StdinQuestions;

impl Ask for StdinQuestions {
    fn ask(
        &mut self,
        questions: &[Question],
    ) -> Pin<Box<dyn Future<Output = Option<Vec<Answer>>> + '_>> {
        // The questions are copied rather than borrowed: the answer is read after
        // the turn has gone on to wait for it, and the future's lifetime is the
        // one the channel is borrowed for, not the caller's slice.
        let questions: Vec<Question> = questions.to_vec();
        Box::pin(async move {
            let mut answers = Vec::with_capacity(questions.len());
            for question in questions {
                // The prompt is written here rather than as a cell because it is
                // not part of the transcript: it is the line the answer is typed
                // on, and the terminal overwrites it with what is typed.
                let mut out = std::io::stdout();
                let _ = write!(out, "{} › ", question.id);
                let _ = out.flush();
                let line = read_line().await?;
                answers.push(answer_for(&question, &line));
            }
            Some(answers)
        })
    }
}

/// Read one line as the answer to one question.
pub fn answer_for(question: &Question, line: &str) -> Answer {
    let line = line.trim();
    let labels = match selection(question, line) {
        Some(labels) => labels,
        None if line.is_empty() => Vec::new(),
        None => vec![line.to_owned()],
    };
    Answer {
        id: question.id.clone(),
        labels,
    }
}

/// The options a typed line names, or `None` when it names none of them.
///
/// Every comma-separated piece has to name one: half a selection and half a
/// sentence is a sentence, and reading it as a choice would answer a question the
/// user did not answer. What comes back is in the order the options were
/// *offered*, not the order they were typed in -- the answer is the question's,
/// and a line typed in a hurry should not hand the model a different answer than
/// the same choices clicked one by one. A question that takes one option keeps the
/// first of those, since the keys that would pick two cannot happen at a menu.
fn selection(question: &Question, line: &str) -> Option<Vec<String>> {
    if question.options.is_empty() {
        return None;
    }
    let mut picked: Vec<usize> = Vec::new();
    for piece in line.split(',') {
        let piece = piece.trim();
        let option = question
            .options
            .iter()
            .position(|o| o.label.eq_ignore_ascii_case(piece))
            .or_else(|| {
                piece
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=question.options.len()).contains(n))
                    .map(|n| n - 1)
            })?;
        // A choice repeated is a choice made once.
        if !picked.contains(&option) {
            picked.push(option);
        }
    }
    if picked.is_empty() {
        return None;
    }
    picked.sort_unstable();
    if !question.multi_select {
        picked.truncate(1);
    }
    Some(
        picked
            .into_iter()
            .map(|at| question.options[at].label.clone())
            .collect(),
    )
}

/// One line from stdin, or `None` at the end of input.
///
/// The blocking read is wrapped in `spawn_blocking`, the way the gate's answer
/// is: the turn is async and stdin is not.
async fn read_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    })
    .await
    .ok()
    .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ask::Choice;

    fn question(options: &[&str], multi_select: bool) -> Question {
        Question {
            id: "auth".into(),
            header: String::new(),
            question: "Which auth?".into(),
            options: options
                .iter()
                .map(|label| Choice {
                    label: (*label).to_owned(),
                    description: String::new(),
                })
                .collect(),
            multi_select,
        }
    }

    fn labels(question: &Question, line: &str) -> Vec<String> {
        answer_for(question, line).labels
    }

    #[test]
    fn a_number_picks_the_option_it_counts_to() {
        let question = question(&["JWT", "Session cookie"], false);
        assert_eq!(labels(&question, "1"), ["JWT"]);
        assert_eq!(labels(&question, " 2 "), ["Session cookie"]);
        // A number no option answers to is not a choice: it is what was typed.
        assert_eq!(labels(&question, "3"), ["3"]);
        assert_eq!(labels(&question, "0"), ["0"]);
    }

    #[test]
    fn an_option_can_be_named_as_well_as_counted() {
        let question = question(&["JWT", "Session cookie"], false);
        assert_eq!(labels(&question, "jwt"), ["JWT"], "as it is spelled");
        assert_eq!(labels(&question, "SESSION COOKIE"), ["Session cookie"]);
    }

    #[test]
    fn several_options_are_read_for_a_question_that_takes_several() {
        let question = question(&["Postgres", "SQLite", "Files"], true);
        assert_eq!(labels(&question, "1,3"), ["Postgres", "Files"]);
        assert_eq!(
            labels(&question, "sqlite, postgres"),
            ["Postgres", "SQLite"]
        );
        // The order is the one the options were offered in, not the one they
        // were typed in; a repeated choice is a choice made once.
        assert_eq!(labels(&question, "2,2"), ["SQLite"]);
    }

    #[test]
    fn a_question_that_takes_one_keeps_the_first_option_it_offers() {
        let question = question(&["JWT", "Session cookie"], false);
        assert_eq!(labels(&question, "1,2"), ["JWT"]);
    }

    #[test]
    fn half_a_choice_and_half_a_sentence_is_a_sentence() {
        // Reading it as a choice would answer a question the user did not answer.
        let question = question(&["JWT", "Session cookie"], false);
        assert_eq!(
            labels(&question, "1, the one we agreed on"),
            ["1, the one we agreed on"]
        );
        assert_eq!(labels(&question, "JWT please"), ["JWT please"]);
    }

    #[test]
    fn an_empty_line_leaves_the_question_unanswered() {
        let question = question(&["JWT"], false);
        assert!(labels(&question, "   ").is_empty());
    }

    #[test]
    fn a_question_with_nothing_to_pick_from_is_answered_in_words() {
        let question = question(&[], false);
        assert_eq!(
            labels(&question, "whatever you think"),
            ["whatever you think"]
        );
        // Even when the words are a number, since there is no option it could be.
        assert_eq!(labels(&question, "2"), ["2"]);
    }
}

//! Tab completion for the plain REPL: the commands the user can name.
//!
//! Rustyline's `Completer` trait returns `(start, candidates)` where `start`
//! is the byte position of the partial word being completed and the line is
//! replaced from there. A caocli command is a single token at the start of
//! the line (no command has a space in its name), so the start position is
//! always 0 and the partial word is the line itself.
//!
//! The other three traits rustyline's `Helper` requires -- `Hinter`,
//! `Highlighter`, `Validator` -- are kept trivial: the prompt is plain text,
//! history is per-line, and every command name is well-formed.

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::Hinter;
use rustyline::validate::{ValidationContext, ValidationResult, Validator};
use rustyline::{Context, Helper, Result};

use crate::repl;

/// Tab-completes the slash commands. The candidate list is the same
/// `repl::completions` the TUI's picker runs against, so both front ends
/// answer `/res` and `/h` the same way.
pub struct CommandCompleter;

impl Completer for CommandCompleter {
    type Candidate = Pair;

    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Result<(usize, Vec<Pair>)> {
        // Only the part before the cursor matters: what the user has typed so
        // far is the prefix, and `repl::completions` only matches against the
        // whole line (a command and an argument are two different shapes).
        let prefix = &line[..pos];
        let matches = repl::completions(prefix);
        // No command and a halfway-typed argument: nothing to complete, and
        // an empty list is the signal rustyline uses to keep the line as is.
        if matches.is_empty() {
            return Ok((0, Vec::new()));
        }
        let candidates = matches
            .into_iter()
            .map(|c| Pair {
                display: c.name.to_owned(),
                replacement: c.name.to_owned(),
            })
            .collect();
        // A command has no characters before itself; replacing from byte 0
        // means "the whole partial word is the one that began the line".
        Ok((0, candidates))
    }
}

impl Hinter for CommandCompleter {
    type Hint = String;

    fn hint(&self, _line: &str, _pos: usize, _ctx: &Context<'_>) -> Option<Self::Hint> {
        None
    }
}

impl Highlighter for CommandCompleter {
    fn highlight_prompt<'b, 's: 'b, 'p: 'b>(
        &'s self,
        prompt: &'p str,
        _default: bool,
    ) -> std::borrow::Cow<'b, str> {
        // No SGR here: the prompt is what the terminal shows as-is, and
        // colouring it is the renderer's job, not the line editor's.
        std::borrow::Cow::Borrowed(prompt)
    }

    fn highlight_hint<'h>(&self, hint: &'h str) -> std::borrow::Cow<'h, str> {
        std::borrow::Cow::Borrowed(hint)
    }

    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> std::borrow::Cow<'l, str> {
        std::borrow::Cow::Borrowed(line)
    }

    fn highlight_candidate<'c>(
        &self,
        candidate: &'c str,
        _completion: rustyline::CompletionType,
    ) -> std::borrow::Cow<'c, str> {
        std::borrow::Cow::Borrowed(candidate)
    }
}

impl Validator for CommandCompleter {
    fn validate(&self, _ctx: &mut ValidationContext<'_>) -> Result<ValidationResult> {
        // Every command line is valid as text; a name that does not match
        // any command is a turn (not an error), and a name that does match
        // is handled by the REPL.
        Ok(ValidationResult::Valid(None))
    }
}

impl Helper for CommandCompleter {}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a context for `complete` -- a History is the only thing the
    /// trait asks for, and the completer does not look at it. The history
    /// is leaked for the lifetime of the test, which is one function: the
    /// test process exits before the leak is observable.
    fn ctx() -> Context<'static> {
        let history = Box::leak(Box::new(rustyline::history::DefaultHistory::new()));
        Context::new(history)
    }

    #[test]
    fn a_bare_slash_offers_every_command() {
        let c = CommandCompleter;
        let (start, candidates) = c.complete("/", 1, &ctx()).unwrap();
        assert_eq!(start, 0, "a command starts at byte 0");
        let names: Vec<&str> = candidates.iter().map(|p| p.replacement.as_str()).collect();
        // Every command in the table is offered; the order is table order.
        let expected: Vec<&str> = crate::repl::COMMANDS.iter().map(|c| c.name).collect();
        assert_eq!(names, expected);
    }

    #[test]
    fn a_prefix_narrows_to_matching_names() {
        let c = CommandCompleter;
        let (_, candidates) = c.complete("/res", 4, &ctx()).unwrap();
        let names: Vec<&str> = candidates.iter().map(|p| p.replacement.as_str()).collect();
        assert_eq!(names, vec!["/resume"]);
    }

    #[test]
    fn a_non_command_line_completes_to_nothing() {
        // A name without the slash is not a command -- a user typing "ls"
        // is composing a turn, not a command, and nothing should pop up.
        let c = CommandCompleter;
        let (start, candidates) = c.complete("ls -la", 6, &ctx()).unwrap();
        assert_eq!(start, 0);
        assert!(candidates.is_empty());
    }

    #[test]
    fn a_command_with_a_trailing_argument_does_not_complete() {
        // "/resume 20260910" is the command plus its argument: completing
        // again would fight what the user is typing, so the list is empty.
        let c = CommandCompleter;
        let (_, candidates) = c.complete("/resume 20260910", 14, &ctx()).unwrap();
        assert!(candidates.is_empty());
    }

    #[test]
    fn an_empty_line_offers_nothing() {
        let c = CommandCompleter;
        let (_, candidates) = c.complete("", 0, &ctx()).unwrap();
        assert!(candidates.is_empty());
    }
}

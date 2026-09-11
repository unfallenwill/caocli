pub mod ask;
mod fs;
mod shell;
pub mod todo;

use crate::types::ToolDef;

/// Name of the read-only tool referenced by the approval gate policy (never
/// needs to ask the user).
pub const READ_NAME: &str = fs::READ_NAME;

/// Name of the one tool whose result is a person's answer rather than the
/// machine's: the interpreter recognizes it before dispatching anything.
pub const ASK_NAME: &str = ask::ASK_NAME;

/// Name of the tool that writes the plan down where the user can see it. Like
/// reading it changes nothing on disk, so it never passes the approval gate.
pub const TODO_NAME: &str = todo::TODO_NAME;

/// Cap on tool output sent back to the model (bytes).
pub const MAX_OUTPUT: usize = 10 * 1024;
/// Per-file read/write cap (bytes), so a huge file cannot be pulled into memory
/// or into the context window.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;
/// Cap on the file a paged read will stream (bytes). A read holds one page and
/// not the file, so this is not about memory: it is about a read numbering every
/// line of the file it is asked for, which is a scan the answer has to wait for.
pub const MAX_READ_BYTES: u64 = 256 * 1024 * 1024;

/// All tool definitions. The order is fixed: changing it changes the request
/// prefix and causes a full KVCache miss.
pub fn definitions() -> Vec<ToolDef> {
    vec![
        shell::definition(),
        fs::read_definition(),
        fs::edit_definition(),
        fs::write_definition(),
        ask::definition(),
        todo::definition(),
    ]
}

/// Whether a call changes something on disk -- which is the whole of what the
/// approval gate asks about.
///
/// A blacklist and not a list of the calls that write: a tool added later is one
/// nobody has decided about yet, and the safe reading of "nobody has decided" is
/// to ask rather than to run. The two that are settled: reading changes nothing,
/// and a todo list is a note to the user rather than a change to anything, so
/// putting a y/N in front of it would only teach the reader to answer without
/// looking.
pub fn changes_files(name: &str) -> bool {
    !matches!(name, READ_NAME | TODO_NAME)
}

/// Dispatch by name. Never returns Err: every failure (unknown tool, bad
/// arguments, IO error) is passed back to the model as tool result text and the
/// model decides what to do next.
///
/// The question tool is answered rather than dispatched: its result is what a
/// person chose, and only a front end has one. The interpreter asks there before
/// anything reaches this function; the arm below is the answer a call that
/// somehow arrives here anyway gets -- text the model can correct itself from,
/// never a panic and never a question nobody can answer.
pub async fn execute(name: &str, args_json: &str) -> String {
    match name {
        shell::NAME => shell::execute(args_json).await,
        fs::READ_NAME => fs::read(args_json),
        fs::EDIT_NAME => fs::edit(args_json),
        fs::WRITE_NAME => fs::write(args_json),
        todo::TODO_NAME => todo::execute(args_json),
        ask::ASK_NAME => {
            format!("error: {ASK_NAME} is answered by the front end and cannot be executed here")
        }
        other => format!(
            "error: unknown tool {other:?}. Available tools: Bash, {}, {}, {}, {}, {}",
            fs::READ_NAME,
            fs::EDIT_NAME,
            fs::WRITE_NAME,
            ASK_NAME,
            TODO_NAME
        ),
    }
}

/// Parse tool arguments JSON. Error text is returned as the tool result.
fn parse_args(args_json: &str) -> Result<serde_json::Value, String> {
    serde_json::from_str(args_json).map_err(|e| format!("error: arguments are not valid JSON: {e}"))
}

/// Fetch a required string argument.
fn str_arg(v: &serde_json::Value, key: &str) -> Result<String, String> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("error: missing required argument {key} (string)"))
}

/// A required string argument that is there and not blank.
///
/// Shared by the two tools whose arguments are a structure the user reads rather
/// than a line the machine runs: both report a bad field by the path it sits at,
/// and a field checked two ways is a field that can be accepted in one tool and
/// refused in the other.
pub(super) fn required_string(v: &serde_json::Value, key: &str) -> Result<String, String> {
    match v.get(key) {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => Ok(s.clone()),
        Some(serde_json::Value::String(_)) => Err(format!("{key} must not be empty")),
        Some(_) => Err(format!("{key} must be a string")),
        None => Err(format!("missing required argument {key} (string)")),
    }
}

/// Prefix a failure with the path it was found at.
pub(super) fn checked<T>(value: Result<T, String>, path: &str) -> Result<T, String> {
    value.map_err(|e| format!("error: {path}: {e}"))
}

/// Truncate at a byte limit, backing off to a UTF-8 character boundary so a
/// multi-byte character is never cut in half.
fn truncate(s: &str, max: usize) -> (String, bool) {
    if s.len() <= max {
        return (s.to_owned(), false);
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    (s[..end].to_owned(), true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_respects_char_boundary() {
        let s = "€".repeat(4096); // each € is 3 bytes in UTF-8, 12288 bytes total
        let (out, cut) = truncate(&s, 10);
        assert!(cut);
        // byte 10 is not a char boundary, so back off to 9 (= 3 complete chars)
        assert_eq!(out, "€€€");
        let (out2, cut2) = truncate("short", 10);
        assert!(!cut2);
        assert_eq!(out2, "short");
    }

    #[test]
    fn definitions_are_stable_and_named() {
        let names: Vec<String> = definitions().into_iter().map(|d| d.function.name).collect();
        assert_eq!(
            names,
            vec![
                "Bash",
                "Read",
                "Edit",
                "Write",
                "AskUserQuestion",
                "TodoWrite"
            ]
        );
    }

    /// The gate's policy: the calls that change something on disk are asked
    /// about, and the two that cannot go wrong are not.
    #[test]
    fn the_gate_asks_about_the_calls_that_change_disk() {
        for name in ["Bash", "Edit", "Write"] {
            assert!(
                changes_files(name),
                "{name} changes disk and is asked about"
            );
        }
        for name in [READ_NAME, TODO_NAME] {
            assert!(!changes_files(name), "{name} changes nothing to ask about");
        }
        // A tool nobody has decided about yet is asked about rather than run:
        // the blacklist is the fail-safe direction.
        assert!(changes_files("SomeToolAddedLater"));
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_text() {
        let out = execute("Delete", "{}").await;
        assert!(out.contains("unknown tool"));
        assert!(out.contains("Bash"));
    }

    /// The question tool never reaches the dispatch: asking is the interpreter's
    /// and the front end's. A call that arrives here anyway is answered with
    /// text, so that the tool result window closes and the model can carry on.
    #[tokio::test]
    async fn the_question_tool_is_not_executed_here() {
        let out = execute(
            "AskUserQuestion",
            r#"{"questions":[{"id":"a","question":"q"}]}"#,
        )
        .await;
        assert!(out.starts_with("error: "), "was {out:?}");
        assert!(out.contains("answered by the front end"));
    }

    #[tokio::test]
    async fn dispatch_reaches_shell() {
        let out = execute("Bash", r#"{"command":"echo dispatched"}"#).await;
        assert!(out.contains("dispatched"));
    }

    /// The todo tool is dispatched like the tools that touch the world, even
    /// though it touches none: it answers with text of its own, and the front
    /// ends read the list back out of the call it answered.
    #[tokio::test]
    async fn dispatch_reaches_the_todo_tool() {
        let out = execute(
            TODO_NAME,
            r#"{"todos":[{"content":"Run the gates","status":"in_progress"}]}"#,
        )
        .await;
        assert_eq!(out, "todo list updated (0/1 done)");
    }
}

pub mod ask;
mod fs;
mod glob;
mod shell;
pub mod todo;

use crate::types::ToolDef;

/// Name of the read-only tool referenced by the approval gate policy (never
/// needs to ask the user).
pub const READ_NAME: &str = fs::READ_NAME;

/// The same for the tool that lists the files a pattern matches: it is the read
/// side of the workspace without opening anything, so it never needs to ask
/// either.
pub const GLOB_NAME: &str = glob::NAME;

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

/// Where a running command's own output goes while the command is still running.
///
/// The result of a call is what the model reads and what the log keeps; this is the
/// same bytes as the person watching reads them, as they arrive. A tool has no way
/// to reach a screen of its own, so the interpreter is what pairs a tool with the
/// front end that is watching it, and the tool is what decides how much of it is
/// worth streaming: a build can print megabytes, and a view of it is not a record
/// of it.
pub trait Live {
    fn chunk(&mut self, text: &str);
}

/// Nobody watching: a sink that drops what it is given, for the tests whose
/// subject is not what a running command prints.
///
/// A call with no front end behind it is a call only a test makes -- the
/// interpreter always has one, and what a tool is handed is the front end that was
/// watching -- so this is a fixture rather than a front end.
#[cfg(test)]
pub struct Silent;

#[cfg(test)]
impl Live for Silent {
    fn chunk(&mut self, _text: &str) {}
}

/// All tool definitions. The order is fixed: changing it changes the request
/// prefix and causes a full KVCache miss -- which is why a tool added later
/// goes at the end of the list, the one place where everything a session
/// already sent stays byte-for-byte what it was.
pub fn definitions() -> Vec<ToolDef> {
    vec![
        shell::definition(),
        fs::read_definition(),
        fs::edit_definition(),
        fs::write_definition(),
        ask::definition(),
        todo::definition(),
        glob::definition(),
    ]
}

/// Whether a call changes something on disk -- which is the whole of what the
/// approval gate asks about.
///
/// A blacklist and not a list of the calls that write: a tool added later is one
/// nobody has decided about yet, and the safe reading of "nobody has decided" is
/// to ask rather than to run. The three that are settled: reading changes
/// nothing, looking for a file to read changes nothing either, and a todo list
/// is a note to the user rather than a change to anything -- so putting a y/N in
/// front of one of them would only teach the reader to answer without looking.
///
/// The tools of an MCP server are the ones nobody has decided about: what a
/// server does with a call is the server's own, and a client that took its word
/// for what is safe would be trusting whatever is on the other end of the pipe
/// -- which is the one thing the protocol says not to do.
pub fn changes_files(name: &str) -> bool {
    !matches!(name, READ_NAME | GLOB_NAME | TODO_NAME)
}

/// Whether a call names a file: the three tools that operate on one. These are
/// the calls whose target directory's own instructions can be discovered from
/// the call — a Bash command can `cat` anything, and what it named is not
/// recoverable from its arguments.
///
/// Glob is not one of them: it names a directory to look in, and the
/// instructions that apply to a file are discovered by the Read that file is
/// read with, which is the call that names the file itself.
pub fn carries_file_path(name: &str) -> bool {
    matches!(name, fs::READ_NAME | fs::EDIT_NAME | fs::WRITE_NAME)
}

/// The file a call names, for the calls that name one. `None` when the
/// arguments carry no readable `file_path` — a fault the tool itself answers
/// with text, not one to act on here.
pub fn file_path(args_json: &str) -> Option<std::path::PathBuf> {
    serde_json::from_str::<serde_json::Value>(args_json)
        .ok()?
        .get("file_path")?
        .as_str()
        .map(std::path::PathBuf::from)
}

/// Whether a tool's answer reports a failure, as this crate's tools write one:
/// the text opens with `error: ` — the same opening every marker uses. The
/// Anthropic wire has a field for this (`is_error`), and it is set from the text
/// rather than recorded beside it: the log holds one statement of what happened,
/// not two that can drift apart.
pub fn reports_failure(output: &str) -> bool {
    output.starts_with("error:")
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
#[cfg(test)]
pub async fn execute(name: &str, args_json: &str) -> String {
    execute_live(name, args_json, &mut Silent, &crate::mcp::Hub::empty()).await
}

/// The same, with the output of a command that is still running streamed to `live`
/// as it arrives: what the person watching reads while the model waits for the
/// result. Only the one tool runs anything, so the sink reaches it and no other.
///
/// `mcp` is where the tools of the servers this session connected to live. A
/// name that carries their prefix is theirs whether or not a server is behind
/// it, so that a log written when one was connected is answered with a sentence
/// rather than with "unknown tool".
pub async fn execute_live(
    name: &str,
    args_json: &str,
    live: &mut dyn Live,
    mcp: &crate::mcp::Hub,
) -> String {
    match name {
        shell::NAME => shell::execute(args_json, live).await,
        fs::READ_NAME => fs::read(args_json),
        fs::EDIT_NAME => fs::edit(args_json),
        fs::WRITE_NAME => fs::write(args_json),
        todo::TODO_NAME => todo::execute(args_json),
        glob::NAME => glob::glob(args_json),
        ask::ASK_NAME => {
            format!("error: {ASK_NAME} is answered by the front end and cannot be executed here")
        }
        other if crate::mcp::is_tool(other) => mcp.call(other, args_json).await,
        other => format!(
            "error: unknown tool {other:?}. Available tools: Bash, {}, {}, {}, {}, {}, {}",
            fs::READ_NAME,
            fs::EDIT_NAME,
            fs::WRITE_NAME,
            ASK_NAME,
            TODO_NAME,
            glob::NAME
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
///
/// Shared with the MCP tools, whose results are the other place a great deal of
/// text can arrive at once: one cap for both, so that what a result may carry is
/// the same answer whichever tool produced it.
pub(crate) fn truncate(s: &str, max: usize) -> (String, bool) {
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
                "TodoWrite",
                "Glob"
            ]
        );
    }

    /// The gate's policy: the calls that change something on disk are asked
    /// about, and the three that cannot go wrong are not.
    #[test]
    fn the_gate_asks_about_the_calls_that_change_disk() {
        for name in ["Bash", "Edit", "Write"] {
            assert!(
                changes_files(name),
                "{name} changes disk and is asked about"
            );
        }
        for name in [READ_NAME, GLOB_NAME, TODO_NAME] {
            assert!(!changes_files(name), "{name} changes nothing to ask about");
        }
        // A tool nobody has decided about yet is asked about rather than run:
        // the blacklist is the fail-safe direction. A server's tools are
        // exactly that — this client cannot know what one does — and the
        // specification says as much beside its own annotations: a hint from a
        // server is not a decision by the user.
        assert!(changes_files("SomeToolAddedLater"));
        assert!(changes_files("mcp__filesystem__read_file"));
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

    /// The glob tool is dispatched like the others: its answer is text, and
    /// which files a pattern matches is the tool's own business.
    #[tokio::test]
    async fn dispatch_reaches_the_glob_tool() {
        let manifest = env!("CARGO_MANIFEST_DIR");
        let out = execute(
            GLOB_NAME,
            &format!(r#"{{"pattern":"Cargo.toml","path":{manifest:?}}}"#),
        )
        .await;
        assert!(out.ends_with("Cargo.toml"), "{out}");
    }

    /// A name with the MCP prefix is a server's, whether or not one is behind
    /// it: what comes back is the hub's answer, which is text either way.
    #[tokio::test]
    async fn dispatch_reaches_an_mcp_server() {
        let stub = crate::mcp::stub::Stub::new();
        let hub = crate::mcp::Hub::of_entries(vec![stub.entry(&[("STUB_TOOLS", "echo")])]).await;
        let out = execute_live("mcp__stub__echo", r#"{"text":"hi"}"#, &mut Silent, &hub).await;
        assert_eq!(out, "called with {text:hi}");
        // A name the server does not offer, and one no server could: both are
        // answered rather than dispatched to nothing.
        let refused = execute_live("mcp__stub__nowhere", "{}", &mut Silent, &hub).await;
        assert!(refused.starts_with("error: "), "{refused}");
        assert!(refused.contains("no MCP tool named"), "{refused}");
        let empty = execute_live(
            "mcp__nobody__nothing",
            "{}",
            &mut Silent,
            &crate::mcp::Hub::empty(),
        )
        .await;
        assert!(empty.contains("no MCP tool named"), "{empty}");
        hub.shutdown().await;
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

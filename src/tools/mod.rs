mod fs;
mod shell;

use crate::types::ToolDef;

/// Name of the read-only tool referenced by the approval gate policy (never
/// needs to ask the user).
pub const READ_NAME: &str = fs::READ_NAME;

/// Cap on tool output sent back to the model (bytes).
pub const MAX_OUTPUT: usize = 10 * 1024;
/// Per-file read/write cap (bytes), so a huge file cannot be pulled into memory
/// or into the context window.
pub const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// All tool definitions. The order is fixed: changing it changes the request
/// prefix and causes a full KVCache miss.
pub fn definitions() -> Vec<ToolDef> {
    vec![
        shell::definition(),
        fs::read_definition(),
        fs::edit_definition(),
        fs::write_definition(),
    ]
}

/// Dispatch by name. Never returns Err: every failure (unknown tool, bad
/// arguments, IO error) is passed back to the model as tool result text and the
/// model decides what to do next.
pub async fn execute(name: &str, args_json: &str) -> String {
    match name {
        shell::NAME => shell::execute(args_json).await,
        fs::READ_NAME => fs::read(args_json),
        fs::EDIT_NAME => fs::edit(args_json),
        fs::WRITE_NAME => fs::write(args_json),
        other => format!(
            "error: unknown tool {other:?}. Available tools: Bash, {}, {}, {}",
            fs::READ_NAME,
            fs::EDIT_NAME,
            fs::WRITE_NAME
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
        assert_eq!(names, vec!["Bash", "Read", "Edit", "Write"]);
    }

    #[tokio::test]
    async fn unknown_tool_returns_error_text() {
        let out = execute("Delete", "{}").await;
        assert!(out.contains("unknown tool"));
        assert!(out.contains("Bash"));
    }

    #[tokio::test]
    async fn dispatch_reaches_shell() {
        let out = execute("Bash", r#"{"command":"echo dispatched"}"#).await;
        assert!(out.contains("dispatched"));
    }
}

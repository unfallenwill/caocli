//! What a call's arguments say in one line: the interesting argument when the
//! call carries one, the raw arguments when it does not.

/// The argument summary shown for a tool call: the interesting argument when the
/// call carries one, otherwise the raw arguments.
///
/// A glob call is the one whose interesting arguments are not `command` or
/// `file_path`: the pattern is the question and the directory is where it is
/// asked, and both are shown, because a pattern asked of the wrong tree is the
/// one thing about the call a reader can catch before it runs.
///
/// Nothing here is measured against a width. A summary that does not fit is
/// wrapped by whoever draws it -- the screen writes it into a region and the
/// plain front end lets the terminal fold it -- so a call whose arguments are
/// long shows them rather than a cut-off prefix of them.
pub(crate) fn hint(args: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return args.to_owned();
    };
    if let Some(named) = v
        .get("command")
        .or_else(|| v.get("file_path"))
        .and_then(|c| c.as_str())
    {
        return named.to_owned();
    }
    if let Some(pattern) = v.get("pattern").and_then(|p| p.as_str()) {
        return match v.get("path").and_then(|p| p.as_str()) {
            Some(dir) => format!("{pattern} in {dir}"),
            None => pattern.to_owned(),
        };
    }
    args.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hint_prefers_command_then_file_path() {
        assert_eq!(hint(r#"{"command":"ls -la"}"#), "ls -la");
        assert_eq!(hint(r#"{"file_path":"/a/b.txt"}"#), "/a/b.txt");
        // command wins when both are present
        assert_eq!(hint(r#"{"command":"ls","file_path":"/a"}"#), "ls");
    }

    /// A glob's line is what it asks for, and of where: the pattern alone when
    /// the call takes the working directory, both when it names one.
    #[test]
    fn hint_reads_a_glob_as_the_question_it_is() {
        assert_eq!(hint(r#"{"pattern":"**/*.rs"}"#), "**/*.rs");
        assert_eq!(
            hint(r#"{"pattern":"**/*.rs","path":"/a/src"}"#),
            "**/*.rs in /a/src"
        );
    }

    #[test]
    fn hint_falls_back_to_the_raw_arguments_whole() {
        assert_eq!(hint("not json at all"), "not json at all");
        assert_eq!(hint(r#"{"other":"x"}"#), r#"{"other":"x"}"#);
        // Nothing is clipped: a long raw argument is the summary, all of it.
        // It is the drawing layer that deals with a line too long for the
        // region, and it wraps rather than cuts.
        let long = "x".repeat(400);
        assert_eq!(hint(&long), long);
    }
}

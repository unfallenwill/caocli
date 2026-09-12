//! What a call's arguments say in one line: the interesting argument when the
//! call carries one, the raw arguments clipped when it does not.

use crate::ui::text;

/// The argument summary shown for a tool call: the interesting argument when the
/// call carries one, otherwise the raw arguments clipped to one line's worth of
/// columns.
///
/// A glob call is the one whose interesting arguments are not `command` or
/// `file_path`: the pattern is the question and the directory is where it is
/// asked, and both are shown, because a pattern asked of the wrong tree is the
/// one thing about the call a reader can catch before it runs.
pub(crate) fn hint(args: &str) -> String {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return text::truncate(args, HINT_COLUMNS).to_owned();
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
    text::truncate(args, HINT_COLUMNS).to_owned()
}

/// Columns of raw arguments kept when they cannot be summarized by name.
const HINT_COLUMNS: usize = 80;

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
    fn hint_falls_back_to_clipped_raw_arguments() {
        assert_eq!(hint("not json at all"), "not json at all");
        assert_eq!(hint(r#"{"other":"x"}"#), r#"{"other":"x"}"#);
        // a long raw argument is clipped to one line's worth of columns
        let long = "x".repeat(HINT_COLUMNS + 40);
        assert_eq!(hint(&long).chars().count(), HINT_COLUMNS);
    }

    #[test]
    fn hint_clips_wide_characters_by_column() {
        // 60 ideographs are 120 columns but only 60 chars; the clip keeps 40
        let wide = "\u{6df1}".repeat(60);
        let got = hint(&wide);
        assert_eq!(crate::ui::text::width(&got), HINT_COLUMNS);
        assert_eq!(got.chars().count(), HINT_COLUMNS / 2);
    }
}

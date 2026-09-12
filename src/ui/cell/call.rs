//! What a tool call shows: the argument summary and the change it makes.
//!
//! These are the only pieces of a tool call that are derived from the wire
//! format the model sent -- the name and the call itself are already in the
//! arguments. Everything in here is *projection*, not interpretation: a call
//! whose arguments cannot be parsed is still a call, just one with nothing to
//! show.
//!
//! Kept out of [`Cell`] on purpose: a [`Cell::ToolCall`] is a value, and these
//! helpers are how the cell is built from the raw arguments. The argument
//! vocabulary belongs to the tools, not to the cell model, and moving it
//! here means the cell never has to know what shape a Bash call's JSON is.

use crate::ui::cell::{DiffKind, DiffLine};
use crate::ui::text;

/// The argument summary shown for a tool call: the interesting argument when the
/// call carries one, otherwise the raw arguments clipped to one line's worth of
/// columns.
///
/// A glob call is the one whose interesting arguments are not `command` or
/// `file_path`: the pattern is the question and the directory is where it is
/// asked, and both are shown, because a pattern asked of the wrong tree is the
/// one thing about the call a reader can catch before it runs.
pub(super) fn hint(args: &str) -> String {
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

/// How many lines of a change a tool call shows before it says how many are left.
///
/// A call is announced before it runs, and an edit can be hundreds of lines: what
/// the announcement is for is seeing what is about to happen, not reading the
/// whole file. The rest is one line, so a long change costs a screenful at most.
const DIFF_LINES: usize = 12;

/// The change a call makes, for the calls that make one.
///
/// The arguments are the wire format of a tool call, which is also what replay
/// reads out of the session log, so live and replayed turns show the same lines
/// without a second source for either one.
pub(super) fn diff_lines(name: &str, args: &str) -> Vec<DiffLine> {
    let Ok(args) = serde_json::from_str::<serde_json::Value>(args) else {
        return Vec::new();
    };
    let side = |key: &str, kind: DiffKind| -> Vec<DiffLine> {
        args.get(key)
            .and_then(|s| s.as_str())
            .map(|text| {
                text.lines()
                    .map(|line| DiffLine {
                        kind,
                        text: line.to_owned(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let mut lines = match name {
        // The order a diff is read in: what goes, then what arrives.
        "Edit" => {
            let mut lines = side("old_string", DiffKind::Removed);
            lines.extend(side("new_string", DiffKind::Added));
            lines
        }
        "Write" => side("content", DiffKind::Added),
        _ => return Vec::new(),
    };
    if lines.len() > DIFF_LINES {
        let omitted = lines.len() - DIFF_LINES;
        lines.truncate(DIFF_LINES);
        lines.push(DiffLine {
            kind: DiffKind::Omitted(omitted),
            text: String::new(),
        });
    }
    lines
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

    /// The arguments an edit arrives with, as the wire format spells them.
    fn edit(old: &str, new: &str) -> String {
        serde_json::json!({
            "file_path": "src/main.rs",
            "old_string": old,
            "new_string": new,
        })
        .to_string()
    }

    #[test]
    fn an_edit_shows_what_leaves_and_what_arrives() {
        let lines = diff_lines("Edit", &edit("let a = 1;\nlet b = 2;", "let a = 3;"));
        assert_eq!(
            lines,
            vec![
                DiffLine {
                    kind: DiffKind::Removed,
                    text: "let a = 1;".into(),
                },
                DiffLine {
                    kind: DiffKind::Removed,
                    text: "let b = 2;".into(),
                },
                DiffLine {
                    kind: DiffKind::Added,
                    text: "let a = 3;".into(),
                },
            ],
            "what goes is shown before what arrives"
        );
    }

    #[test]
    fn a_deletion_shows_only_what_goes() {
        let lines = diff_lines("Edit", &edit("gone", ""));
        assert_eq!(
            lines,
            vec![DiffLine {
                kind: DiffKind::Removed,
                text: "gone".into(),
            }]
        );
    }

    #[test]
    fn a_write_shows_what_it_puts_there() {
        let args = serde_json::json!({ "file_path": "a.txt", "content": "one\ntwo\n" }).to_string();
        let lines = diff_lines("Write", &args);
        assert_eq!(lines.len(), 2, "a trailing newline does not start a line");
        assert!(lines.iter().all(|l| l.kind == DiffKind::Added));
    }

    #[test]
    fn a_long_change_ends_in_a_count_of_the_rest() {
        let many = (0..DIFF_LINES + 5)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = diff_lines("Write", &serde_json::json!({ "content": many }).to_string());
        assert_eq!(lines.len(), DIFF_LINES + 1, "the lines and the count");
        assert_eq!(lines.last().unwrap().kind, DiffKind::Omitted(5));
    }

    #[test]
    fn only_a_change_has_lines_to_show() {
        assert!(diff_lines("Bash", r#"{"command":"ls"}"#).is_empty());
        assert!(diff_lines("Read", r#"{"file_path":"x"}"#).is_empty());
        assert!(diff_lines("Edit", "not json at all").is_empty());
        assert!(
            diff_lines("Edit", r#"{"file_path":"x"}"#).is_empty(),
            "the strings are what makes it a change"
        );
    }
}

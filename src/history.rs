//! The input history, in the format the line editor writes.
//!
//! Both front ends share one file, so a line typed at the plain prompt is still
//! there in the interactive one and the other way round. The format is the line
//! editor's, because it is the one that already writes this file: a `#V2` header,
//! then one entry per line with backslashes and newlines escaped, which is what
//! lets a multi-line entry survive the round trip.

use std::path::Path;

use anyhow::{Context, Result};

/// How many entries are kept. Enough to be useful, short enough that the file
/// stays small.
pub const MAX_ENTRIES: usize = 500;

/// The header the format is versioned with. A file without it is an older
/// format, whose entries carry no escapes.
const HEADER: &str = "#V2";

/// Read the history, oldest first.
///
/// A missing file is an empty history rather than an error: the first run has
/// none, and losing a history is not worth refusing to start over.
pub fn load(path: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut lines: Vec<&str> = text.lines().collect();
    // A file that starts with the header is escaped; one that does not is in the
    // older format, where every line is already an entry.
    let escaped = lines.first() == Some(&HEADER);
    if escaped {
        lines.remove(0);
    }
    let mut entries: Vec<String> = lines
        .into_iter()
        .filter(|line| !line.is_empty())
        .map(|line| {
            if escaped {
                unescape(line)
            } else {
                line.to_owned()
            }
        })
        .collect();
    let keep = entries.len().saturating_sub(MAX_ENTRIES);
    entries.split_off(keep)
}

/// Write the history, oldest first, dropping the oldest beyond [`MAX_ENTRIES`].
///
/// The whole file is rewritten rather than appended to: it is small, and a
/// rewrite cannot leave a half-written entry behind if the process dies.
pub fn save(path: &Path, entries: &[String]) -> Result<()> {
    let keep = entries.len().saturating_sub(MAX_ENTRIES);
    let mut out = String::from(HEADER);
    out.push('\n');
    for entry in &entries[keep..] {
        out.push_str(&escape(entry));
        out.push('\n');
    }
    // The directory is made here rather than by the reader of the path: saving a
    // history is the write that needs it, and the file may be the first thing in
    // a home that has no `~/.caocli` yet.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("failed to create directory: {}", dir.display()))?;
    }
    std::fs::write(path, out)
        .with_context(|| format!("failed to write the history to {}", path.display()))
}

/// One entry, escaped onto a single line.
fn escape(entry: &str) -> String {
    let mut out = String::with_capacity(entry.len());
    for ch in entry.chars() {
        match ch {
            '\\' => out.push_str(r"\\"),
            '\n' => out.push_str(r"\n"),
            _ => out.push(ch),
        }
    }
    out
}

/// The inverse of [`escape`]. An unknown escape keeps both characters, so a
/// hand-edited file degrades rather than losing text.
fn unescape(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpfile(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("caocli-history-{tag}-{}", std::process::id()))
    }

    #[test]
    fn a_missing_file_is_an_empty_history() {
        // Not an error: the first run has none, and refusing to start over a
        // missing history would be a poor trade.
        assert!(load(&tmpfile("absent")).is_empty());
    }

    #[test]
    fn entries_survive_a_round_trip_in_order() {
        let path = tmpfile("round-trip");
        let entries = vec!["first".to_owned(), "second".to_owned()];
        save(&path, &entries).unwrap();
        assert_eq!(load(&path), entries);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_multi_line_entry_survives() {
        // The reason the escaping exists: the file is line-based.
        let path = tmpfile("multiline");
        let entries = vec!["one\ntwo".to_owned(), "three".to_owned()];
        save(&path, &entries).unwrap();
        assert_eq!(load(&path), entries);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_backslash_in_an_entry_survives() {
        let path = tmpfile("backslash");
        let entries = vec![r"C:\path\to\thing".to_owned(), r"a\nb".to_owned()];
        save(&path, &entries).unwrap();
        assert_eq!(load(&path), entries);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_file_is_versioned() {
        let path = tmpfile("header");
        save(&path, &["x".to_owned()]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("#V2\n"), "{text:?}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn an_unversioned_file_is_read_without_unescaping() {
        // The older format wrote entries raw; a backslash there is a backslash,
        // not the start of an escape.
        let path = tmpfile("v1");
        std::fs::write(&path, "first\nsecond\n").unwrap();
        assert_eq!(load(&path), vec!["first", "second"]);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn the_oldest_entries_are_dropped_past_the_cap() {
        let path = tmpfile("cap");
        let entries: Vec<String> = (0..MAX_ENTRIES + 10).map(|i| format!("line {i}")).collect();
        save(&path, &entries).unwrap();
        let loaded = load(&path);
        assert_eq!(loaded.len(), MAX_ENTRIES);
        assert_eq!(loaded[0], "line 10", "the oldest went first");
        assert_eq!(loaded[MAX_ENTRIES - 1], format!("line {}", MAX_ENTRIES + 9));
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn escapes_are_the_inverse_of_each_other() {
        for original in [
            "plain",
            "with\nnewline",
            r"with\backslash",
            r"\n literal",
            "",
        ] {
            assert_eq!(unescape(&escape(original)), original, "{original:?}");
        }
    }

    #[test]
    fn an_unknown_escape_is_kept_rather_than_swallowed() {
        // A hand-edited file should degrade, not lose text.
        assert_eq!(unescape(r"a\qb"), r"a\qb");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }

    #[test]
    fn empty_lines_are_not_entries() {
        let path = tmpfile("blank");
        std::fs::write(&path, "#V2\na\n\nb\n").unwrap();
        assert_eq!(load(&path), vec!["a", "b"]);
        std::fs::remove_file(&path).unwrap();
    }
}

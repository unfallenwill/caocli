//! What a tool result looks like in the transcript: one line, in the style the
//! result's own prefix earns.
//!
//! The prefixes are the tools' own -- `exit_code:`, `ok: wrote`,
//! `ok: replaced`, `error:` -- so the dispatch is over the result text alone,
//! and an unrecognized prefix falls back to the first line and the byte count.

use crate::ui::cell::Style;

/// One line of a result summary, what the transcript says about a tool
/// returning. The text is the first interesting line and the size is the
/// bytes -- the result the model reads is the whole result, but the line
/// the reader sees is the shape of it, and the shape of "1234 bytes of
/// `error: cannot read /etc/shadow`" is the file the call asked for, not
/// the third stack frame.
///
/// Each tool's success message starts with a prefix that names it, so the
/// dispatch is by the result text alone: Bash says `exit_code:`, Write
/// says `ok: wrote`, Edit says `ok: replaced`, and a failure in any tool
/// says `error:`. A reader who does not recognize the prefix sees the
/// default: the first line and the byte count.
///
/// The text is what the cell paints, the style is the color the front end
/// reaches for. Two values because one function returns both: the cell
/// always carries the result string, the style is a label the front end
/// maps to its own palette.
pub(crate) fn result_summary(result: &str) -> (Style, String) {
    // A failure is a failure in every tool: the text starts with `error:`,
    // the style is the one reserved for it, and the per-prefix parsing
    // stops here. The body of the message is the part after the prefix,
    // trimmed, so `error:  cannot read /etc/shadow` reads as
    // `error: cannot read /etc/shadow`.
    if let Some(rest) = result.strip_prefix("error:") {
        return (Style::Red, format!("error: {}", rest.trim_start()));
    }
    if let Some(code) = bash_exit_code(result) {
        return bash_summary(code);
    }
    if let Some(line) = write_summary(result) {
        return (Style::Dim, line);
    }
    if let Some(line) = edit_summary(result) {
        return (Style::Dim, line);
    }
    default_summary(result)
}

/// The exit code at the head of a Bash result, if the result is a Bash
/// result. The line is `exit_code: N` and the parsing stops if it does not
/// parse as an integer -- a tool that put `exit_code: foo` in its result
/// is not Bash, and the default summary is the right thing to fall back
/// to.
fn bash_exit_code(result: &str) -> Option<i64> {
    result
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("exit_code:"))
        .and_then(|n| n.trim().parse::<i64>().ok())
}

/// The Bash summary once we know the exit code: just `exit_code: N`.
///
/// The first line of stdout is *not* shown, even though it would tell a
/// reader what the command produced: the result is what the model reads,
/// and a Bash result can carry anything in stdout -- a secret, a path the
/// user did not consent to be in the transcript. The transcript's rule is
/// that the summary is metadata, never the result's content; the size of
/// the result has the same problem at a smaller scale, and is dropped
/// for the same reason. The exit code is the whole of it.
fn bash_summary(code: i64) -> (Style, String) {
    (Style::Dim, format!("exit_code: {code}"))
}

/// The first interesting line of a Write result, or `None` when the
/// result is not a Write success. The two Write successes are
/// `ok: wrote PATH (N bytes)` and
/// `ok: PATH already has exactly this content (N bytes); left unchanged`,
/// and they differ in whether anything was written.
fn write_summary(result: &str) -> Option<String> {
    if let Some(rest) = result
        .strip_prefix("ok: wrote ")
        .and_then(|r| r.strip_suffix(')'))
        && let Some((path, bytes)) = rest.rsplit_once(" (")
    {
        return Some(format!("wrote {path} · {bytes}"));
    }
    if let Some(rest) = result.strip_prefix("ok: ") {
        // `PATH already has exactly this content (N bytes); left unchanged`
        // The path is the text between `ok: ` and ` already`, the rest is
        // a description of why the call did not need to do anything.
        if let Some(boundary) = rest.find(" already has exactly this content") {
            let path = &rest[..boundary];
            return Some(format!("unchanged {path}"));
        }
    }
    None
}

/// The first interesting line of an Edit result, or `None` when the
/// result is not an Edit success. The success message names the path and
/// the new size; that is what a reader wants, not the raw success line.
fn edit_summary(result: &str) -> Option<String> {
    let rest = result.strip_prefix("ok: replaced 1 occurrence; ")?;
    let (path, bytes) = rest.rsplit_once(" is now ")?;
    Some(format!("replaced in {path} · now {bytes}"))
}

/// The summary every other tool gets: the first line of the result and
/// its byte count. A Read of a Cargo.toml shows `[package] · 312 bytes`;
/// a Glob of `**/*.rs` shows the first match and the total bytes; that
/// is the shape of the result, and the shape is what a reader can act on.
fn default_summary(result: &str) -> (Style, String) {
    (
        Style::Dim,
        format!(
            "{} · {} bytes",
            result.lines().next().unwrap_or(""),
            result.len()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Bash result is just the exit code, dim. The first line of stdout
    /// is content, and the transcript's rule is that the summary is metadata.
    #[test]
    fn result_summary_for_bash_is_just_the_exit_code() {
        assert_eq!(
            result_summary("exit_code: 0"),
            (Style::Dim, "exit_code: 0".to_string())
        );
        assert_eq!(
            result_summary("exit_code: 3\n--- stdout ---\nsecret"),
            (Style::Dim, "exit_code: 3".to_string())
        );
        // Failed-to-start: a `127` with no stdout.
        assert_eq!(
            result_summary("exit_code: 127\n--- stderr ---\nfailed to start"),
            (Style::Dim, "exit_code: 127".to_string())
        );
    }

    /// A Write success: path and size. The path may contain spaces.
    #[test]
    fn result_summary_for_write_names_the_path_and_size() {
        assert_eq!(
            result_summary("ok: wrote /tmp/x.txt (42 bytes)"),
            (Style::Dim, "wrote /tmp/x.txt · 42 bytes".to_string())
        );
        // An idempotent write -- nothing was written, but the path is.
        assert_eq!(
            result_summary(
                "ok: /tmp/x.txt already has exactly this content (42 bytes); left unchanged"
            ),
            (Style::Dim, "unchanged /tmp/x.txt".to_string())
        );
    }

    /// An Edit success: the path and the new size.
    #[test]
    fn result_summary_for_edit_names_the_path_and_new_size() {
        assert_eq!(
            result_summary("ok: replaced 1 occurrence; /tmp/x.rs is now 512 bytes"),
            (
                Style::Dim,
                "replaced in /tmp/x.rs · now 512 bytes".to_string()
            )
        );
    }

    /// Any tool can fail; the failure is always red.
    #[test]
    fn result_summary_for_a_failure_is_red() {
        let (style, text) = result_summary("error: file not found");
        assert_eq!(style, Style::Red);
        assert_eq!(text, "error: file not found");
        // Whitespace after the prefix is trimmed.
        let (style, text) = result_summary("error:   cannot read /etc/shadow");
        assert_eq!(style, Style::Red);
        assert_eq!(text, "error: cannot read /etc/shadow");
    }

    /// Read, Glob, TodoWrite, and other tools fall back to the default
    /// summary: first line and byte count.
    #[test]
    fn result_summary_defaults_to_first_line_and_size() {
        let (style, text) = result_summary("[package]\nname = \"caocli\"");
        assert_eq!(style, Style::Dim);
        assert_eq!(text, "[package] · 25 bytes");
        // An empty result: empty first line, zero bytes.
        let (style, text) = result_summary("");
        assert_eq!(style, Style::Dim);
        assert_eq!(text, " · 0 bytes");
    }
}

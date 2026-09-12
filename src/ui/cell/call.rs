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

use crate::ui::cell::{DiffKind, DiffLine, Style};
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

/// The change a call makes, for the calls that make one.
///
/// The arguments are the wire format of a tool call, which is also what
/// replay reads out of the session log, so live and replayed turns show
/// the same lines without a second source for either one. The shape of the
/// change is a real unified diff: a `@@ -A,B +C,D @@` hunk header, the
/// lines of context and change below it, and an `Omitted(N)` line at the
/// end of a long change. The algorithm is in [`unified_diff_lines`].
pub(super) fn diff_lines(name: &str, args: &str) -> Vec<DiffLine> {
    unified_diff_lines(name, args)
}

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
pub(super) fn result_summary(result: &str) -> (Style, String) {
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

/// The width above which the line-level diff falls back to showing the call's
/// old and new strings verbatim: an LCS table is `O(m*n)` in memory, and a
/// `Write` of a 1000-line file does not need a real diff, just a "what is
/// about to land here" view. Past the cap, the change is a wall of `+` lines
/// either way.
const DIFF_LINE_CAP: usize = 256;

/// Lines of context a hunk keeps on each side of every change.
///
/// Three is the `diff -u` default and what a reader expects: enough to see
/// the function the change is in, few enough that two nearby changes do not
/// merge into a single screenful. A hunk of the whole file, when the file is
/// the change, is what this produces -- and that is what it should.
const DIFF_CONTEXT: usize = 3;

/// The line cap a cell applies to the *displayed* change, regardless of how
/// long the call's strings are. Past the cap, the rest is one `Omitted`
/// line, the way a long diff was shown before this one.
pub(super) const DIFF_DISPLAY_LINES: usize = 12;

/// The change a call makes, computed as a real unified diff.
///
/// The same wire format the model sent is the input, so live and replayed
/// turns read the same lines out of it. The strings are split with
/// [`str::lines`] -- a trailing newline is not its own line -- and a call
/// whose `old_string` and `new_string` are equal comes back with no lines,
/// because nothing changed.
pub(super) fn unified_diff_lines(name: &str, args: &str) -> Vec<DiffLine> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(args) else {
        return Vec::new();
    };
    let (old_text, new_text) = match name {
        // The order a diff is read in: what goes, then what arrives.
        "Edit" => (
            v.get("old_string").and_then(|s| s.as_str()).unwrap_or(""),
            v.get("new_string").and_then(|s| s.as_str()).unwrap_or(""),
        ),
        "Write" => ("", v.get("content").and_then(|s| s.as_str()).unwrap_or("")),
        _ => return Vec::new(),
    };
    let old: Vec<&str> = old_text.lines().collect();
    let new: Vec<&str> = new_text.lines().collect();
    // Past the cap, the LCS table is too big to be worth its row count; the
    // view falls back to "what leaves, then what arrives", the way the old
    // projection did. The result is the same shape a reader had before, and
    // the cell renders it the same way.
    if old.len() > DIFF_LINE_CAP || new.len() > DIFF_LINE_CAP {
        return fallback_diff(&old, &new);
    }
    let ops = diff_ops(&old, &new);
    if !ops.iter().any(|op| *op != Op::Keep) {
        return Vec::new();
    }
    let hunks = to_hunks(&ops, &old, &new, DIFF_CONTEXT);
    let mut lines = Vec::new();
    for hunk in &hunks {
        lines.push(DiffLine {
            kind: DiffKind::Hunk {
                old_start: hunk.old_start,
                old_count: hunk.old_count,
                new_start: hunk.new_start,
                new_count: hunk.new_count,
            },
            text: String::new(),
        });
        for (op, text) in &hunk.lines {
            lines.push(DiffLine {
                kind: match op {
                    Op::Remove => DiffKind::Removed,
                    Op::Add => DiffKind::Added,
                    Op::Keep => DiffKind::Context,
                },
                text: (*text).to_owned(),
            });
        }
    }
    if lines.len() > DIFF_DISPLAY_LINES {
        let omitted = lines.len() - DIFF_DISPLAY_LINES;
        lines.truncate(DIFF_DISPLAY_LINES);
        lines.push(DiffLine {
            kind: DiffKind::Omitted(omitted),
            text: String::new(),
        });
    }
    lines
}

/// The view taken when a call's strings are too long for a real diff: every
/// old line as removed, then every new line as added. No hunk header -- the
/// shape of the view is what it was before unified diffs -- and the same
/// display cap as the real one.
fn fallback_diff(old: &[&str], new: &[&str]) -> Vec<DiffLine> {
    let mut lines = Vec::with_capacity(old.len() + new.len());
    for line in old {
        lines.push(DiffLine {
            kind: DiffKind::Removed,
            text: (*line).to_owned(),
        });
    }
    for line in new {
        lines.push(DiffLine {
            kind: DiffKind::Added,
            text: (*line).to_owned(),
        });
    }
    if lines.len() > DIFF_DISPLAY_LINES {
        let omitted = lines.len() - DIFF_DISPLAY_LINES;
        lines.truncate(DIFF_DISPLAY_LINES);
        lines.push(DiffLine {
            kind: DiffKind::Omitted(omitted),
            text: String::new(),
        });
    }
    lines
}

/// One operation in a line-level diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    /// A line in `old` that is not in `new`.
    Remove,
    /// A line in `new` that is not in `old`.
    Add,
    /// A line in both, kept as context.
    Keep,
}

/// The line-level diff between two slices of lines: a sequence of
/// `Remove`/`Add`/`Keep` operations that, walked in order, turns `old` into
/// `new`.
///
/// The algorithm is a textbook LCS: a `(m+1) x (n+1)` table of longest
/// common subsequence lengths, walked back to recover one edit script. The
/// table is `usize`, so an entry is at most `min(m, n)`, and the table itself
/// is `(m+1) * (n+1) * size_of::<usize>()` bytes -- the [`DIFF_LINE_CAP`]
/// guard on the caller is what keeps that bounded.
fn diff_ops(old: &[&str], new: &[&str]) -> Vec<Op> {
    let m = old.len();
    let n = new.len();
    if m == 0 {
        return vec![Op::Add; n];
    }
    if n == 0 {
        return vec![Op::Remove; m];
    }
    let mut lcs = vec![vec![0usize; n + 1]; m + 1];
    for i in 0..m {
        for j in 0..n {
            lcs[i + 1][j + 1] = if old[i] == new[j] {
                lcs[i][j] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    // Walk back from `(m, n)` to `(0, 0)`, pushing ops in reverse.
    let mut ops = Vec::with_capacity(m + n);
    let (mut i, mut j) = (m, n);
    while i > 0 && j > 0 {
        if old[i - 1] == new[j - 1] {
            ops.push(Op::Keep);
            i -= 1;
            j -= 1;
        } else if lcs[i - 1][j] >= lcs[i][j - 1] {
            ops.push(Op::Remove);
            i -= 1;
        } else {
            ops.push(Op::Add);
            j -= 1;
        }
    }
    while i > 0 {
        ops.push(Op::Remove);
        i -= 1;
    }
    while j > 0 {
        ops.push(Op::Add);
        j -= 1;
    }
    ops.reverse();
    ops
}

/// One hunk of a unified diff: the line ranges in the old and new files, and
/// the lines of context and change that make up the hunk's body.
struct Hunk<'a> {
    /// 1-based line number in `old` of the first line the hunk touches.
    old_start: usize,
    /// Number of `old` lines the hunk spans (context + removed).
    old_count: usize,
    /// 1-based line number in `new` of the first line the hunk touches.
    new_start: usize,
    /// Number of `new` lines the hunk spans (context + added).
    new_count: usize,
    /// The hunk's body, in file order: context, removed, added, context.
    lines: Vec<(Op, &'a str)>,
}

/// Group a sequence of ops into hunks with `context` lines of context on
/// each side of every change. A `Keep` that is more than `context` ops away
/// from any `Remove`/`Add` is left out: a hunk is a region of the file the
/// reader is meant to look at, and a screenful of context nobody is reading
/// is not that.
fn to_hunks<'a>(ops: &[Op], old: &[&'a str], new: &[&'a str], context: usize) -> Vec<Hunk<'a>> {
    // First: mark which ops are included. A `Remove`/`Add` is always in; a
    // `Keep` is in if it sits within `context` ops of an `Remove`/`Add`. The
    // scan is O(n * context) which is fine -- n is the number of lines, and
    // context is three.
    let n = ops.len();
    let mut included = vec![false; n];
    for (i, op) in ops.iter().enumerate() {
        if *op != Op::Keep {
            included[i] = true;
        }
    }
    for i in 0..n {
        if ops[i] != Op::Keep {
            continue;
        }
        for back in 1..=context {
            if i >= back && ops[i - back] != Op::Keep {
                included[i] = true;
                break;
            }
        }
        if included[i] {
            continue;
        }
        for fwd in 1..=context {
            if i + fwd < n && ops[i + fwd] != Op::Keep {
                included[i] = true;
                break;
            }
        }
    }
    // Second: track the 1-based old and new line numbers for each op. A
    // hunk's `old_start` and `new_start` are these numbers, taken at the hunk's
    // first op.
    let mut old_line = vec![0usize; n];
    let mut new_line = vec![0usize; n];
    let (mut o, mut nv) = (1usize, 1usize);
    for (i, op) in ops.iter().enumerate() {
        old_line[i] = o;
        new_line[i] = nv;
        match op {
            Op::Remove => o += 1,
            Op::Add => nv += 1,
            Op::Keep => {
                o += 1;
                nv += 1;
            }
        }
    }
    // Third: walk the included runs and turn each one into a hunk.
    let mut hunks = Vec::new();
    let mut i = 0;
    while i < n {
        if !included[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < n && included[i] {
            i += 1;
        }
        hunks.push(make_hunk(
            &ops[start..i],
            old,
            new,
            old_line[start],
            new_line[start],
        ));
    }
    hunks
}

/// Turn a contiguous run of included ops into one hunk, tracking the old
/// and new line numbers against the run's start.
fn make_hunk<'a>(
    ops: &[Op],
    old: &[&'a str],
    new: &[&'a str],
    old_start: usize,
    new_start: usize,
) -> Hunk<'a> {
    // `old_start` and `new_start` are 1-based, the way unified-diff headers
    // are. The first line of the run is at that index, so the 0-based cursor
    // into `old` / `new` is one less.
    let mut old_i = old_start - 1;
    let mut new_i = new_start - 1;
    let mut old_count = 0;
    let mut new_count = 0;
    let mut raw_lines = Vec::with_capacity(ops.len());
    for op in ops {
        match op {
            Op::Remove => {
                old_count += 1;
                raw_lines.push((Op::Remove, old[old_i]));
                old_i += 1;
            }
            Op::Add => {
                new_count += 1;
                raw_lines.push((Op::Add, new[new_i]));
                new_i += 1;
            }
            Op::Keep => {
                old_count += 1;
                new_count += 1;
                // `old` and `new` agree on this line -- they are equal by
                // construction -- so either side carries the same text.
                raw_lines.push((Op::Keep, old[old_i]));
                old_i += 1;
                new_i += 1;
            }
        }
    }
    Hunk {
        // For a hunk with no old lines (a pure addition), `old_start` is the
        // line in old *after* which the additions appear -- the line of the
        // preceding context, or 0 if there is none. The convention from
        // `diff -u` is `@@ -0,0 +N,M @@` for additions at the very start
        // and `@@ -K,0 +K+1,M @@` for additions in the middle; both follow
        // from `old_start = (line of preceding context) = 0 if none`, which
        // is `old_line[0] - 1` since the first op in the run is the
        // addition and `old_line[0]` is the line of the preceding context
        // (or 1 if there is no preceding op, in which case `saturating_sub`
        // gives 0).
        old_start: if old_count == 0 {
            old_start.saturating_sub(1)
        } else {
            old_start
        },
        old_count,
        new_start: if new_count == 0 {
            new_start.saturating_sub(1)
        } else {
            new_start
        },
        new_count,
        // The walk-back's tie-breaking rule produces `Add` before `Remove`
        // for a mid-file replacement, which is the reverse of what a reader
        // of `diff -u` is used to. Re-group each run of changes so removals
        // come first: a hunk reads as "what leaves, then what arrives",
        // which is the order `git diff` uses and the order a person
        // describes a change in words.
        lines: order_changes(raw_lines),
    }
}

/// The new order of a hunk's lines: the lines themselves, with each run of
/// `Add`/`Remove` ops reordered so the removes come first.
fn order_changes(raw: Vec<(Op, &str)>) -> Vec<(Op, &str)> {
    let mut out = Vec::with_capacity(raw.len());
    let mut pending: Vec<(Op, &str)> = Vec::new();
    for line in raw {
        if line.0 == Op::Keep {
            if !pending.is_empty() {
                // Removes first, then adds -- the conventional unified-diff
                // order within a run of changes.
                pending.sort_by_key(|l| match l.0 {
                    Op::Remove => 0,
                    Op::Add => 1,
                    Op::Keep => 2,
                });
                out.append(&mut pending);
            }
            out.push(line);
        } else {
            pending.push(line);
        }
    }
    if !pending.is_empty() {
        pending.sort_by_key(|l| match l.0 {
            Op::Remove => 0,
            Op::Add => 1,
            Op::Keep => 2,
        });
        out.append(&mut pending);
    }
    out
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

    // ----- Unified diff ------------------------------------------------------

    /// `diff_ops` is the LCS walk. An empty side becomes a run of one op.
    #[test]
    fn diff_ops_handles_empty_sides() {
        assert_eq!(diff_ops(&[], &[]), vec![]);
        assert_eq!(diff_ops(&[], &["a", "b"]), vec![Op::Add, Op::Add]);
        assert_eq!(diff_ops(&["a", "b"], &[]), vec![Op::Remove, Op::Remove]);
    }

    /// Identical inputs are all `Keep`, with nothing to do.
    #[test]
    fn diff_ops_keeps_identical_lines() {
        let old = ["a", "b", "c"];
        assert_eq!(diff_ops(&old, &old), vec![Op::Keep, Op::Keep, Op::Keep]);
    }

    /// A one-line change in the middle: two `Keep`, one `Remove`, one `Add`,
    /// in file order. The order is what the LCS walk-back produces -- the
    /// swap to "remove first, then add" is the hunks' job, not the ops'.
    #[test]
    fn diff_ops_orders_a_middle_change() {
        assert_eq!(
            diff_ops(&["a", "b", "c", "d"], &["a", "b", "X", "d"]),
            vec![Op::Keep, Op::Keep, Op::Add, Op::Remove, Op::Keep],
        );
    }

    /// Two non-adjacent changes stay separate hunks, not one run of
    /// `Remove`/`Add`: the `Keep` between them breaks the stream. The walk
    /// produces `Add` before `Remove`; the test asserts the walk's order.
    #[test]
    fn diff_ops_keeps_two_changes_separate() {
        assert_eq!(
            diff_ops(&["a", "b", "c", "d", "e"], &["a", "X", "c", "d", "Y"]),
            vec![
                Op::Keep,
                Op::Add,
                Op::Remove,
                Op::Keep,
                Op::Keep,
                Op::Add,
                Op::Remove,
            ],
        );
    }

    /// `to_hunks` with one line of context collapses the surrounding `Keep`
    /// ops into the hunk. The hunk's line numbers are 1-based.
    #[test]
    fn to_hunks_marks_context_around_changes() {
        let old = ["a", "b", "c", "d", "e"];
        let new = ["a", "b", "X", "d", "e"];
        let ops = diff_ops(&old, &new);
        let hunks = to_hunks(&ops, &old, &new, 1);
        assert_eq!(hunks.len(), 1);
        let h = &hunks[0];
        assert_eq!(h.old_start, 2, "first op is at old line 2");
        assert_eq!(h.new_start, 2);
        // Lines 2-3 of the hunk body: keep b, remove c, add X, keep d. With
        // 1 line of context, "b" and "d" are the boundary.
        assert_eq!(
            h.lines,
            vec![
                (Op::Keep, "b"),
                (Op::Remove, "c"),
                (Op::Add, "X"),
                (Op::Keep, "d"),
            ],
        );
    }

    /// Two changes more than `context` lines apart stay as two hunks: the
    /// gap between them is `Keep` lines nobody is meant to look at, and
    /// splitting is what `diff -u` does. With `context = 1`, each hunk
    /// includes the changed line and one line on either side.
    #[test]
    fn to_hunks_splits_when_context_cannot_bridge_the_gap() {
        let old = ["a", "b", "c", "d", "e", "f", "g", "h"];
        let new = ["a", "X", "c", "d", "e", "f", "Y", "h"];
        let ops = diff_ops(&old, &new);
        let hunks = to_hunks(&ops, &old, &new, 1);
        assert_eq!(hunks.len(), 2, "two non-mergeable hunks");
        // First hunk: a, change b->X, c. `order_changes` reorders the change
        // to remove-then-add.
        assert_eq!(
            hunks[0].lines,
            vec![
                (Op::Keep, "a"),
                (Op::Remove, "b"),
                (Op::Add, "X"),
                (Op::Keep, "c"),
            ]
        );
        // Second hunk: f, change g->Y, h. The two changes are far enough
        // apart that the keeps between them are not context for either.
        assert_eq!(
            hunks[1].lines,
            vec![
                (Op::Keep, "f"),
                (Op::Remove, "g"),
                (Op::Add, "Y"),
                (Op::Keep, "h"),
            ]
        );
    }

    /// A pure add (empty `old`) has one hunk with `old_count` of 0 and a
    /// 1-based `new_start`: the unified-diff convention for "this hunk only
    /// adds lines, and the additions start at new line 1".
    #[test]
    fn unified_diff_lines_handles_a_pure_addition() {
        let args = serde_json::json!({
            "file_path": "a.txt",
            "old_string": "",
            "new_string": "alpha\nbeta\ngamma",
        })
        .to_string();
        let lines = unified_diff_lines("Edit", &args);
        assert_eq!(
            lines,
            vec![
                DiffLine {
                    kind: DiffKind::Hunk {
                        old_start: 0,
                        old_count: 0,
                        new_start: 1,
                        new_count: 3,
                    },
                    text: String::new(),
                },
                DiffLine {
                    kind: DiffKind::Added,
                    text: "alpha".into(),
                },
                DiffLine {
                    kind: DiffKind::Added,
                    text: "beta".into(),
                },
                DiffLine {
                    kind: DiffKind::Added,
                    text: "gamma".into(),
                },
            ]
        );
    }

    /// A pure removal: one hunk, `new_count` of 0, every line `Removed`.
    #[test]
    fn unified_diff_lines_handles_a_pure_removal() {
        let args = serde_json::json!({
            "file_path": "a.txt",
            "old_string": "alpha\nbeta",
            "new_string": "",
        })
        .to_string();
        let lines = unified_diff_lines("Edit", &args);
        assert_eq!(
            lines[0].kind,
            DiffKind::Hunk {
                old_start: 1,
                old_count: 2,
                new_start: 0,
                new_count: 0
            }
        );
        assert!(lines[1..].iter().all(|l| l.kind == DiffKind::Removed));
    }

    /// An edit whose `old_string` and `new_string` are equal: nothing
    /// changed, nothing to show.
    #[test]
    fn unified_diff_lines_omits_a_no_op() {
        let args = serde_json::json!({
            "file_path": "a.txt",
            "old_string": "alpha\nbeta",
            "new_string": "alpha\nbeta",
        })
        .to_string();
        assert!(unified_diff_lines("Edit", &args).is_empty());
    }

    /// A 1-line change in a 5-line function: the hunk header says where in
    /// the file, the context is the two surrounding lines, and only the
    /// changed line is removed/added.
    #[test]
    fn unified_diff_lines_keeps_the_unchanged_lines_around_the_change() {
        let args = serde_json::json!({
            "file_path": "f.rs",
            "old_string": "fn a() {\n    1\n    2\n    3\n}\n",
            "new_string": "fn a() {\n    1\n    9\n    3\n}\n",
        })
        .to_string();
        let lines = unified_diff_lines("Edit", &args);
        // First line: the hunk header, naming the line ranges in old and new.
        let header = &lines[0];
        assert!(matches!(
            header.kind,
            DiffKind::Hunk {
                old_start: 1,
                old_count: 5,
                new_start: 1,
                new_count: 5
            }
        ));
        // What is left: two context lines, the removed, the added, two more
        // context lines, then the closing brace.
        let text_kinds: Vec<(&str, &str)> = lines[1..]
            .iter()
            .map(|l| (label(&l.kind), l.text.as_str()))
            .collect();
        assert_eq!(
            text_kinds,
            vec![
                ("context", "fn a() {"),
                ("context", "    1"),
                ("remove", "    2"),
                ("add", "    9"),
                ("context", "    3"),
                ("context", "}"),
            ]
        );
    }

    /// Past the line cap, the algorithm is dropped in favor of the
    /// `old → new` view, which is the same shape a reader had before the
    /// unified diff. The cap is what keeps the LCS table bounded.
    #[test]
    fn unified_diff_lines_falls_back_when_input_is_too_long() {
        let many = (0..DIFF_LINE_CAP + 5)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let args = serde_json::json!({ "content": many }).to_string();
        let lines = unified_diff_lines("Write", &args);
        // The fallback emits every line as `Added`, then truncates to the
        // display cap.
        assert!(lines.len() <= DIFF_DISPLAY_LINES + 1);
        assert!(lines.iter().any(|l| matches!(l.kind, DiffKind::Omitted(_))));
        // No hunk header: the fallback is the old view.
        assert!(
            lines
                .iter()
                .all(|l| !matches!(l.kind, DiffKind::Hunk { .. }))
        );
    }

    /// The display cap trims a long real diff to a fixed budget, with one
    /// `Omitted` line at the end saying how many were left out.
    #[test]
    fn unified_diff_lines_truncates_a_long_diff_to_the_display_cap() {
        let many: Vec<String> = (0..(DIFF_DISPLAY_LINES + 5))
            .map(|i| format!("L{i}"))
            .collect();
        let new: Vec<String> = (0..(DIFF_DISPLAY_LINES + 5))
            .map(|i| format!("L{i}'"))
            .collect();
        let args = serde_json::json!({
            "old_string": many.join("\n"),
            "new_string": new.join("\n"),
        })
        .to_string();
        let lines = unified_diff_lines("Edit", &args);
        assert!(lines.len() <= DIFF_DISPLAY_LINES + 1);
        assert!(matches!(lines.last().unwrap().kind, DiffKind::Omitted(n) if n > 0));
    }

    /// `Write` has no `old_string`; every line of `content` is added.
    #[test]
    fn unified_diff_lines_writes_a_write_call_as_a_pure_addition() {
        let args = serde_json::json!({
            "file_path": "a.txt",
            "content": "one\ntwo",
        })
        .to_string();
        let lines = unified_diff_lines("Write", &args);
        assert_eq!(
            lines[0].kind,
            DiffKind::Hunk {
                old_start: 0,
                old_count: 0,
                new_start: 1,
                new_count: 2
            }
        );
        assert!(lines[1..].iter().all(|l| l.kind == DiffKind::Added));
    }

    /// Tools that do not make a change (Bash, Read, Glob, ...) come back
    /// with no lines: the diff is what a call changes, and a call that does
    /// not change anything has nothing to diff.
    #[test]
    fn unified_diff_lines_is_empty_for_non_changing_calls() {
        assert!(unified_diff_lines("Bash", r#"{"command":"ls"}"#).is_empty());
        assert!(unified_diff_lines("Read", r#"{"file_path":"x"}"#).is_empty());
        assert!(unified_diff_lines("Edit", "not json at all").is_empty());
    }

    // ----- Result summary ---------------------------------------------------

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

    /// A short label for a `DiffKind`, so a failing assertion can name the
    /// variant it expected without re-implementing the `Debug` mapping.
    fn label(kind: &DiffKind) -> &'static str {
        match kind {
            DiffKind::Removed => "remove",
            DiffKind::Added => "add",
            DiffKind::Context => "context",
            DiffKind::Omitted(_) => "omitted",
            DiffKind::Hunk { .. } => "hunk",
        }
    }
}

//! One step in the transcript: a tool call's full lifecycle.
//!
//! A [`Step`] is born when the machine sends a `ToolStart` notice and settles
//! when it sends the matching `ToolResult`. While it is open, the tool's own
//! output streams in as children; once it settles, the verdict (a one-line
//! summary of the result) is what sticks. One step is one row in the ledger:
//! the verb, the subject, the verdict, and the children if the step is still
//! running or it failed.
//!
//! Settled successful steps are quiet on purpose: the children stay in the
//! log and are reachable through the transcript viewer's rewind, but the
//! line of the screen is the verdict alone. A failed step carries its
//! children into view -- the failure is what the reader wants to see, and
//! hiding it behind another keystroke is the wrong trade.
//!
//! Step is the cell layer's data shape for "what the agent did". The wire
//! shapes (the `MachineNotice` enum) and the cell shapes (`Cell::Step`) are
//! distinct; the conversion is what happens in [`super::replay`] for
//! resumed sessions and in the front-end states for live ones.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::DiffLine;
use super::call::{diff_lines, hint, result_summary};

/// One step's identity: a monotonically-increasing counter, paired with the
/// machine's wire id by being created at `ToolStart` time. The cell layer
/// uses this for nothing visible -- the wire id is the model's, the StepId
/// is ours -- but tests read it to confirm a step is the same step across
/// `open` and `settle`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StepId(pub u64);

impl StepId {
    fn next() -> StepId {
        static COUNTER: AtomicU64 = AtomicU64::new(1);
        StepId(COUNTER.fetch_add(1, Ordering::Relaxed))
    }
}

/// Where a step stands in its lifecycle.
///
/// A step is open while the tool is running. It settles to one of three
/// states: the tool finished cleanly (`Done`), the tool reported a failure
/// (`Failed`), or the call never ran (`Denied` -- a gate decline, an
/// interrupted turn, or a turn that ended without a result).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    /// The tool is still running; children may still arrive.
    Running,
    /// The tool finished cleanly.
    Done,
    /// The tool reported a failure: non-zero exit, `error:` prefix, or a
    /// wire-level error before any result came back.
    Failed,
    /// The call never ran: the user declined the gate, the turn was
    /// interrupted, or the turn ended with no result.
    Denied,
}

impl StepStatus {
    /// A step that has settled -- it cannot receive more children and the
    /// verdict is what sticks.
    pub fn is_settled(&self) -> bool {
        !matches!(self, StepStatus::Running)
    }

    /// A step whose result is a failure of some kind -- red marker, auto-
    /// expanded children.
    ///
    /// Wired up by the gate's refusal path (task 4+) and the failure
    /// renderer (task 2's verbose toggle); kept available now so the
    /// step layer does not need to be edited twice.
    #[allow(dead_code)]
    pub fn is_failure(&self) -> bool {
        matches!(self, StepStatus::Failed | StepStatus::Denied)
    }

    /// The single character that opens the step's header line, with its
    /// trailing space.
    ///
    /// The vocabulary is the one the rest of the transcript uses: `▸` for
    /// something about to happen (a running step), `✔` for something that
    /// finished well, `✘` for something that did not.
    pub fn marker(&self) -> &'static str {
        match self {
            StepStatus::Running => "▸ ",
            StepStatus::Done => "✔ ",
            StepStatus::Failed => "✘ ",
            StepStatus::Denied => "✘ ",
        }
    }
}

/// One tool call's full lifecycle: born running, settled by the result.
///
/// `diff` is non-empty only for `Edit` (computed from the arguments at open
/// time); other tools do not produce a diff. `verdict` is empty until
/// [`Step::settle`] fills it. `started_at` is `None` for steps replayed
/// from a session log -- a past step's wall-clock time is not data we keep.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub id: StepId,
    pub verb: String,
    pub subject: String,
    pub diff: Vec<DiffLine>,
    pub status: StepStatus,
    pub children: Vec<String>,
    pub verdict: String,
    pub started_at: Option<Instant>,
}

impl Step {
    /// Open a new step from a `ToolStart` notice. The verb and subject are
    /// extracted from the call's arguments; the step is `Running` with no
    /// children and no verdict.
    pub fn open(verb: &str, args: &str) -> Self {
        // Edit and Write both carry a change in their arguments: an
        // Edit's old/new strings or a Write's full content. Compute the
        // diff at open time so the settled step (or running one) can
        // show what is about to land without re-parsing the args.
        let diff = if verb == "Edit" || verb == "Write" {
            diff_lines(verb, args)
        } else {
            Vec::new()
        };
        Self {
            id: StepId::next(),
            verb: verb.to_owned(),
            subject: hint(args),
            diff,
            status: StepStatus::Running,
            children: Vec::new(),
            verdict: String::new(),
            started_at: Some(Instant::now()),
        }
    }

    /// Open a step and immediately settle it with a result. Convenience
    /// for tests and for replay paths that already have the full result
    /// in hand and do not need the running step in between.
    #[cfg(test)]
    pub fn settled(verb: &str, args: &str, result: &str) -> Self {
        let mut step = Self::open(verb, args);
        step.settle(result);
        step
    }

    /// Reconstruct a step in its settled form for replay. The step is built
    /// `Done` by default; callers flip it to `Failed` if the result parses
    /// as one, or to `Denied` for the gate's refusal path.
    ///
    /// `wall_started` is `None` for replay (we do not know when the past
    /// step began); children are passed through verbatim.
    pub fn replayed(
        verb: &str,
        args: &str,
        children: Vec<String>,
        verdict: String,
        failed: bool,
    ) -> Self {
        let diff = if verb == "Edit" || verb == "Write" {
            diff_lines(verb, args)
        } else {
            Vec::new()
        };
        Self {
            id: StepId::next(),
            verb: verb.to_owned(),
            subject: hint(args),
            diff,
            status: if failed {
                StepStatus::Failed
            } else {
                StepStatus::Done
            },
            children,
            verdict,
            started_at: None,
        }
    }

    /// Settle a running step with the result text from the model.
    ///
    /// The verdict is the result line as the transcript would summarise it
    /// (`exit_code: 0`, `wrote X · N bytes`, `error: ...`). The status is
    /// `Failed` if the result is a failure (non-zero exit or `error:`
    /// prefix), `Done` otherwise.
    pub fn settle(&mut self, result: &str) {
        let (_style, text) = result_summary(result);
        let failed = text.starts_with("error:") || bash_exit_nonzero(result);
        self.verdict = text;
        self.status = if failed {
            StepStatus::Failed
        } else {
            StepStatus::Done
        };
    }

    /// Mark a step as denied. The verdict is a short note that goes into
    /// the header instead of a result line.
    ///
    /// Wired up by the gate's refusal path (task 4+); kept available
    /// now so the lifecycle's `Denied` state has a constructor and the
    /// cell layer does not need to be edited twice.
    #[allow(dead_code)]
    pub fn deny(&mut self, note: &str) {
        self.verdict = note.to_owned();
        self.status = StepStatus::Denied;
    }

    /// Mark a step as failed by interruption: the verdict carries the
    /// cause, the children stay visible.
    pub fn interrupt(&mut self) {
        self.verdict = "interrupted".to_owned();
        self.status = StepStatus::Failed;
    }

    /// Append a chunk of tool output to the children, splitting on newlines
    /// so one cell maps to one laid-out line per chunk line. A trailing
    /// partial line is kept as the last child (a leading empty string).
    pub fn push_output(&mut self, chunk: &str) {
        for line in chunk.split('\n') {
            self.children.push(line.to_owned());
        }
    }

    /// The header line of a step, **without** the marker: the verb, the
    /// subject, and (when settled) the verdict. The marker is the gutter's
    /// job -- adding it here would give the rendered line two markers.
    pub fn header(&self) -> String {
        match (self.status.is_settled(), self.verdict.is_empty()) {
            (false, _) => format!("{} {}", self.verb, self.subject),
            (true, true) => format!("{} {}", self.verb, self.subject),
            (true, false) => format!("{} {} · {}", self.verb, self.subject, self.verdict),
        }
    }

    /// Whether this step should carry its children on screen.
    ///
    /// - `Running`: yes (while it has them), the output is live.
    /// - `Failed`: yes (while it has them), auto-expand so the reader can
    ///   see why; nothing to show if the tool printed nothing.
    /// - `Denied`: no, the call never ran; no children to show.
    /// - `Done`: only when `verbose` is on. The Ctrl-O toggle re-exposes
    ///   settled-Done children on demand; the default keeps successful
    ///   steps quiet.
    pub fn shows_children_when(&self, verbose: bool) -> bool {
        match self.status {
            StepStatus::Running | StepStatus::Failed => !self.children.is_empty(),
            StepStatus::Done => verbose && !self.children.is_empty(),
            StepStatus::Denied => false,
        }
    }

    /// The styled spans this step emits at `verbose`. The header (verb,
    /// subject, and -- when settled -- verdict) is always there; the
    /// diff is there for Edit/Write calls (hiding it would mean an Edit
    /// step tells the reader "yes, edited" without saying what changed);
    /// the children are there when `shows_children_when` says so.
    ///
    /// The cell layer wraps this in a gutter and a `Vec<Line>`; this
    /// method only hands back the words.
    pub fn spans_with(&self, verbose: bool) -> Vec<crate::ui::cell::Span> {
        use crate::ui::cell::Style;
        use StepStatus::*;
        let header_style = match self.status {
            Running => Style::Yellow,
            Done => Style::Green,
            Failed | Denied => Style::Red,
        };
        let mut spans = vec![crate::ui::cell::Span::new(header_style, self.header())];
        if !self.diff.is_empty() {
            // The diff rides with the cell whether settled or running:
            // an Edit's diff is what the call is for, and the change
            // would otherwise be invisible until Ctrl-O.
            for line in &self.diff {
                spans.push(diff_span_pub(line));
            }
        }
        if self.shows_children_when(verbose) {
            for child in &self.children {
                if child.is_empty() {
                    // Skip the empty tail that `push_output` leaves
                    // behind when the chunk ended on a newline -- it
                    // would render as a blank line.
                    continue;
                }
                spans.push(crate::ui::cell::Span::new(Style::Dim, format!("\n{child}")));
            }
        }
        spans
    }
}

// `diff_span` is a private helper in `cell/mod.rs`; this version lives
// here for `spans_with` so the cell layer keeps the same wrapping rules.
// Duplicated because pulling `diff_span` out of `cell/mod.rs` would
// make that module's re-export surface larger than it needs to be.
fn diff_span_pub(line: &DiffLine) -> crate::ui::cell::Span {
    use crate::ui::cell::{DiffKind, Style};
    match &line.kind {
        DiffKind::Removed => crate::ui::cell::Span::new(Style::Red, format!("\n- {}", line.text)),
        DiffKind::Added => crate::ui::cell::Span::new(Style::Green, format!("\n+ {}", line.text)),
        DiffKind::Omitted(n) => {
            crate::ui::cell::Span::new(Style::Dim, format!("\n… {n} more lines"))
        }
        DiffKind::Context => crate::ui::cell::Span::new(Style::Plain, format!("\n {}", line.text)),
        DiffKind::Hunk {
            old_start,
            old_count,
            new_start,
            new_count,
        } => crate::ui::cell::Span::new(
            Style::Dim,
            format!("\n@@ -{old_start},{old_count} +{new_start},{new_count} @@"),
        ),
    }
}

/// Whether a Bash result reports a non-zero exit code. Bash results begin
/// with `exit_code: N`; a parse failure here means the result is not Bash
/// and the default success path applies.
fn bash_exit_nonzero(result: &str) -> bool {
    result
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("exit_code:"))
        .and_then(|n| n.trim().parse::<i64>().ok())
        .is_some_and(|code| code != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_step_has_no_verdict_and_no_children() {
        let step = Step::open("Read", r#"{"file_path":"/a/b.rs"}"#);
        assert_eq!(step.status, StepStatus::Running);
        assert!(!step.status.is_settled());
        assert!(step.verdict.is_empty());
        assert!(step.children.is_empty());
        assert_eq!(step.verb, "Read");
        assert_eq!(step.subject, "/a/b.rs");
        assert!(step.started_at.is_some());
    }

    #[test]
    fn an_edit_step_carries_its_diff() {
        let args = r#"{"file_path":"/a.rs","old_string":"a\nb","new_string":"a\nc"}"#;
        let step = Step::open("Edit", args);
        assert!(
            !step.diff.is_empty(),
            "Edit steps compute the diff at open time"
        );
    }

    #[test]
    fn other_tools_have_no_diff() {
        let step = Step::open("Bash", r#"{"command":"ls"}"#);
        assert!(step.diff.is_empty());
        let step = Step::open("Read", r#"{"file_path":"/a.rs"}"#);
        assert!(step.diff.is_empty());
    }

    #[test]
    fn settling_a_bash_zero_exit_is_done() {
        let mut step = Step::open("Bash", r#"{"command":"ls"}"#);
        step.settle("exit_code: 0\n--- stdout ---\nSECRET");
        assert_eq!(step.status, StepStatus::Done);
        assert_eq!(step.verdict, "exit_code: 0");
        assert!(!step.status.is_failure());
        assert!(step.status.is_settled());
    }

    #[test]
    fn settling_a_bash_nonzero_exit_is_failed() {
        let mut step = Step::open("Bash", r#"{"command":"false"}"#);
        step.settle("exit_code: 1\n--- stdout ---\nfoo");
        assert_eq!(step.status, StepStatus::Failed);
        assert!(step.status.is_failure());
        assert_eq!(step.verdict, "exit_code: 1");
    }

    #[test]
    fn settling_an_error_result_is_failed() {
        let mut step = Step::open("Read", r#"{"file_path":"/nope"}"#);
        step.settle("error: file not found");
        assert_eq!(step.status, StepStatus::Failed);
    }

    #[test]
    fn settle_is_idempotent_on_already_settled_steps() {
        let mut step = Step::open("Bash", r#"{"command":"ls"}"#);
        step.settle("exit_code: 0");
        step.settle("exit_code: 99");
        assert_eq!(step.status, StepStatus::Failed);
        assert_eq!(step.verdict, "exit_code: 99");
    }

    #[test]
    fn pushing_output_appends_split_on_newlines() {
        let mut step = Step::open("Bash", r#"{"command":"ls"}"#);
        step.push_output("Compiling foo\nwarning: unused\n");
        assert_eq!(step.children, vec!["Compiling foo", "warning: unused", ""]);
    }

    #[test]
    fn pushing_output_preserves_a_trailing_partial_line() {
        let mut step = Step::open("Bash", r#"{"command":"yes"}"#);
        step.push_output("y\ny");
        assert_eq!(step.children, vec!["y", "y"]);
    }

    #[test]
    fn deny_marks_a_step_denied_with_a_note() {
        let mut step = Step::open("Bash", r#"{"command":"rm -rf /"}"#);
        step.deny("denied by user");
        assert_eq!(step.status, StepStatus::Denied);
        assert_eq!(step.verdict, "denied by user");
        assert!(step.status.is_failure());
    }

    #[test]
    fn interrupt_marks_failed_with_a_short_note() {
        let mut step = Step::open("Bash", r#"{"command":"sleep 100"}"#);
        step.interrupt();
        assert_eq!(step.status, StepStatus::Failed);
        assert_eq!(step.verdict, "interrupted");
    }

    #[test]
    fn step_ids_are_unique() {
        let a = StepId::next();
        let b = StepId::next();
        assert_ne!(a, b);
    }

    #[test]
    fn header_omits_verdict_for_a_running_step() {
        let step = Step::open("Bash", r#"{"command":"ls"}"#);
        assert_eq!(step.header(), "Bash ls");
    }

    #[test]
    fn header_includes_verdict_for_a_settled_step() {
        let mut step = Step::open("Bash", r#"{"command":"ls"}"#);
        step.settle("exit_code: 0");
        assert_eq!(step.header(), "Bash ls · exit_code: 0");
    }

    #[test]
    fn header_for_a_failure_still_has_the_verb_and_subject() {
        let mut step = Step::open("Bash", r#"{"command":"false"}"#);
        step.settle("exit_code: 1");
        // The marker is on the gutter; the header just names the verb and
        // its verdict.
        assert_eq!(step.header(), "Bash false · exit_code: 1");
    }

    #[test]
    fn shows_children_for_running_or_failed_steps() {
        let mut step = Step::open("Bash", r#"{"command":"ls"}"#);
        step.push_output("line one\nline two\n");
        assert!(step.shows_children_when(false), "running shows children");
        assert!(
            step.shows_children_when(true),
            "running shows children in verbose too"
        );

        step.settle("exit_code: 0");
        assert!(
            !step.shows_children_when(false),
            "settled Done hides children in compact"
        );
        assert!(
            step.shows_children_when(true),
            "Ctrl-O re-exposes settled children"
        );

        let mut failed = Step::open("Bash", r#"{"command":"false"}"#);
        failed.push_output("nope\n");
        failed.settle("exit_code: 1");
        assert!(
            failed.shows_children_when(false),
            "failed auto-expands even in compact"
        );
        assert!(
            failed.shows_children_when(true),
            "failed auto-expands in verbose too"
        );
    }

    #[test]
    fn denied_never_shows_children() {
        // A denied call never ran, so it has no children to display in
        // any mode. Verbose is irrelevant to a Denied step.
        let mut step = Step::open("Bash", r#"{"command":"rm -rf /"}"#);
        step.push_output("would be bad");
        step.deny("user declined");
        assert!(!step.shows_children_when(false));
        assert!(!step.shows_children_when(true));
    }

    #[test]
    fn settled_done_with_empty_children_never_shows_them() {
        // Bash that produced no output: a Done step with empty children
        // has nothing to expand to even at verbose.
        let mut step = Step::open("Bash", r#"{"command":"true"}"#);
        step.settle("exit_code: 0");
        assert!(!step.shows_children_when(false));
        assert!(!step.shows_children_when(true));
    }

    #[test]
    fn spans_with_distinguishes_compact_and_verbose() {
        // A settled Bash with children: compact shows the verdict line
        // alone, verbose shows verdict + every child line.
        let mut step = Step::open("Bash", r#"{"command":"echo one two"}"#);
        step.push_output("alpha\nbeta\n");
        step.settle("exit_code: 0");
        let compact = step.spans_with(false);
        let verbose = step.spans_with(true);
        // Compact: one span, the verdict header. No children.
        assert_eq!(compact.len(), 1, "compact is just the header");
        assert!(!compact[0].text.contains("alpha"));
        assert!(!compact[0].text.contains("beta"));
        // Verbose: header + child 'alpha' + child 'beta'.
        assert!(verbose.len() > compact.len(), "verbose is longer");
        let joined: String = verbose.iter().map(|s| s.text.clone()).collect();
        assert!(joined.contains("alpha") && joined.contains("beta"));
    }

    #[test]
    fn spans_with_failed_always_shows_children_regardless_of_verbose() {
        // Auto-expansion is a separate rule from the Ctrl-O toggle; a
        // failure that has children shows them in both modes.
        let mut step = Step::open("Bash", r#"{"command":"false"}"#);
        step.push_output("boom\n");
        step.settle("exit_code: 1");
        let compact = step.spans_with(false);
        let verbose = step.spans_with(true);
        let compact_joined: String = compact.iter().map(|s| s.text.clone()).collect();
        let verbose_joined: String = verbose.iter().map(|s| s.text.clone()).collect();
        assert!(compact_joined.contains("boom"));
        assert!(verbose_joined.contains("boom"));
    }

    #[test]
    fn spans_with_edit_keeps_diff_in_both_modes() {
        // An Edit's diff is what the call is for; the verdict line
        // "replaced in X · now Y bytes" without the change would be a
        // lie. Both compact and verbose keep the diff lines visible.
        let mut step = Step::open(
            "Edit",
            r#"{"file_path":"a.rs","old_string":"one","new_string":"two"}"#,
        );
        step.settle("ok: replaced 1 occurrence; /tmp/a.rs is now 8 bytes");
        let compact = step.spans_with(false);
        let verbose = step.spans_with(true);
        let compact_joined: String = compact.iter().map(|s| s.text.clone()).collect();
        let verbose_joined: String = verbose.iter().map(|s| s.text.clone()).collect();
        assert!(
            compact_joined.contains("- one"),
            "compact keeps the removed line"
        );
        assert!(
            compact_joined.contains("+ two"),
            "compact keeps the added line"
        );
        assert!(verbose_joined.contains("- one"));
        assert!(verbose_joined.contains("+ two"));
    }

    #[test]
    fn shows_children_is_false_when_there_are_none() {
        let mut step = Step::open("Read", r#"{"file_path":"/a.rs"}"#);
        assert!(
            !step.shows_children_when(false),
            "running with no children has nothing to show"
        );
        step.settle("error: nope");
        assert!(
            !step.shows_children_when(false),
            "failed with no children still has nothing"
        );
        assert!(
            !step.shows_children_when(true),
            "verbose with no children still has nothing"
        );
    }
}

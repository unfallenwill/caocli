//! Machine core: state = a fold of the session log, decisions = pure functions,
//! UI/IO = stateless executors.
//!
//! Architecture invariants (the event vocabulary lands incrementally; whatever
//! has not landed is only recorded here, and no dead code is left behind):
//!
//! - **The machine only decides, it never executes**: [`next_action`] reads the
//!   history and yields an [`Action`]; the interpreter (`Agent::turn`) executes it
//!   and writes the result back into the log. The machine never awaits IO.
//! - **Callbacks receive notifications only, never return data**: `ui::Ui` is the
//!   machine's Notice channel. The three questions that *do* need an answer — the
//!   user's cancel, the gate's verdict and the question tool's answer — are
//!   answered through channels of their own (`ui::Cancel`, `ui::Approve`,
//!   `ui::Ask`) and read by the interpreter, which decides what to run, never what
//!   comes next: a denial or an answer reaches the decision function the way every
//!   other result does, as a tool result in the log.
//! - **State = a fold of the log**: `Session` is the only persistent state and can
//!   be rebuilt at any time via `Session::load`; a second source of truth is not
//!   allowed to exist in memory.
//!
//! | Vocabulary | Status | Members |
//! |---|---|---|
//! | Input | landed (implicit) | UserLine (the `turn` argument), Delta (SSE stream), ToolFinished (execute return value) |
//! | Command | partially landed | Cancel has landed (Ctrl-C during a turn; out-of-band, handled in the interpreter layer, never enters `next_action`); New / Resume / Exit have not |
//! | Notice | landed | the seven methods of `ui::Ui` |
//! | Effect | landed, in the interpreter | one per [`Action`], run by `agent::turn`: a sub-request, one tool call (the step budget, then the gate, then the tool — or, for the question tool, the user's answer instead of the tool), and the cleanup a cancelled turn owes the log. The interpreter's own vocabulary (`Step`, `Ran`, `Gate`) describes what it did, never what to do next — the decision stays here |

use std::collections::HashSet;

use crate::types::{Message, Role, ToolCall};

// ============================================================================
// History validity specification (executable form).
// "What the history sent to the backend must satisfy" is authoritatively
// defined here as of now:
//   is_request_valid = windows_complete ∧ no_stray_tools
// Shapes that violate it are rejected with a 400 by DeepSeek/GLM. Both `heal`
// and request construction target it as their invariant; the tests below pin
// down "every crash point heals into something valid" by exhaustively
// enumerating a bounded shape family.
// ============================================================================

/// For every assistant message carrying tool_calls: the tool result window that
/// follows matches the declarations one for one (equal count, identical id set).
pub fn windows_complete(messages: &[Message]) -> bool {
    for (i, m) in messages.iter().enumerate() {
        let Some(calls) = &m.tool_calls else { continue };
        let mut j = i + 1;
        while j < messages.len() && messages[j].role == Role::Tool {
            j += 1;
        }
        let window = &messages[i + 1..j];
        if window.len() != calls.len() {
            return false;
        }
        let declared: HashSet<&str> = calls.iter().map(|c| c.id.as_str()).collect();
        let answered: HashSet<&str> = window
            .iter()
            .filter_map(|t| t.tool_call_id.as_deref())
            .collect();
        if declared != answered {
            return false;
        }
    }
    true
}

/// Every tool result falls inside the result window of some assistant with
/// (non-empty) calls; there are no stray results.
pub fn no_stray_tools(messages: &[Message]) -> bool {
    let mut under_calls = false;
    for m in messages {
        match m.role {
            Role::Tool => {
                if !under_calls {
                    return false;
                }
            }
            _ => {
                under_calls = m.tool_calls.as_deref().is_some_and(|c| !c.is_empty());
            }
        }
    }
    true
}

/// Request history validity. Enforced by a debug tripwire where requests are
/// constructed; the invariant that `heal` targets.
pub fn is_request_valid(messages: &[Message]) -> bool {
    windows_complete(messages) && no_stray_tools(messages)
}

/// A text the interpreter or the load-time heal writes where a tool call has no
/// result of its own.
///
/// The texts are byte-for-byte constants because they are in the log, and the
/// log is the state: history is replayed as the prefix cache sees it, and a log
/// that violates the window specification (a call with no result in its window)
/// is a 400 from the backend. An enum rather than four loose strings: the set is
/// closed, and a caller that invents its own marker is a caller that can write
/// an invalid history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Marker {
    /// Synthesized at load time for a call that never executed — the process
    /// died between the declaration and its result. In memory only: the file is
    /// never rewritten.
    Interrupted,
    /// Persisted for a call the user cancelled (Ctrl-C). Unlike `Interrupted`
    /// this one really lands in the file: the process is still alive.
    Cancelled,
    /// Persisted for a call the approval gate denied. The model can adjust its
    /// plan after reading it.
    Denied,
    /// Persisted for the calls still open when the turn hit [`MAX_TOOL_STEPS`].
    StepLimit,
    /// Persisted for a question the user did not answer: they dismissed it, or
    /// there was nobody there to ask. The model can ask again, ask differently,
    /// or go on without an answer.
    Unanswered,
}

impl Marker {
    /// The text as it goes into the log. Byte for byte: a changed letter is a
    /// different history, and a different history is a full cache miss.
    pub fn text(self) -> &'static str {
        match self {
            Marker::Interrupted => "error: interrupted before execution; no result was recorded",
            Marker::Cancelled => "error: cancelled by user before a result was recorded",
            Marker::Denied => "error: the user declined this tool call",
            Marker::StepLimit => "error: tool step limit reached; turn aborted",
            Marker::Unanswered => "error: the user did not answer the question",
        }
    }
}

/// Per-turn tool step cap (every ExecTool action counts as one step, including
/// denied ones). A product-level termination guarantee: a model that goes
/// haywire in a loop can burn at most this much.
pub const MAX_TOOL_STEPS: usize = 500;

/// What the next beat should do. The machine's decision exit, obtained by
/// folding the log.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// History is ready (the tail is a user message, or the tool results are
    /// complete), so a new model sub-request should be issued.
    CallModel,
    /// The tool_calls declared by the trailing assistant still have unexecuted
    /// ones; execute that one (in declaration order).
    ExecTool(ToolCall),
    /// The trailing assistant has given its final answer; the turn is over.
    Done,
}

/// Decision function: read the history, produce the next beat. No IO, no clock,
/// table-driven and testable.
///
/// The multi-tool-call subtlety: when an assistant declares c1 and c2 at once,
/// the history grows one message at a time as
/// `Assistant(calls) → Tool(c1) → Tool(c2)`. Looking only at the trailing
/// message would misread "the tail is a Tool" as "the results are complete",
/// skip c2 and send the request (API 400). So we must locate the last assistant
/// message and compare all of its declarations against the run of results that
/// follows it.
pub fn next_action(messages: &[Message]) -> Option<Action> {
    let last = messages.last()?;
    match last.role {
        Role::User => Some(Action::CallModel),
        Role::Tool | Role::Assistant => {
            let ai = messages.iter().rposition(|m| m.role == Role::Assistant)?;
            let calls = messages[ai].tool_calls.as_deref().unwrap_or_default();
            if calls.is_empty() {
                // The trailing assistant has no calls → the answer is complete.
                // (A trailing Tool with no assistant carrying calls in front of
                // it is invalid history: consistent with "errors are text", we
                // stop instead of panicking so the user can see and handle it.)
                return Some(Action::Done);
            }
            let answered: HashSet<&str> = messages[ai + 1..]
                .iter()
                .filter(|m| m.role == Role::Tool)
                .filter_map(|m| m.tool_call_id.as_deref())
                .collect();
            match calls.iter().find(|c| !answered.contains(c.id.as_str())) {
                Some(call) => Some(Action::ExecTool(call.clone())),
                None => Some(Action::CallModel),
            }
        }
        Role::System => None,
    }
}

/// Ids of calls that were declared but not answered inside their result window
/// (in declaration order).
/// The basis for cancellation cleanup: these calls need a cancellation marker to
/// close the window, otherwise the next turn resurrects zombie calls (or
/// constructing the request gets a 400).
pub fn open_call_ids(messages: &[Message]) -> Vec<String> {
    let mut open = Vec::new();
    let mut i = 0;
    while i < messages.len() {
        let Some(calls) = &messages[i].tool_calls else {
            i += 1;
            continue;
        };
        let mut j = i + 1;
        while j < messages.len() && messages[j].role == Role::Tool {
            j += 1;
        }
        let answered: HashSet<&str> = messages[i + 1..j]
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        open.extend(
            calls
                .iter()
                .filter(|c| !answered.contains(c.id.as_str()))
                .map(|c| c.id.clone()),
        );
        i = j;
    }
    open
}

/// Crash healing: for an assistant that "declared tool_calls but whose results
/// are incomplete", pad in placeholder results so the history satisfies the API
/// constraint again (exactly one immediately following tool result per call).
/// This only modifies the in-memory view passed in and **does not write the
/// file** — the log is append-only and never rewritten; the next load
/// deterministically synthesizes the same content.
/// Returns the number of inserted messages.
pub fn heal(messages: &mut Vec<Message>) -> usize {
    let mut inserted = 0;
    let mut i = 0;
    while i < messages.len() {
        if messages[i].tool_calls.is_none() {
            i += 1;
            continue;
        }
        // Find the end of the run of tool results immediately following this
        // assistant message
        let mut j = i + 1;
        while j < messages.len() && messages[j].role == Role::Tool {
            j += 1;
        }
        let answered: HashSet<&str> = messages[i + 1..j]
            .iter()
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let missing: Vec<String> = messages[i]
            .tool_calls
            .as_deref()
            .unwrap_or_default()
            .iter()
            .filter(|c| !answered.contains(c.id.as_str()))
            .map(|c| c.id.clone())
            .collect();
        for (n, id) in missing.iter().enumerate() {
            // j is already a coordinate in the current vector: appending one by
            // one inside the same window only needs + n. A cumulative
            // insertion count carried across windows must not be mixed in —
            // that would send the second window out of bounds.
            messages.insert(j + n, Message::tool(id, Marker::Interrupted.text()));
        }
        inserted += missing.len();
        // Skip the whole window (including the placeholder results just inserted)
        i = j + missing.len();
    }
    inserted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ToolCall, ToolCallFunction};

    fn assistant(calls: Vec<ToolCall>) -> Message {
        Message {
            role: Role::Assistant,
            content: Some("".into()),
            reasoning_content: None,
            tool_calls: (!calls.is_empty()).then_some(calls),
            tool_call_id: None,
        }
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            r#type: "function".into(),
            function: ToolCallFunction {
                name: "Bash".into(),
                arguments: "{}".into(),
            },
        }
    }

    #[test]
    fn empty_history_has_no_action() {
        assert_eq!(next_action(&[]), None);
    }

    #[test]
    fn user_tail_requests_model() {
        let msgs = vec![Message::user("q")];
        assert_eq!(next_action(&msgs), Some(Action::CallModel));
    }

    #[test]
    fn plain_assistant_tail_ends_turn() {
        let mut m = assistant(vec![]);
        m.content = Some("answer".into());
        let msgs = vec![Message::user("q"), m];
        assert_eq!(next_action(&msgs), Some(Action::Done));
    }

    #[test]
    fn declared_calls_execute_in_declaration_order() {
        let msgs = vec![Message::user("q"), assistant(vec![call("a"), call("b")])];
        assert_eq!(
            next_action(&msgs),
            Some(Action::ExecTool(call("a"))),
            "the earlier declared call runs first"
        );
    }

    #[test]
    fn partial_results_run_remaining_call_before_requesting() {
        // Key regression: after Assistant(a,b) → Tool(a) we must keep executing
        // b rather than sending a request with a history that is missing b's
        // result (API 400).
        let msgs = vec![
            Message::user("q"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
        ];
        assert_eq!(
            next_action(&msgs),
            Some(Action::ExecTool(call("b"))),
            "b has no result on disk yet, so it must run first"
        );
    }

    #[test]
    fn complete_results_request_model() {
        let msgs = vec![
            Message::user("q"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
            Message::tool("b", "ok"),
        ];
        assert_eq!(next_action(&msgs), Some(Action::CallModel));
    }

    #[test]
    fn healthy_history_is_not_healed() {
        let mut msgs = vec![
            Message::user("q"),
            assistant(vec![call("a")]),
            Message::tool("a", "ok"),
            Message::user("again"),
        ];
        assert_eq!(heal(&mut msgs), 0);
        assert_eq!(msgs.len(), 4);
    }

    /// The markers are in the log, and history is replayed byte for byte: a
    /// changed letter is a different history and voids the prefix cache of every
    /// session that ran before it. Pinned so that rewording one is a decision
    /// rather than an accident.
    #[test]
    fn marker_texts_are_frozen() {
        assert_eq!(
            Marker::Interrupted.text(),
            "error: interrupted before execution; no result was recorded"
        );
        assert_eq!(
            Marker::Cancelled.text(),
            "error: cancelled by user before a result was recorded"
        );
        assert_eq!(
            Marker::Denied.text(),
            "error: the user declined this tool call"
        );
        assert_eq!(
            Marker::StepLimit.text(),
            "error: tool step limit reached; turn aborted"
        );
        assert_eq!(
            Marker::Unanswered.text(),
            "error: the user did not answer the question"
        );
    }

    #[test]
    fn orphan_calls_get_synthetic_results() {
        // Crash site: both calls were cut off before executing
        let mut msgs = vec![Message::user("q"), assistant(vec![call("a"), call("b")])];
        assert_eq!(heal(&mut msgs), 2);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("a"));
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("b"));
        assert_eq!(
            msgs[2].text().as_deref(),
            Some(Marker::Interrupted.text()),
            "the synthesized text must be byte-for-byte deterministic (prefix cache depends on it)"
        );
    }

    #[test]
    fn partial_run_is_completed_in_place() {
        // Crash site: c1 is on disk, c2 was lost → c2's placeholder goes at the
        // end of the result run
        let mut msgs = vec![
            Message::user("q"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
            Message::user("next"),
        ];
        assert_eq!(heal(&mut msgs), 1);
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("a"));
        assert_eq!(
            msgs[3].tool_call_id.as_deref(),
            Some("b"),
            "appended after the existing results"
        );
        assert_eq!(msgs[4].role, Role::User, "existing messages are not moved");
    }

    /// Exhaustive argument for the transition function: after an assistant
    /// declares n calls, the run of results that follows is one of 2^n subsets.
    /// Fully enumerate n ≤ 3 and check that each case decides exactly "the first
    /// unexecuted call in declaration order", switching to CallModel only once
    /// all of them have run.
    /// This is a completeness check over the reachable abstract states, not
    /// sampled coverage.
    #[test]
    fn exhaustive_answer_subsets_yield_first_unanswered_call() {
        for n in 1..=3usize {
            let ids: Vec<String> = (0..n).map(|i| format!("c{i}")).collect();
            let calls: Vec<ToolCall> = ids.iter().map(|id| call(id)).collect();
            for mask in 0..(1u32 << n) {
                let mut msgs = vec![Message::user("q"), assistant(calls.clone())];
                for (i, id) in ids.iter().enumerate() {
                    if mask & (1 << i) != 0 {
                        msgs.push(Message::tool(id, "ok"));
                    }
                }
                let expected = match (0..n).find(|&i| mask & (1 << i) == 0) {
                    Some(i) => Action::ExecTool(call(&ids[i])),
                    None => Action::CallModel,
                };
                assert_eq!(
                    next_action(&msgs),
                    Some(expected),
                    "n={n}, persisted-result mask={mask:#b}"
                );
            }
        }
    }

    /// heal is idempotent: healing an already-healed history must insert nothing
    /// and change nothing.
    /// This is a necessary condition for "the same log yields the same view on
    /// every load" (prefix cache depends on it).
    #[test]
    fn heal_is_idempotent() {
        let mut msgs = vec![
            Message::user("q"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
            Message::user("next"),
        ];
        assert_eq!(heal(&mut msgs), 1);
        let once = msgs.clone();
        assert_eq!(heal(&mut msgs), 0, "a second heal must not insert again");
        assert_eq!(msgs, once, "a second heal must not change any message");
    }

    /// Directed cases for the executable specification itself: accept healthy
    /// shapes, reject the shapes known to produce a 400.
    #[test]
    fn spec_predicate_directed_cases() {
        let ok = |msgs: &[Message]| assert!(is_request_valid(msgs), "should be valid: {msgs:?}");
        let bad =
            |msgs: &[Message]| assert!(!is_request_valid(msgs), "should be invalid: {msgs:?}");

        ok(&[]);
        ok(&[
            Message::user("q"),
            assistant(vec![]),
            Message::user("again"),
        ]);
        ok(&[
            Message::user("q"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "1"),
            Message::tool("b", "2"),
        ]);
        ok(&[
            assistant(vec![call("a")]),
            Message::tool("a", "1"),
            Message::user("next round"),
            assistant(vec![call("b")]),
            Message::tool("b", "2"),
        ]);
        // orphan declaration (crash shape): the result is missing
        bad(&[Message::user("q"), assistant(vec![call("a")])]);
        // stray result: no assistant carrying calls in front of it
        bad(&[Message::user("q"), Message::tool("a", "1")]);
        bad(&[assistant(vec![]), Message::tool("a", "1")]);
        // duplicate result
        bad(&[
            assistant(vec![call("a")]),
            Message::tool("a", "1"),
            Message::tool("a", "2"),
        ]);
        // id does not match
        bad(&[assistant(vec![call("a")]), Message::tool("b", "1")]);
        // the window was already closed by a non-tool message, so the result is late
        bad(&[
            assistant(vec![call("a")]),
            Message::user("x"),
            Message::tool("a", "1"),
        ]);
    }

    /// Bounded exhaustive family: all 7^0+…+7^5 = 19608 sequences of length ≤ 5
    /// over a 7-symbol message alphabet (User; Assistant ∅/{a}/{b}/{a,b};
    /// Tool(a)/Tool(b)).
    fn bounded_family() -> Vec<Vec<Message>> {
        fn kind(d: u32) -> Message {
            match d {
                0 => Message::user("q"),
                1 => assistant(vec![]),
                2 => assistant(vec![call("a")]),
                3 => assistant(vec![call("b")]),
                4 => assistant(vec![call("a"), call("b")]),
                5 => Message::tool("a", "ok"),
                _ => Message::tool("b", "ok"),
            }
        }
        let mut out = Vec::new();
        for len in 0..=5u32 {
            for code in 0..7u32.pow(len) {
                let mut seq = Vec::with_capacity(len as usize);
                let mut c = code;
                for _ in 0..len {
                    seq.push(kind(c % 7));
                    c /= 7;
                }
                out.push(seq);
            }
        }
        out
    }

    /// Theorem A (bounded exhaustive · all 19608 shapes): after heal, every
    /// declared call is answered inside its result window
    /// (declared ⊆ answered).
    /// Note that heal only pads missing results and never de-duplicates: full
    /// validity belongs to theorem B's crash-reachable family — that boundary
    /// is itself part of the specification.
    #[test]
    fn theorem_a_heal_answers_every_declared_call_exhaustive() {
        for mut seq in bounded_family() {
            heal(&mut seq);
            for (i, m) in seq.iter().enumerate() {
                let Some(calls) = &m.tool_calls else { continue };
                let mut j = i + 1;
                while j < seq.len() && seq[j].role == Role::Tool {
                    j += 1;
                }
                let answered: HashSet<&str> = seq[i + 1..j]
                    .iter()
                    .filter_map(|t| t.tool_call_id.as_deref())
                    .collect();
                for c in calls {
                    assert!(
                        answered.contains(c.id.as_str()),
                        "an unanswered call {c:?} survived heal; shape: {seq:?}"
                    );
                }
            }
        }
    }

    /// Theorem B (bounded exhaustive · crash-reachable family): every prefix of
    /// a valid history = every crash point the process can stop at. All prefixes
    /// must satisfy the executable specification after heal.
    /// This upgrades the manual enumeration of "each of the six crash points has
    /// a recovery path" into a mechanical check.
    #[test]
    fn theorem_b_every_crash_prefix_of_valid_history_recovers() {
        let mut checked = 0usize;
        for seq in bounded_family() {
            if !is_request_valid(&seq) {
                continue;
            }
            for cut in 0..=seq.len() {
                let mut prefix = seq[..cut].to_vec();
                heal(&mut prefix);
                assert!(
                    is_request_valid(&prefix),
                    "cut point {cut}/{} still invalid after healing: {prefix:?} (original {seq:?})",
                    seq.len()
                );
                checked += 1;
            }
        }
        assert!(
            checked > 1000,
            "the exhaustive family shrank unexpectedly: only {checked} prefixes"
        );
    }

    /// Regression: when several windows are missing results at once, the
    /// insertion coordinate must stay local to each window.
    /// This shape used to make heal panic out of bounds (insertion index out of
    /// bounds) — first exposed by theorem A's exhaustive sweep.
    #[test]
    fn heal_two_deficient_windows_inserts_locally() {
        let mut msgs = vec![
            assistant(vec![call("a")]),
            assistant(vec![call("b")]),
            Message::tool("b", "ok"),
        ];
        assert_eq!(heal(&mut msgs), 1);
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[1].tool_call_id.as_deref(), Some("a"));
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("b"));
        assert!(is_request_valid(&msgs));
    }

    #[test]
    fn open_call_ids_reports_unanswered_declarations() {
        assert!(open_call_ids(&[Message::user("q")]).is_empty());
        let two = vec![Message::user("q"), assistant(vec![call("a"), call("b")])];
        assert_eq!(open_call_ids(&two), vec!["a", "b"]);
        let partial = vec![
            Message::user("q"),
            assistant(vec![call("a"), call("b")]),
            Message::tool("a", "ok"),
        ];
        assert_eq!(open_call_ids(&partial), vec!["b"]);
        let mut closed = partial;
        closed.push(Message::tool("b", "ok"));
        assert!(open_call_ids(&closed).is_empty());
    }

    #[test]
    fn healed_history_yields_callmodel() {
        let mut msgs = vec![Message::user("q"), assistant(vec![call("a")])];
        heal(&mut msgs);
        assert_eq!(next_action(&msgs), Some(Action::CallModel));
    }
}

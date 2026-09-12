//! What a tool call shows: the argument summary, the change it makes, and what
//! its result leaves behind.
//!
//! These are the only pieces of a tool call that are derived from the wire
//! format the model sent -- the name and the call itself are already in the
//! arguments. Everything in here is *projection*, not interpretation: a call
//! whose arguments cannot be parsed is still a call, just one with nothing to
//! show.
//!
//! Kept out of [`Cell`](super::Cell) on purpose: a
//! [`Cell::ToolCall`](super::Cell::ToolCall) is a value, and these helpers are
//! how the cell is built from the raw arguments. The argument vocabulary
//! belongs to the tools, not to the cell model, and having it here means the
//! cell never has to know what shape a Bash call's JSON is.
//!
//! Three files, because the three answers are independent: [`hint`] is what the
//! call asked for, [`diff_lines`] is what it changes, and [`result_summary`] is
//! what came back. They share the arguments they parse and nothing else, which
//! is why the modules are named for the answer rather than for the function that
//! gives it: a re-exported name never has to share a word with its module.

mod args;
mod diff;
mod result;

pub(super) use args::hint;
pub(super) use diff::diff_lines;
pub(super) use result::result_summary;

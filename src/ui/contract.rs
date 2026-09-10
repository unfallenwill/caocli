//! The channels between the machine and a front end, and nothing else.
//!
//! [`Ui`] is the vocabulary the machine notifies with; [`Interrupt`] and
//! [`Approve`] are the two it asks its questions on; [`Front`] is what handling a
//! submitted line asks of whichever front end is running. Keeping all of them
//! apart from any implementation is what lets a front end be written against them
//! alone -- the plain renderer, the one that owns the screen, or a double in a
//! test -- without depending on how any of them draws.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use crate::types::{Message, ToolCall, Usage};

/// The machine → UI notification vocabulary (the Notice channel).
/// Discipline: notifications only, never returning data back; implementations
/// must not block, and a failed terminal write counts as fatal.
/// The machine (agent) depends only on this trait, not on a concrete renderer.
pub trait Ui {
    /// A thinking fragment (arrives before the body text).
    fn reasoning_delta(&mut self, s: &str);
    /// A body-text fragment.
    fn content_delta(&mut self, s: &str);
    /// One streaming round is over; blocks are reset.
    fn finish_turn(&mut self);
    /// A tool is starting: echo the tool name and an argument summary.
    fn tool_start(&mut self, name: &str, args: &str);
    /// A tool result summary.
    fn tool_result(&mut self, result: &str);
    /// Token usage for a sub-request, with the wall time the stream took (also
    /// accumulates session-level cache stats). The duration is what a
    /// tokens-per-second figure is computed from; a front end that shows none
    /// ignores it.
    fn usage(&mut self, usage: &Usage, stream: Duration);
    /// The turn was cancelled by the user (Ctrl-C): close the streaming block and
    /// print an interruption notice.
    fn interrupted(&mut self);
    /// Approval gate question: echo the tool and an argument summary and prompt
    /// y/N (the answer is read through the Input channel, not through this trait —
    /// a Notice never returns data).
    fn approval_requested(&mut self, name: &str, args: &str);
}

/// What a front end offers the *application*, as opposed to [`Ui`], which is what
/// the *machine* sends.
///
/// The two are kept apart on purpose: `Ui` is the machine's notification
/// vocabulary and stays that, while this is the handful of operations the
/// submitted-line handling needs from whichever front end is running -- the
/// plain renderer, or the interactive one that owns the terminal.
pub trait Front: Ui {
    /// Draw a resumed session's history.
    fn replay(&mut self, messages: &[Message]);
    /// A dim informational line.
    fn info(&mut self, s: &str);
    /// A failure. The plain front end writes it to the error stream so it
    /// survives a redirected stdout; an interactive one has to place it in its
    /// own output, which is why this takes `&mut self`.
    fn error(&mut self, s: &str);
    /// Adopt the model id the status line reports.
    fn set_model(&mut self, model: &str);
    /// Adopt the reasoning effort tier the status line reports.
    fn set_effort(&mut self, effort: &str);
    /// Clear the session's cache statistics.
    fn reset_stats(&mut self);
    /// Ask for a secret: `prompt` says what it is for, and the answer is the
    /// text typed in reply. `None` means nothing was entered — a cancellation,
    /// an end of input, or a front end that has gone away.
    ///
    /// A question rather than a line to submit, because an answer that reached
    /// the session log or the transcript would no longer be a secret. The
    /// interactive front end draws the prompt and takes the answer in its box
    /// with the text hidden; the plain one writes the prompt and reads a line
    /// from its own input with the terminal's echo off.
    fn ask_secret(&mut self, prompt: &str) -> Pin<Box<dyn Future<Output = Option<String>> + '_>>;
}

// ------------------------------------------------- the machine's questions ---

// The two channels the machine *asks* on, as opposed to the one it notifies on
// (`Ui`). Both are traits rather than closures so that the answer can be taken
// from wherever the front end already is -- an event loop, a blocking read, a
// test -- and so the output lifetime is bound to `&mut self`: the `Fn` family
// cannot express "the return value borrows the receiver".

/// Out-of-band cancellation (Ctrl-C) source: one long-lived listener is held for
/// the whole turn and lends out a droppable wait future on demand. Waiting is
/// cancel-safe: dropping the future does not lose the signal (the state lives in
/// the listener), and an unconsumed signal makes the next `wait()` ready
/// immediately.
pub trait Interrupt {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>>;
}

/// The approval gate's answer source: the Input channel that pairs with the
/// Notice a `Ui` sends when it asks. A Notice never returns data, so the answer
/// comes back through a channel of its own -- this is that channel.
///
/// Like `Interrupt` it is a trait rather than a closure so that a front end
/// owning the terminal can take the answer from its own event loop, and so the
/// output lifetime can be bound to `&mut self`.
pub trait Approve {
    fn ask(&mut self, call: &ToolCall) -> Pin<Box<dyn Future<Output = bool> + '_>>;
}

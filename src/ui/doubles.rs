//! Test doubles for the two channels a front end answers on: the cancel source
//! and the approval gate.
//!
//! One fixture rather than a copy per test module: "a front end that never
//! cancels" is the same three lines however often it is written, and a copy per
//! module is a copy to keep in step with the contract every time it changes.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;

use crate::types::ToolCall;

use super::contract::{Approve, Cancel, Verdict};

/// A front end that never cancels: for tests that are not about cancellation.
pub(crate) struct NoCancel;

impl Cancel for NoCancel {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(std::future::pending())
    }
}

/// A front end whose cancel has already fired: the first wait returns at once.
/// An effect that is ready in the same moment still wins — the turn's race polls
/// the effect first — which is what makes a cancel arriving together with an
/// answer keep the answer.
pub(crate) struct CancelNow;

impl Cancel for CancelNow {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async {})
    }
}

/// A front end that cancels at chosen wait points.
///
/// Points are numbered from one, in the order the turn reaches them: a turn's
/// first wait is the sub-request's stream, the next is the first tool call, and
/// so on. Every other wait stays pending, so every stage the points do not name
/// runs to completion. A point that has already gone by fires at the next wait,
/// the way a signal that arrived while nothing was listening would.
pub(crate) struct CancelAt {
    points: VecDeque<usize>,
    waits: usize,
}

impl CancelAt {
    /// Cancel at the given wait points (1-based, ascending).
    pub(crate) fn on(points: &[usize]) -> Self {
        Self {
            points: points.iter().copied().collect(),
            waits: 0,
        }
    }
}

impl Cancel for CancelAt {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        self.waits += 1;
        let fired = self
            .points
            .front()
            .is_some_and(|point| *point <= self.waits);
        if fired {
            self.points.pop_front();
            Box::pin(async {})
        } else {
            Box::pin(std::future::pending())
        }
    }
}

/// A front end that answers the gate the same way every time.
pub(crate) struct Answer(Verdict);

impl Answer {
    /// Run the call.
    pub(crate) fn allows() -> Self {
        Self(Verdict::Allowed)
    }

    /// Do not run it.
    pub(crate) fn denies() -> Self {
        Self(Verdict::Denied)
    }
}

impl Approve for Answer {
    fn approve(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = Verdict> + '_>> {
        let verdict = self.0;
        Box::pin(async move { verdict })
    }
}

/// A front end that never answers, which leaves the gate open so that a cancel
/// has something to race against.
pub(crate) struct NoAnswer;

impl Approve for NoAnswer {
    fn approve(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = Verdict> + '_>> {
        Box::pin(std::future::pending())
    }
}

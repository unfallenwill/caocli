//! The turn queue: lines submitted while another turn was running.
//!
//! One in, one out per turn: a head runs when the turn in flight ends,
//! interrupted or not; a tail is appended by Enter while the loop is in its
//! working state. The queue is part of the turn, not the edit, because what
//! owns it is the lifetime of the work in flight, and the next head depends on
//! whether the previous turn finished, was interrupted, or never started.
//!
//! Lives apart from `working.rs` because the queue is data, while `working.rs`
//! is the key-routing for the duration of a turn. Two files, one owner:
//! `State` carries the field and the two halves reach for it.

use super::state::State;

impl State {
    /// Put a line after the running turn. It is the whole of what Enter does
    /// during a turn: a session written as if this turn had ended would record two
    /// answers at once.
    pub(super) fn enqueue(&mut self, line: String) {
        self.revision += 1;
        self.turn.queued.push_back(line);
    }

    /// Take the head of the queue: the line to run next, if there is one.
    pub(super) fn dequeue(&mut self) -> Option<String> {
        let next = self.turn.queued.pop_front();
        if next.is_some() {
            self.revision += 1;
        }
        next
    }
}

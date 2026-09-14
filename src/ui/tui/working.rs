//! The input box while a turn is running.
//!
//! The loop is in a different state then: the box is for the next message
//! rather than for the one being written, Enter queues rather than submits,
//! and Ctrl-C cancels the turn in flight instead of clearing the draft. The
//! question panel, when one is up, takes the keys that choose rather than
//! passing them to the editor.
//!
//! Split from the idle-time [`super::input`] because the meaning of every
//! key changes when the loop is no longer waiting at the prompt. The queue
//! itself lives one module over (see [`super::queue`]): this is the routing
//! for the keys, that is the data the routing produces.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use tokio::sync::watch;

use super::input::Submitted;
use super::state::State;

impl State {
    /// Handle a key while a turn is running.
    ///
    /// Three things take typing here: the cancel key, always; the answer to an
    /// approval question, while the gate is waiting for one; and otherwise the
    /// next line, which Enter puts in the queue rather than running now. A turn is
    /// not the place to *start* anything -- the one in flight is what the user is
    /// watching -- but it is exactly the place to say what should follow it, which
    /// is what the queue is for: it runs from its head when the turn ends, whether
    /// it ended by finishing or by being interrupted.
    pub(super) fn key_while_working(&mut self, event: Event, cancel: &watch::Sender<bool>) {
        // A resize arrives here too, and it changes the layout, so anything
        // arriving at all is reason enough to redraw -- and a draw that was not
        // needed costs one comparison.
        self.revision += 1;
        // The wheel is the one thing a turn does not have to be told about:
        // reading back is what there is to do while the model writes, and a notch
        // is not a line being composed. Answered before the gate, which is shut for
        // all but the moment it asks its question.
        if let Event::Mouse(mouse) = &event {
            self.wheel(mouse.kind);
            return;
        }
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('c')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let _ = cancel.send(true);
            return;
        }
        // Ctrl-O toggles the latest thought block's expanded view. The
        // active block can be expanded and collapsed; a historical
        // block that was expanded at the moment it became historical
        // can be collapsed back to its header; a historical block
        // that was folded stays folded -- it is not the "latest"
        // subject of the widget, and there is nothing to expand it
        // to.
        if let Event::Key(key) = &event
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('o')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.toggle_latest_thought();
            return;
        }
        // The panel is up: it takes the keys that choose, and the box takes what
        // is typed into it. A line is not queued while a question is waiting --
        // the box belongs to the answer, the same rule the gate keeps.
        if self.panel_open() {
            self.panel_key(event);
            return;
        }
        // No question is open, so the box is free for the next line. Enter queues
        // it, which is the whole point of typing here. One key is still dropped --
        // Ctrl-D, which leaves the session -- because a turn in flight is not the
        // place to leave from either.
        if self.overlay.reply.is_none() {
            if let Submitted::Line = self.key(event) {
                let line = self.take_line();
                self.enqueue(line);
            }
            return;
        }
        // A question is open: the box is where the answer goes and nowhere else,
        // so what is typed is the answer and Enter gives it. Enter submits it
        // whether or not anything was typed: a blank line denies, or cancels a
        // secret, which is the rule the plain front end reads from stdin.
        let answering = matches!(
            &event,
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.code == KeyCode::Enter
                    && key.modifiers == KeyModifiers::NONE
        );
        if answering {
            self.close_question();
        } else {
            // Backspace, a paste, a letter: all of it is the answer being typed.
            self.key(event);
        }
    }
}

//! The input box while a turn is running.
//!
//! The loop is in a different state then: the box is for the next message
//! rather than for the one being written, Enter queues rather than submits,
//! and Ctrl-C cancels the turn in flight instead of clearing the draft. The
//! question panel, when one is up, takes the keys that choose rather than
//! passing them to the editor.
//!
//! Split from the idle-time [`super::input`] because the meaning of every
//! key changes when the loop is no longer waiting at the prompt, and the
//! queue that holds lines submitted during a turn is a working-time
//! concern: a head runs when the turn ends, a tail is appended by Enter.

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
        if self.reply.is_none() {
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

    /// Handle a key while the question panel is up.
    ///
    /// The panel takes the keys that choose: the arrows and the digits move the
    /// cursor, space toggles an option for a question that takes several, Enter
    /// confirms and Esc dismisses the whole call. Everything else is typed into
    /// the box, which is where an answer in the user's own words goes -- so a
    /// digit or a space is only the panel's while the box is still empty. Once the
    /// user is typing, the keyboard is theirs, and a space is a space.
    pub(super) fn panel_key(&mut self, event: Event) {
        let Event::Key(key) = event else {
            // A paste is an answer typed the fast way.
            self.key(event);
            return;
        };
        if key.kind != KeyEventKind::Press {
            return;
        }
        if key.code == KeyCode::Up {
            self.panel_move(-1);
            return;
        }
        if key.code == KeyCode::Down {
            self.panel_move(1);
            return;
        }
        if let KeyCode::Char(digit @ '1'..='9') = key.code
            && self.text().is_empty()
        {
            self.panel_jump(digit.to_digit(10).unwrap_or(1) as usize);
            return;
        }
        if key.code == KeyCode::Char(' ') && self.text().is_empty() && self.panel_takes_many() {
            self.panel_toggle();
            return;
        }
        if crate::ui::tui::input::matches(key, KeyCode::Enter, KeyModifiers::NONE) {
            let typed = self.text();
            self.panel_confirm(&typed);
            return;
        }
        if key.code == KeyCode::Esc {
            self.panel_dismiss();
            return;
        }
        self.key(Event::Key(key));
    }

    /// Put a line after the running turn. It is the whole of what Enter does
    /// during a turn: a session written as if this turn had ended would record two
    /// answers at once.
    pub(super) fn enqueue(&mut self, line: String) {
        self.revision += 1;
        self.queued.push_back(line);
    }

    /// Take the head of the queue: the line to run next, if there is one.
    pub(super) fn dequeue(&mut self) -> Option<String> {
        let next = self.queued.pop_front();
        if next.is_some() {
            self.revision += 1;
        }
        next
    }
}

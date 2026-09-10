//! The input box: the keys it takes, the line it yields, and everything that
//! stands between the user and the machine - the queue behind a running turn,
//! the history to recall, and the questions the box answers in the machine's
//! stead.
//!
//! Key handling decides over the state and says what happened as a
//! [`Submitted`]; the loop acts on the value rather than on the keystroke.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::style::Style as RStyle;
use ratatui::text::Line;
use ratatui_textarea::{CursorMove, TextArea};
use tokio::sync::{oneshot, watch};

use super::picker::Choosing;
use crate::history;
use crate::ui::Verdict;
use crate::ui::cell::{Cell, Span, Style};
use crate::ui::tui::layout::box_rows;
use crate::ui::tui::paint::measure;
use crate::ui::tui::paint::{more_line, wrapped_lines};
use crate::ui::tui::state::State;

/// How many rows of the queue are drawn above the input box. The queue is what
/// was asked for while a turn ran, and a long one costs the transcript rows it is
/// capped at: what is worth seeing is that the line arrived.
pub(super) const QUEUE_ROWS: usize = 3;

/// What the box says while a turn runs: the line being typed is not this turn's
/// message, it is the one to run when this turn ends -- and the turn itself can
/// be stopped, which is worth saying, since a key that stops work is no good to
/// a reader who cannot find it.
pub(super) const QUEUE_PLACEHOLDER: &str = "the turn is running · Enter queues · Ctrl-C stops";

/// What the box says while nothing runs.
///
/// Neither it nor the queue line carries the box's marker: the marker is the
/// box's own, drawn in the column before this text whatever the text says, so a
/// placeholder cannot displace it and typing cannot take it away.
pub(super) const IDLE_PLACEHOLDER: &str = "type a message · /help for commands";

/// What the box says while the approval gate is open. The box is where the answer
/// goes, so it says so rather than inviting the next message.
pub(super) const ANSWER_PLACEHOLDER: &str = "y to allow · anything else denies";

/// What the box says while a secret is being asked for. The text is not shown --
/// the box masks it -- so the line has to say what is expected of it.
pub(super) const SECRET_PLACEHOLDER: &str = "type or paste it · Enter saves · empty cancels";

/// What a secret's characters are drawn as. A character and not a blank: the
/// length of a key is not the key, but a box that shows nothing at all looks
/// like a box that is not taking anything.
pub(super) const SECRET_MASK: char = '•';

/// A question the box is waiting on, and who to give the answer to.
///
/// The box is the one place an answer is typed, so the two kinds share it and
/// are told apart by what they do with what was typed.
pub(super) enum Answer {
    /// The approval gate: a line starting with `y` allows and anything else
    /// denies, which is the rule the plain front end applies to a line of stdin.
    YesNo(oneshot::Sender<Verdict>),
    /// A secret (an API key): whatever was typed, with the text hidden while it
    /// is typed, and an empty line for a cancellation. Nothing of it is echoed
    /// into the transcript, and nothing of it is remembered.
    Secret(oneshot::Sender<Option<String>>),
}

/// The input box's editor. Enter submits and Ctrl-J inserts a newline, matching
/// the plain prompt's keys.
///
/// It carries no block of its own: the rules are drawn by the front end, so that
/// the draft can start past the marker while the rules still run the width of the
/// screen.
///
/// Its cursor cell is left plain. The widget draws one of its own by reversing
/// whatever is under it, and the terminal's own cursor is put on that same cell by
/// `place_cursor` -- two carets on one cell, and the visible one is the terminal's:
/// it is the one that blinks, that a bar-shaped cursor can be told apart in, and
/// that says where a typed character will land. So the cell is drawn as what it
/// holds and the cursor is left to the terminal.
///
/// A placeholder is drawn one column right of where the draft starts, and that is the
/// editor's and not this: an empty box has no cursor cell of its own to draw, so the
/// widget puts one at the head of the placeholder's first line, and the hint follows
/// it. The hint is not the draft -- nothing is being typed while it is up -- so the
/// column it starts in is not one anything can be compared against.
pub(super) fn input_box() -> TextArea<'static> {
    let mut textarea = TextArea::default();
    textarea.set_placeholder_text(IDLE_PLACEHOLDER);
    textarea.set_cursor_line_style(RStyle::new());
    textarea.set_cursor_style(RStyle::new());
    textarea
}

/// What a key did while the input box had focus.
pub(super) enum Submitted {
    /// The box holds a line to run.
    Line,
    /// The user asked to leave.
    Exit,
    /// Nothing to run; keep waiting.
    Nothing,
}

impl State {
    /// Handle a key at the prompt.
    pub(super) fn key(&mut self, event: Event) -> Submitted {
        self.revision += 1;
        let key = match event {
            Event::Key(key) => key,
            // The wheel is the reader's, not the box's: nothing that is typed
            // here is what a notch moves.
            Event::Mouse(mouse) => {
                self.wheel(mouse.kind);
                return Submitted::Nothing;
            }
            // A paste goes to the box; the caller redraws either way.
            Event::Paste(text) => {
                self.textarea.insert_str(text);
                return Submitted::Nothing;
            }
            _ => return Submitted::Nothing,
        };
        if key.kind != KeyEventKind::Press {
            return Submitted::Nothing;
        }
        match key {
            // Enter submits. Ctrl-J is deliberately left to the box, which
            // inserts a newline, so multi-line input works as it does at the
            // plain prompt.
            KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::NONE,
                ..
            } => {
                // A list of sessions, providers or models is a list of things to
                // do rather than a name being typed, so Enter takes the
                // highlighted row and submits the line it stands for.
                if self.choose() {
                    return Submitted::Line;
                }
                if self.textarea.is_empty() {
                    Submitted::Nothing
                } else {
                    Submitted::Line
                }
            }
            // At the prompt Ctrl-C clears the line, as it does in the plain front
            // end: there is no turn to cancel here.
            KeyEvent {
                code: KeyCode::Char('c'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } => {
                self.textarea = input_box();
                Submitted::Nothing
            }
            // Ctrl-J is the plain prompt's newline key, and Shift-Enter is what
            // everyone tries first. Both are taken here because the box binds
            // Ctrl-J to delete-to-line-start, which is not what this front end
            // promises.
            KeyEvent {
                code: KeyCode::Char('j'),
                modifiers: KeyModifiers::CONTROL,
                ..
            }
            | KeyEvent {
                code: KeyCode::Enter,
                modifiers: KeyModifiers::SHIFT,
                ..
            } => {
                self.textarea.insert_newline();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Char('d'),
                modifiers: KeyModifiers::CONTROL,
                ..
            } if self.textarea.is_empty() => Submitted::Exit,
            // Paging through the transcript. The box scrolls itself with the same
            // two keys, so a draft with more lines than it has rows keeps them --
            // which is the only case where the box has anything to page.
            KeyEvent {
                code: KeyCode::PageUp,
                modifiers: KeyModifiers::NONE,
                ..
            } if !self.text().contains('\n') => {
                self.page(-1);
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::PageDown,
                modifiers: KeyModifiers::NONE,
                ..
            } if !self.text().contains('\n') => {
                self.page(1);
                Submitted::Nothing
            }
            // Up and Down mean the picker while it is open and the history
            // otherwise: the picker is only open while a command is being named,
            // so the two never compete for the same keystroke.
            KeyEvent {
                code: KeyCode::Up, ..
            } => {
                self.up();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Down,
                ..
            } => {
                self.down();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Tab, ..
            } => {
                // Completing a command leaves it in the box; choosing a row is
                // the whole action, so it submits.
                if self.choose() {
                    return Submitted::Line;
                }
                self.complete();
                Submitted::Nothing
            }
            KeyEvent {
                code: KeyCode::Esc, ..
            } => {
                // Dismissing the command picker takes its line with it: the line
                // is the query the list was filtered by, and a lone `/` left
                // behind glues itself to the next word, which then goes out as
                // an unknown command instead of as a message. A row list was
                // asked for in full by a submitted command, so what the box
                // holds is whatever has been typed since it opened -- a draft,
                // which dismissing has no claim on.
                if self
                    .picker
                    .take()
                    .is_some_and(|p| p.kind == Choosing::Command)
                {
                    self.textarea = input_box();
                    self.refresh_placeholder();
                }
                Submitted::Nothing
            }
            _ => {
                self.textarea.input(Event::Key(key));
                self.refresh_picker();
                Submitted::Nothing
            }
        }
    }

    /// What is in the box.
    pub(super) fn text(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// How many rows the box needs on a terminal `height` rows tall: one per line
    /// of the draft, so that Ctrl-J -- the key that makes the draft multi-line --
    /// makes room for the line it adds.
    pub(super) fn input_rows(&self, height: u16) -> u16 {
        box_rows(self.textarea.lines().len(), height)
    }

    /// Put the box's window back where the draft it now holds wants it, for a box
    /// `rows` rows tall -- what [`input_rows`] says it is.
    ///
    /// A box tall enough for every line has nothing to hide, but the editor
    /// remembers where it was scrolled to while the box was shorter and only moves
    /// that window when the cursor leaves it: a draft that has just lost lines, or
    /// a terminal that has just grown, would be drawn from the remembered row --
    /// the lines above it missing, blank rows where they were. Scrolling the window
    /// to the top is what clears that, and the editor then puts it where the cursor
    /// needs it; the cursor is put back afterwards, since a scroll takes it along.
    ///
    /// Called before the box is drawn, which is the only time the window matters.
    pub(super) fn reset_box_scroll(&mut self, rows: u16) {
        let lines = self.textarea.lines().len();
        let shrank = lines < self.drawn_draft;
        self.drawn_draft = lines;
        // A draft too tall for the box is the editor's to page -- the window is the
        // point there, and taking it over would undo what the box's own keys did.
        // One that has just lost lines is the exception, whatever its length: it
        // was paged against a longer draft than it is now.
        if !shrank && lines + 2 > usize::from(rows) {
            return;
        }
        let cursor = self.textarea.cursor();
        // Scrolling further than there is to scroll is how the window is sent to
        // the top from wherever it was: the editor has no "go to the top".
        self.textarea.scroll((-i16::MAX, 0));
        if let (Ok(row), Ok(col)) = (u16::try_from(cursor.0), u16::try_from(cursor.1)) {
            self.textarea.move_cursor(CursorMove::Jump(row, col));
        }
    }

    /// Replace what is in the box, leaving the cursor after it.
    pub(super) fn set_text(&mut self, text: &str) {
        let mut box_ = input_box();
        if !text.is_empty() {
            box_.insert_str(text);
        }
        self.textarea = box_;
        // A box built from scratch carries the idle invitation, and the box this
        // replaces may have been saying something else: what it says is the
        // state's, not the constructor's.
        self.refresh_placeholder();
    }

    /// Up: the previous command, or the previous line typed.
    pub(super) fn up(&mut self) {
        if let Some(picker) = &mut self.picker {
            let len = picker.choices.len().max(1);
            picker.selected = if picker.selected == 0 {
                len - 1
            } else {
                picker.selected - 1
            };
            return;
        }
        self.browse(-1);
    }

    /// Down: the next command, or the next line typed.
    pub(super) fn down(&mut self) {
        if let Some(picker) = &mut self.picker {
            let len = picker.choices.len().max(1);
            picker.selected = (picker.selected + 1) % len;
            return;
        }
        self.browse(1);
    }

    /// Step through the history. `-1` is older and `+1` newer; stepping past the
    /// newest entry returns what was being typed before browsing started.
    ///
    /// Browsing closes the picker: a recalled line may well be a command, and
    /// letting the picker open would take the very keys being used to browse.
    pub(super) fn browse(&mut self, step: isize) {
        if self.history.is_empty() {
            return;
        }
        let next = match self.browsing {
            None if step < 0 => {
                self.draft = self.text();
                Some(self.history.len() - 1)
            }
            // Already showing the draft, and there is nothing newer to show.
            None => None,
            Some(0) if step < 0 => Some(0),
            Some(at) if step > 0 && at + 1 >= self.history.len() => None,
            Some(at) => Some(at.checked_add_signed(step).unwrap_or(at)),
        };
        self.browsing = next;
        let text = match next {
            Some(at) => self.history[at].clone(),
            None => std::mem::take(&mut self.draft),
        };
        self.picker = None;
        self.set_text(&text);
    }

    /// Record a submitted line. Repeating the previous one is not recorded
    /// again: it is noise when stepping back through the history.
    pub(super) fn remember(&mut self, line: &str) {
        if line.is_empty() || self.history.last().is_some_and(|last| last == line) {
            return;
        }
        self.history.push(line.to_owned());
        let excess = self.history.len().saturating_sub(history::MAX_ENTRIES);
        if excess > 0 {
            self.history.drain(..excess);
        }
    }

    /// Take a submitted line as part of the session: it goes into the transcript
    /// the way replay would put it there, and into the history for recall.
    ///
    /// A command is not part of the session: the handler answers it without the
    /// model ever seeing it, so a session replayed later does not have it either,
    /// and showing one live would make the same session read two ways depending on
    /// when it was looked at.
    pub(super) fn submit(&mut self, line: &str) {
        self.remember(line);
        if !line.starts_with('/') {
            self.revision += 1;
            self.transcript.push(Cell::User(line.to_owned()));
        }
        // What was just asked is what the user wants to watch, so the transcript
        // goes back to its end whether or not that line becomes a cell.
        self.follow();
    }

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

    /// The queue as it is drawn: one dim line per queued line, the end of it last,
    /// like the transcript's own window. Capped at [`QUEUE_ROWS`] rows, so that a
    /// queue longer than that -- more lines, or longer ones -- costs the transcript
    /// those rows and no more. What the end of the window keeps is the newest line,
    /// which is the one just typed and the one being waited for; what it is not
    /// showing is counted rather than dropped, the rule the picker keeps to as
    /// well, so that a queue running past the cap does not read as a queue of
    /// three. The count is drawn on one of the rows the cap allows rather than on
    /// a row of its own: the cap is what the transcript is paying.
    pub(super) fn queue_lines(&self, width: usize) -> Vec<Line<'static>> {
        let width = measure(width);
        let mut lines = Vec::new();
        for line in &self.queued {
            lines.extend(wrapped_lines(
                &[Span::new(Style::Dim, format!("› {line}"))],
                width,
            ));
        }
        if lines.len() <= QUEUE_ROWS {
            return lines;
        }
        let hidden = lines.len() - (QUEUE_ROWS - 1);
        let mut window = vec![more_line("  ", hidden)];
        window.extend(lines.split_off(hidden));
        window
    }

    /// The placeholder for what the box is for right now: the answer while a
    /// question is open, the queue while a turn runs, the next message
    /// otherwise.
    pub(super) fn placeholder(&self) -> &'static str {
        match self.reply {
            Some(Answer::YesNo(_)) => ANSWER_PLACEHOLDER,
            Some(Answer::Secret(_)) => SECRET_PLACEHOLDER,
            None if self.turn_running => QUEUE_PLACEHOLDER,
            None => IDLE_PLACEHOLDER,
        }
    }

    /// Put that placeholder on the box, which is a thing of the box's own rather
    /// than of the screen's.
    pub(super) fn refresh_placeholder(&mut self) {
        self.textarea.set_placeholder_text(self.placeholder());
    }

    /// Take the submitted line out of the box, leaving it empty for the next one.
    pub(super) fn take_line(&mut self) -> String {
        let line = self.text();
        self.textarea = input_box();
        self.refresh_placeholder();
        self.picker = None;
        self.browsing = None;
        self.draft.clear();
        line
    }

    /// The approval gate is asking: remember who to answer.
    pub(super) fn open_question(&mut self, reply: oneshot::Sender<Verdict>) {
        self.open_answer(Answer::YesNo(reply));
    }

    /// A question is open: take the box for its answer.
    pub(super) fn open_answer(&mut self, reply: Answer) {
        self.revision += 1;
        // A secret is typed on a screen other people can see, so the box shows
        // dots instead of what is in it. The answer is still what was typed:
        // this is the drawing, not the text.
        let secret = matches!(reply, Answer::Secret(_));
        self.reply = Some(reply);
        // A line being composed when the question arrives is held aside: the
        // answer to "run it?" is a `y`, and a sentence that happened to be in the
        // box is not one. It comes back when the question closes.
        self.held_draft = Some(self.text());
        self.textarea = input_box();
        if secret {
            self.textarea.set_mask_char(SECRET_MASK);
        }
        self.refresh_placeholder();
    }

    /// Answer the open question from what is in the box, if anything. What the
    /// answer means is the question's: `y` allows a tool call, an empty line
    /// cancels a secret.
    pub(super) fn close_question(&mut self) {
        self.revision += 1;
        if let Some(reply) = self.reply.take() {
            let answer = self.textarea.lines().join("\n").trim().to_owned();
            match reply {
                Answer::YesNo(reply) => {
                    let verdict = if answer.to_lowercase().starts_with('y') {
                        Verdict::Allowed
                    } else {
                        Verdict::Denied
                    };
                    let _ = reply.send(verdict);
                }
                Answer::Secret(reply) => {
                    let _ = reply.send((!answer.is_empty()).then_some(answer));
                }
            }
            let held = self.held_draft.take().unwrap_or_default();
            self.set_text(&held);
        }
        self.question = None;
        self.refresh_placeholder();
    }
}

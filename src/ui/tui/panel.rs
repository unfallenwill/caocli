//! The question tool's panel: the questions it asked, and the keys that answer
//! them.
//!
//! The call that asked is a cell in the transcript, with the questions and their
//! options laid out the way a resumed session lays them out. The panel is the
//! live copy of it, standing over the bottom of the transcript where the picker
//! stands: what is being chosen is chosen with the keyboard, and the row the next
//! key will act on has to be on the screen.
//!
//! The box is still the place an answer is typed -- a question has to be
//! answerable in the user's own words, or the model could only ask about things
//! it can list -- so the panel takes the keys that choose and leaves everything
//! else to the editor.

use ratatui::style::Modifier;
use ratatui::text::Line;
use tokio::sync::oneshot;

use crate::tools::ask::{Answer, Question};
use crate::ui::cell::{Span, Style};

use super::layout::picker_window;
use super::paint::{measure, more_line, wrapped_lines};
use super::state::State;

/// How many option rows the panel shows at once. Like the picker's cap, this is
/// what the panel may cost the transcript it stands over; what the window keeps
/// is the option the cursor is on, and what it cuts is counted.
pub(super) const PANEL_ROWS: usize = 8;

/// What the box says while a question is open: the panel is what offers the
/// choices, so the box says what it is for -- the answer in the user's own words.
pub(super) const PANEL_PLACEHOLDER: &str = "or type an answer · Enter confirms";

/// The columns that open a row: where the cursor is, and which options are
/// chosen. One marker per question kind, so that a row is never saying two things
/// in one column -- a cursor sitting on a chosen option still shows both.
const CURSOR: &str = "❯ ";
const CHOSEN: &str = "✓ ";
const BLANK: &str = "  ";

/// Lines the panel shows under the options. The keys are the whole of what a
/// reader has to learn here, and the panel is where they are learnt.
const MOVE: &str = "↑/↓ move";
const TOGGLE: &str = "space toggles";
const TYPE: &str = "type to answer in your own words";
const CONFIRM: &str = "Enter confirms";
const SKIP: &str = "Esc skips";

/// The question tool's panel: the questions, which one is on screen, and what has
/// been chosen so far.
pub(super) struct Panel {
    pub(super) questions: Vec<Question>,
    /// The question being answered.
    pub(super) at: usize,
    /// The option the cursor is on.
    pub(super) cursor: usize,
    /// What has been chosen for each question, in the order they were asked; the
    /// entry for the question on screen fills in as it is answered.
    pub(super) chosen: Vec<Vec<String>>,
    /// Where the answers go once the last question has been answered. Dropping it
    /// without sending is a dismissal, which is the same thing the panel's own
    /// escape key sends.
    pub(super) reply: oneshot::Sender<Option<Vec<Answer>>>,
}

impl Panel {
    fn question(&self) -> &Question {
        &self.questions[self.at]
    }

    /// What to answer with: one entry per question, in the order they were asked.
    fn answers(&self) -> Vec<Answer> {
        self.questions
            .iter()
            .zip(&self.chosen)
            .map(|(question, labels)| Answer {
                id: question.id.clone(),
                labels: labels.clone(),
            })
            .collect()
    }
}

impl State {
    /// A question is open: take the box for its answers and put the panel up.
    ///
    /// The panel is not a `Notice`: it is the answer half of a question the
    /// machine asked on a channel of its own, and the questions themselves are
    /// already in the transcript as the cell of the call that asked them.
    pub(super) fn open_panel(
        &mut self,
        questions: Vec<Question>,
        reply: oneshot::Sender<Option<Vec<Answer>>>,
    ) {
        self.revision += 1;
        let chosen = vec![Vec::new(); questions.len()];
        // A line being composed when the question arrives is held aside, the way
        // it is for the gate: a sentence that happened to be in the box is not an
        // answer, and it comes back when the questions are over.
        self.held_draft = Some(self.text());
        self.textarea = super::input::input_box();
        self.panel = Some(Panel {
            questions,
            at: 0,
            cursor: 0,
            chosen,
            reply,
        });
        self.refresh_placeholder();
    }

    /// A panel is up: the box is not the queue's while it is.
    pub(super) fn panel_open(&self) -> bool {
        self.panel.is_some()
    }

    /// Whether the question on screen allows more than one option to be chosen.
    pub(super) fn panel_takes_many(&self) -> bool {
        self.panel
            .as_ref()
            .is_some_and(|panel| panel.question().multi_select)
    }

    /// Move the cursor by `step` options, wrapping around the ends the way the
    /// picker's list does: an option is always one key away.
    pub(super) fn panel_move(&mut self, step: isize) {
        let Some(panel) = &mut self.panel else { return };
        let len = panel.question().options.len();
        if len == 0 {
            return;
        }
        self.revision += 1;
        panel.cursor = (panel.cursor as isize + step).rem_euclid(len as isize) as usize;
    }

    /// Jump the cursor to the `n`th option (1-based), when there is one.
    pub(super) fn panel_jump(&mut self, n: usize) {
        let Some(panel) = &mut self.panel else { return };
        if n >= 1 && n <= panel.question().options.len() && panel.cursor != n - 1 {
            self.revision += 1;
            panel.cursor = n - 1;
        }
    }

    /// Toggle the option under the cursor, for a question that takes several.
    ///
    /// The answer is kept in the order the options were offered, not the order
    /// they were toggled in: it is the question's answer, and it should read the
    /// same as the options it was chosen from.
    pub(super) fn panel_toggle(&mut self) {
        let Some(panel) = &mut self.panel else { return };
        let question = panel.question();
        if !question.multi_select || question.options.is_empty() {
            return;
        }
        self.revision += 1;
        let toggled = question.options[panel.cursor].label.clone();
        let before = panel.chosen[panel.at].clone();
        let after: Vec<String> = question
            .options
            .iter()
            .filter(|option| {
                let on = before.contains(&option.label);
                if option.label == toggled { !on } else { on }
            })
            .map(|option| option.label.clone())
            .collect();
        panel.chosen[panel.at] = after;
    }

    /// Enter: take the answer to the question on screen and move to the next one.
    ///
    /// A line typed in the box is an answer in the user's own words and wins over
    /// the cursor: the panel offers choices, it does not restrict them. Otherwise
    /// the cursor's option is the answer -- or, for a question that takes several,
    /// whatever has been toggled, which is what toggling is for. A question with
    /// no options and nothing typed is left blank, which the model is told.
    pub(super) fn panel_confirm(&mut self, typed: &str) {
        let typed = typed.trim().to_owned();
        let Some(panel) = &mut self.panel else { return };
        let question = panel.question();
        let labels = if !typed.is_empty() {
            vec![typed]
        } else if question.multi_select && !panel.chosen[panel.at].is_empty() {
            panel.chosen[panel.at].clone()
        } else if question.options.is_empty() {
            Vec::new()
        } else {
            vec![question.options[panel.cursor].label.clone()]
        };
        self.revision += 1;
        panel.chosen[panel.at] = labels;
        if panel.at + 1 < panel.questions.len() {
            panel.at += 1;
            panel.cursor = 0;
            // The box answers one question at a time: what was typed answered
            // this one, and the next starts empty.
            self.set_text("");
        } else {
            // The last answer closes the panel, and closing it is what gives the
            // box back the line that was being written when the questions
            // arrived -- so nothing may be typed over it here.
            self.finish_panel();
        }
    }

    /// Esc: every question is dismissed, and the model is told the user did not
    /// answer rather than which option they did not pick.
    pub(super) fn panel_dismiss(&mut self) {
        let Some(panel) = self.panel.take() else {
            return;
        };
        self.revision += 1;
        self.close_answer_box();
        let _ = panel.reply.send(None);
    }

    /// The last question has been answered: hand the answers back.
    fn finish_panel(&mut self) {
        let Some(panel) = self.panel.take() else {
            return;
        };
        self.close_answer_box();
        let answers = panel.answers();
        let _ = panel.reply.send(Some(answers));
    }

    /// The turn is over with the panel still up: it goes, and whoever was waiting
    /// on it hears nothing -- which is the dismissal the machine already knows how
    /// to write down.
    pub(super) fn close_panel(&mut self) {
        if self.panel.take().is_some() {
            self.revision += 1;
            self.close_answer_box();
        }
    }

    /// Give the box back to what it was holding before the questions arrived.
    fn close_answer_box(&mut self) {
        let held = self.held_draft.take().unwrap_or_default();
        self.set_text(&held);
        self.refresh_placeholder();
    }

    /// The panel as it is drawn: which question of how many, the question itself,
    /// its options with the cursor on one of them, and the keys.
    ///
    /// Wrapped, never clipped: it is drawn over the transcript, so an over-long
    /// line would be cut at the edge of the region instead of folded, which is the
    /// one thing the terminal cannot be left to fix.
    pub(super) fn panel_lines(&self, width: usize) -> Vec<Line<'static>> {
        let Some(panel) = &self.panel else {
            return Vec::new();
        };
        let width = measure(width);
        let question = panel.question();
        let mut lines: Vec<Line<'static>> = Vec::new();
        if !question.header.is_empty() || panel.questions.len() > 1 {
            let mut heading = String::new();
            if !question.header.is_empty() {
                heading.push_str(&question.header);
                heading.push_str(" · ");
            }
            heading.push_str(&format!(
                "question {} of {}",
                panel.at + 1,
                panel.questions.len()
            ));
            lines.extend(wrapped_lines(&[Span::new(Style::Dim, heading)], width));
        }
        let mut ask = vec![Span::new(Style::Yellow, question.question.clone())];
        if question.multi_select {
            ask.push(Span::new(Style::Dim, " · choose any"));
        }
        lines.extend(wrapped_lines(&ask, width));
        let count = question.options.len();
        if count > 0 {
            let window = picker_window(count, panel.cursor, PANEL_ROWS);
            let blank = " ".repeat(lead_columns(question));
            if window.above > 0 {
                lines.push(more_line(&blank, window.above));
            }
            for at in window.first..window.last {
                lines.extend(option_lines(panel, at, width));
            }
            if window.below > 0 {
                lines.push(more_line(&blank, window.below));
            }
        }
        lines.extend(wrapped_lines(
            &[Span::new(Style::Dim, footer(question))],
            width,
        ));
        lines
    }
}

/// The columns a question's rows open in: the cursor, and for a question that
/// takes several, the mark of what has been chosen. Also the indent the counts of
/// a cut option list are written at, so that they line up with what they count.
fn lead_columns(question: &Question) -> usize {
    if question.multi_select {
        2 * CURSOR.chars().count()
    } else {
        CURSOR.chars().count()
    }
}

/// One option as the panel draws it, the cursor's row set in reverse.
fn option_lines(panel: &Panel, at: usize, width: usize) -> Vec<Line<'static>> {
    let question = panel.question();
    let option = &question.options[at];
    let cursor = if at == panel.cursor { CURSOR } else { BLANK };
    let mut spans = Vec::new();
    if question.multi_select {
        let chosen = panel.chosen[panel.at].contains(&option.label);
        spans.push(Span::new(
            Style::Dim,
            format!("{cursor}{}", if chosen { CHOSEN } else { BLANK }),
        ));
    } else {
        spans.push(Span::new(Style::Dim, cursor));
    }
    spans.push(Span::new(Style::Dim, format!("{}. ", at + 1)));
    spans.push(Span::new(Style::Plain, option.label.clone()));
    if !option.description.is_empty() {
        spans.push(Span::new(Style::Dim, format!(" — {}", option.description)));
    }
    let mut lines = wrapped_lines(&spans, width);
    if at == panel.cursor {
        // The row the next key acts on is the one reversed, the way the picker
        // marks the row `Enter` would take.
        for line in &mut lines {
            for span in &mut line.spans {
                span.style = span.style.add_modifier(Modifier::REVERSED);
            }
        }
    }
    lines
}

/// What the panel says the keys do, under the options it is offering.
fn footer(question: &Question) -> String {
    if question.options.is_empty() {
        return format!("{TYPE} · {CONFIRM} · {SKIP}");
    }
    let mut keys = vec![MOVE];
    if question.multi_select {
        keys.push(TOGGLE);
    }
    format!("{} · {TYPE} · {CONFIRM} · {SKIP}", keys.join(" · "))
}

//! The picker: the menu that stands over the transcript while a line is being
//! named or a choice made.
//!
//! Menus exist here and not in the plain front end because a menu needs a
//! screen to stand on; the commands themselves are the same, and a chosen row
//! is submitted as the very line the plain prompt would have been given.

use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::{Line, Span as RSpan};

use crate::repl;
use crate::session;
use crate::ui::text;

use super::layout::picker_window;
use super::paint::more_line;
use super::state::State;

/// How many command rows the picker shows at once. It draws over the bottom of the
/// transcript, so it has to leave the transcript somewhere to live.
pub(super) const PICKER_ROWS: usize = 6;

/// A menu as the picker draws it: the rows are the application's, the drawing is
/// this front end's.
#[cfg(test)]
pub(super) fn named_rows(rows: Vec<(String, String)>) -> Vec<Choice> {
    rows.into_iter()
        .map(|(label, detail)| Choice {
            argument: label.clone(),
            label,
            detail,
        })
        .collect()
}

/// A menu as the picker draws it: the rows are the application's, the drawing is
/// this front end's.
pub(super) fn choice_rows(rows: Vec<crate::config::Choice>) -> Vec<Choice> {
    rows.into_iter()
        .map(|row| Choice {
            label: row.label,
            argument: row.argument,
            detail: row.detail,
        })
        .collect()
}

/// The picker: what it is offering, and which row is highlighted.
pub(super) struct Picker {
    pub(super) kind: Choosing,
    pub(super) choices: Vec<Choice>,
    pub(super) selected: usize,
}

/// What the picker is for, which is what choosing a row means.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum Choosing {
    /// A command name still being typed. What matches is recomputed on every
    /// keystroke, Tab completes, and Enter runs what is in the box -- an argument
    /// may still be wanted.
    Command,
    /// A session to switch to. The list is the whole message and nothing is
    /// half-typed, so Enter takes the highlighted row.
    Session,
    /// A provider to store a key for: `/login`'s menu, which the plain front end
    /// can only print.
    Provider,
    /// A model to switch to: `/model`'s menu.
    Model,
}

impl Choosing {
    /// The command a chosen row is submitted as: the row becomes the line the
    /// plain prompt would have been given, so that choosing has one
    /// implementation rather than one per front end.
    fn command(self) -> &'static str {
        match self {
            Choosing::Command => "",
            Choosing::Session => "/resume",
            Choosing::Provider => "/login",
            Choosing::Model => "/model",
        }
    }

    /// Whether the list is the whole message -- a list of things to do rather
    /// than a name being typed -- and so whether Enter takes the highlighted row
    /// instead of submitting what is in the box.
    fn takes_a_row(self) -> bool {
        self != Choosing::Command
    }
}

/// One row of the picker: what it says, what it is called, and what choosing it
/// means.
pub(super) struct Choice {
    /// What the row shows.
    pub(super) label: String,
    /// What choosing it submits as the argument of the command the picker is
    /// offering: the same string as the label, except for a provider, which is
    /// listed by name and addressed by id.
    pub(super) argument: String,
    /// The dim second column.
    pub(super) detail: String,
}

impl State {
    /// Recompute what the picker offers for what is in the box, keeping the
    /// highlight on the same command while it is still among the matches.
    pub(super) fn refresh_picker(&mut self) {
        // An offered list is not a completion: it was asked for in full, and it
        // stays until it is answered or dismissed, whatever is typed next.
        if self.picker.as_ref().is_some_and(|p| p.kind.takes_a_row()) {
            return;
        }
        let matches = repl::completions(&self.text());
        if matches.is_empty() {
            self.picker = None;
            return;
        }
        let previous = self
            .picker
            .as_ref()
            .and_then(|p| p.choices.get(p.selected))
            .map(|c| c.argument.clone());
        let selected = previous
            .and_then(|name| matches.iter().position(|c| c.name == name))
            .unwrap_or(0);
        self.picker = Some(Picker {
            kind: Choosing::Command,
            choices: matches
                .into_iter()
                .map(|c| Choice {
                    label: c.name.to_owned(),
                    argument: c.name.to_owned(),
                    detail: c.description.to_owned(),
                })
                .collect(),
            selected,
        });
    }

    /// Offer the sessions in `list` to switch to, and say whether there was
    /// anything to offer.
    ///
    /// The sessions are passed in rather than read here: the terminal is not where
    /// the filesystem is read, and this is the state half of the front end.
    pub(super) fn open_sessions(&mut self, list: &[session::SessionInfo]) -> bool {
        self.open_choices(
            Choosing::Session,
            list.iter()
                .map(|s| Choice {
                    label: s.id.clone(),
                    argument: s.id.clone(),
                    detail: format!("{} messages · {}", s.message_count, s.preview),
                })
                .collect(),
        )
    }

    /// Offer `rows` to choose from, and say whether there was anything to offer.
    /// The rows are built from [`crate::config`]'s tables -- what exists is the
    /// application's to know, and how a row is drawn is this front end's.
    pub(super) fn open_choices(&mut self, kind: Choosing, rows: Vec<Choice>) -> bool {
        self.revision += 1;
        if rows.is_empty() {
            self.picker = None;
            return false;
        }
        self.picker = Some(Picker {
            kind,
            choices: rows,
            selected: 0,
        });
        true
    }

    /// Take the highlighted row. It becomes the line the plain prompt would have
    /// been given, so choosing has one implementation rather than one per front
    /// end.
    pub(super) fn choose(&mut self) -> bool {
        let Some(picker) = &self.picker else {
            return false;
        };
        if !picker.kind.takes_a_row() {
            return false;
        }
        let Some(argument) = picker
            .choices
            .get(picker.selected)
            .map(|c| c.argument.clone())
        else {
            return false;
        };
        let line = format!("{} {argument}", picker.kind.command());
        self.picker = None;
        self.set_text(&line);
        true
    }

    /// Tab: put the highlighted command in the box without running it, since an
    /// argument may still be wanted. Only a command reaches here: the other lists
    /// are menus, and `Tab` submits a menu's row the way `Enter` does.
    pub(super) fn complete(&mut self) {
        let Some(picker) = self.picker.take() else {
            return;
        };
        if let Some(choice) = picker.choices.get(picker.selected) {
            self.set_text(&choice.argument);
        }
    }

    /// The picker as it is drawn: the rows its window holds, the highlighted one
    /// reversed, and a count at each end the list is cut off at.
    ///
    /// The window follows the selection ([`picker_window`]), so however long the
    /// menu is, the row `Enter` is about is one of the rows on the screen. It used
    /// not to be: the rows were drawn from the first choice down, so a menu longer
    /// than [`PICKER_ROWS`] could be scrolled -- by a key that wraps around, no
    /// less -- past its own end, and what `Enter` would choose was then not on the
    /// screen at all.
    pub(super) fn picker_lines(&self) -> Vec<Line<'static>> {
        let Some(picker) = &self.picker else {
            return Vec::new();
        };
        // As wide as the widest name in this list, so its rows line up without
        // every menu paying for the widest one there is: a menu of command names
        // is narrow, one of `<provider id>/<modelid>` is not.
        let width = picker
            .choices
            .iter()
            .map(|c| text::width(&c.label))
            .max()
            .unwrap_or(0);
        let row = |i: usize| {
            let choice = &picker.choices[i];
            let selected = i == picker.selected;
            let name = if selected {
                RStyle::new().add_modifier(Modifier::REVERSED)
            } else {
                RStyle::new()
            };
            let detail = if selected {
                RStyle::new().add_modifier(Modifier::REVERSED)
            } else {
                RStyle::new().add_modifier(Modifier::DIM)
            };
            Line::from(vec![
                RSpan::styled(format!(" {}", text::padded(&choice.label, width)), name),
                RSpan::styled(format!(" {}", choice.detail), detail),
            ])
        };
        let window = picker_window(picker.choices.len(), picker.selected, PICKER_ROWS);
        let mut lines: Vec<Line<'static>> = Vec::new();
        if window.above > 0 {
            lines.push(more_line(" ", window.above));
        }
        lines.extend((window.first..window.last).map(row));
        if window.below > 0 {
            lines.push(more_line(" ", window.below));
        }
        lines
    }
}

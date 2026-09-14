//! The picker: the menu that stands over the transcript while a line is being
//! named or a choice made.
//!
//! Menus exist here and not in the plain front end because a menu needs a
//! screen to stand on; the commands themselves are the same, and a chosen row
//! is submitted as the very line the plain prompt would have been given.

use std::io::Stdout;
use std::path::Path;

use ratatui::backend::CrosstermBackend;
use ratatui::style::{Modifier, Style as RStyle};
use ratatui::text::{Line, Span as RSpan};

use crate::agent::Agent;
use crate::config;
use crate::repl;
use crate::session;
use crate::ui::cell::{Style, style_of};
use crate::ui::glyphs;
use crate::ui::text;

use super::layout::picker_window;
use super::screen::Screen;
use super::state::State;
use crate::ui::paint::more_line;

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
/// The style the row cursor is drawn in: the palette's attention colour on the
/// selected row, so the mark is a colour as well as a shape, and nothing on any
/// other row. Under reverse video the colour is the *foreground* of an inverted
/// cell, which is what makes it read as a mark rather than as a highlight of its
/// own.
fn cursor_style(selected: bool) -> RStyle {
    if selected {
        style_of(Style::Yellow)
    } else {
        RStyle::new()
    }
}

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
    /// A reasoning effort tier to switch to: `/effort`'s menu.
    Effort,
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
            Choosing::Effort => "/effort",
        }
    }

    /// The picker this command opens, or `None` when the line names no menu.
    /// `Command` is excluded on purpose: it is the picker the box grows while
    /// a name is being typed, and it is never asked for by a submitted line.
    fn for_command(line: &str) -> Option<Self> {
        let trimmed = line.trim();
        [
            Choosing::Session,
            Choosing::Provider,
            Choosing::Model,
            Choosing::Effort,
        ]
        .into_iter()
        .find(|c| c.command() == trimmed)
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
        if self
            .overlay
            .picker
            .as_ref()
            .is_some_and(|p| p.kind.takes_a_row())
        {
            return;
        }
        let matches = repl::completions(&self.text());
        if matches.is_empty() {
            self.overlay.picker = None;
            return;
        }
        let previous = self
            .overlay
            .picker
            .as_ref()
            .and_then(|p| p.choices.get(p.selected))
            .map(|c| c.argument.clone());
        let selected = previous
            .and_then(|name| matches.iter().position(|c| c.name == name))
            .unwrap_or(0);
        self.overlay.picker = Some(Picker {
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
                    detail: format!("{} messages{}{}", s.message_count, glyphs::sep(), s.preview),
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
            self.overlay.picker = None;
            return false;
        }
        self.overlay.picker = Some(Picker {
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
        let Some(picker) = &self.overlay.picker else {
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
        self.overlay.picker = None;
        self.set_text(&line);
        true
    }

    /// Tab: put the highlighted command in the box without running it, since an
    /// argument may still be wanted. Only a command reaches here: the other lists
    /// are menus, and `Tab` submits a menu's row the way `Enter` does.
    pub(super) fn complete(&mut self) {
        let Some(picker) = self.overlay.picker.take() else {
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
        let Some(picker) = &self.overlay.picker else {
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
            // Reverse video is the one highlight that works in every terminal
            // without the application knowing the background, which is why the
            // selected row is marked that way rather than with a colour of its
            // own: a selection *colour* would be a background, and this process
            // does not know what background it is being drawn on.
            let reversed = RStyle::new().add_modifier(Modifier::REVERSED);
            let name = if selected { reversed } else { RStyle::new() };
            // The detail is muted, except on the selected row: `DIM` under
            // reverse video is a modifier whose rendering is undefined, and
            // painting the muted colour there would be the muted colour asked to
            // sit on an inverted background. The selected row carries its
            // hierarchy by position instead -- the detail is the right column.
            let detail = if selected {
                reversed
            } else {
                style_of(Style::Dim)
            };
            // And a cursor in the signal colour, so that the highlight is not
            // the only thing saying which row `Enter` is about: `REVERSED` is a
            // whole-row inversion, which on a narrow terminal or a badly
            // rendered one is not much of a mark.
            Line::from(vec![
                RSpan::styled(
                    if selected { glyphs::get().cursor } else { "  " },
                    cursor_style(selected),
                ),
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

/// The command a line names whose answer is a menu rather than a turn.
///
/// The list of what this can return is the same list [`Choosing::for_command`]
/// walks, with the same strings; the two have to agree, so the table lives in
/// one place.
pub(super) fn menu_for(line: &str) -> Option<Choosing> {
    Choosing::for_command(line)
}

/// Open the menu a command asked for. `Ok(true)` says the menu is up and the
/// answer comes from the keyboard; `Ok(false)` says there was nothing to offer
/// -- an empty session directory -- and the line runs as it would have.
pub(super) fn offer_menu(
    screen: &mut Screen<CrosstermBackend<Stdout>>,
    menu: Choosing,
    agent: &Agent,
    sdir: &Path,
) -> anyhow::Result<bool> {
    match menu {
        Choosing::Session => Ok(screen.state.open_sessions(&session::list(sdir)?)),
        Choosing::Provider => Ok(screen
            .state
            .open_choices(Choosing::Provider, choice_rows(config::provider_choices()))),
        Choosing::Model => Ok(screen.state.open_choices(
            Choosing::Model,
            choice_rows(config::model_menu(&agent.model_label())),
        )),
        Choosing::Effort => Ok(screen.state.open_choices(
            Choosing::Effort,
            choice_rows(config::effort_menu(&agent.provider(), agent.effort_label())),
        )),
        // `menu_for` only returns the menu-shaped variants, so Command never
        // reaches here -- treat it as "no menu".
        Choosing::Command => Ok(false),
    }
}

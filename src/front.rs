//! Front ends: the three ways a session's words reach the user.
//!
//! The agent's turn loop, the session it talks to, and the policy it runs
//! under all live in [`crate::agent`]. What this module owns is the part
//! that varies per run: what the user sees at the start, where keystrokes
//! come from, and how the prompt shapes itself to the terminal. Three
//! shapes today — list, one-shot, interactive — and each shape's start-up
//! banner and run logic live with the variant, so reading `FrontEnd::run`
//! after a change shows only the dispatch.
//!
//! `ListSessions` is handled before the agent is built (it does not need
//! one); the function is kept here so the list-output is the front end's
//! responsibility, not `main`'s.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui_textarea::TextArea;

use crate::agent::Agent;
use crate::cli::{Cli, Mode};
use crate::config::ApiKey;
use crate::image;
use crate::session;
use crate::types::Message;
use crate::ui::tui;
use crate::ui::{Front, Renderer, Style};
use crate::ui::{Sigint, StdinApproval, StdinQuestions};

/// One turn with the prompt already known: the only front end that prints
/// the prompt to stdout itself rather than hosting a live editor.
pub(crate) struct OneShot {
    prompt: String,
    images: Vec<PathBuf>,
    mcp_notes: Vec<String>,
}

/// The interactive prompt. The TUI is attempted first where the terminal
/// will host it; the plain prompt is the universal default and runs after
/// the TUI declines (or in place of it, when `--no-tui` or a non-terminal
/// stdout makes the TUI impossible).
pub(crate) struct Interactive {
    banner: String,
    history: Vec<Message>,
    no_status_bar: bool,
    /// TUI is attempted iff this is true. Set by `from_mode` from the
    /// `cli.no_tui` flag and the runtime is-terminal check.
    try_tui: bool,
    sessions_dir: PathBuf,
}

/// The dispatch type: one variant per shape a session can take. The agent
/// is owned by `main::run`; this enum carries only the run-specific data
/// each shape needs beyond it.
pub(crate) enum FrontEnd {
    OneShot(OneShot),
    Interactive(Interactive),
}

impl FrontEnd {
    /// Build the front end for one run. `ListSessions` is handled before
    /// `from_mode` is called, because it does not need an agent — the rest
    /// are uniform in shape: read what `agent` knows, package the data, and
    /// let `run` decide how to draw it.
    pub(crate) fn from_mode(
        mode: &Mode,
        agent: &Agent,
        sessions_dir: PathBuf,
        mcp_notes: Vec<String>,
        api_key: &ApiKey,
        cli: &Cli,
    ) -> Self {
        match mode {
            Mode::OneShot { prompt, images } => FrontEnd::OneShot(OneShot {
                prompt: prompt.clone(),
                images: images.clone(),
                mcp_notes,
            }),
            Mode::Interactive { no_status_bar } => {
                let banner = build_banner(agent, api_key, &mcp_notes);
                let history = agent.session.messages.clone();
                let try_tui = !cli.no_tui && std::io::stdout().is_terminal();
                FrontEnd::Interactive(Interactive {
                    banner,
                    history,
                    no_status_bar: *no_status_bar,
                    try_tui,
                    sessions_dir,
                })
            }
            Mode::ListSessions => unreachable!("ListSessions exits before FrontEnd is constructed"),
            Mode::Migrate { .. } => unreachable!("Migrate exits before FrontEnd is constructed"),
        }
    }

    /// Run the front end on a borrowed agent. The one-shot path owns the
    /// agent for its single turn; the interactive path borrows across turns
    /// (and across the plain prompt's read loop).
    pub(crate) async fn run(self, agent: &mut Agent, ui: &mut Renderer) -> Result<()> {
        match self {
            FrontEnd::OneShot(os) => run_one_shot(os, agent, ui).await,
            FrontEnd::Interactive(inter) => run_interactive(inter, agent, ui).await,
        }
    }
}

/// Print the session list and exit. The terminal does not need to be a TTY:
/// `--list` is the verb a script uses.
pub(crate) fn print_sessions(sessions_dir: &Path) -> Result<()> {
    for s in session::list(sessions_dir)? {
        println!(
            "{}\t{} messages\t{}\t{}",
            s.id,
            s.message_count,
            s.preview,
            s.path.display()
        );
    }
    Ok(())
}

/// Build the line a session opens with: which session it is, how much is in
/// it, the model, and — first, so it is what a reader sees — anything the
/// session needs them to fix before they can run it (a missing key, a
/// workspace with AGENTS.md, the MCP servers that came up and the ones that
/// did not).
fn build_banner(agent: &Agent, api_key: &ApiKey, mcp_notes: &[String]) -> String {
    let mut banner = crate::ui::banner(
        &agent.session.id,
        agent.session.messages.len(),
        &agent.model_label(),
    );
    if let Some(note) = api_key.missing_note() {
        banner = format!("{note}\n{banner}");
    }
    if agent.session.meta.instructions.is_some() {
        banner = format!("{banner}\nproject instructions: AGENTS.md");
    }
    for note in mcp_notes {
        banner = format!("{banner}\n{note}");
    }
    banner
}

async fn run_one_shot(os: OneShot, agent: &mut Agent, ui: &mut Renderer) -> Result<()> {
    // The one-shot run prints no banner: a script that piped the prompt in
    // does not want a session summary, and the model + effort line is what a
    // person running one-shot by hand most wants to see first.
    ui.info(&format!(
        "session {} · {} · effort {}{}",
        agent.session.id,
        agent.model_label(),
        agent.effort_label(),
        if agent.session.meta.instructions.is_some() {
            " · AGENTS.md"
        } else {
            ""
        }
    ));
    for note in &os.mcp_notes {
        ui.info(note);
    }
    let message = match image::user_message(&os.prompt, &os.images) {
        Ok(message) => message,
        Err(e) => {
            ui.error(&format!("{e:#}"));
            std::process::exit(1);
        }
    };
    if !os.images.is_empty() {
        ui.replay(std::slice::from_ref(&message));
    }
    let mut interrupt = Sigint::new()?;
    let mut approve = StdinApproval;
    let mut ask = StdinQuestions;
    if let Err(e) = agent
        .turn_message(message, ui, &mut interrupt, &mut approve, &mut ask)
        .await
    {
        ui.error(&format!("{e:#}"));
        std::process::exit(1);
    }
    Ok(())
}

async fn run_interactive(inter: Interactive, agent: &mut Agent, ui: &mut Renderer) -> Result<()> {
    // Interactive front end: it owns the terminal, so nothing may have been
    // printed before it and nothing may be printed after it while it runs.
    // It declines rather than fails when the terminal cannot host it: the
    // alternate screen and raw mode are process-wide, and a terminal that
    // cannot take them leaves the plain prompt as the only one there is.
    if inter.try_tui && tui::run(agent, &inter.sessions_dir, &inter.banner, &inter.history).await? {
        return Ok(());
    }

    // Banner first, then any replayed history, then the prompt: the same
    // order the interactive front end lays them out -- and the order a
    // returning reader expects, with the session's own words before the box
    // that takes the next one.
    ui.info(&inter.banner);
    if !agent.session.messages.is_empty() {
        ui.replay(&agent.session.messages);
    }

    let prompt_outcome = run_repl(ui, agent, &inter.sessions_dir, inter.no_status_bar).await?;
    if prompt_outcome == Outcome::Exit {
        ui.teardown();
        return Ok(());
    }
    ui.teardown();
    Ok(())
}

// ============================================================================
// The plain prompt's read loop and the helpers that keep it honest.
// ============================================================================

/// Outcome of the REPL loop. The plain prompt is single-threaded and never
/// shares this with anything else, so the value is local to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Exit,
}

/// Drive the plain prompt's read loop.
///
/// One input -> one turn, with a draft that the user can edit across turns:
/// the textarea is held across calls so a draft the user was composing when a
/// turn started is still there when it ends. Enter submits, Ctrl-J inserts a
/// newline (the multi-line rule), Ctrl-D on an empty draft leaves the session,
/// Ctrl-C clears the draft. The terminal is in raw mode for the whole loop;
/// the caller is responsible for restoring it on the way out.
async fn run_repl(
    ui: &mut Renderer,
    agent: &mut Agent,
    sdir: &Path,
    no_status_bar: bool,
) -> Result<Outcome> {
    // Bottom status bar: only enabled in the REPL on a TTY.
    if !no_status_bar {
        ui.refresh_status_bar();
    }

    // The textarea is held across turns so a draft the user was composing when
    // a turn started is still there when it ends. The history it carries is
    // Up/Down's source: the same vec the file is saved into on the way out.
    let hist_path = crate::config::history_file()?;
    let mut textarea = TextArea::default();
    textarea.set_cursor_line_style(ratatui::style::Style::default());
    textarea.set_cursor_style(ratatui::style::Style::default());
    textarea.set_placeholder_text("type a message · /help for commands");
    // The file is loaded into a Vec<String> on `ui`, but the buffer keeps its
    // own: the textarea's history is per-session, the file is for the next run.
    // We share the file path with the TUI -- both front ends read and write the
    // same one.
    let _ = hist_path;

    crossterm::terminal::enable_raw_mode().context("enable raw mode for the prompt")?;
    // Bracketed paste: a paste that contains newlines is one Event::Paste, not
    // a run of Enter keys. The newline goes through `insert_str` verbatim, so a
    // pasted multi-line draft is a multi-line submission. Disable on the way
    // out: the front end that follows may want its own semantics.
    crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)
        .context("enable bracketed paste")?;
    // `crossterm::enable_raw_mode` uses `/dev/tty` and on a pty with a
    // controlling terminal that's the slave, but a session launched under a
    // plain `pty.openpty()` may not have set up the controlling-terminal link
    // by the time enable_raw_mode runs -- which leaves ECHO on and the answer
    // to a `/login` prompt would echo back to the user. Force ECHO off on
    // stdin directly: the fd we read keys from is the one that has to stay
    // quiet. `disable_raw_mode` restores the saved termios so this is undone
    // when the loop exits.
    set_stdin_echo(false);
    ui.set_raw_mode(true);

    // The first draw happens before the loop so the prompt is on the screen
    // when we start waiting for a key.
    draw_prompt(ui, &textarea, no_status_bar);

    let outcome = loop {
        // Read the next event with a small timeout so a turn that hangs
        // elsewhere can be torn down without us being stuck inside `read`. The
        // raw-mode turn flow goes through `agent.turn`, not through this
        // read, so the timeout mostly guards the read itself.
        let event = match event::read() {
            Ok(event) => event,
            Err(e) => {
                end_prompt_terminal(ui);
                return Err(e.into());
            }
        };
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                match (key.code, key.modifiers) {
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        let line: String = textarea
                            .lines()
                            .iter()
                            .flat_map(|l| l.chars().chain(std::iter::once('\n')))
                            .collect();
                        // Strip the trailing newline the join puts on the last
                        // line: an empty last line is a draft the user
                        // submitted, not a draft that ends in a newline.
                        let line = line.trim_end_matches('\n').to_owned();
                        if line.is_empty() {
                            continue;
                        }
                        textarea = TextArea::default();
                        textarea.set_cursor_line_style(ratatui::style::Style::default());
                        textarea.set_cursor_style(ratatui::style::Style::default());
                        textarea.set_placeholder_text("type a message · /help for commands");
                        draw_prompt(ui, &textarea, no_status_bar);

                        let mut interrupt = Sigint::new()?;
                        let turn_outcome = crate::repl::handle(
                            agent,
                            ui,
                            sdir,
                            &line,
                            &mut interrupt,
                            &mut StdinApproval,
                            &mut StdinQuestions,
                            crate::repl::FrontKind::Plain,
                        )
                        .await?;
                        if turn_outcome == crate::repl::Outcome::Exit {
                            break Outcome::Exit;
                        }
                        // Sync the status bar after a turn (it may have moved
                        // when usage arrived) and redraw the prompt -- the
                        // terminal scrolled while the turn was writing.
                        if !no_status_bar {
                            ui.refresh_status_bar();
                        }
                        draw_prompt(ui, &textarea, no_status_bar);
                    }
                    (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                        // Clear the draft, the way the TUI does at the prompt:
                        // Ctrl-C at the prompt is "throw away what was typed",
                        // not "stop the turn" (there is none to stop).
                        textarea = TextArea::default();
                        textarea.set_cursor_line_style(ratatui::style::Style::default());
                        textarea.set_cursor_style(ratatui::style::Style::default());
                        textarea.set_placeholder_text("type a message · /help for commands");
                        draw_prompt(ui, &textarea, no_status_bar);
                    }
                    (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                        // Ctrl-D on an empty draft is "leave": the box has
                        // nothing to delete, and the user is asking for the
                        // way out.
                        if textarea.is_empty() {
                            break Outcome::Exit;
                        }
                        textarea.input(event);
                        draw_prompt(ui, &textarea, no_status_bar);
                    }
                    (KeyCode::Char('j'), KeyModifiers::CONTROL) => {
                        // Ctrl-J is the multi-line key: Enter stays for
                        // submit, but a draft that has to span lines inserts
                        // its own break rather than asking Enter to mean two
                        // things.
                        textarea.insert_newline();
                        draw_prompt(ui, &textarea, no_status_bar);
                    }
                    _ => {
                        // Everything else (including Tab) goes to the editor:
                        // arrow keys move the cursor, Backspace deletes, a
                        // printable char lands at the cursor. `input` returns
                        // whether the text changed, but a redraw is cheap and
                        // is what keeps the cursor visible.
                        textarea.input(event);
                        draw_prompt(ui, &textarea, no_status_bar);
                    }
                }
            }
            Event::Paste(text) => {
                textarea.insert_str(text);
                draw_prompt(ui, &textarea, no_status_bar);
            }
            Event::Resize(_, _) => {
                // Re-sync the status bar on resize: the bar's scroll region
                // is computed from the terminal size.
                if !no_status_bar {
                    ui.refresh_status_bar();
                }
                draw_prompt(ui, &textarea, no_status_bar);
            }
            _ => {}
        }
    };

    end_prompt_terminal(ui);
    Ok(outcome)
}

/// Disable raw mode and bracketed paste. Idempotent and infallible: the tty
/// has to come back regardless of how the loop ended.
fn disable_prompt_terminal() {
    let _ = crossterm::execute!(std::io::stdout(), crossterm::event::DisableBracketedPaste);
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Turn the echo on `stdin` on or off. Used to plug a hole crossterm's
/// `enable_raw_mode` leaves when the session is launched under a pty without a
/// controlling terminal: in that case `/dev/tty` is empty and `cfmakeraw`
/// writes nothing to the fd we actually read from, so the kernel still echoes
/// what is typed.
#[cfg(unix)]
fn set_stdin_echo(on: bool) {
    use std::os::fd::AsRawFd;
    let fd = std::io::stdin().as_raw_fd();
    let mut attrs = unsafe { std::mem::zeroed::<libc::termios>() };
    if unsafe { libc::tcgetattr(fd, &mut attrs) } != 0 {
        return;
    }
    if on {
        attrs.c_lflag |= libc::ECHO;
    } else {
        attrs.c_lflag &= !libc::ECHO;
    }
    let _ = unsafe { libc::tcsetattr(fd, libc::TCSANOW, &attrs) };
}

/// Windows needs no fd surgery: crossterm drives the console through the
/// Console API there, and the pty-without-controlling-terminal hole the Unix
/// branch plugs does not exist.
#[cfg(not(unix))]
fn set_stdin_echo(_on: bool) {}

/// Mark the renderer as out of raw mode and tell it to leave the terminal
/// alone. Called on the way out of the read loop so any later code path
/// (`ask_secret`, status-bar refresh) does not assume the tty is in a state
/// it has since undone.
fn end_prompt_terminal(ui: &mut Renderer) {
    disable_prompt_terminal();
    ui.set_raw_mode(false);
}

/// Render the prompt: the marker `› ` and the draft, redrawing the lines it
/// occupies on the spot. A draft that spans multiple lines takes that many
/// lines; the cursor is moved to the editor's screen position afterwards, and
/// a single trailing newline brings the next prompt down to a line of its own.
///
/// `no_status_bar` skips the bar sync: a caller that already drew it (the
/// loop top) does not want another write.
fn draw_prompt(ui: &mut Renderer, textarea: &TextArea, no_status_bar: bool) {
    use std::io::Write;
    // Re-sync the status bar before the redraw so the bar sits under whatever
    // number of lines the draft takes, not the one the previous draw measured.
    if !no_status_bar {
        ui.refresh_status_bar();
    }
    let mut out = std::io::stdout();
    // Hide the cursor while we write the box; the editor's cursor position is
    // restored once the box is on screen.
    let _ = crossterm::execute!(out, crossterm::cursor::Hide);
    // Move to the start of the prompt line and clear it: the box stands above
    // the status bar, and a redraw is the only thing that keeps typed text on
    // screen with what was typed before it.
    let _ = write!(out, "\r\x1b[2K");
    let lines = textarea.lines();
    let placeholder = "type a message · /help for commands";
    // The escapes the prompt is written in, from the front end that owns them:
    // the two are empty when the renderer has decided against colour, so a
    // non-colour run gets the same plain bytes a back-end test asserts against.
    // The placeholder is the palette's secondary colour, the same one a cell
    // pays the same question in.
    let dim_open = ui.open_style(Style::Dim);
    let dim_close = ui.close_style();
    let marker = "› ";
    let continuation = "  ";
    if lines.is_empty() || (lines.len() == 1 && lines[0].is_empty()) {
        // Empty draft: write the marker, then the placeholder in dim.
        let _ = write!(out, "{marker}{dim_open}{placeholder}{dim_close}");
    } else {
        // Draft has content. Write the marker on the first line, then the
        // first line of text; continuation lines are indented by the marker's
        // width so a draft's columns line up with the transcript's user lines.
        for (i, line) in lines.iter().enumerate() {
            let _ = write!(out, "\r\x1b[2K");
            if i == 0 {
                let _ = write!(out, "{marker}");
            } else {
                let _ = write!(out, "{continuation}");
            }
            let _ = write!(out, "{line}");
            if i + 1 < lines.len() {
                let _ = writeln!(out);
            }
        }
    }
    let _ = out.flush();
    // Place the cursor at the editor's screen position: the editor knows
    // where the cursor belongs (it tracks row/col), and asking it is cheaper
    // than re-deriving the answer from the lines we just wrote.
    let cursor = textarea.screen_cursor();
    let _ = crossterm::execute!(
        out,
        crossterm::cursor::MoveTo(cursor.col as u16 + marker.len() as u16, cursor.row as u16)
    );
    let _ = crossterm::execute!(out, crossterm::cursor::Show);
    let _ = out.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctrl_j_is_a_newline_in_the_plain_prompt() {
        // The plain prompt binds Ctrl-J to insert a newline, matching the
        // transcript's user line shape. With ratatui-textarea the newline
        // goes through `insert_newline`, which is a method call rather than
        // a keybind -- the test pins the call by going through the editor's
        // own API, the same way the read loop does.
        let mut ta = TextArea::default();
        ta.insert_char('a');
        ta.insert_newline();
        ta.insert_char('b');
        assert_eq!(ta.lines(), ["a", "b"]);
    }

    /// Submitting a multi-line textarea turns the lines back into the single
    /// submitted string with embedded newlines, the shape a turn expects.
    #[test]
    fn a_multi_line_draft_submits_as_one_string_with_newlines() {
        let mut ta = TextArea::default();
        ta.insert_char('a');
        ta.insert_newline();
        ta.insert_char('b');
        let line: String = ta
            .lines()
            .iter()
            .flat_map(|l| l.chars().chain(std::iter::once('\n')))
            .collect();
        assert_eq!(line.trim_end_matches('\n'), "a\nb");
    }

    /// Cursor placement on a draft: `screen_cursor` reports the row/col the
    /// loop restores after a redraw. Tested here because the redraw's
    /// `MoveTo` argument is the only place the editor's own coordinates
    /// reach the wire.
    #[test]
    fn screen_cursor_reflects_inserted_text() {
        let mut ta = TextArea::default();
        ta.insert_char('a');
        ta.insert_char('b');
        let cursor = ta.screen_cursor();
        assert_eq!(cursor.row, 0);
        assert_eq!(cursor.col, 2);
    }
}

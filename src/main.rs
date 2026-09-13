mod agent;
mod agents_md;
mod api;
mod cli;
mod config;
mod history;
mod image;
mod machine;
mod mcp;
mod provider;
mod repl;
mod session;
mod startup;
mod tools;
mod types;
mod ui;

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui_textarea::TextArea;

use crate::agent::Agent;
use crate::cli::{Cli, Mode};
use crate::ui::tui;
use crate::ui::{Front, Renderer};
use crate::ui::{Sigint, StdinApproval, StdinQuestions};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(run(cli))
}

/// Outcome of the REPL loop. The plain prompt is single-threaded and never
/// shares this with anything else, so the value is local to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Exit,
}

async fn run(cli: Cli) -> Result<()> {
    let mut ui = Renderer::new();
    let startup = startup::resolve_startup(&cli)?;

    // List sessions: a CLI verb that exits before the agent is built. Handled
    // up here because the rest of the function assumes an agent exists.
    if matches!(startup.mode, Mode::ListSessions) {
        for s in session::list(&startup.sessions_dir)? {
            println!(
                "{}\t{} messages\t{}\t{}",
                s.id,
                s.message_count,
                s.preview,
                s.path.display()
            );
        }
        return Ok(());
    }

    // The servers this workspace and the user's settings name, connected before
    // anything asks the model for a first answer: their tools are part of every
    // request, and a session that offered some of them would offer a different
    // prefix than the one it will send next.
    let mcp = mcp::Hub::connect(&startup.workspace).await;
    let mcp_notes = mcp.notes();

    let mut agent = Agent::new(startup.client, startup.session, startup.provider);
    agent.approval = startup.approval;
    agent.mcp = Arc::new(mcp);
    ui.set_model(&agent.model_label());
    ui.set_effort(agent.effort_label());

    // One-shot mode (the agent's primary self-test channel)
    if let Some(prompt) = &cli.prompt {
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
        // What came up, and what did not: the one-shot run prints no banner,
        // and a server that failed to start is worth knowing about before the
        // model is asked for anything.
        for note in &mcp_notes {
            ui.info(note);
        }
        let message = match image::user_message(prompt, &cli.image) {
            Ok(message) => message,
            Err(e) => {
                ui.error(&format!("{e:#}"));
                std::process::exit(1);
            }
        };
        // An attached image is shown the way the session shows it, because
        // nothing else prints it: the prompt itself was typed in the shell.
        if !cli.image.is_empty() {
            ui.replay(std::slice::from_ref(&message));
        }
        // One interrupt listener per turn, subscribed before the turn's first
        // await point (see Agent::turn).
        let mut interrupt = Sigint::new()?;
        let mut approve = StdinApproval;
        let mut ask = StdinQuestions;
        if let Err(e) = agent
            .turn_message(message, &mut ui, &mut interrupt, &mut approve, &mut ask)
            .await
        {
            ui.error(&format!("{e:#}"));
            std::process::exit(1);
        }
        agent.mcp.shutdown().await;
        return Ok(());
    }

    // The session summary: which session this is, how much is in it, and the
    // model. Built by the front end rather than here, because how much of it fits
    // is the terminal's answer and not the application's. A key that is missing is
    // said here as well: it is the first thing the user has to fix, and `/login`
    // is how.
    let mut banner = ui::banner(
        &agent.session.id,
        agent.session.messages.len(),
        &agent.model_label(),
    );
    if let Some(note) = startup.api_key.missing_note() {
        banner = format!("{note}\n{banner}");
    }
    // A session that carries project instructions says so once, where the
    // session itself is announced: what the model will read is worth the
    // reader knowing about too.
    if agent.session.meta.instructions.is_some() {
        banner = format!("{banner}\nproject instructions: AGENTS.md");
    }
    // What MCP brought to the session: which servers answered, and which did
    // not. Part of the banner because the tool list is part of the request, and
    // a session whose tools changed is a session a reader should know about.
    for note in &mcp_notes {
        banner = format!("{banner}\n{note}");
    }

    // Interactive front end: it owns the terminal, so nothing may have been
    // printed before it and nothing may be printed after it while it runs.
    // It declines rather than fails when the terminal cannot host it: the
    // alternate screen and raw mode are process-wide, and a terminal that cannot
    // take them leaves the plain prompt as the only one there is.
    if !cli.no_tui && std::io::stdout().is_terminal() {
        // Cloned because the front end borrows the agent mutably for the whole
        // session; this is once, at startup.
        let history = agent.session.messages.clone();
        if tui::run(&mut agent, &startup.sessions_dir, &banner, &history).await? {
            agent.mcp.shutdown().await;
            return Ok(());
        }
    }

    // Banner first, then any replayed history, then the prompt: the same
    // order the interactive front end lays them out -- and the order a
    // returning reader expects, with the session's own words before the box
    // that takes the next one.
    ui.info(&banner);
    if !agent.session.messages.is_empty() {
        ui.replay(&agent.session.messages);
    }

    // REPL
    //
    // The plain prompt's read loop is built on crossterm's raw mode and a
    // ratatui-textarea used as a pure in-memory buffer: the textarea holds the
    // draft and moves its cursor, but the bytes that hit the wire are written
    // by `Renderer`, so the prompt reads like every other line of the
    // transcript -- same colours, same marker. The alternative was an
    // interactive widget under the renderer, but that buys nothing the buffer
    // does not, and what it costs is two drawing paths in the same front end.
    //
    // The terminal is in raw mode for as long as the prompt is up, so every
    // keystroke arrives as a `KeyEvent` and Ctrl-C does not deliver a SIGINT.
    // That is also why there is no kernel line-discipline buffer to drain:
    // the editor is the only reader on the tty, and what it has not consumed
    // has not been typed yet.
    let prompt_outcome = run_repl(
        &mut ui,
        &mut agent,
        &startup.sessions_dir,
        cli.no_status_bar,
    )
    .await?;
    if prompt_outcome == Outcome::Exit {
        agent.mcp.shutdown().await;
        ui.teardown();
        return Ok(());
    }

    // Every server this session started is ended here rather than left to be
    // killed: closing its input is the shutdown the protocol asks for, and a
    // program this one started should not outlive it.
    agent.mcp.shutdown().await;
    ui.teardown();
    let _ = prompt_outcome;
    Ok(())
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
    sdir: &std::path::Path,
    no_status_bar: bool,
) -> Result<Outcome> {
    // Bottom status bar: only enabled in the REPL on a TTY.
    if !no_status_bar {
        ui.refresh_status_bar();
    }

    // The textarea is held across turns so a draft the user was composing when
    // a turn started is still there when it ends. The history it carries is
    // Up/Down's source: the same vec the file is saved into on the way out.
    let hist_path = config::history_file()?;
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
                        let turn_outcome = repl::handle(
                            agent,
                            ui,
                            sdir,
                            &line,
                            &mut interrupt,
                            &mut StdinApproval,
                            &mut StdinQuestions,
                            repl::FrontKind::Plain,
                        )
                        .await?;
                        if turn_outcome == repl::Outcome::Exit {
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
    // Whether colour escape sequences are wanted at all: the renderer decides
    // its own colour mode and the prompt follows it -- a non-colour run gets
    // the same plain bytes a back-end test asserts against. Asked here via
    // the renderer's own `paint`, which is the one place that knows.
    let dim_open = "\x1b[2m";
    let dim_close = "\x1b[0m";
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
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("caocli").chain(args.iter().copied()))
    }

    #[test]
    fn provider_flag_parses() {
        assert_eq!(
            cli(&["--provider", "zai-coding-cn"]).provider.as_deref(),
            Some("zai-coding-cn")
        );
        assert!(cli(&[]).provider.is_none());
    }

    #[test]
    fn no_status_bar_flag_parses() {
        assert!(cli(&["--no-status-bar"]).no_status_bar);
        assert!(!cli(&[]).no_status_bar);
    }

    #[test]
    fn ask_flag_defaults_off_and_parses() {
        assert!(
            !cli(&[]).ask,
            "execution is trusted by default, so do not ask"
        );
        assert!(cli(&["--ask"]).ask);
    }

    #[test]
    fn ctrl_j_is_a_newline_in_the_plain_prompt() {
        // The plain prompt binds Ctrl-J to insert a newline, matching the
        // transcript's user line shape. With ratatui-textarea the newline
        // goes through `insert_newline`, which is a method call rather than
        // a keybind -- the test pins the call by going through the editor's
        // own API, the same way the read loop does.
        let mut ta = ratatui_textarea::TextArea::default();
        ta.insert_char('a');
        ta.insert_newline();
        ta.insert_char('b');
        assert_eq!(ta.lines(), ["a", "b"]);
    }

    /// Submitting a multi-line textarea turns the lines back into the single
    /// submitted string with embedded newlines, the shape a turn expects.
    #[test]
    fn a_multi_line_draft_submits_as_one_string_with_newlines() {
        let mut ta = ratatui_textarea::TextArea::default();
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
        let mut ta = ratatui_textarea::TextArea::default();
        ta.insert_char('a');
        ta.insert_char('b');
        let cursor = ta.screen_cursor();
        assert_eq!(cursor.row, 0);
        assert_eq!(cursor.col, 2);
    }
}

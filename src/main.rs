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
mod tools;
mod types;
mod ui;

use std::io::IsTerminal;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui_textarea::TextArea;

use crate::agent::{Agent, Approval};
use crate::api::Client;
use crate::cli::Cli;
use crate::session::{Session, SessionMeta};
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

/// Meta for a new session: the provider is the one this run selected, and the
/// model comes from `--model` — which may name a provider of its own as
/// `<provider>/<modelid>` — otherwise it is that provider's default. The
/// workspace's project instructions (its AGENTS.md files) are read here, once,
/// and frozen into the session: everything the session later sends is what it
/// stored, never what the files say by then.
fn fresh_meta(cli: &Cli, start: provider::Provider) -> Result<SessionMeta> {
    let (provider, model) = match &cli.model {
        Some(spec) => provider::model_spec(spec, start.id)?,
        None => (start, start.default_model().to_string()),
    };
    // Checked against the provider the session will run on — which `--model`
    // may name, independently of the run's own choice.
    if let Some(e) = &cli.effort {
        provider.validate_effort(e)?;
    }
    Ok(SessionMeta {
        provider: Some(provider.id.to_string()),
        model,
        reasoning_effort: cli
            .effort
            .clone()
            .or_else(|| Some(provider.default_effort.to_string())),
        instructions: agents_md::load(),
    })
}

/// When resuming a session: parameters given explicitly on the command line
/// override the meta, while those not given keep the value stored in the session.
/// Returning true means the meta changed (a meta line must be appended).
fn apply_overrides(
    meta: &mut SessionMeta,
    cli: &Cli,
    fallback: provider::Provider,
) -> Result<bool> {
    let mut changed = false;
    // The provider a bare model id belongs to: the session's own, if it names
    // one, and this run's choice otherwise.
    let current = match meta.provider.as_deref() {
        Some(id) => provider::provider(id)?,
        None => fallback,
    };
    match (&cli.provider, &cli.model) {
        // An explicit model settles the provider too:
        // `--model zai-coding-cn/glm-5.3` needs no `--provider` beside it, and a
        // bare id stays where it was.
        (_, Some(spec)) => {
            let id = cli.provider.as_deref().unwrap_or(current.id);
            let (chosen, model) = provider::model_spec(spec, id)?;
            if meta.provider.as_deref() != Some(chosen.id) {
                meta.provider = Some(chosen.id.to_string());
                changed = true;
            }
            if meta.model != model {
                meta.model = model;
                changed = true;
            }
        }
        // An explicit provider switch without a model: follow that provider's
        // default model, so the old model name is not sent to the new backend.
        (Some(id), None) => {
            let chosen = provider::provider(id)?;
            if meta.provider.as_deref() != Some(chosen.id) || meta.model != chosen.default_model() {
                meta.provider = Some(chosen.id.to_string());
                meta.model = chosen.default_model().to_string();
                changed = true;
            }
        }
        (None, None) => {}
    }
    // The provider the session ends up on: the one `--model`/`--provider` chose
    // above, or the session's own when neither was given.
    let on = match meta.provider.as_deref() {
        Some(id) => provider::provider(id)?,
        None => current,
    };
    // The effort, when given, is checked against that provider.
    if let Some(e) = &cli.effort {
        on.validate_effort(e)?;
    }
    if let Some(e) = &cli.effort
        && meta.reasoning_effort.as_deref() != Some(e.as_str())
    {
        meta.reasoning_effort = Some(e.clone());
        changed = true;
    }
    // A tier the provider does not offer is not carried into it: the session
    // would send a value this backend may reject, and show a tier it is not
    // running on. See `Provider::fit_effort`.
    let fitted = on.fit_effort(meta.reasoning_effort.as_deref());
    if fitted != meta.reasoning_effort {
        meta.reasoning_effort = fitted;
        changed = true;
    }
    Ok(changed)
}

/// Outcome of the REPL loop. The plain prompt is single-threaded and never
/// shares this with anything else, so the value is local to the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Exit,
}

async fn run(cli: Cli) -> Result<()> {
    let mut ui = Renderer::new();
    let sdir = config::sessions_dir()?;

    if cli.list {
        for s in session::list(&sdir)? {
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

    // An image has to go somewhere, and only a turn carries one: a one-shot run
    // has `-p` to attach it to, and the interactive front ends have `/image`.
    if !cli.image.is_empty() && cli.prompt.is_none() {
        bail!(
            "--image attaches an image to the one-shot prompt (-p); \
             in the interactive front end, use /image <path> [text]"
        );
    }

    // The provider this run starts from: --provider, or the default. It settles
    // the endpoint of a new session and of one whose provider is not recorded;
    // `--model <provider>/<model>` and a resumed session's own meta name one too.
    let start = provider::provider(
        cli.provider
            .as_deref()
            .unwrap_or(provider::DEFAULT_PROVIDER),
    )?;

    // Session resolution priority: --resume > --continue > create new
    let mut session = if let Some(id) = &cli.resume {
        let path = sdir.join(format!("{id}.jsonl"));
        Session::load(&path).with_context(|| format!("failed to resume session {id}"))?
    } else if cli.cont {
        match session::latest(sdir.clone())? {
            Some(path) => Session::load(&path)?,
            None => Session::create(&sdir, fresh_meta(&cli, start)?)?,
        }
    } else {
        Session::create(&sdir, fresh_meta(&cli, start)?)?
    };

    if apply_overrides(&mut session.meta, &cli, start)? {
        session.set_meta(session.meta.clone())?;
    }

    // The provider this session runs on: the one its meta names, or this run's
    // choice for a session written before providers were recorded.
    let provider = match session.meta.provider.as_deref() {
        Some(id) => provider::provider(id)?,
        None => start,
    };

    // Authentication is settled before the session. A key that is missing is not
    // fatal where it can be stored -- `/login` exists for exactly that -- but a
    // one-shot run has no way to ask, so there it fails now rather than at the
    // first request.
    let mut missing_key = None;
    let api = match config::api_key(&provider) {
        Ok(key) => Client::for_provider(&provider, key)?,
        Err(e) if cli.prompt.is_some() => return Err(e),
        Err(e) => {
            missing_key = Some(format!("{e:#}"));
            Client::for_provider(&provider, String::new())?
        }
    };

    let mut agent = Agent::new(api, session, provider);
    agent.approval = Approval::from_flag(cli.ask);
    // The servers this workspace and the user's settings name, connected before
    // anything asks the model for a first answer: their tools are part of every
    // request, and a session that offered some of them would offer a different
    // prefix than the one it will send next.
    let workspace = std::env::current_dir().context("cannot determine the working directory")?;
    let mcp = mcp::Hub::connect(&workspace).await;
    let mcp_notes = mcp.notes();
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
    if let Some(note) = missing_key {
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
        if tui::run(&mut agent, &sdir, &banner, &history).await? {
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
    let prompt_outcome = run_repl(&mut ui, &mut agent, &sdir, cli.no_status_bar).await?;
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
mod tests {
    use super::*;
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::parse_from(std::iter::once("caocli").chain(args.iter().copied()))
    }

    /// A meta as a session file holds it, with the provider recorded.
    fn meta_of(provider: &str, model: &str) -> SessionMeta {
        SessionMeta {
            provider: Some(provider.into()),
            model: model.into(),
            reasoning_effort: None,
            instructions: None,
        }
    }

    #[test]
    fn no_cli_args_keeps_session_meta_untouched() {
        // Without --effort, the default tier max must not be written into an
        // existing session; the value stored in the session has to be kept.
        let mut meta = meta_of("deepseek", "deepseek-v4-pro");
        meta.reasoning_effort = Some("low".into());
        assert!(!apply_overrides(&mut meta, &cli(&[]), provider::DEEPSEEK).unwrap());
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn no_cli_args_keeps_the_sessions_own_provider() {
        // A resumed session runs where it ran before, whatever this run's
        // `--provider` default is: the meta is what the session is.
        let mut meta = meta_of("zai-coding-cn", "glm-5.3");
        assert!(!apply_overrides(&mut meta, &cli(&[]), provider::DEEPSEEK).unwrap());
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3");
    }

    #[test]
    fn explicit_model_overrides_only_model() {
        let mut meta = meta_of("deepseek", "deepseek-flash");
        meta.reasoning_effort = Some("high".into());
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "deepseek-v4-pro"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.provider.as_deref(), Some("deepseek"));
        assert_eq!(meta.reasoning_effort.as_deref(), Some("high"));
    }

    #[test]
    fn a_qualified_model_carries_its_own_provider() {
        // `--model zai-coding-cn/glm-5.3` needs no `--provider` beside it, and on
        // a session that is running elsewhere it moves the session.
        let mut meta = meta_of("deepseek", "deepseek-flash");
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "zai-coding-cn/glm-5.3"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3");
    }

    #[test]
    fn a_bare_model_stays_with_the_sessions_provider() {
        let mut meta = meta_of("zai-coding-cn", "glm-5.3-flash");
        assert!(
            apply_overrides(&mut meta, &cli(&["--model", "glm-4.6"]), provider::DEEPSEEK).unwrap()
        );
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-4.6");
    }

    #[test]
    fn a_switch_brings_only_a_tier_the_new_provider_serves() {
        // A resumed session carries its tier into the provider it moves to only
        // where that provider serves it. MiniMax's only control is a thinking
        // switch, so DeepSeek's `max` is replaced by MiniMax's own default
        // rather than sent as a tier this backend answers with a 400.
        let mut meta = meta_of("deepseek", "deepseek-flash");
        meta.reasoning_effort = Some("max".into());
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "minimax/MiniMax-M3"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.provider.as_deref(), Some("minimax"));
        assert_eq!(meta.model, "MiniMax-M3");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("on"));
        // And back: `off` is not a DeepSeek tier either.
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "deepseek/deepseek-v4-pro"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.reasoning_effort.as_deref(), Some("max"));
        // A tier both providers serve is the session's to keep.
        let mut meta = meta_of("deepseek", "deepseek-flash");
        meta.reasoning_effort = Some("low".into());
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--model", "zai-coding-cn/glm-5.3"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));
    }

    #[test]
    fn a_session_from_before_providers_were_recorded_takes_this_runs_choice() {
        // No provider in the meta: the run's provider is what the model is sent
        // to, which is how every session behaved before the field existed.
        let mut meta = SessionMeta {
            provider: None,
            model: "deepseek-v4-pro".into(),
            reasoning_effort: None,
            instructions: None,
        };
        assert!(!apply_overrides(&mut meta, &cli(&[]), provider::ZAI_CODING_CN).unwrap());
        assert_eq!(meta.provider, None, "nothing is invented for it");
        assert_eq!(meta.model, "deepseek-v4-pro");
    }

    #[test]
    fn switching_provider_without_model_follows_default_model() {
        let mut meta = meta_of("deepseek", "deepseek-flash");
        assert!(
            apply_overrides(
                &mut meta,
                &cli(&["--provider", "zai-coding-cn"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3-flash");

        // an explicit --model wins over the provider default
        let mut meta2 = meta_of("deepseek", "deepseek-flash");
        assert!(
            apply_overrides(
                &mut meta2,
                &cli(&["--provider", "zai-coding-cn", "--model", "glm-4.6"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta2.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta2.model, "glm-4.6");
    }

    #[test]
    fn switching_to_the_provider_the_session_already_names_changes_nothing() {
        // A meta line is a write to an append-only log, so the same choice must
        // not put another one in it.
        let mut meta = meta_of("zai-coding-cn", "glm-5.3-flash");
        assert!(
            !apply_overrides(
                &mut meta,
                &cli(&["--provider", "zai-coding-cn"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        // ... but a model that is not the provider's default does move.
        let mut meta2 = meta_of("zai-coding-cn", "glm-4.6");
        assert!(
            apply_overrides(
                &mut meta2,
                &cli(&["--provider", "zai-coding-cn"]),
                provider::DEEPSEEK
            )
            .unwrap()
        );
        assert_eq!(meta2.model, "glm-5.3-flash");
    }

    #[test]
    fn an_unknown_model_provider_is_rejected_before_anything_runs() {
        let mut meta = meta_of("deepseek", "deepseek-flash");
        let err = apply_overrides(&mut meta, &cli(&["--model", "nope/x"]), provider::DEEPSEEK)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown provider"), "{err}");
    }

    #[test]
    fn effort_overrides_stored_value() {
        let mut meta = meta_of("deepseek", "deepseek-v4-flash");
        meta.reasoning_effort = Some("high".into());
        assert!(
            apply_overrides(&mut meta, &cli(&["--effort", "low"]), provider::DEEPSEEK).unwrap()
        );
        assert_eq!(meta.reasoning_effort.as_deref(), Some("low"));

        // an identical value does not trigger a write
        assert!(
            !apply_overrides(&mut meta, &cli(&["--effort", "low"]), provider::DEEPSEEK).unwrap()
        );
    }

    #[test]
    fn fresh_meta_defaults() {
        let meta = fresh_meta(&cli(&[]), provider::DEEPSEEK).unwrap();
        assert_eq!(meta.provider.as_deref(), Some("deepseek"));
        assert_eq!(meta.model, provider::DEEPSEEK.default_model());
        assert_eq!(
            meta.reasoning_effort.as_deref(),
            Some(provider::DEEPSEEK.default_effort)
        );
    }

    #[test]
    fn fresh_meta_uses_provider_default_model() {
        let meta = fresh_meta(&cli(&[]), provider::ZAI_CODING_CN).unwrap();
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3-flash");
    }

    #[test]
    fn fresh_meta_carries_model_and_effort() {
        let meta = fresh_meta(
            &cli(&["--model", "deepseek-v4-pro", "--effort", "max"]),
            provider::DEEPSEEK,
        )
        .unwrap();
        assert_eq!(meta.model, "deepseek-v4-pro");
        assert_eq!(meta.reasoning_effort.as_deref(), Some("max"));
    }

    #[test]
    fn fresh_meta_takes_the_provider_a_qualified_model_names() {
        let meta = fresh_meta(
            &cli(&["--model", "zai-coding-cn/glm-5.3"]),
            provider::DEEPSEEK,
        )
        .unwrap();
        assert_eq!(meta.provider.as_deref(), Some("zai-coding-cn"));
        assert_eq!(meta.model, "glm-5.3");
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
    fn an_effort_the_provider_does_not_offer_is_rejected_where_it_lands() {
        // A new session is checked against the provider it will run on — the one
        // `--model` names, when it names one.
        let err = fresh_meta(&cli(&["--effort", "bogus"]), provider::DEEPSEEK)
            .unwrap_err()
            .to_string();
        assert!(err.contains("low | high | max"), "{err}");
        let err = fresh_meta(
            &cli(&["--model", "zai-coding-cn/glm-5.3", "--effort", "bogus"]),
            provider::DEEPSEEK,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("Z.AI Coding CN"), "{err}");
        // A resumed session is checked against the provider its meta names,
        // not against this run's choice.
        let mut meta = meta_of("zai-coding-cn", "glm-5.3");
        let err = apply_overrides(&mut meta, &cli(&["--effort", "bogus"]), provider::DEEPSEEK)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Z.AI Coding CN"), "{err}");
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

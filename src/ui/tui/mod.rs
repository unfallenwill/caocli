//! The interactive front end: the whole screen, with the session held in the
//! application rather than in the terminal.
//!
//! The alternate screen is what makes the layout the application's to decide, and
//! it is also what costs the terminal's own scrollback: nothing drawn here reaches
//! it, so the transcript is kept as cells and read back by moving a window over
//! them. The session log is the durable copy either way.
//!
//! It owns the terminal, so it also owns the two channels the machine needs: raw
//! mode clears `ISIG`, which means Ctrl-C arrives as a key event and there is no
//! SIGINT left to listen for, and the approval answer comes from the input box
//! rather than from stdin.
//!
//! A turn therefore runs *concurrently* with this event loop rather than inside
//! it. The machine's notifications become [`Notice`]s on a channel, the loop
//! applies them and redraws, and both channels above are answered from the same
//! loop. Nothing on the machine's side ever touches the terminal.

use std::io::{self, Stdout};
use std::path::Path;
use std::time::{Duration, Instant};

use crossterm::event::Event;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{mpsc, watch};

use crate::agent::Agent;
use crate::history;
use crate::repl;
use crate::types::Message;
use crate::ui::cell::{self, Cell};

mod channels;
mod edit;
mod input;
pub(crate) mod layout;
mod notice;
mod overlay;
mod panel;
mod picker;
mod queue;
mod render;
mod screen;
mod scroll;
mod state;
mod turn;
mod view;
mod working;

use channels::{LoopHalf, TurnHalf};
use input::Submitted;
use notice::{Notifier, drain};
use picker::{menu_for, offer_menu};
use screen::Screen;
use state::State;

// ----------------------------------------------------------------- polling ---

/// How long the loop will wait for a key before looking at everything else.
///
/// Short enough to feel immediate, long enough not to spin.
const TICK: Duration = Duration::from_millis(8);

/// Read a key if one is waiting, without blocking for longer than a tick.
///
/// This is deliberately *not* a reader thread, and that is the whole reason the
/// loop is shaped the way it is: a reader parked in `event::read` would own the
/// handle, and a turn running in this one would then be waiting on a thread that
/// nothing else can wake. A read that never returns is a front end that never
/// redraws.
fn poll_key(timeout: Duration) -> io::Result<Option<Event>> {
    if crossterm::event::poll(timeout)? {
        Ok(Some(crossterm::event::read()?))
    } else {
        Ok(None)
    }
}

// ------------------------------------------------------------ event loop ---

/// Run the interactive front end until the user leaves.
///
/// `banner` and `history` are what the plain front end prints before its first
/// prompt; here they are the first thing in the transcript, so a resumed session
/// reads the same way either way.
/// Returns `Ok(false)` when the terminal will not take raw mode and the alternate
/// screen, which is the caller's signal to fall back to the plain front end rather
/// than to fail. `Ok(true)` means the front end ran and the session is over.
pub async fn run(
    agent: &mut Agent,
    sdir: &Path,
    banner: &str,
    history: &[Message],
) -> anyhow::Result<bool> {
    let mut screen = match Screen::enter() {
        Ok(screen) => screen,
        Err(_) => return Ok(false),
    };
    let history_path = crate::config::history_file()?;
    screen.state.edit.history = history::load(&history_path);
    screen.state.view.status.set_model(&agent.model_label());
    screen.state.view.status.set_effort(agent.effort_label());
    screen.state.show(Cell::Notice(banner.to_owned()));
    screen
        .state
        .view
        .transcript
        .extend(cell::from_messages(history));

    let (machine_tx, machine_rx) = mpsc::unbounded_channel();
    let (app_tx, app_rx) = mpsc::unbounded_channel();
    let (secret_tx, secret_rx) = mpsc::unbounded_channel();
    let (gate_tx, gates) = mpsc::unbounded_channel();
    let (question_tx, questions) = mpsc::unbounded_channel();
    let mut handle = Notifier {
        machine: machine_tx,
        app: app_tx,
        secret: secret_tx,
    };
    let mut loop_half = LoopHalf {
        machine: machine_rx,
        app: app_rx,
        secret: secret_rx,
        gates,
        questions,
    };
    let turn_half = TurnHalf {
        gate_tx,
        question_tx,
    };

    let result: anyhow::Result<()> = 'session: loop {
        // Idle: draw, then wait for something to submit.
        let line = match idle_line(&mut screen, &mut loop_half)? {
            Some(line) => line,
            None => break 'session Ok(()),
        };
        // Everything from here runs without returning to the keyboard in between,
        // and the next line is the head of the queue: what was asked for while the
        // last one ran is what comes after it. That is the whole point of the
        // queue -- an interruption ends the turn, not the sequence.
        let mut line = line;
        loop {
            if line.is_empty() {
                break 'session Ok(());
            }
            screen.state.submit(&line);
            // A menu stands between the line and the turn: the command still
            // needs an argument, and the front end that can offer the
            // possibilities does. The choice is typed, so the queue waits behind
            // it -- a picker answered by a line that is still in the queue would
            // answer itself.
            if let Some(menu) = menu_for(&line)
                && offer_menu(&mut screen, menu, agent, sdir)?
            {
                break;
            }
            let outcome = run_turn(
                agent,
                &mut handle,
                &mut screen,
                &mut loop_half,
                &turn_half,
                sdir,
                &line,
            )
            .await?;
            match outcome {
                Ok(repl::Outcome::Exit) => break 'session Ok(()),
                Ok(repl::Outcome::Continue) => {}
                Err(e) => {
                    // `repl::handle` reports turn failures itself; anything
                    // escaping it is a session-level problem worth showing.
                    screen.state.show(Cell::Failure(format!("{e:#}")));
                }
            }
            // Next: the head of the queue, or back to the keyboard.
            match screen.state.dequeue() {
                Some(next) => line = next,
                None => break,
            }
        }
    };

    screen.leave();
    // Written on the way out rather than per line: the file is small, and a
    // rewrite per keystroke would be work for nothing.
    if let Err(e) = history::save(&history_path, &screen.state.edit.history) {
        screen.state.show(Cell::Failure(format!("{e:#}")));
    }
    result?;
    Ok(true)
}

/// Draw and wait at the prompt until the user submits a line. `None` says they
/// asked to leave.
fn idle_line(
    screen: &mut Screen<CrosstermBackend<Stdout>>,
    channels: &mut LoopHalf,
) -> anyhow::Result<Option<String>> {
    if let Err(e) = screen.draw_if_changed() {
        return Err(e.into());
    }
    loop {
        drain_channels(&mut screen.state, channels);
        screen.draw_if_changed()?;
        match poll_key(TICK)? {
            Some(event) => match screen.state.key(event) {
                Submitted::Line => return Ok(Some(screen.state.take_line())),
                Submitted::Exit => return Ok(None),
                Submitted::Nothing => {}
            },
            None => continue,
        }
    }
}

/// Apply every notice, gate question and panel question that has arrived on
/// the loop's channels. Called from both the per-tick loop inside `run_turn`
/// and after the turn ends, because notifications sent after the turn's last
/// drain are still queued when it resolves.
fn drain_channels(state: &mut State, channels: &mut LoopHalf) {
    for notice in drain(&mut channels.machine) {
        state.apply(notice);
    }
    for notice in drain(&mut channels.app) {
        state.apply_app(notice);
    }
    for asked in drain(&mut channels.secret) {
        state.open_secret(asked.prompt, asked.reply);
    }
    for reply in drain(&mut channels.gates) {
        state.open_question(reply);
    }
    for asked in drain(&mut channels.questions) {
        state.open_panel(asked.questions, asked.reply);
    }
}

/// Run one line through the machine while the keyboard keeps working: Ctrl-C
/// can reach it, a next line can be queued behind it, and an approval question
/// is answered from the box.
///
/// The outer result is the session's -- a screen or a keyboard that has gone
/// away; the inner one is the turn's, which the session survives. The turn is
/// polled *inside* the select below, so its last notifications are sent after
/// the loop's last drain and are still queued when it resolves; returning
/// without draining again would leave the tail of the turn to open one of its
/// own.
async fn run_turn(
    agent: &mut Agent,
    handle: &mut Notifier,
    screen: &mut Screen<CrosstermBackend<Stdout>>,
    channels: &mut LoopHalf,
    turn: &TurnHalf,
    sdir: &Path,
    line: &str,
) -> anyhow::Result<anyhow::Result<repl::Outcome>> {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut interrupt = channels::CtrlC(cancel_rx);
    let mut approve = channels::Gate {
        tx: turn.gate_tx.clone(),
    };
    let mut ask = channels::Questions {
        tx: turn.question_tx.clone(),
    };
    let turn = repl::handle(
        agent,
        handle,
        sdir,
        line,
        &mut interrupt,
        &mut approve,
        &mut ask,
        repl::FrontKind::Tui,
    );
    tokio::pin!(turn);
    screen.state.begin_turn(Instant::now());
    let outcome = loop {
        // Keys are read here, never on another thread: see `poll_key`.
        if let Some(event) = poll_key(TICK)? {
            screen.state.key_while_working(event, &cancel_tx);
        }
        drain_channels(&mut screen.state, channels);
        screen.state.tick_activity(Instant::now());
        screen.draw_if_changed()?;
        tokio::select! {
            outcome = &mut turn => break outcome,
            // Nothing else to wait on: the tick above paces the loop, and this
            // branch only gives the turn a real waker so that it is driven by
            // readiness rather than by the tick.
            _ = tokio::time::sleep(TICK) => {}
        }
    };
    drain_channels(&mut screen.state, channels);
    screen.state.end_turn();
    screen.state.close_question();
    screen.state.close_panel();
    screen.commit();
    Ok(outcome)
}

#[cfg(test)]
mod tests;

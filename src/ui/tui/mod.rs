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

use std::future::Future;
use std::io::{self, Stdout};
use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, Instant};

use crossterm::event::Event;
use tokio::sync::{mpsc, oneshot, watch};

use crate::agent::Agent;
use crate::config;
use crate::history;
use crate::repl;
use crate::session;
use crate::types::{Message, ToolCall};
use crate::ui::{Approve, Interrupt};

use super::cell::{self, Cell};

mod input;
mod layout;
mod notice;
mod screen;

use ratatui::backend::CrosstermBackend;
use screen::Screen;
mod paint;
mod picker;
mod state;

use input::Submitted;
use notice::{Notice, Notifier, drain};
use picker::{Choosing, choice_rows};

// ------------------------------------------------------------- activity ---

// ---------------------------------------------------------------- notices ---

// --------------------------------------------------------------- channels ---

/// Cancellation as the event loop delivers it: Ctrl-C sets the watch and every
/// await point in the turn races against [`CtrlC::wait`].
///
/// This is why the cancel channel is a parameter of `Agent::turn`: in raw mode
/// there is no SIGINT to subscribe to, so a key event is the only source there is.
struct CtrlC(watch::Receiver<bool>);

impl Interrupt for CtrlC {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        let rx = &mut self.0;
        Box::pin(async move {
            // `borrow_and_update` first: a cancel that arrived before this wait
            // was created must still be seen, which a bare `changed()` would miss.
            while !*rx.borrow_and_update() {
                if rx.changed().await.is_err() {
                    // The front end is gone; never fire again.
                    std::future::pending::<()>().await;
                }
            }
        })
    }
}

/// The approval gate, answered from the input box.
///
/// The question goes to the event loop as a reply handle; the next line the user
/// submits is the answer. Denial is the default, including when the front end has
/// gone away mid-ask.
struct Ask {
    tx: mpsc::UnboundedSender<oneshot::Sender<bool>>,
}

impl Approve for Ask {
    fn ask(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = bool> + '_>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            if self.tx.send(reply).is_err() {
                return false;
            }
            answer.await.unwrap_or(false)
        })
    }
}

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

// ----------------------------------------------------------------- screen ---

// ------------------------------------------------------------ scroll view ---

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
    screen.state.history = history::load(&history_path);
    screen.state.status.set_model(&agent.model_label());
    screen.state.status.set_effort(agent.effort_label());
    screen.state.show(Cell::Notice(banner.to_owned()));
    screen.state.transcript.extend(cell::from_messages(history));

    let (tx, notices) = mpsc::unbounded_channel();
    let (ask_tx, asked) = mpsc::unbounded_channel();
    let mut handle = Notifier { tx };
    let mut channels = Channels {
        notices,
        asked,
        ask_tx,
    };

    let result: anyhow::Result<()> = 'session: loop {
        // Idle: draw, then wait for something to submit.
        let line = match idle_line(&mut screen, &mut channels)? {
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
            let outcome =
                run_turn(agent, &mut handle, &mut screen, &mut channels, sdir, &line).await?;
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
    if let Err(e) = history::save(&history_path, &screen.state.history) {
        screen
            .state
            .transcript
            .push(Cell::Failure(format!("{e:#}")));
    }
    result?;
    Ok(true)
}

/// The menus the front end can offer where the plain front end can only print.
///
/// `/resume` with nothing to resume is a request for the list rather than a
/// command to run; `/login`, `/model` and `/effort` are commands whose argument
/// is a row of a menu. The chosen row is submitted as the very line the plain
/// prompt would have been given, so the switching itself is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Menu {
    /// The sessions there are to switch to.
    Sessions,
    /// The providers a key can be stored for.
    Login,
    /// The models the session can switch to.
    Model,
    /// The reasoning effort tiers the provider in use accepts.
    Effort,
}

/// The command a line names whose answer is a menu rather than a turn.
fn menu_for(line: &str) -> Option<Menu> {
    match line.trim() {
        "/resume" => Some(Menu::Sessions),
        "/login" => Some(Menu::Login),
        "/model" => Some(Menu::Model),
        "/effort" => Some(Menu::Effort),
        _ => None,
    }
}

/// Open the menu a command asked for. `Ok(true)` says the menu is up and the
/// answer comes from the keyboard; `Ok(false)` says there was nothing to offer
/// -- an empty session directory -- and the line runs as it would have.
fn offer_menu(
    screen: &mut Screen<CrosstermBackend<Stdout>>,
    menu: Menu,
    agent: &Agent,
    sdir: &Path,
) -> anyhow::Result<bool> {
    match menu {
        Menu::Sessions => Ok(screen.state.open_sessions(&session::list(sdir)?)),
        Menu::Login => Ok(screen
            .state
            .open_choices(Choosing::Provider, choice_rows(config::provider_choices()))),
        Menu::Model => Ok(screen.state.open_choices(
            Choosing::Model,
            choice_rows(config::model_menu(&agent.model_label())),
        )),
        Menu::Effort => Ok(screen.state.open_choices(
            Choosing::Effort,
            choice_rows(config::effort_menu(&agent.provider(), agent.effort_label())),
        )),
    }
}

/// The channels the event loop reads and answers while it runs.
///
/// The machine's notifications arrive on one, the approval questions on
/// another, and the sender of the second is what the machine holds.
struct Channels {
    notices: mpsc::UnboundedReceiver<Notice>,
    asked: mpsc::UnboundedReceiver<oneshot::Sender<bool>>,
    ask_tx: mpsc::UnboundedSender<oneshot::Sender<bool>>,
}

/// Draw and wait at the prompt until the user submits a line. `None` says they
/// asked to leave.
fn idle_line(
    screen: &mut Screen<CrosstermBackend<Stdout>>,
    channels: &mut Channels,
) -> anyhow::Result<Option<String>> {
    if let Err(e) = screen.draw_if_changed() {
        return Err(e.into());
    }
    loop {
        for notice in drain(&mut channels.notices) {
            screen.state.apply(notice);
        }
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
    channels: &mut Channels,
    sdir: &Path,
    line: &str,
) -> anyhow::Result<anyhow::Result<repl::Outcome>> {
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut interrupt = CtrlC(cancel_rx);
    let mut approve = Ask {
        tx: channels.ask_tx.clone(),
    };
    let turn = repl::handle(agent, handle, sdir, line, &mut interrupt, &mut approve);
    tokio::pin!(turn);
    screen.state.begin_turn(Instant::now());
    let outcome = loop {
        // Keys are read here, never on another thread: see `poll_key`.
        if let Some(event) = poll_key(TICK)? {
            screen.state.key_while_working(event, &cancel_tx);
        }
        for notice in drain(&mut channels.notices) {
            screen.state.apply(notice);
        }
        for reply in drain(&mut channels.asked) {
            screen.state.open_question(reply);
        }
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
    for notice in drain(&mut channels.notices) {
        screen.state.apply(notice);
    }
    for reply in drain(&mut channels.asked) {
        screen.state.open_question(reply);
    }
    screen.state.end_turn();
    screen.state.close_question();
    screen.commit();
    Ok(outcome)
}

#[cfg(test)]
mod tests;

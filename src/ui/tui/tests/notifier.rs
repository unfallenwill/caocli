//! Tests for the contract-level glue.
//!
//! `repl::handle` is written against the `Front` trait, and this front end
//! must keep satisfying it alongside the plain one: a compile-time guard,
//! not a behavioural one. The notifier is the same: it is what the machine
//! talks to, and the only contract is that every notice ends up somewhere
//! a loop is reading from. `menu_for` is the dispatch table that says which
//! of `/login`, `/model`, `/effort` and `/resume` opens a menu rather than
//! running as a turn.

use std::time::Duration;
use tokio::sync::mpsc;

use super::super::notice::{Notice, Notifier};
use super::super::{Menu, menu_for};
use crate::ui::Renderer;
use crate::ui::{Cancel, Front, Ui};

#[test]
fn both_front_ends_satisfy_the_handle_the_repl_needs() {
    // Compile-time guard: `repl::handle` is written against `Front`, and the
    // interactive front end must keep satisfying it alongside the plain one.
    fn assert_front<T: Front>() {}
    assert_front::<Notifier>();
    assert_front::<Renderer>();
}

#[test]
fn the_notifier_reaches_the_loop_through_the_channel() {
    // The `Ui` side must not touch the terminal: everything it is told has to
    // come out as a notice.
    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut n = Notifier { tx };
    n.content_delta("hi");
    assert!(matches!(rx.try_recv(), Ok(Notice::Content(s)) if s == "hi"));
}

#[test]
fn a_menu_command_names_its_menu() {
    assert_eq!(menu_for("/resume"), Some(Menu::Sessions));
    assert_eq!(menu_for(" /login "), Some(Menu::Login));
    assert_eq!(menu_for("/model"), Some(Menu::Model));
    assert_eq!(menu_for("/effort"), Some(Menu::Effort));
}

#[test]
fn anything_else_runs_as_a_turn_and_not_as_a_menu() {
    assert_eq!(menu_for("say something"), None);
    assert_eq!(menu_for("/help"), None);
    // A menu command with an argument already has what it needs.
    assert_eq!(menu_for("/resume abc123"), None);
    assert_eq!(menu_for("/model glm-4.6"), None);
    assert_eq!(menu_for("/effort low"), None);
}

/// The cancel source this front end hands the turn is the one it built around
/// Ctrl-C arriving as a key (raw mode leaves no SIGINT to listen for), and what
/// it delivers through is a watch. A key that lands between two waits must not
/// be lost: while a turn runs, this loop is between waits most of the time.
#[tokio::test]
async fn a_cancel_between_two_waits_is_not_lost() {
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let mut cancel = super::super::CtrlC(cancel_rx);

    // The first wait is polled once and dropped, which is what the end of a
    // select does with it.
    let waiting = cancel.wait();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), waiting)
            .await
            .is_err(),
        "nothing was cancelled yet"
    );

    // The key arrives while nothing is waiting for it.
    cancel_tx.send(true).unwrap();

    let waiting = cancel.wait();
    assert!(
        tokio::time::timeout(Duration::from_millis(1), waiting)
            .await
            .is_ok(),
        "the cancel must still be seen by the next wait"
    );
}

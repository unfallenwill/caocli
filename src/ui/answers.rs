//! How the plain front end answers the machine's two questions.
//!
//! It has no event loop of its own: the terminal it sits in is not in raw mode, so
//! Ctrl-C arrives as SIGINT and the approval answer arrives on stdin. Both are
//! built here rather than by the interpreter, because which answer source exists
//! is a property of the front end that is running — the one that owns the screen
//! answers both from its own loop instead.

use std::future::Future;
use std::pin::Pin;

use anyhow::Result;

use crate::types::ToolCall;

use super::contract::{Approve, Interrupt};

/// The real SIGINT listener. It subscribes to tokio's watch at construction time
/// (no poll needed), so a signal arriving at any moment from the start of the
/// turn to its end cannot be lost to a "no listener" gap.
pub struct Sigint(
    #[cfg(unix)] tokio::signal::unix::Signal,
    #[cfg(not(unix))] tokio::signal::windows::CtrlC,
);

impl Sigint {
    /// Subscribe to SIGINT. The caller constructs one per turn, before the first
    /// await point, so no signal can arrive before the subscription exists.
    pub fn new() -> Result<Self> {
        #[cfg(unix)]
        let listener = Self(tokio::signal::unix::signal(
            tokio::signal::unix::SignalKind::interrupt(),
        )?);
        #[cfg(not(unix))]
        let listener = Self(tokio::signal::windows::ctrl_c()?);
        Ok(listener)
    }
}

impl Interrupt for Sigint {
    fn wait(&mut self) -> Pin<Box<dyn Future<Output = ()> + '_>> {
        Box::pin(async {
            self.0.recv().await;
        })
    }
}

/// One line of stdin per question, denial by default: the plain front end's
/// answer source.
pub struct StdinApproval;

impl Approve for StdinApproval {
    fn ask(&mut self, _call: &ToolCall) -> Pin<Box<dyn Future<Output = bool> + '_>> {
        Box::pin(async {
            // The blocking read is wrapped in spawn_blocking: the global stdin
            // buffer is shared across calls, so surplus type-ahead is not lost.
            // (Cost: on cancellation a blocked thread lingers and swallows the
            // first line typed afterwards -- a known trade-off.)
            tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                let read = std::io::stdin().read_line(&mut line);
                let line = line.trim();
                read.map(|n| n > 0).unwrap_or(false)
                    && (line.eq_ignore_ascii_case("y") || line.starts_with('y'))
            })
            .await
            .unwrap_or(false)
        })
    }
}

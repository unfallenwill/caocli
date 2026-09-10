//! The terminal, as one capability: the only place in this subtree that touches
//! the process's own terminal.
//!
//! Everything above this file is arithmetic over facts -- how wide the terminal
//! is, whether it can take escape sequences, whether its echo is off -- and both
//! facts and the acting on them used to be spread through the renderer, which is
//! why the lines that actually call `ioctl` and `tcsetattr` were the ones no test
//! could reach. Here they are behind one trait, so the decisions built on them
//! can be tested against a stand-in and only these leaves need a real terminal.
//!
//! Same shape as the machine's `Cancel` and `Approve`: the front end supplies
//! the process-global thing, and a double supplies it in a test.
//!
//! **What a unit test cannot reach**: `RealTerminal` is the leaves themselves --
//! `ioctl`, `isatty`, `tcgetattr`, `tcsetattr`, a blocking read of stdin. A
//! test's stdout is a pipe, so those branches only run under a pty
//! (`script -qec "..." /dev/null`, or `scripts/tui_smoke.py` for the screen-owning
//! front end); the decisions they feed are covered against a stand-in.

use std::future::Future;
use std::pin::Pin;

/// What a front end needs from the terminal it is attached to.
pub(crate) trait Terminal {
    /// Terminal size (rows, cols). None when there is no terminal to measure.
    fn size(&self) -> Option<(u16, u16)>;

    /// Whether stdout is a terminal. A pipe or a redirected file is not, and
    /// nothing that addresses the screen may be written to one.
    fn is_tty(&self) -> bool;

    /// Whether the terminal says it cannot do cursor addressing at all
    /// (`TERM=dumb`), which is a terminal that will not take the scroll region
    /// the status bar sets up.
    fn is_dumb(&self) -> bool;

    /// Turn the input's echo off for as long as the returned value lives, and
    /// back on when it is dropped. None when there is no terminal to silence,
    /// which is not a failure: the answer is still read, it is just not hidden.
    fn echo_off(&self) -> Option<Box<dyn Echo>>;

    /// One line, without its line ending. None is an end of input.
    fn read_line(&self) -> Pin<Box<dyn Future<Output = Option<String>> + '_>>;

    /// Whether color is wanted here: the terminal can show it and `NO_COLOR`
    /// does not say otherwise.
    ///
    /// A question about the environment rather than about one escape sequence,
    /// and defaulted because it is the one capability a stand-in nearly always
    /// answers the same way.
    fn wants_color(&self) -> bool {
        self.is_tty() && std::env::var_os("NO_COLOR").is_none()
    }
}

/// Held for as long as the terminal's echo should stay off: dropping it puts the
/// echo back.
///
/// A value rather than a pair of calls, because the answer is read on a future
/// that can end anywhere -- including on an early return -- and a terminal left
/// quiet is a terminal that has stopped showing what is typed into it.
pub(crate) trait Echo {}

/// The terminal this process is attached to.
pub(crate) struct RealTerminal;

impl Terminal for RealTerminal {
    /// Terminal size (rows, cols) as the kernel reports it. None when the ioctl
    /// fails or reports a degenerate size, and on a platform without it.
    #[cfg(unix)]
    fn size(&self) -> Option<(u16, u16)> {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        // SAFETY: STDOUT_FILENO is a valid fd, and ws has the winsize layout that
        // TIOCGWINSZ requires
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
        (rc == 0 && ws.ws_row > 0 && ws.ws_col > 0).then_some((ws.ws_row, ws.ws_col))
    }

    #[cfg(not(unix))]
    fn size(&self) -> Option<(u16, u16)> {
        None
    }

    fn is_tty(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdout().is_terminal()
    }

    fn is_dumb(&self) -> bool {
        std::env::var("TERM").is_ok_and(|t| t == "dumb")
    }

    #[cfg(unix)]
    fn echo_off(&self) -> Option<Box<dyn Echo>> {
        /// The settings as they were, put back on drop.
        struct Saved(libc::termios);
        impl Echo for Saved {}
        impl Drop for Saved {
            fn drop(&mut self) {
                // SAFETY: `saved` came from tcgetattr on this same descriptor.
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
            }
        }

        let fd = libc::STDIN_FILENO;
        // SAFETY: fd is a valid descriptor and `saved` has the termios layout
        // the call fills in; a failure leaves both untouched, and it is only
        // used when a touch is wanted.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            return None;
        }
        Some(Box::new(Saved(saved)))
    }

    /// Where termios does not exist there is nothing to silence and nothing to
    /// restore. The value still exists, so the asker stays one code on every
    /// platform: the answer is read the same way, and what the terminal keeps
    /// showing while it is typed is the price of a key on a machine without a
    /// termios -- the same decline-and-still-work the plain front end accepts
    /// everywhere else it cannot take the terminal's full cooperation.
    #[cfg(not(unix))]
    fn echo_off(&self) -> Option<Box<dyn Echo>> {
        struct NothingToRestore;
        impl Echo for NothingToRestore {}
        Some(Box::new(NothingToRestore))
    }

    /// Read on a blocking task: the terminal's own line editing is what ends the
    /// line, and a runtime worker is not what should be waiting on it.
    fn read_line(&self) -> Pin<Box<dyn Future<Output = Option<String>> + '_>> {
        Box::pin(async {
            tokio::task::spawn_blocking(|| {
                let mut line = String::new();
                match std::io::stdin().read_line(&mut line) {
                    Ok(n) if n > 0 => Some(line.trim_end_matches(['\n', '\r']).to_owned()),
                    _ => None,
                }
            })
            .await
            .ok()
            .flatten()
        })
    }
}

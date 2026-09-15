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

use crate::ui::theme::Rgb;

/// The escape that asks a terminal what its background is, and the string
/// terminator that closes the reply.
///
/// `OSC 11 ; ? ST`, which every terminal that implements the query answers with
/// `OSC 11 ; rgb:RRRR/GGGG/BBBB ST`. Spelled with the `ST` terminator rather than
/// `BEL`, because `BEL` is a character a reply could contain and `ESC \` is not.
///
/// Unix only, like the round trip that asks it: [`RealTerminal::background`]
/// writes this and reads the answer with `termios` and `poll`. On a platform
/// where that round trip has no spelling there is nothing to ask with, so the
/// asker answers `None` -- the same "the terminal was not asked" every caller
/// already has a default for -- and this query, its deadline and the parse
/// below are unix-only with it.
#[cfg(unix)]
const BACKGROUND_QUERY: &str = "\x1b]11;?\x1b\\";

/// How long the reply is waited for, in milliseconds.
///
/// This is startup latency on terminals that do not implement the query, so it is
/// short: a terminal that answers at all answers in single-digit milliseconds, and
/// one that does not is not going to answer in a hundred. The cost of being wrong
/// is the default theme, which is the answer anyway.
#[cfg(unix)]
const BACKGROUND_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(60);

/// The colour in an `OSC 11` reply, or `None` when the reply is not one.
///
/// Split out from the plumbing so the spellings can be a test: the spec says
/// `rgb:RRRR/GGGG/BBBB` with one to four hex digits per channel, terminals
/// variously send `rgb:ff/ff/ff` and `#ffffff` and `#fff`, and a terminal that
/// does not implement the query often answers with something else entirely --
/// which has to be `None` rather than a colour.
#[cfg(unix)]
pub(crate) fn parse_osc11(reply: &str) -> Option<Rgb> {
    // The reply carries the query it answers; anything before it is none of our
    // business (a bracketed paste, a key the user pressed first).
    let body = reply.split_once("]11;").map(|(_, rest)| rest)?;
    let body = body
        .strip_suffix('\u{7}')
        .or_else(|| body.strip_suffix("\x1b\\"))
        .unwrap_or(body);
    if let Some(channels) = body.strip_prefix("rgb:") {
        let parts: Vec<&str> = channels.split('/').collect();
        let [r, g, b] = parts.as_slice() else {
            return None;
        };
        return Some(Rgb {
            r: scale_channel(r)?,
            g: scale_channel(g)?,
            b: scale_channel(b)?,
        });
    }
    let digits = body.strip_prefix('#')?;
    match digits.len() {
        3 => Some(Rgb {
            r: scale_channel(&digits[0..1])?,
            g: scale_channel(&digits[1..2])?,
            b: scale_channel(&digits[2..3])?,
        }),
        6 => Some(Rgb {
            r: scale_channel(&digits[0..2])?,
            g: scale_channel(&digits[2..4])?,
            b: scale_channel(&digits[4..6])?,
        }),
        _ => None,
    }
}

/// One channel's hex digits as eight bits, whatever width the terminal spelled
/// them at.
///
/// Every width is a fraction of its own maximum, so `f` and `ff` and `ffff` are
/// all full -- scaling the short form down instead of up is how a white terminal
/// comes out grey, which for this question means a light theme read as a dark
/// one.
#[cfg(unix)]
fn scale_channel(digits: &str) -> Option<u8> {
    if digits.is_empty() || digits.len() > 4 || !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    let max = 16u32.pow(digits.len() as u32) - 1;
    Some(((value * 255 + max / 2) / max) as u8)
}

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

    /// What the terminal says its background is, when it can be asked.
    ///
    /// `None` is the ordinary answer: most terminals do not implement the query,
    /// some that do are behind a multiplexer that swallows it, and a stand-in has
    /// no terminal to ask at all. So this is a question with an optional answer,
    /// and every caller has a default for the silence.
    ///
    /// Defaulted here rather than required, because "I cannot tell you" is not a
    /// special case a front end should have to write.
    fn background(&self) -> Option<Rgb> {
        None
    }

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

    /// Ask the terminal for its background with `OSC 11`.
    ///
    /// The one thing this process ever asks a terminal and waits for an answer
    /// to, so it is written to be defensible about it:
    ///
    /// - **Only when both ends are terminals.** A query written to a pipe is
    ///   bytes in somebody's file, and a reply read from one never comes.
    /// - **With a deadline.** [`BACKGROUND_TIMEOUT`] and then the answer is
    ///   `None`. A terminal that does not implement `OSC 11` says nothing at all
    ///   -- it is not an error, there is simply no reply -- so the read has to be
    ///   one that ends on its own.
    /// - **With the terminal put back.** `ECHO` and `ICANON` go off for the
    ///   round trip and back on afterwards, including on every early return: the
    ///   reply arrives before the newline the kernel would otherwise wait for,
    ///   and a crash in between must not leave the shell with no echo.
    /// - **When nothing else has answered.** The caller decides when to ask, and
    ///   `COLORFGBG` existing is a reason not to.
    ///
    /// The parse is [`parse_osc11`], which is where the interesting cases live;
    /// this function is the plumbing around it.
    #[cfg(unix)]
    fn background(&self) -> Option<Rgb> {
        use std::io::{Read, Write};

        if !self.is_tty() {
            return None;
        }
        {
            use std::io::IsTerminal;
            if !std::io::stdin().is_terminal() {
                return None;
            }
        }

        /// The terminal's settings, put back on drop -- the same bargain
        /// [`Echo`] makes, for the same reason.
        struct Raw(libc::termios);
        impl Drop for Raw {
            fn drop(&mut self) {
                // SAFETY: `saved` came from tcgetattr on this same descriptor.
                unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
            }
        }

        let fd = libc::STDIN_FILENO;
        // SAFETY: fd is a valid descriptor and `saved` has the termios layout
        // the call fills in.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut saved) } != 0 {
            return None;
        }
        let mut quiet = saved;
        quiet.c_lflag &= !(libc::ICANON | libc::ECHO);
        quiet.c_cc[libc::VMIN] = 0;
        quiet.c_cc[libc::VTIME] = 0;
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &quiet) } != 0 {
            return None;
        }
        let _raw = Raw(saved);

        let mut out = std::io::stdout();
        // The query and its terminator, flushed on its own: a query still sitting
        // in a buffer when the read starts is a deadline that expires looking at
        // a terminal that was never asked.
        if out.write_all(BACKGROUND_QUERY.as_bytes()).is_err() || out.flush().is_err() {
            return None;
        }

        let mut reply = Vec::new();
        let mut buf = [0u8; 64];
        let deadline = std::time::Instant::now() + BACKGROUND_TIMEOUT;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return None;
            }
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: pfd is one initialised pollfd and the count is 1.
            let ready = unsafe { libc::poll(&mut pfd, 1, left.as_millis() as libc::c_int) };
            if ready < 0 {
                return None;
            }
            if ready == 0 {
                // The deadline passed with no reply: the terminal does not answer
                // this question, which is the common case rather than a fault.
                return None;
            }
            let mut stdin = std::io::stdin();
            let n = stdin.read(&mut buf).ok()?;
            if n == 0 {
                return None;
            }
            reply.extend_from_slice(&buf[..n]);
            // A complete reply ends with the string terminator; the loop waits
            // for one rather than for a fixed number of bytes, because how a
            // terminal spells an RGB value is up to it.
            if let Some(color) = parse_osc11(&String::from_utf8_lossy(&reply)) {
                return Some(color);
            }
            if reply.len() > 128 {
                return None;
            }
        }
    }

    #[cfg(not(unix))]
    fn background(&self) -> Option<Rgb> {
        None
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

/// Gated with what it tests: `parse_osc11` is unix-only, so a platform without
/// the round trip has no parser to cover.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::ui::theme::Rgb;

    /// `parse_osc11` is the one piece of the OSC 11 round trip that does not
    /// need a terminal to run, and it is the one where a terminal's odd
    /// spelling is most likely to break what the rest of the install path has
    /// decided. Three formats the spec and the deployments together produce:
    /// `rgb:RRRR/GGGG/BBBB`, `#rrggbb`, and `#rgb`. Plus a `BEL` terminator
    /// instead of `ST` on terminals that close the reply differently.
    fn reply(body: &str) -> String {
        format!("\x1b]11;{body}")
    }

    /// The spec's spelling: `rgb:` followed by four-digit hex per channel, the
    /// channels separated by `/`. Every channel is the high end of eight bits
    /// (`f` is full) so a "low" value does not look like a dark pixel of a
    /// darker colour.
    #[test]
    fn parses_rgb_colon_with_full_hex() {
        assert_eq!(
            parse_osc11(&reply("rgb:ffff/ffff/ffff\x1b\\")),
            Some(Rgb::new(0xff, 0xff, 0xff))
        );
        assert_eq!(
            parse_osc11(&reply("rgb:0000/0000/0000\x1b\\")),
            Some(Rgb::new(0, 0, 0))
        );
        assert_eq!(
            parse_osc11(&reply("rgb:1234/5678/9abc\x1b\\")),
            Some(Rgb::new(0x12, 0x56, 0x9a))
        );
    }

    /// Short hex (`#rrggbb`) is what xterm sends, and the spec's per-channel
    /// shorthand is what some other terminals send. Both go through the same
    /// hex-digit-to-byte conversion.
    #[test]
    fn parses_hash_with_six_digits() {
        assert_eq!(
            parse_osc11(&reply("#ffffff\x1b\\")),
            Some(Rgb::new(0xff, 0xff, 0xff))
        );
        assert_eq!(
            parse_osc11(&reply("#000000\x1b\\")),
            Some(Rgb::new(0, 0, 0))
        );
    }

    #[test]
    fn parses_hash_with_three_digits() {
        assert_eq!(
            parse_osc11(&reply("#fff\x1b\\")),
            Some(Rgb::new(0xff, 0xff, 0xff))
        );
        assert_eq!(parse_osc11(&reply("#000\x1b\\")), Some(Rgb::new(0, 0, 0)));
        assert_eq!(
            parse_osc11(&reply("#abc\x1b\\")),
            Some(Rgb::new(0xaa, 0xbb, 0xcc))
        );
    }

    /// Short hex with one-digit channels is the most permissive spelling the
    /// spec allows; a terminal that chooses to send it is a terminal where
    /// `f`/`F` is full and `0` is zero, not a terminal where `f` is `0x0f`.
    /// `scale_channel` is what makes that true.
    #[test]
    fn parses_rgb_colon_with_short_hex() {
        assert_eq!(
            parse_osc11(&reply("rgb:f/f/f\x1b\\")),
            Some(Rgb::new(0xff, 0xff, 0xff))
        );
        assert_eq!(
            parse_osc11(&reply("rgb:0/0/0\x1b\\")),
            Some(Rgb::new(0, 0, 0))
        );
    }

    /// A terminal that does not implement `OSC 11` either says nothing at all
    /// (no `]11;` substring) or replies with something unrelated. Both have to
    /// land as `None`, not as a colour out of nowhere -- `None` is what the
    /// install path reads as "no answer, fall back to the default".
    #[test]
    fn rejects_what_is_not_a_reply() {
        assert_eq!(parse_osc11(""), None);
        assert_eq!(parse_osc11("\x1b]0;some window title\x1b\\"), None);
        assert_eq!(parse_osc11("\x1b]11;rgb:not-hex\x1b\\"), None);
        assert_eq!(parse_osc11("\x1b]11;rgb:ff/ff\x1b\\"), None);
        assert_eq!(parse_osc11("\x1b]11;#ggg\x1b\\"), None);
    }

    /// A reply closed with `BEL` (the older terminator) reads the same as one
    /// closed with `ST`. Some terminals prefer one over the other; the parser
    /// must not.
    #[test]
    fn both_terminators_are_accepted() {
        let st = parse_osc11(&reply("rgb:ff/ff/ff\x1b\\")).unwrap();
        let bel = parse_osc11(&reply("rgb:ff/ff/ff\u{7}")).unwrap();
        assert_eq!(st, bel);
        assert_eq!(st, Rgb::new(0xff, 0xff, 0xff));
    }
}

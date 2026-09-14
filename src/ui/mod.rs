//! The front ends, in their parts.
//!
//! Two of the parts are not about writing anything at all. `contract` is what
//! every front end must offer the machine and the application -- it is the only
//! thing the machine depends on, and it knows no implementation. `terminal` is
//! the process's own terminal behind one seam: the width, whether it can take
//! escape sequences, how to silence its echo, and where a line of input comes
//! from. Everything above that seam is arithmetic over those facts, which is why
//! the decisions built on them are tested without one.
//!
//! Three layers write what a session shows:
//!
//! - [`cell`](self::cell) holds the content: what a cell *is*, terminal-agnostic
//!   data with a [`Style`](self::cell::Style) that is a label rather than an
//!   escape sequence and a [`Gutter`](self::cell::Gutter) that names its own
//!   columns. Cells know nothing about the wire or the screen.
//! - [`paint`](self::paint) holds how a cell is *drawn*: pure functions that
//!   take cells, spans and widths, and produce the terminal lines a front end
//!   hands to its backend. The TUI uses this layer directly; the plain front
//!   end writes its own bytes because leaving wrapping to the terminal is the
//!   right thing for a stream with no fixed-width region.
//! - The two front ends: the plain `renderer` that writes SGR to a `Write`,
//!   and the full-screen `tui` front end with the input box, the standing
//!   task list, the queue, and the question panel.
//!
//! The two front ends are mutually exclusive, and neither is a fallback for
//! the other at the level of a single line of output.

mod answers;
mod banner;
mod cell;
mod contract;
#[cfg(test)]
pub(crate) mod doubles;
pub(crate) mod paint;
mod plain_writer;
mod renderer;
mod status;
mod status_bar;
mod terminal;
pub(crate) mod text;
pub(crate) mod theme;
pub mod tui;

pub use answers::{Sigint, StdinApproval, StdinQuestions};
pub(crate) use banner::banner;
pub use contract::{Approve, Ask, Cancel, Front, Ui, Verdict};
pub use renderer::Renderer;

#[cfg(test)]
mod tests;

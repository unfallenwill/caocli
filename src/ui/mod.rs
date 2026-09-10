//! The plain front end, in its parts.
//!
//! Two of the parts are not about writing anything at all. `contract` is what
//! every front end must offer the machine and the application -- it is the only
//! thing the machine depends on, and it knows no implementation. `terminal` is
//! the process's own terminal behind one seam: the width, whether it can take
//! escape sequences, how to silence its echo, and where a line of input comes
//! from. Everything above that seam is arithmetic over those facts, which is why
//! the decisions built on them are tested without one.
//!
//! The rest is what this front end writes: the `banner` a session opens with,
//! the `cell`s the transcript is made of, the status line's `status` (what it
//! says) and `status_bar` (where it goes), the `text` measurements both of them
//! are laid out with, and `renderer`, which turns notifications into cells and
//! writes them.
//!
//! The front end that owns the whole screen lives in `tui`, next to this one
//! rather than under it: the two are mutually exclusive, and neither is a
//! fallback for the other at the level of a single line of output.

mod answers;
mod banner;
mod cell;
mod contract;
mod renderer;
mod status;
mod status_bar;
mod terminal;
pub(crate) mod text;
pub mod tui;

pub use answers::{Sigint, StdinApproval};
pub(crate) use banner::banner;
pub use contract::{Approve, Front, Interrupt, Ui};
pub use renderer::Renderer;

#[cfg(test)]
mod tests;

//! The overlays: the picker, the panel, and the answer channel.
//!
//! Three pieces because each has a different lifetime:
//! - the picker is a menu the box opened while a line is being typed, and is
//!   answered by the next key,
//! - the panel is a question the model asked, and is answered over the same
//!   turns as the picker (the loop's keys),
//! - the answer channel is what the box is currently driving -- either the
//!   approval gate's `y/N`, the secret prompt, or the panel's typed line.
//!
//! They share one half of [`super::State`] because a panel cannot open while a
//! picker is up (the box belongs to one or the other), and a secret prompt
//! cannot open while a panel is up (the box belongs to the panel). Three flags
//! that say "the box is busy" cohere more than they fragment.

use tokio::sync::oneshot;

use crate::ui::Verdict;

use super::panel::Panel;
use super::picker::Picker;

/// What the box is busy with, and who to give the answer to.
pub(super) enum Answer {
    /// The approval gate: a line starting with `y` allows and anything else
    /// denies, which is the rule the plain front end applies to a line of stdin.
    YesNo(oneshot::Sender<Verdict>),
    /// A secret (an API key): whatever was typed, with the text hidden while it
    /// is typed, and an empty line for a cancellation. Nothing of it is echoed
    /// into the transcript, and nothing of it is remembered.
    Secret(oneshot::Sender<Option<String>>),
}

/// The overlays that stand over the transcript while a question is being
/// named or answered.
#[derive(Default)]
pub(crate) struct Overlay {
    /// The command picker, while what is in the box is a command still being
    /// named.
    pub(crate) picker: Option<Picker>,
    /// The question tool's panel, while the tool is waiting on an answer.
    pub(crate) panel: Option<Panel>,
    /// Where the answer to that question goes, and what kind of answer it is.
    pub(crate) reply: Option<Answer>,
}

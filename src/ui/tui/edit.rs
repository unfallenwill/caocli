//! The edit half of the session: the input box, the history it browses, and
//! the draft the box holds back when a question takes the box over.
//!
//! Kept apart from [`super::View`] (what the screen shows) and the other
//! halves so a test about browsing history or masking a secret does not have
//! to invent a transcript to test it.

use ratatui_textarea::TextArea;

/// What the input box carries between turns -- the textarea, the history it
/// recalls, and the lines it holds aside when a question takes the box over.
pub(crate) struct Edit {
    /// The answer being typed.
    pub(crate) textarea: TextArea<'static>,
    /// What was in the box when the approval gate opened. The answer is typed
    /// there, and a line being composed is not an answer.
    pub(crate) held_draft: Option<String>,
    /// Submitted lines, oldest first.
    pub(crate) history: Vec<String>,
    /// Where the user is browsing the history from, if they are.
    pub(crate) browsing: Option<usize>,
    /// What was in the box before browsing started, so that stepping past the
    /// newest entry gives it back.
    pub(crate) draft: String,
}

impl Default for Edit {
    fn default() -> Self {
        Self {
            textarea: super::input::input_box(),
            held_draft: None,
            history: Vec::new(),
            browsing: None,
            draft: String::new(),
        }
    }
}

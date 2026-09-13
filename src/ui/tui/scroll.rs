//! Where the window over the transcript sits.
//!
//! Held as lines back from the newest one, because that is where it opens: the
//! end is what the reader is watching. Kept apart from [`super::view::View`]
//! because the arithmetic is the same wherever the window is opened -- the
//! view holds it, but its move/follow rules are independent of any cell or
//! draft.

/// Where the window over the transcript sits.
///
/// Held as lines back from the newest one, because that is where it opens: the
/// end is what the reader is watching.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Scroll {
    pub(crate) back: usize,
}

impl Scroll {
    /// Scroll by `step` lines, positive being towards the newest line, as far
    /// as there is anything to see.
    ///
    /// `total` and `height` are the transcript's length and the rows available
    /// for it; together they say how far back the start is.
    pub(crate) fn by(&mut self, step: isize, total: usize, height: usize) {
        let most = total.saturating_sub(height) as isize;
        self.back = (self.back as isize - step).clamp(0, most) as usize;
    }

    /// Go back to the end, which is where it starts.
    pub(crate) fn bottom(&mut self) {
        self.back = 0;
    }

    /// The first line to draw.
    pub(crate) fn first(&self, total: usize, height: usize) -> usize {
        total.saturating_sub(height).saturating_sub(self.back)
    }
}

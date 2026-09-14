//! The glyph set: every character the screen draws that is not ASCII, and the
//! ASCII arm that stands in for it.
//!
//! A terminal application cannot choose its typeface, so a character it emits is
//! a requirement it places on whatever font the user has -- and, because the
//! terminal decides how wide a character is, a requirement on the terminal too.
//! Two things can go wrong with one:
//!
//! - **The font does not have it.** Terminals substitute another font for a
//!   missing glyph, which is usually ugly and sometimes absent altogether.
//! - **The terminal gives it two columns instead of one.** A character in the
//!   Unicode East Asian Ambiguous (`A`) class is one column in most terminals
//!   and two in a CJK-width one, while [`crate::ui::text::width`] resolves it as
//!   one either way. That is a column the layout counted and the screen did not
//!   spend, which is how a border shifts, a right-aligned line overflows, or a
//!   box rule wraps the frame.
//!
//! The markers below are chosen for the first problem and checked for the
//! second: all six of the non-ASCII gutter markers are class `N` (neutral), so
//! a terminal that widens ambiguous characters still gives them one column. The
//! decorations are not so lucky -- the spinner, the separators, the ellipsis and
//! the box rule are all `A` -- which is what [`ASCII`] is for.
//!
//! **What the set is not**: it is not the transcript's words. A separator here
//! is a glyph the *screen* draws, and the strings the tools send the model
//! (`src/tools`, `src/provider`) are the wire and never pass through this module.
//! The line between the two is that a glyph in this set can be swapped for its
//! ASCII twin without changing a single byte of what was run or what was sent.
//!
//! The set is process-wide and installed once, from `main`, before any front end
//! paints: it is the same bargain [`crate::ui::theme`] makes, and for the same
//! reason -- the marker a cell opens with is asked for from inside a method that
//! has no caller to take a parameter from.

use std::sync::OnceLock;

/// Which arm of the set is in effect. Carried as a value rather than inferred
/// from a field, because the screen-owning front end picks its border set from
/// it and a `ratatui` border set is not a glyph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// The Unicode set: the default, and what the screen is designed around.
    Unicode,
    /// Every character ASCII. The mode that cannot go wrong.
    Ascii,
}

/// One glyph per thing the screen draws. The field names are the meanings; the
/// values are [`UNICODE`] and [`ASCII`].
///
/// Every entry is a `&'static str` rather than a `char` because half of them are
/// a marker *and* the column that sets it off from the text beside it: the gutter
/// markers are two columns wide by definition ([`crate::ui::cell::MARKER_COLUMNS`]),
/// and writing the pair as one string is what keeps the width and the glyph from
/// drifting apart.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct GlyphSet {
    /// Which arm this is. Read by the screen-owning front end for its borders.
    pub(crate) kind: Kind,
    /// A line the user typed.
    pub(crate) user: &'static str,
    /// A tool call, a question, a task in hand: something about to happen.
    pub(crate) running: &'static str,
    /// Something that finished well.
    pub(crate) done: &'static str,
    /// Something that did not, or that was refused.
    pub(crate) failed: &'static str,
    /// A task on the standing list that has not been started.
    pub(crate) pending: &'static str,
    /// The usage summary's gutter marker.
    pub(crate) usage: &'static str,
    /// Rows a window cut off: the count at the end it was cut at.
    pub(crate) more: &'static str,
    /// A turn that was stopped rather than finished.
    pub(crate) stopped: &'static str,
    /// The picker's cursor.
    pub(crate) cursor: &'static str,
    /// The picker's chosen row, while the panel is open.
    pub(crate) chosen: &'static str,
    /// The two arrow keys, for a footer that names them. A pair rather than a
    /// glyph because what it says is "the keys above and below this one", and a
    /// single character cannot.
    pub(crate) arrows: &'static str,
    /// Between two segments of one line: `cache 12.3% · 40/120`.
    pub(crate) separator: &'static str,
    /// In front of a line that is a note about the screen rather than part of
    /// it: the standing task list's title, which is pinned under the transcript
    /// for the whole of a turn. The same character the separator is, standing
    /// alone -- which is why the ASCII arm spells it the way the notice gutter
    /// already does.
    pub(crate) bullet: char,
    /// Where text was cut.
    pub(crate) ellipsis: &'static str,
    /// Inside a sentence, where a hyphen would be too short.
    pub(crate) dash: &'static str,
    /// The frames of the working indicator, one per `SPINNER_MS`.
    pub(crate) spinner: &'static [char],
    /// A rule drawn by hand, where the screen wants one.
    pub(crate) rule: char,
    /// A character of a secret, one cell each.
    pub(crate) mask: char,
}

/// The default: what the screen is designed around.
///
/// The six gutter markers are class `N` and the decorations are not, which is
/// why [`ASCII`] exists and why this one is not the safe choice -- it is the
/// *designed* choice, and the safe one is one setting away.
pub(crate) const UNICODE: GlyphSet = GlyphSet {
    kind: Kind::Unicode,
    user: "› ",
    running: "▸ ",
    done: "✔ ",
    failed: "✘ ",
    pending: "☐ ",
    usage: "≡ ",
    more: "⋮",
    // `▪` rather than `⏹`: the stop sign sits in Miscellaneous Technical, the one
    // block in this whole set that monospace fonts routinely skip, and the small
    // black square says the same thing in a block they all carry.
    stopped: "▪",
    cursor: "❯ ",
    chosen: "✓ ",
    arrows: "↑/↓",
    separator: " · ",
    bullet: '·',
    ellipsis: "…",
    dash: "—",
    // Braille, not the half-circles `◐◓◑◒` that were here: those alternate
    // between the Ambiguous and Neutral classes frame by frame -- `◐` is `A` and
    // `◓` is `N` -- so a terminal that widens ambiguous characters drew two
    // frames one column wide and two frames two columns wide, and the box rule
    // the spinner rides on twitched sideways twice per revolution. Braille is
    // uniformly `N`, evenly spaced, and in every coding font.
    spinner: &['⣾', '⣽', '⣻', '⢿', '⡿', '⣟', '⣯', '⣷'],
    rule: '─',
    mask: '•',
};

/// Everything ASCII, for a terminal whose ambiguous characters are wide or whose
/// font is missing something.
///
/// Sixteen of the seventeen substitutions keep the same column count, because a
/// marker that changed width would re-flow every line it is on the moment the
/// setting flipped: `› ` and `> ` are both two columns, `⋮` and `:` are both one.
/// The exception is the ellipsis, which is one column as `…` and three as `...`,
/// and it is called out here so a reader does not have to find it.
pub(crate) const ASCII: GlyphSet = GlyphSet {
    kind: Kind::Ascii,
    user: "> ",
    running: "> ",
    done: "+ ",
    failed: "x ",
    pending: "- ",
    usage: "= ",
    more: ":",
    stopped: "!",
    cursor: "> ",
    chosen: "* ",
    arrows: "Up/Down",
    separator: " | ",
    bullet: '*',
    ellipsis: "...",
    dash: "-",
    spinner: &['|', '/', '-', '\\'],
    rule: '-',
    mask: '*',
};

/// The set in effect.
static SET: OnceLock<GlyphSet> = OnceLock::new();

/// Install `set` as the process's glyph set. The first call wins; a second is a
/// no-op rather than a panic, because it is the same one-value-installed-once
/// shape [`crate::ui::theme::install`] has and a library caller is not a reason
/// to bring the process down.
pub(crate) fn install(set: GlyphSet) {
    let _ = SET.set(set);
}

impl GlyphSet {
    /// The set's name, for `/debug` and for the message that says which ones
    /// `--glyphs` takes.
    pub(crate) fn name(&self) -> &'static str {
        match self.kind {
            Kind::Unicode => "unicode",
            Kind::Ascii => "ascii",
        }
    }
}

/// The set in effect, or [`UNICODE`] when nothing has installed one -- which is
/// the answer in a test, and the answer for a caller that only needs to render
/// something.
pub(crate) fn get() -> &'static GlyphSet {
    SET.get_or_init(|| UNICODE)
}

/// The separator between two segments of one line, as a function so that a
/// `format!` can name it without naming the whole set: `format!("{a}{}{b}",
/// glyphs::sep())` reads as the line it builds, which `glyphs::get().separator`
/// inside the format string does not.
pub(crate) fn sep() -> &'static str {
    get().separator
}

/// Every glyph in `set` that is more than one byte, as `(name, glyph)` pairs.
///
/// The one list over the fields, so that a property of "every glyph" -- that it
/// is ASCII, that it is one column, that it is not a control character -- can be
/// a test against the set rather than a `match` written a second time beside it.
/// Adding a field without adding it here fails the tests that read this, which
/// is the point.
#[cfg(test)]
pub(crate) fn named(set: &'static GlyphSet) -> Vec<(&'static str, &'static str)> {
    vec![
        ("user", set.user),
        ("running", set.running),
        ("done", set.done),
        ("failed", set.failed),
        ("pending", set.pending),
        ("usage", set.usage),
        ("more", set.more),
        ("stopped", set.stopped),
        ("cursor", set.cursor),
        ("chosen", set.chosen),
        ("separator", set.separator),
        ("ellipsis", set.ellipsis),
        ("dash", set.dash),
        ("arrows", set.arrows),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::cell::MARKER_COLUMNS;
    use crate::ui::text;

    /// The markers that are set in a gutter are exactly [`MARKER_COLUMNS`]: they
    /// are what every line of a cell is indented by, and the box draws its own
    /// draft marker in the same columns, so one of them being a column wider is
    /// a submitted line that moves the moment it is submitted.
    #[test]
    fn the_gutter_markers_are_the_marker_columns() {
        for set in [&UNICODE, &ASCII] {
            for (name, glyph) in [
                ("user", set.user),
                ("running", set.running),
                ("done", set.done),
                ("failed", set.failed),
                ("pending", set.pending),
                ("usage", set.usage),
            ] {
                assert_eq!(text::width(glyph), MARKER_COLUMNS, "{name} in {set:?}");
            }
        }
    }

    /// The ASCII arm is ASCII. Not "mostly": every printable string in it, one
    /// byte per column, which is the whole reason a reader turns it on.
    #[test]
    fn the_ascii_arm_is_ascii() {
        for (name, glyph) in named(&ASCII) {
            assert!(glyph.is_ascii(), "{name} is not ASCII: {glyph:?}");
        }
        assert!(ASCII.rule.is_ascii());
        assert!(ASCII.mask.is_ascii());
        assert!(ASCII.bullet.is_ascii());
        assert!(ASCII.spinner.iter().all(|c| c.is_ascii()));
    }

    /// A substitution that changes a line's width re-flows the transcript the
    /// moment the setting is flipped, and there are two of those, both on
    /// purpose.
    ///
    /// The ellipsis: `…` is one column and `...` is three, because a lone `.` in
    /// the middle of a sentence reads as a full stop. The arrows: `↑/↓` is three
    /// columns and `Up/Down` is seven, because there is no ASCII way to draw an
    /// arrow and the footer that names them reads better as words than as the
    /// `^` and `v` it would otherwise have to be.
    ///
    /// Both are in a footer or an elision rather than in a gutter, which is where
    /// the columns are load-bearing -- a marker that changed width would move
    /// every line of the cell it opens.
    #[test]
    fn every_substitution_but_the_two_that_say_so_keeps_its_columns() {
        let mut changed = Vec::new();
        for ((name, unicode), (_, ascii)) in named(&UNICODE).into_iter().zip(named(&ASCII)) {
            if text::width(unicode) != text::width(ascii) {
                changed.push(name);
            }
        }
        assert_eq!(changed, vec!["ellipsis", "arrows"]);
    }

    /// The spinner frames are one column each and all different, in a font that
    /// has them: a frame that is two columns wide moves the box rule it rides
    /// on, and two identical frames are an indicator that stops.
    #[test]
    fn the_spinner_is_evenly_spaced() {
        for set in [&UNICODE, &ASCII] {
            assert!(set.spinner.len() >= 4, "{set:?}");
            for frame in set.spinner {
                assert_eq!(text::width(&frame.to_string()), 1, "{frame:?} in {set:?}");
            }
            let mut frames = set.spinner.to_vec();
            frames.sort_unstable();
            frames.dedup();
            assert_eq!(frames.len(), set.spinner.len(), "{set:?} repeats a frame");
        }
    }

    /// Braille, so that the working indicator is the same width in a terminal
    /// that widens ambiguous characters as in one that does not. The half-circles
    /// this replaced mixed the two classes, which is the defect the change fixes.
    #[test]
    fn the_unicode_spinner_is_braille() {
        for frame in UNICODE.spinner {
            assert!(
                ('\u{2800}'..='\u{28ff}').contains(frame),
                "{frame:?} is not braille"
            );
        }
    }

    /// The one thing the ASCII set is allowed to be worse at: it is not the set
    /// anyone sees by default.
    #[test]
    fn the_default_is_the_unicode_set() {
        assert_eq!(get(), &UNICODE);
        // The fields the two arms agree on, so that a name added to one and not
        // the other is a compile error rather than a surprise on screen.
        assert_ne!(UNICODE.user, ASCII.user);
        assert_eq!(UNICODE.kind, Kind::Unicode);
        assert_eq!(ASCII.kind, Kind::Ascii);
    }
}

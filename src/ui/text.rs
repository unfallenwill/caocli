//! Display-width text primitives.
//!
//! A terminal lays text out in columns, and a column is not a character: a CJK
//! ideograph or an emoji occupies two columns while `str::chars` counts it as
//! one. Anything that positions or clips text against a terminal width has to
//! measure columns, which is what this module provides.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Display width of `s` in terminal columns.
pub fn width(s: &str) -> usize {
    s.width()
}

/// `value` followed by spaces until it takes `columns` columns, for lining text
/// up in a column of its own.
///
/// Not `format!("{:<columns$}")`: that fills up to a count of *characters*, and
/// what is being lined up is measured in the columns the terminal gives it.
pub fn padded(value: &str, columns: usize) -> String {
    let mut out = String::from(value);
    out.push_str(&" ".repeat(columns.saturating_sub(width(value))));
    out
}

/// Clip `s` to at most `max` columns, ending on a char boundary.
///
/// A character that would straddle the limit is dropped whole rather than
/// split, so the result is valid UTF-8 and never occupies more than `max`
/// columns.
pub fn truncate(s: &str, max: usize) -> &str {
    let mut used = 0;
    for (idx, ch) in s.char_indices() {
        // Control characters have no defined width; counting them as zero keeps
        // the walk total. None of them belong in a measured field anyway.
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + w > max {
            return &s[..idx];
        }
        used += w;
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regression this module exists for: a field measured in chars
    /// overflows by one column per wide character, and the overflow lands past
    /// the column the caller deliberately left free.
    #[test]
    fn width_counts_columns_not_chars() {
        // A CJK model id is the real-world case: it is one char per glyph in
        // Rust but two columns per glyph in the terminal.
        let cjk = "\u{6df1}\u{5ea6}\u{6c42}\u{7d22}"; // four ideographs
        assert_eq!(cjk.chars().count(), 4);
        assert_eq!(width(cjk), 8);
        // An emoji is the same trap and needs no CJK to demonstrate it.
        assert_eq!("\u{1f680}".chars().count(), 1);
        assert_eq!(width("\u{1f680}"), 2);
    }

    #[test]
    fn width_is_ascii_identity() {
        assert_eq!(width(""), 0);
        assert_eq!(width("abc"), 3);
        assert_eq!(width("glm-4.6"), 7);
        assert_eq!(width("deepseek-chat"), 13);
    }

    #[test]
    fn padded_fills_to_a_column_count() {
        assert_eq!(padded("abc", 5), "abc  ");
        assert_eq!(padded("abcde", 3), "abcde", "never cuts what is there");
        assert_eq!(padded("", 2), "  ");
        // A wide glyph is two columns, so one character is already one column
        // past what a two-column field would take.
        assert_eq!(padded("\u{6df1}", 2), "\u{6df1}");
        assert_eq!(padded("\u{6df1}", 3), "\u{6df1} ");
    }

    #[test]
    fn truncate_keeps_text_that_already_fits() {
        assert_eq!(truncate("abc", 5), "abc");
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn truncate_lands_on_char_boundaries() {
        assert_eq!(truncate("abcd", 2), "ab");
        // the 2-byte "é" is dropped whole, never split in half
        assert_eq!(truncate("café", 3), "caf");
    }

    #[test]
    fn truncate_never_splits_a_wide_char() {
        let rocket = "\u{1f680}\u{1f680}"; // two chars, four columns
        assert_eq!(truncate(rocket, 3), "\u{1f680}");
        assert_eq!(truncate(rocket, 2), "\u{1f680}");
        // a single wide char does not fit in one column at all
        assert_eq!(truncate(rocket, 1), "");
    }

    #[test]
    fn truncate_never_exceeds_the_limit() {
        let cases = [
            "café ☕ rocket \u{1f680} mix",
            "ascii only",
            "\u{6df1}\u{5ea6} · cache 98.6% · hit 32384",
        ];
        for s in cases {
            for max in 0..=width(s) + 2 {
                let got = truncate(s, max);
                assert!(s.starts_with(got), "{s:?} @ {max} -> {got:?}");
                assert!(width(got) <= max, "{s:?} @ {max} -> {got:?}");
            }
        }
    }
}

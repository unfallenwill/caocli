//! The palette: what a semantic [`Style`] is painted in.
//!
//! The [Dracula](https://draculatheme.com) palette, from the theme's own
//! published colours (`dracula/dracula-theme`): one copy of the values, and the
//! one place the two front ends agree on what a `Style` looks like. The plain
//! front end writes the RGB out as an SGR sequence, the TUI (through
//! [`style_of`]) hands the same values to `ratatui` as a [`Color::Rgb`], and
//! neither derives a colour of its own -- a second mapping is how one front end
//! starts disagreeing with the other about what a `Reasoning` line is.
//!
//! The mapping is the theme's, not the terminal's, on purpose. A cell's `Dim`,
//! `Yellow`, `Green` and `Red` used to be the 16 ANSI colours, which is a
//! palette the terminal picked for a background this code cannot see; Dracula
//! names the colours itself, so the output is the theme whatever the terminal's
//! own palette has been set to. What it costs is the terminals without RGB
//! support, which approximate a `38;2;r;g;b` sequence -- and `NO_COLOR`, which
//! takes the sequences out entirely, is the answer for a reader who wants the
//! terminal's own colours back.
//!
//! Secondary text is the theme's `comment`, not SGR 2. The Dracula comment
//! colour is what the theme's own ports paint a muted line with, and a dim
//! modifier laid on top of it would be that line darkened twice -- on a
//! `#282a36` background it reads as a line that is not there. The weight is
//! carried by the colour, which is one of the palette's deliberate choices
//! rather than a terminal's rendering hint.
//
// Everything in this module is the palette's answer to the cell layer's
// `Style`. The two backends wire to it from commits that follow this one; for
// now nothing reaches in, and the dead-code warnings are the seam.
#![allow(dead_code, unused_imports)]

use ratatui::style::{Color, Modifier, Style as RStyle};

use crate::ui::cell::Style;

/// One colour: the red, green and blue the terminal is asked for.
///
/// Kept as the three channels rather than as a string, because the three are
/// what each backend needs: an SGR sequence spells them out, and `ratatui`
/// takes them as a [`Color::Rgb`]. A string would be one of the two formats and
/// a parse for the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rgb {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

/// The palette as it is written in the theme, named the way the theme names its
/// colours. All nine are here, including the background and the foreground the
/// front ends do not paint: a palette missing the two colours its own
/// secondary ones were chosen against is a palette a reader has to go look up.
///
/// The rest of the Dracula palette -- `current line`, `selection`, the bright
/// variants -- is left out because nothing here paints with it. Adding a colour
/// the moment a `Style` needs it is the cheap direction; carrying colours no
/// call site reaches for is the one that rots.
pub(crate) const DRACULA: Palette = Palette {
    background: Rgb {
        r: 0x28,
        g: 0x2a,
        b: 0x36,
    },
    foreground: Rgb {
        r: 0xf8,
        g: 0xf8,
        b: 0xf2,
    },
    comment: Rgb {
        r: 0x62,
        g: 0x72,
        b: 0xa4,
    },
    cyan: Rgb {
        r: 0x8b,
        g: 0xe9,
        b: 0xfd,
    },
    green: Rgb {
        r: 0x50,
        g: 0xfa,
        b: 0x7b,
    },
    orange: Rgb {
        r: 0xff,
        g: 0xb8,
        b: 0x6c,
    },
    pink: Rgb {
        r: 0xff,
        g: 0x79,
        b: 0xc6,
    },
    purple: Rgb {
        r: 0xbd,
        g: 0x93,
        b: 0xf9,
    },
    red: Rgb {
        r: 0xff,
        g: 0x55,
        b: 0x55,
    },
    yellow: Rgb {
        r: 0xf1,
        g: 0xfa,
        b: 0x8c,
    },
};

/// The colours a session paints with, and the names the theme gives them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Palette {
    /// Everything that is not set apart.
    pub(crate) foreground: Rgb,
    /// The colour the theme's own ports use for secondary text: comments, the
    /// one surviving line of a folded block, the standing task list. What
    /// [`Style::Dim`] and [`Style::Reasoning`] are painted in.
    pub(crate) comment: Rgb,
    /// Failures and the lines a change takes out.
    pub(crate) red: Rgb,
    /// A change's added lines.
    pub(crate) green: Rgb,
    /// Tool calls, questions, the working indicator: attention without failure.
    /// [`Style::Yellow`]'s colour -- the theme's `orange`, because the palette's
    /// own `yellow` is a near-white that reads as a highlight rather than as a
    /// call to look.
    pub(crate) orange: Rgb,
    /// The theme's background and its four remaining accents.
    ///
    /// Nothing paints with them yet. They are here because the palette is one
    /// value: a reader comparing this file against the theme's README should
    /// see the same nine colours, and a `Style` added later picks the accent it
    /// wants out of the palette rather than inventing a sixth colour.
    #[allow(dead_code)]
    pub(crate) background: Rgb,
    #[allow(dead_code)]
    pub(crate) cyan: Rgb,
    #[allow(dead_code)]
    pub(crate) pink: Rgb,
    #[allow(dead_code)]
    pub(crate) purple: Rgb,
    #[allow(dead_code)]
    pub(crate) yellow: Rgb,
}

/// The palette in effect.
pub(crate) const PALETTE: Palette = DRACULA;

/// The colour `style` is painted in.
pub(crate) fn color_of(style: Style) -> Rgb {
    match style {
        Style::Plain => PALETTE.foreground,
        // One colour for both: what tells thinking apart from a tool result is
        // the rule in its gutter, which is a difference in the layout rather
        // than in the colours, and so one that survives both a terminal that
        // ignores colour and a reader who has turned it off.
        Style::Dim | Style::Reasoning => PALETTE.comment,
        Style::Yellow => PALETTE.orange,
        Style::Green => PALETTE.green,
        Style::Red => PALETTE.red,
    }
}

/// The SGR escape that opens `style`: the plain front end's backend, and the
/// only place a `Style` becomes bytes.
///
/// Truecolour, spelled out rather than mapped onto the 16 ANSI colours: the
/// theme names its own RGB values, and asking the terminal for colour 3 is
/// asking for whatever the user has that set to.
///
/// The weight comes from [`Style::is_bold`] -- the same rule [`style_of`] reads
/// -- so the two front ends cannot disagree on which styles carry it.
pub(crate) fn style_code(style: Style) -> String {
    let mut out = String::new();
    if style.is_bold() {
        out.push_str("\x1b[1m");
    }
    // `Plain` is the palette's foreground rather than "whatever the terminal is
    // set to", so a cell knows its own colour: the answer and the machinery
    // around it are painted by the same palette, and a reset in the middle of
    // one is a line that changes colour.
    out.push_str(&sgr_color(color_of(style)));
    out
}

/// Our style, as `ratatui` sees it: the same palette [`style_code`] writes out,
/// handed over as data.
///
/// Lives on this module rather than on a front end because both of them paint:
/// the plain front end writes the bytes itself, and the painter, the wrap
/// module and the screen all ask for the framework's shape of the same answer.
pub(crate) fn style_of(style: Style) -> RStyle {
    let mut s = RStyle::new().fg(rgb(color_of(style)));
    if style.is_bold() {
        s = s.add_modifier(Modifier::BOLD);
    }
    s
}

/// The SGR for a colour, for the bytes that are not a cell: the prompt box's
/// placeholder and the secret prompt, which are written by the front end that
/// owns the box rather than through a [`Cell`](crate::ui::cell::Cell).
pub(crate) fn sgr_color(color: Rgb) -> String {
    format!("\x1b[38;2;{};{};{}m", color.r, color.g, color.b)
}

/// The escape that closes every style: SGR 0, which is every attribute off
/// rather than the one this palette opened.
pub(crate) const RESET: &str = "\x1b[0m";

fn rgb(color: Rgb) -> Color {
    Color::Rgb(color.r, color.g, color.b)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The theme's own values, frozen: the palette is the one thing in this
    /// crate a reader can go and check against the theme's README, so a
    /// slip of a digit has to fail here rather than in a screenshot.
    #[test]
    fn the_palette_is_the_themes() {
        assert_eq!(DRACULA, PALETTE);
        assert_eq!(
            (
                PALETTE.background,
                PALETTE.foreground,
                PALETTE.comment,
                PALETTE.cyan
            ),
            (
                Rgb {
                    r: 0x28,
                    g: 0x2a,
                    b: 0x36
                },
                Rgb {
                    r: 0xf8,
                    g: 0xf8,
                    b: 0xf2
                },
                Rgb {
                    r: 0x62,
                    g: 0x72,
                    b: 0xa4
                },
                Rgb {
                    r: 0x8b,
                    g: 0xe9,
                    b: 0xfd
                }
            )
        );
        assert_eq!(
            (PALETTE.green, PALETTE.orange, PALETTE.pink),
            (
                Rgb {
                    r: 0x50,
                    g: 0xfa,
                    b: 0x7b
                },
                Rgb {
                    r: 0xff,
                    g: 0xb8,
                    b: 0x6c
                },
                Rgb {
                    r: 0xff,
                    g: 0x79,
                    b: 0xc6
                }
            )
        );
        assert_eq!(
            (PALETTE.purple, PALETTE.red, PALETTE.yellow),
            (
                Rgb {
                    r: 0xbd,
                    g: 0x93,
                    b: 0xf9
                },
                Rgb {
                    r: 0xff,
                    g: 0x55,
                    b: 0x55
                },
                Rgb {
                    r: 0xf1,
                    g: 0xfa,
                    b: 0x8c
                }
            )
        );
    }

    /// Every `Style` is painted, and no two of the painted ones are the same
    /// colour by accident: `Dim` and `Reasoning` share the comment on purpose,
    /// which is the one pair this test allows to be equal.
    #[test]
    fn the_colors_are_the_styles_own() {
        for style in [
            Style::Plain,
            Style::Dim,
            Style::Reasoning,
            Style::Yellow,
            Style::Green,
            Style::Red,
        ] {
            let _ = color_of(style);
        }
        assert_eq!(color_of(Style::Dim), color_of(Style::Reasoning));
        assert_ne!(color_of(Style::Dim), color_of(Style::Plain));
        assert_ne!(color_of(Style::Yellow), color_of(Style::Red));
        assert_ne!(color_of(Style::Green), color_of(Style::Red));
    }

    /// The SGR the plain front end writes: the colour in truecolour, and the
    /// weight the style carries.
    #[test]
    fn the_escape_is_the_color_the_theme_named() {
        assert_eq!(style_code(Style::Plain), "\x1b[38;2;248;248;242m");
        assert_eq!(style_code(Style::Dim), "\x1b[38;2;98;114;164m");
        // The same colour as `Dim`, and deliberately: thinking is told apart
        // from a tool result by the rule in its gutter.
        assert_eq!(style_code(Style::Reasoning), "\x1b[38;2;98;114;164m");
        // Painted styles carry weight as well as colour.
        assert_eq!(style_code(Style::Yellow), "\x1b[1m\x1b[38;2;255;184;108m");
        assert_eq!(style_code(Style::Green), "\x1b[1m\x1b[38;2;80;250;123m");
        assert_eq!(style_code(Style::Red), "\x1b[1m\x1b[38;2;255;85;85m");
    }

    /// The two backends are two spellings of one palette: whatever the TUI
    /// paints a style with is what the plain front end writes for it.
    #[test]
    fn the_two_backends_paint_the_same_color() {
        for style in [
            Style::Plain,
            Style::Dim,
            Style::Reasoning,
            Style::Yellow,
            Style::Green,
            Style::Red,
        ] {
            let c = color_of(style);
            assert_eq!(
                style_of(style).fg,
                Some(Color::Rgb(c.r, c.g, c.b)),
                "{style:?}"
            );
            assert!(style_code(style).contains(&sgr_color(c)), "{style:?}");
        }
    }

    /// Secondary text is the comment colour, not SGR 2: the theme's own ports
    /// dim a line by painting it, and a modifier on top of that colour would
    /// be the same line darkened twice.
    #[test]
    fn secondary_text_carries_no_tag_of_its_own() {
        for style in [Style::Dim, Style::Reasoning] {
            let s = style_of(style);
            assert!(!s.add_modifier.contains(Modifier::DIM), "{style:?}");
            assert!(!s.add_modifier.contains(Modifier::BOLD), "{style:?}");
            assert!(!style_code(style).contains("\x1b[2m"), "{style:?}");
        }
    }
}

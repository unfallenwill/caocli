//! The palette as arithmetic: contrast, colour vision, and the three tiers.
//!
//! A palette is the one thing in this crate a reader cannot check by running it
//! -- "does this grey look right" is not a question a test can ask, but "is this
//! grey legible on the screens it will land on" is, and it is the question that
//! matters. So the whole file is one property per test, over the palettes as
//! data: the numbers are not recorded so much as *enforced*, and a change that
//! breaks one fails here rather than in a screenshot somebody has to notice.

use super::*;

/// Every text role in a palette, named, in the order they are listed. `rule` is
/// not here: it is geometry, and the one colour allowed under the floor.
fn text_roles(palette: &Palette) -> [(&'static str, Rgb); 5] {
    [
        ("fg", palette.fg),
        ("muted", palette.muted),
        ("signal", palette.signal),
        ("ok", palette.ok),
        ("bad", palette.bad),
    ]
}

/// The backgrounds each theme is calibrated for, its own first.
///
/// The neighbours are not decoration: the application does not paint a
/// background, so "legible on the reference" is only half the claim. These are
/// the dark backgrounds a terminal is actually likely to be set to -- pure black,
/// GitHub's, Catppuccin's, Dracula's -- and the light ones a themed editor
/// produces.
fn backgrounds(name: Name) -> Vec<(&'static str, Rgb)> {
    match name {
        Name::Ink => vec![
            ("ink #16161E", INK.background),
            ("black #000000", Rgb::new(0, 0, 0)),
            ("github #0D1117", Rgb::new(0x0d, 0x11, 0x17)),
            ("mocha #1E1E2E", Rgb::new(0x1e, 0x1e, 0x2e)),
            ("dracula #282A36", Rgb::new(0x28, 0x2a, 0x36)),
        ],
        Name::Paper => vec![
            ("paper #FFFFFF", PAPER.background),
            ("warm #F7F7F5", Rgb::new(0xf7, 0xf7, 0xf5)),
            ("dim #E8E8E4", Rgb::new(0xe8, 0xe8, 0xe4)),
        ],
    }
}

/// 4.5:1 is WCAG AA for body text, and this is a screen that is almost entirely
/// body text: the answer, the thinking, the tool output, the diffs.
///
/// `bad` is the binding constraint in both themes -- it is the deepest accent in
/// ink and the lightest in paper -- and it is the reason the red is not the
/// bright one on a dark screen.
#[test]
fn every_text_colour_clears_the_contrast_floor() {
    for name in NAMED {
        let palette = name.palette();
        for (background, bg) in backgrounds(name) {
            for (role, color) in text_roles(&palette) {
                let ratio = color.contrast(bg);
                assert!(
                    ratio >= 4.5,
                    "{role} {} on {background} is {ratio:.2}:1 ({name:?})",
                    hex(color)
                );
            }
        }
    }
}

/// The rule is geometry, not text, and it is allowed to be quiet -- but not
/// invisible: a border the reader cannot see is a border that might as well not
/// be drawn, because the box it surrounds is what says which line is being
/// typed into.
#[test]
fn the_rule_is_visible_but_quiet() {
    for name in NAMED {
        let palette = name.palette();
        let ratio = palette.rule.contrast(palette.background);
        assert!(
            ratio > 1.2,
            "rule {} is invisible on {name:?}: {ratio:.2}:1",
            hex(palette.rule)
        );
        assert!(
            ratio < 4.5,
            "rule {} is loud enough to read on {name:?}: {ratio:.2}:1",
            hex(palette.rule)
        );
    }
}

/// The green and the red are the pair people confuse, and the palette separates
/// them on the axis colour blindness does not touch.
///
/// To a reader with deuteranopia -- around 6% of men -- there is no red-green
/// axis at all: an added line and a removed line are the same colour, differing
/// only in how light they are. So the green is the bright one and the red is the
/// deep one, which is the opposite of the usual "red is the loud one", and this
/// is the test that keeps it that way. A tone ratio of 1.5 is roughly "obviously
/// different on a greyscale printout".
#[test]
fn the_added_and_removed_lines_differ_in_tone() {
    for name in NAMED {
        let palette = name.palette();
        let ratio = palette.ok.contrast(palette.bad);
        assert!(
            ratio >= 1.5,
            "{name:?}: ok {} and bad {} are only {ratio:.2}:1 apart",
            hex(palette.ok),
            hex(palette.bad)
        );
    }
}

/// The same claim, measured rather than argued: every pair of text colours stays
/// far enough apart under simulated colour blindness to be told apart.
///
/// The simulation is Machado, Oliveira & Fernandes (2009) at severity 1.0 --
/// the published matrices for full dichromacy, applied in linear light. The floor
/// is ΔE 13, which is well above "two colours that look the same" and below the
/// worst pair these palettes actually produce (ΔE 14.4, `ok` against `bad` in
/// paper under deuteranopia). `fg` against `muted` is deliberately low, and
/// deliberately not asserted here: they are the same grey at two lightnesses, and
/// what tells them apart is not that they are far apart but that everything in
/// between them is `fg`.
#[test]
fn no_two_colours_collapse_under_colour_blindness() {
    for name in NAMED {
        let palette = name.palette();
        let roles = text_roles(&palette);
        for (i, (a_name, a)) in roles.iter().enumerate() {
            for (b_name, b) in &roles[i + 1..] {
                for (kind, matrix) in [("deuteranopia", DEUTAN), ("protanopia", PROTAN)] {
                    let distance =
                        lab_of(simulate(*a, matrix)).distance(lab_of(simulate(*b, matrix)));
                    assert!(
                        distance >= 13.0,
                        "{name:?}: {a_name} and {b_name} are ΔE {distance:.1} apart under {kind}"
                    );
                }
            }
        }
    }
}

/// With the colour axis taken away entirely -- a monochrome terminal, a printed
/// screenshot, a reader who turned colour off -- the three accents still have to
/// be three things, which is what their hue is for.
#[test]
fn the_accents_are_distinct_in_normal_vision() {
    for name in NAMED {
        let palette = name.palette();
        let accents = [
            ("signal", palette.signal),
            ("ok", palette.ok),
            ("bad", palette.bad),
        ];
        for (i, (a_name, a)) in accents.iter().enumerate() {
            for (b_name, b) in &accents[i + 1..] {
                let distance = lab_of(*a).distance(lab_of(*b));
                assert!(
                    distance >= 40.0,
                    "{name:?}: {a_name} and {b_name} are only ΔE {distance:.1} apart"
                );
            }
        }
    }
}

/// The palettes themselves, frozen. A reader can check these against the design
/// document; a slip of one digit is a colour nobody chose, and it should fail
/// here rather than on a user's screen.
#[test]
fn the_palettes_are_the_designed_ones() {
    let ink = [
        ("fg", INK.fg, (0xd7, 0xda, 0xe4)),
        ("muted", INK.muted, (0x9a, 0xa1, 0xb5)),
        ("signal", INK.signal, (0xe2, 0xb1, 0x5c)),
        ("ok", INK.ok, (0x8f, 0xd9, 0xa3)),
        ("bad", INK.bad, (0xe9, 0x70, 0x7e)),
        ("rule", INK.rule, (0x3a, 0x3f, 0x52)),
        ("background", INK.background, (0x16, 0x16, 0x1e)),
    ];
    let paper = [
        ("fg", PAPER.fg, (0x23, 0x25, 0x2e)),
        ("muted", PAPER.muted, (0x5c, 0x61, 0x72)),
        ("signal", PAPER.signal, (0x8a, 0x5c, 0x06)),
        ("ok", PAPER.ok, (0x1a, 0x72, 0x45)),
        ("bad", PAPER.bad, (0x7a, 0x11, 0x24)),
        ("rule", PAPER.rule, (0xc9, 0xcc, 0xd6)),
        ("background", PAPER.background, (0xff, 0xff, 0xff)),
    ];
    for (name, palette) in [("ink", ink), ("paper", paper)] {
        for (role, actual, (r, g, b)) in palette {
            assert_eq!(
                actual,
                Rgb::new(r, g, b),
                "{name}: {role} is {}",
                hex(actual)
            );
        }
    }
}

// ---------------------------------------------------------------------------
// The tiers
// ---------------------------------------------------------------------------

/// Truecolour writes the exact channels, and the two backends agree on them: the
/// bytes the plain front end writes are the ones the screen hands to `ratatui`.
#[test]
fn the_two_backends_paint_the_same_colour() {
    let theme = Theme::new(INK, Tier::True, true);
    for style in [
        Style::Plain,
        Style::Dim,
        Style::Reasoning,
        Style::Yellow,
        Style::Green,
        Style::Red,
    ] {
        let Ink::Rgb(c) = theme.ink(style) else {
            panic!("{style:?} is not a truecolour under Tier::True");
        };
        assert_eq!(
            theme.color_of(style),
            Color::Rgb(c.r, c.g, c.b),
            "{style:?}"
        );
        assert!(
            theme.style_code(style).contains(&theme.sgr(Ink::Rgb(c))),
            "{style:?}"
        );
    }
}

/// The escapes are what they have always been, at the tier the screen was
/// designed at. Written out rather than computed: this is the interface the
/// terminal sees, and a change to it is a change a reader should have to make
/// deliberately.
#[test]
fn the_escapes_are_truecolour() {
    let theme = Theme::new(INK, Tier::True, true);
    assert_eq!(theme.style_code(Style::Plain), "\x1b[38;2;215;218;228m");
    assert_eq!(theme.style_code(Style::Dim), "\x1b[38;2;154;161;181m");
    // The same colour as `Dim`, and deliberately: thinking is told apart from a
    // tool result by the rule in its gutter.
    assert_eq!(
        theme.style_code(Style::Reasoning),
        theme.style_code(Style::Dim)
    );
    // The painted styles carry weight as well as colour.
    assert_eq!(
        theme.style_code(Style::Yellow),
        "\x1b[1m\x1b[38;2;226;177;92m"
    );
    assert_eq!(
        theme.style_code(Style::Green),
        "\x1b[1m\x1b[38;2;143;217;163m"
    );
    assert_eq!(
        theme.style_code(Style::Red),
        "\x1b[1m\x1b[38;2;233;112;126m"
    );
}

/// The middle tier rounds to the xterm palette, and the rounding is recorded: a
/// reader with a 256-colour terminal is seeing these colours, and which ones they
/// are is a decision rather than an accident.
///
/// What the tier costs is chroma -- `muted` comes out a neutral grey, having no
/// blue-tinted neighbour in the cube -- and what it keeps it keeps on purpose:
/// `ok` and `bad` are rounded with a lightness bound rather than to the nearest
/// entry, so the pair comes out *further* apart in tone than it is in truecolour
/// (2.31:1 against 1.78:1). Nearest-by-distance alone would have rounded the deep
/// red up to `#FF8787` and the split down to 1.36:1, which is the colour-blind
/// reader's one remaining cue thrown away to gain ΔE 1.4 of hue.
#[test]
fn the_256_colour_tier_rounds_to_the_cube() {
    let theme = Theme::new(INK, Tier::Ansi256, true);
    let expected = [
        (Style::Plain, 253u8),
        (Style::Dim, 247),
        (Style::Yellow, 179),
        (Style::Green, 151),
        (Style::Red, 167),
    ];
    for (style, index) in expected {
        assert_eq!(theme.ink(style), Ink::Indexed(index), "{style:?}");
        assert!(
            theme
                .style_code(style)
                .contains(&format!("\x1b[38;5;{index}m"))
        );
    }
    assert_eq!(theme.ink_of(Role::Rule), Ink::Indexed(238));
    let ok = ink_rgb(theme.ink(Style::Green));
    let bad = ink_rgb(theme.ink(Style::Red));
    assert!(
        ok.contrast(bad) >= 1.5,
        "the tone split survives the cube: {:.2}:1",
        ok.contrast(bad)
    );
    // Paper's two accents need the bound twice over -- `ok` rounds up and `bad`
    // rounds *down*, past the crimson it was asked for and into the darkest red
    // on the ramp -- and come out 3.12:1 apart.
    let paper = Theme::new(PAPER, Tier::Ansi256, true);
    assert_eq!(paper.ink(Style::Green), Ink::Indexed(29));
    assert_eq!(paper.ink(Style::Red), Ink::Indexed(52));
    let (ok, bad) = (
        ink_rgb(paper.ink(Style::Green)),
        ink_rgb(paper.ink(Style::Red)),
    );
    assert!(ok.contrast(bad) >= 1.5, "{:.2}:1", ok.contrast(bad));
}

/// The rounding bound is a bound on the pair, not on one colour: `ok` may only
/// come out lighter than the truecolour green and `bad` only darker than the
/// truecolour red. Either could round to something that exaggerates the split --
/// and does -- but neither can round to something that flattens it, whatever
/// future palette the hexes become.
#[test]
fn the_rounded_accents_never_lose_tone() {
    for name in NAMED {
        let palette = name.palette();
        let theme = Theme::new(palette, Tier::Ansi256, true);
        let ok = ink_rgb(theme.ink(Style::Green));
        let bad = ink_rgb(theme.ink(Style::Red));
        assert!(
            ok.luminance() >= palette.ok.luminance(),
            "{name:?}: ok rounded down to a darker tone"
        );
        assert!(
            bad.luminance() <= palette.bad.luminance(),
            "{name:?}: bad rounded up to a lighter tone"
        );
        // Same colours under the simulation, so the guarantee is about what a
        // deuteranope sees and not only about the numbers.
        let distance = lab_of(simulate(ok, DEUTAN)).distance(lab_of(simulate(bad, DEUTAN)));
        assert!(
            distance >= 13.0,
            "{name:?}: ΔE {distance:.1} under deuteranopia"
        );
    }
}

/// The sixteen-colour tier hands the decision back to the terminal: no colour at
/// all for body text, and the slots every scheme fills for the rest.
///
/// Bold goes with it. In a sixteen-colour terminal `SGR 1` usually means "the
/// bright version of that colour" rather than "thicker strokes", so a style that
/// asked for bold would get a colour the mapping above did not choose.
///
/// The escapes are the `30`/`90` forms rather than the `38;5;n` ones, for the same
/// reason the tier exists: this is the arm for a terminal that may not have the
/// extended palette at all.
#[test]
fn the_16_colour_tier_defers_and_drops_bold() {
    let theme = Theme::new(INK, Tier::Ansi16, true);
    assert_eq!(theme.ink(Style::Plain), Ink::Terminal);
    assert_eq!(theme.style_code(Style::Plain), "");
    assert_eq!(theme.ink(Style::Dim), Ink::Ansi(8));
    assert_eq!(theme.style_code(Style::Dim), "\x1b[90m");
    assert_eq!(theme.style_code(Style::Red), "\x1b[31m");
    assert_eq!(theme.style_code(Style::Green), "\x1b[32m");
    assert_eq!(theme.style_code(Style::Yellow), "\x1b[33m");
    for style in [Style::Yellow, Style::Green, Style::Red] {
        assert!(
            !theme.style_code(style).contains("\x1b[1m"),
            "{style:?} asks for weight it cannot have"
        );
    }
    // The tier is the terminal's own colours, so the colours do not depend on
    // which of the two themes is in effect. There is one mapping, not two.
    assert_eq!(
        Theme::new(PAPER, Tier::Ansi16, true).style_code(Style::Red),
        theme.style_code(Style::Red)
    );
    // And `ratatui` is handed the *named* colour, so the screen gets `31` too
    // rather than the `38;5;1` an index would produce.
    assert_eq!(theme.color_of_ink(Ink::Ansi(1)), Color::Red);
    assert_eq!(theme.color_of_ink(Ink::Ansi(8)), Color::DarkGray);
    assert_eq!(theme.color_of_ink(Ink::Ansi(15)), Color::White);
    assert_eq!(theme.color_of(Style::Plain), Color::Reset);
}

/// `NO_COLOR` is no colour: every style is empty, and the style `ratatui` is
/// handed carries no foreground. What still works is the whole of the design --
/// the markers, the gutters, the words -- which is the point of the rule that
/// colour never carries meaning alone.
#[test]
fn no_color_asks_for_nothing() {
    let theme = Theme::new(INK, Tier::True, false);
    for style in [
        Style::Plain,
        Style::Dim,
        Style::Reasoning,
        Style::Yellow,
        Style::Green,
        Style::Red,
    ] {
        assert_eq!(theme.style_code(style), "", "{style:?}");
        assert_eq!(theme.style_of(style), RStyle::new(), "{style:?}");
    }
    assert_eq!(theme.rule_style(), RStyle::new());
    assert_eq!(theme.sgr(Ink::Rgb(INK.fg)), "");
}

/// Secondary text is the muted colour, not `SGR 2`.
///
/// The modifier is what this rule exists to keep out: a terminal renders `DIM` as
/// anything from "slightly grey" to "invisible", it stacks unpredictably with the
/// colour under it, and it is not what the design uses to say "behind the
/// answer".
#[test]
fn secondary_text_carries_no_modifier_of_its_own() {
    let theme = Theme::new(INK, Tier::True, true);
    for style in [Style::Dim, Style::Reasoning] {
        let s = theme.style_of(style);
        assert!(!s.add_modifier.contains(Modifier::DIM), "{style:?}");
        assert!(!s.add_modifier.contains(Modifier::BOLD), "{style:?}");
        assert!(!theme.style_code(style).contains("\x1b[2m"), "{style:?}");
    }
    // And the rule is painted, not dimmed.
    assert!(!theme.rule_style().add_modifier.contains(Modifier::DIM));
    assert_eq!(
        theme.rule_style().fg,
        Some(theme.color_of_ink(Ink::Rgb(INK.rule)))
    );
}

/// The nearest cube entry is picked in CIELAB rather than by channel arithmetic,
/// which is what keeps a blue off a grey. Checked against the landmarks a reader
/// can verify without a colour picker: white is the top of the cube, black is its
/// corner, and a mid grey has its own place on the ramp rather than having to take
/// a cube corner.
#[test]
fn the_256_search_lands_on_the_nearest_cube_entry() {
    assert_eq!(
        Ink::nearest256(Rgb::new(0xff, 0xff, 0xff)),
        Ink::Indexed(231)
    );
    assert_eq!(Ink::nearest256(Rgb::new(0, 0, 0)), Ink::Indexed(16));
    assert_eq!(
        Ink::nearest256(Rgb::new(0x80, 0x80, 0x80)),
        Ink::Indexed(244)
    );
    // Nothing reaches into the terminal's own sixteen slots: they are the tier
    // below this one, and this one is not theirs to claim.
    for color in [
        INK.fg,
        INK.muted,
        INK.signal,
        INK.ok,
        INK.bad,
        INK.rule,
        PAPER.fg,
        PAPER.muted,
        PAPER.ok,
        PAPER.rule,
    ] {
        let Ink::Indexed(i) = Ink::nearest256(color) else {
            panic!("not indexed");
        };
        assert!(i >= 16, "{} claimed slot {i}", hex(color));
    }
}

// ---------------------------------------------------------------------------
// Choosing one
// ---------------------------------------------------------------------------

/// A flag or a setting names a palette; a name nothing answers to is `None` so
/// that the caller can report it rather than silently painting the default.
#[test]
fn a_theme_is_named_or_defaulted() {
    assert_eq!(Choice::parse("ink"), Some(Choice::Named(Name::Ink)));
    assert_eq!(Choice::parse("paper"), Some(Choice::Named(Name::Paper)));
    assert_eq!(Choice::parse(" auto "), Some(Choice::Auto));
    assert_eq!(Choice::parse(""), Some(Choice::Auto));
    assert_eq!(Choice::parse("solarized"), None);
    assert_eq!(Choice::default(), Choice::Auto);
    assert_eq!(Name::Paper.as_str(), "paper");
}

/// `auto` resolves from the environment in a fixed order: what the terminal said
/// about itself, then what it answered when asked, then the default.
///
/// An explicit choice outranks all three -- a user who wrote `paper` has said so
/// -- which is what stops a background query from overriding a setting.
#[test]
fn auto_prefers_what_the_terminal_says() {
    let ink = Some(Name::Ink);
    let paper = Some(Name::Paper);
    // Nothing anywhere: a dark terminal, which is what most of them are.
    assert_eq!(resolve(Choice::Auto, None, None), Name::Ink);
    // COLORFGBG wins over the query, because it is free and the query is not.
    assert_eq!(resolve(Choice::Auto, Some("15;0"), paper), Name::Ink);
    assert_eq!(resolve(Choice::Auto, Some("0;15"), ink), Name::Paper);
    // The query is the second opinion, and the only one when there is no
    // COLORFGBG.
    assert_eq!(resolve(Choice::Auto, None, paper), Name::Paper);
    // A COLORFGBG nothing can be read out of is no opinion, not an answer.
    assert_eq!(resolve(Choice::Auto, Some("0;3"), paper), Name::Paper);
    assert_eq!(resolve(Choice::Auto, Some("rgb:00/00/00"), None), Name::Ink);
    // And an explicit choice outranks both.
    assert_eq!(
        resolve(Choice::Named(Name::Paper), Some("15;0"), ink),
        Name::Paper
    );
}

/// The `COLORFGBG` convention, including the values it is not allowed to guess
/// at: a terminal that reports a 256-colour index for its background has told us
/// something this cannot read, and reading it as "dark" is how a light terminal
/// gets dark text on it.
#[test]
fn colorfgbg_is_read_or_ignored() {
    assert_eq!(theme_from_colorfgbg("15;0"), Some(Name::Ink));
    assert_eq!(theme_from_colorfgbg("0;15"), Some(Name::Paper));
    assert_eq!(theme_from_colorfgbg("0;7"), Some(Name::Paper));
    assert_eq!(theme_from_colorfgbg("0;8"), Some(Name::Ink));
    assert_eq!(theme_from_colorfgbg("white;black"), Some(Name::Ink));
    assert_eq!(theme_from_colorfgbg("black;white"), Some(Name::Paper));
    assert_eq!(theme_from_colorfgbg("0;235"), None);
    assert_eq!(theme_from_colorfgbg(""), None);
    assert_eq!(theme_from_colorfgbg("nonsense"), None);
}

/// The tier, from the two variables that say anything about it. The interesting
/// cases are the ones where they disagree and the one where neither exists.
#[test]
fn the_tier_is_read_from_the_environment() {
    let env = |pairs: &'static [(&'static str, &'static str)]| {
        move |name: &str| {
            pairs
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| (*v).to_owned())
        }
    };
    assert_eq!(tier_from_env(env(&[])), Tier::True);
    assert_eq!(
        tier_from_env(env(&[("TERM", "xterm-256color")])),
        Tier::Ansi256
    );
    assert_eq!(tier_from_env(env(&[("TERM", "xterm")])), Tier::Ansi16);
    assert_eq!(tier_from_env(env(&[("TERM", "linux")])), Tier::Ansi16);
    // COLORTERM is the honest signal and outranks the TERM suffix, which is a
    // convention rather than a capability.
    assert_eq!(
        tier_from_env(env(&[
            ("TERM", "xterm-256color"),
            ("COLORTERM", "truecolor")
        ])),
        Tier::True
    );
    assert_eq!(
        tier_from_env(env(&[("TERM", "linux"), ("COLORTERM", "24bit")])),
        Tier::True
    );
    // A terminal that says nothing is not assumed to be an old one.
    assert_eq!(
        tier_from_env(env(&[("COLORTERM", "yes")])),
        Tier::True,
        "an uninformative COLORTERM is not a claim about TERM"
    );
}

/// The process-wide theme is a value installed once, and the answer before
/// anything installs one is the design's own default rather than an absence.
///
/// Nothing here calls `install`, and that is the point: installation is
/// process-wide and the tests in this crate run side by side, so a test that
/// installed a palette would be a test that repainted another one's screen.
/// Every other test in this file holds its own [`Theme`] value instead, which is
/// why the type exists rather than three globals.
#[test]
fn the_default_theme_is_the_designed_one() {
    let t = theme();
    assert_eq!(t.palette(), INK);
    assert_eq!(t.tier(), Tier::True);
    assert!(t.color());
}

// ---------------------------------------------------------------------------
// The colour-vision simulation
// ---------------------------------------------------------------------------

/// Machado, Oliveira & Fernandes (2009), severity 1.0, for the two dichromacies
/// that matter here. Applied to linear light, which is the space they are defined
/// in.
const DEUTAN: [[f64; 3]; 3] = [
    [0.367_322, 0.860_646, -0.227_968],
    [0.280_085, 0.672_501, 0.047_413],
    [-0.011_820, 0.042_940, 0.968_881],
];

const PROTAN: [[f64; 3]; 3] = [
    [0.152_286, 1.052_583, -0.204_868],
    [0.114_503, 0.786_281, 0.099_216],
    [-0.003_882, -0.048_116, 1.051_998],
];

/// What `color` looks like through `matrix`.
///
/// The matrices can push a channel out of gamut; clamping is what every
/// implementation does and what the display would do anyway.
fn simulate(color: Rgb, matrix: [[f64; 3]; 3]) -> Rgb {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let unlin = |c: f64| {
        let c = if c <= 0.003_130_8 {
            c * 12.92
        } else {
            1.055 * c.powf(1.0 / 2.4) - 0.055
        };
        (c.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    let v = [lin(color.r), lin(color.g), lin(color.b)];
    let out = matrix.map(|row| row[0] * v[0] + row[1] * v[1] + row[2] * v[2]);
    Rgb::new(unlin(out[0]), unlin(out[1]), unlin(out[2]))
}

/// An [`Ink`] back to channels, for the tests that measure one.
///
/// The indexed arm is resolved through the same cube the search picked from, so
/// a test can measure what a 256-colour terminal would actually be shown rather
/// than what the palette asked for.
fn ink_rgb(ink: Ink) -> Rgb {
    match ink {
        Ink::Rgb(c) => c,
        Ink::Indexed(i) => cube256(i),
        other => panic!("no channels behind {other:?}"),
    }
}

/// `#RRGGBB`, so a failing assertion names the colour a reader can look up.
fn hex(color: Rgb) -> String {
    format!("#{:02X}{:02X}{:02X}", color.r, color.g, color.b)
}

//! The palette: what a semantic [`Style`] is painted in.
//!
//! One copy of the values, and the one place the two front ends agree on what a
//! `Style` looks like. The plain front end writes the codes out as SGR
//! sequences, the TUI (through [`style_of`]) hands the same values to `ratatui`,
//! and neither derives a colour of its own -- a second mapping is how one front
//! end starts disagreeing with the other about what a `Reasoning` line is.
//!
//! # Two themes, and why the application cannot pick one by itself
//!
//! A terminal application does not paint a background: the frame is drawn on
//! whatever the user has set. That makes a palette a *pair* -- a foreground and
//! the background it was chosen against -- and only half of the pair is ours. So
//! there are two: [`INK`] for text on a dark screen, [`PAPER`] for text on a
//! light one, and neither is a tint of the other. A single palette cannot serve
//! both, which is what the theme before this got wrong: its body colour was
//! `#F8F8F2`, which is 12:1 on a dark background and 1.1:1 on a white one.
//!
//! # What decides each colour
//!
//! Three rules, in the order they bind:
//!
//! 1. **Contrast is a floor, not a goal.** Every text colour clears 4.5:1
//!    against the theme's reference background *and* against a band of
//!    neighbouring ones, because the user's background is not the reference. The
//!    tests in this module are that rule as arithmetic.
//! 2. **Colour never carries meaning alone.** Every styled thing on the screen
//!    has a glyph, a column or a word that says the same thing -- see
//!    [`crate::ui::glyphs`] -- so `NO_COLOR`, a monochrome terminal and
//!    red-green colour blindness all still read.
//! 3. **De-emphasis is a colour, never a modifier.** `SGR 2` renders as anything
//!    from "slightly grey" to "nothing at all" depending on the terminal, and it
//!    stacks unpredictably with the colour under it. Secondary text is painted
//!    in [`Palette::muted`]; nothing is dimmed by asking for less of the same
//!    colour.
//!
//! # The tiers
//!
//! Truecolour is not universal, so a palette is really four palettes: the exact
//! channels, a 256-colour rounding, a sixteen-colour mapping onto the terminal's
//! own slots, and `NO_COLOR`'s nothing at all. [`Theme`] holds which one is in
//! effect and answers with an [`Ink`] the backends can each spell their own way.
//!
//! The sixteen-colour tier is the one that stops meaning anything, and the
//! honest answer there is to stop guessing: the terminal already decided light
//! or dark when the user chose their colour scheme, so the mapping is onto the
//! conventional slots rather than onto our own colours.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style as RStyle};

use crate::ui::cell::Style;

/// One colour: the red, green and blue the terminal is asked for.
///
/// Kept as the three channels rather than as a string, because the three are what
/// each backend needs: an SGR sequence spells them out, and `ratatui` takes them
/// as a [`Color::Rgb`]. A string would be one of the two formats and a parse for
/// the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Rgb {
    pub(crate) r: u8,
    pub(crate) g: u8,
    pub(crate) b: u8,
}

impl Rgb {
    const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Relative luminance, WCAG 2.x: the sRGB channels linearised, then weighted.
    ///
    /// The one function the contrast and colour-vision tests are built on, and
    /// the reason it lives here rather than in a test: a reader checking whether
    /// a change to the palette is still legible wants the number the tests use,
    /// not a second implementation of it.
    pub(crate) fn luminance(self) -> f64 {
        let lin = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.r) + 0.7152 * lin(self.g) + 0.0722 * lin(self.b)
    }

    /// The WCAG contrast ratio between two colours: 1.0 for two colours that are
    /// the same, 21.0 for black on white. Order does not matter.
    ///
    /// The tests are the only caller, and a reader changing a palette is the
    /// reader this exists for: the number is recorded in the design document, and
    /// this is the same number rather than a second implementation of it.
    #[cfg(test)]
    pub(crate) fn contrast(self, other: Rgb) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        let (hi, lo) = if a > b { (a, b) } else { (b, a) };
        (hi + 0.05) / (lo + 0.05)
    }
}

/// The colours a session paints with, and the jobs they do.
///
/// Six colours, of which five are text: one per [`Style`] job, with `Dim` and
/// `Reasoning` deliberately sharing one. The sixth is [`Palette::rule`], which is
/// geometry -- a border, a separator -- and is the only colour here allowed under
/// the contrast floor, because no one reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Palette {
    /// Everything that is not set apart: the answer itself.
    pub(crate) fg: Rgb,
    /// Everything the machine says about itself: the thinking block, tool
    /// results, step children, notices, fold lines, inline code, the status
    /// line, and the second column of a menu row. What [`Style::Dim`] and
    /// [`Style::Reasoning`] are painted in.
    pub(crate) muted: Rgb,
    /// Attention without failure: a tool call, a question, the working
    /// indicator, a task in hand.
    pub(crate) signal: Rgb,
    /// A change's added lines, and a step that finished well.
    pub(crate) ok: Rgb,
    /// Failures, refusals, and the lines a change takes out.
    pub(crate) bad: Rgb,
    /// Geometry: the box's rule, a border, a separator. Not text.
    pub(crate) rule: Rgb,
    /// The background this palette was calibrated against. Not painted -- the
    /// terminal owns the background -- but carried, because a palette that
    /// forgot what its own foregrounds were chosen to sit on is a palette a
    /// reader has to take on faith.
    pub(crate) background: Rgb,
}

/// The dark theme: text on a screen that is nearly black.
///
/// Calibrated against `#16161E`, and required to hold on the other dark
/// backgrounds a terminal is plausibly set to (`#000000`, `#0D1117`, and
/// Dracula's lighter `#282A36`) -- which is why nothing here is as light as pure
/// white and `bad` is deeper than the usual "red is the loud one".
///
/// The green is deliberately the bright one and the red the deep one, which is
/// the opposite of the usual convention and is the point: to a reader with
/// deuteranopia green and red are the same colour, and what is left to tell an
/// added line from a removed one is that one is lighter than the other. They are
/// 1.78:1 apart in tone, so that difference survives.
pub(crate) const INK: Palette = Palette {
    fg: Rgb::new(0xd7, 0xda, 0xe4),
    muted: Rgb::new(0x9a, 0xa1, 0xb5),
    signal: Rgb::new(0xe2, 0xb1, 0x5c),
    ok: Rgb::new(0x8f, 0xd9, 0xa3),
    bad: Rgb::new(0xe9, 0x70, 0x7e),
    rule: Rgb::new(0x3a, 0x3f, 0x52),
    background: Rgb::new(0x16, 0x16, 0x1e),
};

/// The light theme: text on a screen that is nearly white.
///
/// The same six jobs, re-derived rather than lightened: on white there is room
/// above the text and none below it, so every colour is dark and the hierarchy is
/// carried by how dark. `bad` is the deepest of the three accents (10.9:1) and
/// `ok` the lightest that still clears the floor (5.9:1), which is the same tone
/// split the dark theme has, read from the other end -- 1.83:1 apart, against the
/// dark theme's 1.78:1.
///
/// The floor is the binding constraint over the whole family of light
/// backgrounds, not just over white: a themed editor's light grey is darker than
/// white, and every accent has to clear 4.5:1 there too. That is why `ok` is a
/// forest green rather than the brighter one the design started from, and why
/// `bad` is a deep oxblood: the pair has to stay two colours after the room the
/// light background took away.
pub(crate) const PAPER: Palette = Palette {
    fg: Rgb::new(0x23, 0x25, 0x2e),
    muted: Rgb::new(0x5c, 0x61, 0x72),
    signal: Rgb::new(0x8a, 0x5c, 0x06),
    ok: Rgb::new(0x1a, 0x72, 0x45),
    bad: Rgb::new(0x7a, 0x11, 0x24),
    rule: Rgb::new(0xc9, 0xcc, 0xd6),
    background: Rgb::new(0xff, 0xff, 0xff),
};

/// The themes by name, in the order they would be listed to a user.
pub(crate) const NAMED: [Name; 2] = [Name::Ink, Name::Paper];

/// What the user asked for, before the environment is consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Choice {
    /// Pick from the terminal: `COLORFGBG`, then a background query, then ink.
    #[default]
    Auto,
    /// The palette by name -- `ink`, `paper`, or an error.
    Named(Name),
}

/// One of [`NAMED`], resolved rather than parsed: what a setting becomes once it
/// has been checked, so a caller cannot carry a string nothing answers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Name {
    Ink,
    Paper,
}

impl Name {
    /// The palette by name, or `None` for a name nothing answers to. The
    /// caller decides whether that is a typo to report or a default to fall back
    /// on; this end only answers.
    pub(crate) fn parse(name: &str) -> Option<Self> {
        match name {
            "ink" => Some(Self::Ink),
            "paper" => Some(Self::Paper),
            _ => None,
        }
    }

    pub(crate) fn palette(self) -> Palette {
        match self {
            Self::Ink => INK,
            Self::Paper => PAPER,
        }
    }

    /// The name as the user would write it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ink => "ink",
            Self::Paper => "paper",
        }
    }
}

impl Choice {
    /// What `--theme` and the `theme` setting accept, resolved. `auto` is the
    /// only un-named value, and an empty string is `auto` rather than an error:
    /// a settings file with `"theme": ""` asked for the default.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value.trim() {
            "" | "auto" => Some(Self::Auto),
            name => Name::parse(name).map(Self::Named),
        }
    }
}

/// How many colours the terminal can take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tier {
    /// `38;2;r;g;b`: the exact channels. What the palettes were designed at.
    True,
    /// `38;5;n`: the nearest xterm palette entry, which loses chroma and keeps
    /// the relationships.
    Ansi256,
    /// The sixteen slots the terminal's own colour scheme fills. Here the
    /// palette stops meaning anything and defers: see [`Theme::ink`].
    Ansi16,
}

/// A colour, in the shape the tier in effect can spell it.
///
/// The backends are two -- SGR bytes for the plain front end, `ratatui`'s
/// [`Color`] for the screen -- and neither should have to know which tier it is
/// painting under, so the decision is made once, here, and this is the answer
/// they share.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Ink {
    /// The exact channels.
    Rgb(Rgb),
    /// An xterm palette index, 0..=255.
    Indexed(u8),
    /// One of the terminal's sixteen slots, 0..=15.
    Ansi(u8),
    /// No colour asked for at all: whatever the terminal is set to. What `fg`
    /// is at the sixteen-colour tier, and what every style is under `NO_COLOR`.
    Terminal,
}

impl Ink {
    /// The palette entry nearest `color` in the xterm 256-colour cube, for the
    /// middle tier.
    ///
    /// Nearest by CIELAB distance rather than by channel arithmetic: the cube is
    /// a perceptual layout, and picking the closest corner in RGB is how a blue
    /// lands on a grey. The first sixteen entries are skipped -- they are the
    /// terminal's own slots, the ones the tier below this one exists for, and
    /// nothing here may claim them.
    fn nearest256(color: Rgb) -> Self {
        Self::nearest256_where(color, |_| true)
    }

    /// The same, restricted to the entries whose luminance passes `keep`.
    ///
    /// This is what keeps the added/removed tone split through the rounding. The
    /// cube is coarse and a colour sits between two neighbours of very different
    /// lightness; nearest-by-distance happily rounds a deep red *up* into a pale
    /// one, and a pale red is the same colour as a green to a reader with
    /// deuteranopia. So the pair that carries that meaning is rounded with a
    /// bound: the green may come out lighter than it was, the red darker, and
    /// neither may lose the ground the truecolour pair holds.
    ///
    /// A `keep` nothing satisfies falls back to the unrestricted search, because
    /// an entry that is merely off-tone is better than no colour at all.
    fn nearest256_where(color: Rgb, keep: impl Fn(f64) -> bool) -> Self {
        let pick = |keep: &dyn Fn(f64) -> bool| {
            let (mut best, mut distance) = (None, f64::MAX);
            for index in 16..=255u8 {
                let rgb = cube256(index);
                if !keep(rgb.luminance()) {
                    continue;
                }
                let d = lab_of(rgb).distance(lab_of(color));
                if d < distance {
                    distance = d;
                    best = Some(index);
                }
            }
            best
        };
        // The closure is passed twice because the fallback needs a second,
        // unrestricted pass and `keep` is moved into the first one.
        match pick(&keep) {
            Some(index) => Ink::Indexed(index),
            None => match pick(&(|_: f64| true)) {
                Some(index) => Ink::Indexed(index),
                None => Ink::Indexed(16),
            },
        }
    }
}

/// The xterm 256-colour palette entry `index` names.
///
/// The first sixteen are the terminal's own and are never reached for (see
/// [`Ink::nearest256`]); 16..=231 are a 6x6x6 cube on `#000000`, `#5F5F5F`,
/// `#878787`, `#AFAFAF`, `#D7D7D7`, `#FFFFFF`; 232..=255 are the greyscale
/// ramp from `#080808` in steps of ten.
fn cube256(index: u8) -> Rgb {
    let steps = [0, 95, 135, 175, 215, 255];
    match index {
        0..=15 => {
            let base = [
                (0, 0, 0),
                (128, 0, 0),
                (0, 128, 0),
                (128, 128, 0),
                (0, 0, 128),
                (128, 0, 128),
                (0, 128, 128),
                (192, 192, 192),
                (128, 128, 128),
                (255, 0, 0),
                (0, 255, 0),
                (255, 255, 0),
                (0, 0, 255),
                (255, 0, 255),
                (0, 255, 255),
                (255, 255, 255),
            ][index as usize];
            Rgb::new(base.0, base.1, base.2)
        }
        16..=231 => {
            let i = index as usize - 16;
            Rgb::new(steps[i / 36], steps[(i / 6) % 6], steps[i % 6])
        }
        _ => {
            let v = 8 + (index as usize - 232) * 10;
            let v = v as u8;
            Rgb::new(v, v, v)
        }
    }
}

/// A colour in CIELAB, which is where "nearest" means what a reader means by it.
///
/// Used only by [`Ink::nearest256`] and the colour-vision tests. The
/// transformation is the standard sRGB → XYZ (D65) → Lab chain; it is written out
/// rather than pulled in because it is twenty lines and a dependency for twenty
/// lines of arithmetic is the wrong trade for a binary this size.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Lab {
    l: f64,
    a: f64,
    b: f64,
}

impl Lab {
    /// How far apart two colours look. Not scaled to anything: the only use is
    /// picking a minimum and asserting a floor, and both are self-consistent.
    pub(crate) fn distance(self, other: Lab) -> f64 {
        ((self.l - other.l).powi(2) + (self.a - other.a).powi(2) + (self.b - other.b).powi(2))
            .sqrt()
    }
}

pub(crate) fn lab_of(color: Rgb) -> Lab {
    let lin = |c: u8| {
        let c = f64::from(c) / 255.0;
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    let (r, g, b) = (lin(color.r), lin(color.g), lin(color.b));
    let x = (0.4124 * r + 0.3576 * g + 0.1805 * b) / 0.95047;
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let z = (0.0193 * r + 0.1192 * g + 0.9505 * b) / 1.08883;
    let f = |t: f64| {
        if t > 0.008856 {
            t.cbrt()
        } else {
            7.787 * t + 16.0 / 116.0
        }
    };
    let (fx, fy, fz) = (f(x), f(y), f(z));
    Lab {
        l: 116.0 * fy - 16.0,
        a: 500.0 * (fx - fy),
        b: 200.0 * (fy - fz),
    }
}

/// The palette in effect, the tier it is being spelled at, and whether colour is
/// wanted at all.
///
/// A value rather than three globals, so that every combination of theme, tier
/// and `NO_COLOR` is a thing a test can hold and ask a question of. The process
/// holds one of these -- see [`install`] -- and the free functions below read it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Theme {
    palette: Palette,
    tier: Tier,
    color: bool,
}

impl Theme {
    pub(crate) const fn new(palette: Palette, tier: Tier, color: bool) -> Self {
        Self {
            palette,
            tier,
            color,
        }
    }

    /// The palette this theme is painting with, for a test that is about the
    /// colours rather than about the painting of them. `describe` reads the
    /// field directly; nothing in a running session needs to.
    #[cfg(test)]
    pub(crate) fn palette(&self) -> Palette {
        self.palette
    }

    /// The theme in one line, for `/debug`: which palette, at which tier, with
    /// colour on or off.
    ///
    /// The name is looked up from the palette rather than carried, so that a
    /// theme value cannot say it is one palette while holding another's colours.
    pub(crate) fn describe(&self) -> String {
        let name = NAMED
            .iter()
            .find(|n| n.palette() == self.palette)
            .map_or("custom", |n| n.as_str());
        let tier = match self.tier {
            Tier::True => "24-bit",
            Tier::Ansi256 => "256 colours",
            Tier::Ansi16 => "16 colours",
        };
        if self.color {
            format!("{name} · {tier}")
        } else {
            format!("{name} · no colour (NO_COLOR)")
        }
    }

    /// How many colours this theme may use. Read by the tests, which assert the
    /// tiers' own spellings; `describe` reads the field directly.
    #[cfg(test)]
    pub(crate) fn tier(&self) -> Tier {
        self.tier
    }

    /// Whether colour is wanted here: the terminal can show it, and `NO_COLOR`
    /// does not say otherwise.
    pub(crate) fn color(&self) -> bool {
        self.color
    }

    /// The ink one of the palette's six roles is painted in, at this tier.
    ///
    /// The sixteen-colour arm is the one worth reading twice: there, the roles
    /// stop being colours and become the *slots* a colour scheme conventionally
    /// fills -- bright black for secondary text, the four named hues for the
    /// three accents. The terminal's own scheme decides what those are, which is
    /// the whole point: at sixteen colours the user has already said what light
    /// or dark means, and a code that insists on its own channel values is a code
    /// that draws dark text on a dark screen.
    fn ink_of(&self, role: Role) -> Ink {
        if !self.color {
            return Ink::Terminal;
        }
        match self.tier {
            Tier::True => Ink::Rgb(role.color(self.palette)),
            // `Ok` and `Bad` are rounded with a bound rather than to the nearest
            // entry: they are the pair that has to stay two colours when the hue
            // is gone, and the cube would otherwise round the deep red up and the
            // bright green down until they met in the middle.
            Tier::Ansi256 => match role {
                Role::Ok => {
                    Ink::nearest256_where(self.palette.ok, |l| l >= self.palette.ok.luminance())
                }
                Role::Bad => {
                    Ink::nearest256_where(self.palette.bad, |l| l <= self.palette.bad.luminance())
                }
                other => Ink::nearest256(other.color(self.palette)),
            },
            Tier::Ansi16 => match role {
                // No colour at all: the terminal's default foreground, which is
                // the one thing on the screen that is guaranteed to contrast
                // with the background the terminal itself chose.
                Role::Fg => Ink::Terminal,
                Role::Muted | Role::Rule => Ink::Ansi(8),
                Role::Signal => Ink::Ansi(3),
                Role::Ok => Ink::Ansi(2),
                Role::Bad => Ink::Ansi(1),
            },
        }
    }

    /// The ink a `Style` is painted in. `Dim` and `Reasoning` share one, on
    /// purpose: what tells thinking apart from a tool result is the rule in its
    /// gutter, which is a difference in the layout rather than in the colours,
    /// and so one that survives both a terminal that ignores colour and a reader
    /// who has turned it off.
    pub(crate) fn ink(&self, style: Style) -> Ink {
        self.ink_of(match style {
            Style::Plain => Role::Fg,
            Style::Dim | Style::Reasoning => Role::Muted,
            Style::Yellow => Role::Signal,
            Style::Green => Role::Ok,
            Style::Red => Role::Bad,
        })
    }

    /// The ink for geometry, which is not a `Style`: a border is not text and
    /// the cell layer has no opinion about it.
    pub(crate) fn rule(&self) -> Ink {
        self.ink_of(Role::Rule)
    }

    /// The SGR escape that opens `style`: the plain front end's backend, and the
    /// only place a `Style` becomes bytes.
    ///
    /// The weight comes from [`Style::is_bold`] -- the same rule [`Theme::style_of`]
    /// reads -- so the two front ends cannot disagree on which styles carry it.
    /// It is dropped at the sixteen-colour tier, where `SGR 1` usually *brightens*
    /// the foreground rather than thickening it: a bold green there is a
    /// different colour than the one the mapping above asked for.
    pub(crate) fn style_code(&self, style: Style) -> String {
        if !self.color {
            return String::new();
        }
        let mut out = String::new();
        if style.is_bold() && self.tier != Tier::Ansi16 {
            out.push_str("\x1b[1m");
        }
        out.push_str(&self.sgr(self.ink(style)));
        out
    }

    /// Our style, as `ratatui` sees it: the same palette the SGR path writes out,
    /// handed over as data.
    pub(crate) fn style_of(&self, style: Style) -> RStyle {
        let mut s = RStyle::new();
        if self.color {
            s = s.fg(self.color_of(style));
        }
        if style.is_bold() && self.color && self.tier != Tier::Ansi16 {
            s = s.add_modifier(Modifier::BOLD);
        }
        s
    }

    /// The theme's secondary weight, for the chrome that is not a cell: the box's
    /// rule and the prompt a front end draws for itself.
    pub(crate) fn rule_style(&self) -> RStyle {
        if self.color {
            RStyle::new().fg(self.color_of_ink(self.rule()))
        } else {
            RStyle::new()
        }
    }

    /// An [`Ink`] as `ratatui` spells it.
    ///
    /// The sixteen-colour tier is handed over as `ratatui`'s *named* colours
    /// rather than as indexed ones, so that it comes out as `31` and `91` -- the
    /// sequences every terminal since the VT100 has understood -- rather than as
    /// `38;5;1`, which is the same colour but only on a terminal that has the
    /// extended palette. The tier exists for terminals that may not.
    pub(crate) fn color_of_ink(&self, ink: Ink) -> Color {
        match ink {
            Ink::Rgb(c) => Color::Rgb(c.r, c.g, c.b),
            Ink::Indexed(i) => Color::Indexed(i),
            Ink::Ansi(i) => match i {
                0 => Color::Black,
                1 => Color::Red,
                2 => Color::Green,
                3 => Color::Yellow,
                4 => Color::Blue,
                5 => Color::Magenta,
                6 => Color::Cyan,
                7 => Color::Gray,
                8 => Color::DarkGray,
                9 => Color::LightRed,
                10 => Color::LightGreen,
                11 => Color::LightYellow,
                12 => Color::LightBlue,
                13 => Color::LightMagenta,
                14 => Color::LightCyan,
                _ => Color::White,
            },
            Ink::Terminal => Color::Reset,
        }
    }

    /// The colour a `Style` is painted in.
    pub(crate) fn color_of(&self, style: Style) -> Color {
        self.color_of_ink(self.ink(style))
    }

    /// The SGR for an [`Ink`], for the bytes that are not a cell: the prompt box's
    /// placeholder and the secret prompt, which are written by the front end that
    /// owns the box rather than through a [`Cell`](crate::ui::cell::Cell).
    ///
    /// Empty under `NO_COLOR`, like every other escape this module produces: a
    /// caller writing a prompt of its own is not a reason for a colour code to
    /// escape the rule.
    pub(crate) fn sgr(&self, ink: Ink) -> String {
        if !self.color {
            return String::new();
        }
        match ink {
            Ink::Rgb(c) => format!("\x1b[38;2;{};{};{}m", c.r, c.g, c.b),
            Ink::Indexed(i) => format!("\x1b[38;5;{i}m"),
            Ink::Ansi(i) => {
                if i < 8 {
                    format!("\x1b[{}m", 30 + i)
                } else {
                    format!("\x1b[{}m", 90 + i - 8)
                }
            }
            Ink::Terminal => String::new(),
        }
    }

    /// The escape that closed the last style: SGR 0, which is every attribute off
    /// rather than the one this palette opened.
    pub(crate) const RESET: &'static str = "\x1b[0m";
}

/// One of the six jobs a colour in the palette does. A `Role` rather than a
/// `Style` because two styles share a colour and geometry has no style at all.
#[derive(Debug, Clone, Copy)]
enum Role {
    Fg,
    Muted,
    Signal,
    Ok,
    Bad,
    Rule,
}

impl Role {
    fn color(self, palette: Palette) -> Rgb {
        match self {
            Self::Fg => palette.fg,
            Self::Muted => palette.muted,
            Self::Signal => palette.signal,
            Self::Ok => palette.ok,
            Self::Bad => palette.bad,
            Self::Rule => palette.rule,
        }
    }
}

/// The theme in effect.
static THEME: OnceLock<Theme> = OnceLock::new();

/// Install `theme` as the process's theme. The first call wins; a second is a
/// no-op rather than a panic, so that a caller which is not `main` -- a test
/// binary, an embedding -- cannot bring the process down by asking twice.
pub(crate) fn install(theme: Theme) {
    let _ = THEME.set(theme);
}

/// The theme in effect, or the default when nothing has installed one: ink, at
/// the true-colour tier, with colour on. That is the answer in a test and the
/// answer for a caller that only wants to render something.
pub(crate) fn theme() -> &'static Theme {
    THEME.get_or_init(|| Theme::new(INK, Tier::True, true))
}

/// The SGR escape that opens `style`.
pub(crate) fn style_code(style: Style) -> String {
    theme().style_code(style)
}

/// Our style, as `ratatui` sees it.
pub(crate) fn style_of(style: Style) -> RStyle {
    theme().style_of(style)
}

/// The theme's rule, as `ratatui` sees it: the box's border and anything else
/// that is geometry rather than text.
pub(crate) fn rule_style() -> RStyle {
    theme().rule_style()
}

/// The escape that closes every style.
pub(crate) const RESET: &str = Theme::RESET;

/// The tier this environment can be spelled at, from the two variables that say
/// anything about it.
///
/// `COLORTERM` is the honest signal -- it exists to say "24-bit colour" and
/// nothing else -- and `TERM`'s `256color` suffix is the conventional one.
/// Checked in that order rather than the other way round, because a terminal
/// which sets `TERM=xterm-256color` and `COLORTERM=truecolor` is telling the
/// truth twice and only one of the two is a capability.
///
/// **An uninformative environment gets truecolour, which is the interesting
/// decision here.** A terminal that sets neither variable is far more likely to
/// be a modern embedder -- Windows Terminal, an editor's panel, a web terminal --
/// than a VT100, and the two ways of being wrong are not symmetric: a truecolour
/// sequence on a terminal that cannot show it is *ignored*, so the text comes out
/// in the terminal's own colour, while sixteen-colour slots on a terminal that
/// can show more throws away the entire palette. The tier is a guess, so it is
/// made in the direction whose failure is legible.
pub(crate) fn tier_from_env(get: impl Fn(&str) -> Option<String>) -> Tier {
    match get("COLORTERM").unwrap_or_default().as_str() {
        "truecolor" | "24bit" => return Tier::True,
        _ => {}
    }
    match get("TERM").unwrap_or_default().as_str() {
        "" => Tier::True,
        t if t.contains("256") => Tier::Ansi256,
        _ => Tier::Ansi16,
    }
}

/// Which theme a bare `auto` should resolve to, from what the environment says
/// about the background.
///
/// `COLORFGBG` is the old convention -- `fg;bg`, either as an index or a name --
/// and it is free to read, so it is read first; a terminal that sets it is a
/// terminal that has told us the answer. The second opinion is a real query to
/// the terminal ([`ThemeChoice::detect`](crate::ui::theme::detect)), which only
/// some terminals answer and which costs a round trip, so it is the fallback
/// rather than the first move.
///
/// Anything unrecognised answers `None`, which is "no opinion" rather than
/// "dark": the caller decides what an uninformative environment means.
pub(crate) fn theme_from_colorfgbg(value: &str) -> Option<Name> {
    let bg = value.rsplit(';').next()?.trim().to_ascii_lowercase();
    let light = match bg.as_str() {
        // The conventional slot numbers whose meaning is not in doubt: white and
        // bright white on a scheme that fills them.
        "7" | "15" | "white" | "brightwhite" => true,
        "0" | "8" | "black" | "brightblack" => false,
        // Anything else is either an index this cannot read or a 24-bit value,
        // and guessing at it is how a light terminal gets the dark palette.
        _ => return None,
    };
    Some(if light { Name::Paper } else { Name::Ink })
}

/// Which theme a background colour asks for.
///
/// The one decision a query to the terminal feeds: a background lighter than
/// mid-grey is a light terminal, and wants the palette with the dark text on it.
/// Mid-grey rather than some cleverer threshold because the two palettes are
/// calibrated at the extremes and the space between them is small -- a reader who
/// has a genuinely mid-grey background is a reader who should pass `--theme`.
pub(crate) fn theme_from_background(background: Rgb) -> Name {
    if background.luminance() > 0.5 {
        Name::Paper
    } else {
        Name::Ink
    }
}

/// The names `--theme` and the `theme` setting accept, for the message that says
/// so when one of them is not among them.
pub(crate) fn theme_names() -> String {
    let mut names: Vec<&str> = NAMED.iter().map(|n| n.as_str()).collect();
    names.push("auto");
    names.join(", ")
}

/// The theme a choice resolves to, given what the environment already said and
/// what a terminal answered when asked.
///
/// Split from the reading of either, so the whole decision -- a flag, a settings
/// file, `COLORFGBG`, a query, and the default -- is a pure function of four
/// values and every branch of it is a test.
pub(crate) fn resolve(choice: Choice, colorfgbg: Option<&str>, queried: Option<Name>) -> Name {
    match choice {
        Choice::Named(name) => name,
        Choice::Auto => colorfgbg
            .and_then(theme_from_colorfgbg)
            .or(queried)
            // No opinion from anywhere is a dark terminal: it is what most
            // terminals are, and ink is the theme the screen was designed at.
            .unwrap_or(Name::Ink),
    }
}

#[cfg(test)]
mod tests;
